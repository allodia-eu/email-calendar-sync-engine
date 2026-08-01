//! The `Execute` request type ([MS-OXCMAPIHTTP] §2.2.4.2) and the `RopBuffer`
//! framing inside it ([MS-OXCROPS] §2.2.1).
//!
//! The framing is where implementations go wrong, so the three rules are stated
//! here rather than left implicit:
//!
//! 1. **`RopSize` counts itself and the ROP list — not the handle table.** The
//!    spec's wording is "the size of *both this field and the RopsList field*".
//!    Writing `RopsList.len()` is the classic off-by-two.
//! 2. **The handle table is sized by `max(index) + 1`, not by ROP count.** Its
//!    length is whatever remains after `RopSize` bytes, so a wrong size silently
//!    reinterprets the boundary rather than erroring.
//! 3. **Handles never appear inside a ROP body.** Every ROP carries a *1-byte
//!    index* into the u32 table. Writing the handle itself is failure mode (a).
//!
//! Because the server processes ROPs in order and updates the table in place,
//! one ROP may consume a handle an earlier ROP in the *same* buffer produced.
//! That in-buffer chaining is what makes a folder walk one round trip instead
//! of three, and whether a given server honours it is a live measurement.

use crate::cursor::{Reader, Result, Writer};

/// An unowned handle slot ([MS-OXCDATA] §2.3).
pub const HANDLE_NONE: u32 = 0xFFFF_FFFF;

/// `RPC_HEADER_EXT.Flags` ([MS-OXCRPC] §2.2.2.1).
pub const RHE_COMPRESSED: u16 = 0x0001;
pub const RHE_XOR_MAGIC: u16 = 0x0002;
pub const RHE_LAST: u16 = 0x0004;

/// `Execute`'s `Flags` — *not* the same field as `Connect`'s. Here bit 0 asks
/// the server not to compress the response payload and bit 1 not to obfuscate
/// it with the 0xA5 XOR. Setting both is what keeps the LZ77+DIRECT2
/// decompressor and the XOR out of this spike entirely.
pub const EXEC_NO_COMPRESSION: u32 = 0x0000_0001;
pub const EXEC_NO_XOR_MAGIC: u32 = 0x0000_0002;

/// Cap on the response ROP buffer. Too small yields `RopBufferTooSmall` (0xFF)
/// carrying the size actually needed.
pub const MAX_ROP_OUT: u32 = 0x0001_0000;

/// A ROP list plus the handle table it indexes into.
#[derive(Debug, Default, Clone)]
pub struct RopBuffer {
    pub rops: Vec<u8>,
    pub handles: Vec<u32>,
}

impl RopBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ensure slot `index` exists, filling any gap with `HANDLE_NONE`.
    pub fn ensure_handle(&mut self, index: u8) -> &mut Self {
        let need = index as usize + 1;
        if self.handles.len() < need {
            self.handles.resize(need, HANDLE_NONE);
        }
        self
    }

    pub fn set_handle(&mut self, index: u8, handle: u32) -> &mut Self {
        self.ensure_handle(index);
        self.handles[index as usize] = handle;
        self
    }

    pub fn push_rop(&mut self, bytes: &[u8]) -> &mut Self {
        self.rops.extend_from_slice(bytes);
        self
    }

    /// Serialize to `RPC_HEADER_EXT || RopSize || RopsList || HandleTable`.
    pub fn serialize(&self) -> Vec<u8> {
        let mut payload = Writer::new();
        // RopSize covers itself (2) plus the ROP list.
        payload.u16((self.rops.len() + 2) as u16);
        payload.bytes(&self.rops);
        for h in &self.handles {
            payload.u32(*h);
        }
        let payload = payload.finish();

        let mut out = Writer::new();
        out.u16(0x0000) // Version
            .u16(RHE_LAST)
            .u16(payload.len() as u16) // Size
            .u16(payload.len() as u16); // SizeActual (uncompressed)
        out.bytes(&payload);
        out.finish()
    }

    /// Parse a server `RopBuffer`. Returns the ROP response bytes and the
    /// updated handle table.
    ///
    /// Fails loudly if the server compressed or obfuscated the payload rather
    /// than mis-parsing it — this spike asks for neither, and silently decoding
    /// garbage is worse than an error.
    pub fn parse(buf: &[u8]) -> Result<(Vec<u8>, Vec<u32>)> {
        let mut r = Reader::new(buf);
        let _version = r.u16()?;
        let flags = r.u16()?;
        let size = r.u16()? as usize;
        let _size_actual = r.u16()?;

        if flags & (RHE_COMPRESSED | RHE_XOR_MAGIC) != 0 {
            return Err(crate::cursor::Error::Truncated {
                at: 2,
                need: 0,
                have: flags as usize,
            });
        }

        let payload = r.bytes(size)?;
        let mut p = Reader::new(payload);
        let rop_size = p.u16()? as usize;
        // RopSize includes its own 2 bytes.
        let rops = p.bytes(rop_size.saturating_sub(2))?.to_vec();

        let mut handles = Vec::new();
        while p.remaining() >= 4 {
            handles.push(p.u32()?);
        }
        Ok((rops, handles))
    }
}

/// `Execute` request body ([MS-OXCMAPIHTTP] §2.2.4.2.1): Flags, RopBufferSize,
/// RopBuffer, MaxRopOut, AuxiliaryBufferSize, AuxiliaryBuffer.
pub fn execute_request(rop_buffer: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(EXEC_NO_COMPRESSION | EXEC_NO_XOR_MAGIC)
        .u32(rop_buffer.len() as u32)
        .bytes(rop_buffer)
        .u32(MAX_ROP_OUT)
        .u32(0); // AuxiliaryBufferSize — empty, same as Connect
    w.finish()
}

