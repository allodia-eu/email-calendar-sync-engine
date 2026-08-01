//! Little-endian read/write cursors over MAPI wire buffers.
//!
//! Every MAPI structure is little-endian and length-prefixed, which makes the
//! read side a hostile-input parser: a truncated or lying length must produce
//! an error, never a panic or an out-of-bounds index. So `Reader` returns
//! `Result` from every method and never indexes directly. That discipline is
//! kept even in a throwaway spike, because this is the exact code that would
//! later face a server we do not control.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// Wanted `need` bytes at `at`, but only `have` remained.
    Truncated { at: usize, need: usize, have: usize },
    /// A string field ran to the end of the buffer without its terminator.
    Unterminated { at: usize },
    /// A UTF-16 field held an unpaired surrogate.
    BadUtf16 { at: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { at, need, have } => {
                write!(f, "truncated at {at}: need {need} bytes, {have} remain")
            }
            Self::Unterminated { at } => write!(f, "unterminated string at {at}"),
            Self::BadUtf16 { at } => write!(f, "invalid UTF-16 at {at}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Appends little-endian fields to a byte buffer.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }

    /// A null-terminated ASCII string, as used by `Connect`'s `UserDn`
    /// ([MS-OXCMAPIHTTP] §2.2.4.1.1) and `RopLogon`'s `Essdn`.
    pub fn ascii_z(&mut self, v: &str) -> &mut Self {
        self.buf.extend_from_slice(v.as_bytes());
        self.buf.push(0);
        self
    }

    /// Overwrite a previously reserved little-endian u16. Used for `RopSize`,
    /// which cannot be known until the ROP list has been serialized.
    pub fn patch_u16(&mut self, at: usize, v: u16) {
        self.buf[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Reads little-endian fields from a byte slice, fallibly.
#[derive(Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(Error::Truncated {
                at: self.pos,
                need: n,
                have: self.remaining(),
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }

    pub fn rest(&mut self) -> &'a [u8] {
        let out = &self.buf[self.pos..];
        self.pos = self.buf.len();
        out
    }

    /// A null-terminated ASCII string. Non-ASCII bytes are kept lossily; the
    /// terminator is consumed.
    pub fn ascii_z(&mut self) -> Result<String> {
        let start = self.pos;
        let end = self.buf[self.pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or(Error::Unterminated { at: start })?;
        let s = String::from_utf8_lossy(&self.buf[self.pos..self.pos + end]).into_owned();
        self.pos += end + 1;
        Ok(s)
    }

    /// A null-terminated UTF-16LE string, as used by `Connect`'s `DisplayName`
    /// response field. The terminator is two zero bytes on an even boundary.
    pub fn utf16_z(&mut self) -> Result<String> {
        let start = self.pos;
        let mut units = Vec::new();
        loop {
            let u = self.u16().map_err(|_| Error::Unterminated { at: start })?;
            if u == 0 {
                break;
            }
            units.push(u);
        }
        String::from_utf16(&units).map_err(|_| Error::BadUtf16 { at: start })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_scalars_and_strings() {
        let mut w = Writer::new();
        w.u8(0x01)
            .u16(0x0203)
            .u32(0x0405_0607)
            .u64(0x0809_0a0b_0c0d_0e0f);
        w.ascii_z("/o=Gromox");
        let buf = w.finish();

        let mut r = Reader::new(&buf);
        assert_eq!(r.u8().unwrap(), 0x01);
        assert_eq!(r.u16().unwrap(), 0x0203);
        assert_eq!(r.u32().unwrap(), 0x0405_0607);
        assert_eq!(r.u64().unwrap(), 0x0809_0a0b_0c0d_0e0f);
        assert_eq!(r.ascii_z().unwrap(), "/o=Gromox");
        assert!(r.is_empty());
    }

    #[test]
    fn little_endian_is_on_the_wire_not_native() {
        let mut w = Writer::new();
        w.u32(0x0037_001F); // PidTagSubject, canonical notation
        assert_eq!(w.finish(), vec![0x1F, 0x00, 0x37, 0x00]);
    }

    #[test]
    fn patch_u16_rewrites_a_reserved_slot() {
        let mut w = Writer::new();
        w.u16(0); // reserved for RopSize
        w.u8(0xFE);
        let at = 0;
        let len = w.len() as u16;
        w.patch_u16(at, len);
        assert_eq!(w.finish(), vec![0x03, 0x00, 0xFE]);
    }

    #[test]
    fn utf16_z_reads_a_display_name() {
        let mut w = Writer::new();
        for u in "alice".encode_utf16() {
            w.u16(u);
        }
        w.u16(0);
        let buf = w.finish();
        assert_eq!(Reader::new(&buf).utf16_z().unwrap(), "alice");
    }

    // The reason every read returns Result: this is a parser over bytes from a
    // server we do not control. None of these may panic.
    #[test]
    fn hostile_input_errors_never_panics() {
        assert!(Reader::new(&[]).u8().is_err());
        assert!(Reader::new(&[0x01]).u32().is_err());
        assert!(Reader::new(&[0x01, 0x02, 0x03]).u64().is_err());
        assert!(Reader::new(b"no terminator").ascii_z().is_err());
        assert!(Reader::new(&[0x41]).utf16_z().is_err()); // odd length
        assert!(Reader::new(&[0xFF, 0xFF]).bytes(9999).is_err());

        // A lying length prefix must not index out of bounds.
        let mut r = Reader::new(&[0x10, 0x00]);
        let n = r.u16().unwrap() as usize;
        assert!(r.bytes(n).is_err());
    }

    #[test]
    fn unpaired_surrogate_is_an_error_not_a_panic() {
        // 0xD800 with no low surrogate following.
        let buf = [0x00, 0xD8, 0x00, 0x00];
        assert!(Reader::new(&buf).utf16_z().is_err());
    }
}