/// `Execute` success response body ([MS-OXCMAPIHTTP] §2.2.4.2.2).
#[derive(Debug)]
pub struct ExecuteResponse {
    pub status_code: u32,
    pub error_code: u32,
    pub rop_buffer: Vec<u8>,
}

impl ExecuteResponse {
    pub fn parse(body: &[u8]) -> Result<Self> {
        let mut r = Reader::new(body);
        let status_code = r.u32()?;
        if status_code != 0 {
            // §2.2.4.2.3: a failure body stops after StatusCode.
            return Ok(Self {
                status_code,
                error_code: 0,
                rop_buffer: Vec::new(),
            });
        }
        let error_code = r.u32()?;
        let _flags = r.u32()?;
        let size = r.u32()? as usize;
        let rop_buffer = r.bytes(size)?.to_vec();
        Ok(Self {
            status_code,
            error_code,
            rop_buffer,
        })
    }

    pub fn ok(&self) -> bool {
        self.status_code == 0 && self.error_code == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vector: one 3-byte ROP and a 1-entry handle table.
    #[test]
    fn rop_size_counts_itself_and_the_rop_list_only() {
        let mut b = RopBuffer::new();
        b.push_rop(&[0xFE, 0x00, 0x00]);
        b.set_handle(0, HANDLE_NONE);

        #[rustfmt::skip]
        let expected = vec![
            0x00, 0x00,             // Version
            0x04, 0x00,             // Flags = Last
            0x09, 0x00,             // Size = 2 (RopSize) + 3 (rops) + 4 (handles)
            0x09, 0x00,             // SizeActual
            0x05, 0x00,             // RopSize = 2 + 3  <- NOT 3, and NOT 9
            0xFE, 0x00, 0x00,       // RopsList
            0xFF, 0xFF, 0xFF, 0xFF, // ServerObjectHandleTable[0]
        ];
        assert_eq!(b.serialize(), expected);
    }

    #[test]
    fn handle_table_is_sized_by_max_index_not_rop_count() {
        // Three ROPs but only slot 2 touched: the table must be 3 long, with
        // the gap filled by HANDLE_NONE.
        let mut b = RopBuffer::new();
        b.push_rop(&[0x01]).push_rop(&[0x02]).push_rop(&[0x03]);
        b.set_handle(2, 0xDEAD_BEEF);
        assert_eq!(b.handles, vec![HANDLE_NONE, HANDLE_NONE, 0xDEAD_BEEF]);
    }

    #[test]
    fn round_trips_through_parse() {
        let mut b = RopBuffer::new();
        b.push_rop(&[0xFE, 0x00, 0x01, 0x02]);
        b.set_handle(0, 0x0000_002A);
        b.set_handle(1, HANDLE_NONE);

        let (rops, handles) = RopBuffer::parse(&b.serialize()).unwrap();
        assert_eq!(rops, vec![0xFE, 0x00, 0x01, 0x02]);
        assert_eq!(handles, vec![0x0000_002A, HANDLE_NONE]);
    }

    #[test]
    fn compressed_or_xored_payloads_are_refused_not_misparsed() {
        let mut w = Writer::new();
        w.u16(0).u16(RHE_LAST | RHE_COMPRESSED).u16(2).u16(2).u16(2);
        assert!(RopBuffer::parse(&w.finish()).is_err());

        let mut w = Writer::new();
        w.u16(0).u16(RHE_LAST | RHE_XOR_MAGIC).u16(2).u16(2).u16(2);
        assert!(RopBuffer::parse(&w.finish()).is_err());
    }

    #[test]
    fn execute_request_layout() {
        let body = execute_request(&[0xAA, 0xBB]);
        #[rustfmt::skip]
        let expected = vec![
            0x03, 0x00, 0x00, 0x00, // Flags = NoCompression | NoXorMagic
            0x02, 0x00, 0x00, 0x00, // RopBufferSize
            0xAA, 0xBB,             // RopBuffer
            0x00, 0x00, 0x01, 0x00, // MaxRopOut = 0x10000
            0x00, 0x00, 0x00, 0x00, // AuxiliaryBufferSize
        ];
        assert_eq!(body, expected);
    }

    #[test]
    fn execute_failure_body_is_truncated_after_status() {
        let mut w = Writer::new();
        w.u32(0x0000_000A);
        let resp = ExecuteResponse::parse(&w.finish()).unwrap();
        assert!(!resp.ok());
        assert_eq!(resp.status_code, 0x0000_000A);
    }

    #[test]
    fn hostile_buffers_never_panic() {
        for buf in [
            &b""[..],
            &[0x00, 0x00][..],
            &[0x00, 0x00, 0x04, 0x00, 0xFF, 0xFF, 0x00, 0x00][..], // Size lies
            &[0x00, 0x00, 0x04, 0x00, 0x02, 0x00, 0x02, 0x00, 0xFF, 0xFF][..], // RopSize lies
            &[0xFF; 32][..],
        ] {
            let _ = RopBuffer::parse(buf);
            let _ = ExecuteResponse::parse(buf);
        }
    }

    /// A RopSize smaller than 2 would underflow the ROP-list length; the
    /// saturating subtraction must keep that an empty list, not a panic.
    #[test]
    fn rop_size_below_two_does_not_underflow() {
        let mut w = Writer::new();
        w.u16(0).u16(RHE_LAST).u16(6).u16(6);
        w.u16(0).u32(HANDLE_NONE); // RopSize = 0
        let (rops, handles) = RopBuffer::parse(&w.finish()).unwrap();
        assert!(rops.is_empty());
        assert_eq!(handles, vec![HANDLE_NONE]);
    }
}
