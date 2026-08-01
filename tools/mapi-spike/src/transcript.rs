//! Raw request/response capture.
//!
//! Per AGENTS.md the offline provider fakes answer canned bytes regardless of
//! what they were sent, so they cannot catch a wrong *request* shape. These
//! byte pairs are the only artifact in the spike that cannot be re-derived from
//! a spec, and they are what a future `provider-mapi`'s offline fixture suite
//! would be built from.
//!
//! **Credentials are never written.** The recorder is given the request body
//! and the response payload only — the `Authorization` header does not reach
//! it, which is a structural guarantee rather than a scrubbing pass that could
//! be forgotten. Bodies still carry the mailbox GUID and the LegacyDN, so
//! `scrub` rewrites the capture host's realm to a placeholder.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Substrings replaced on the way out, as (needle, replacement) pairs. These
/// are lab-only identifiers, but a transcript is a committed artifact and the
/// next reader should not have to wonder whether a real tenant leaked in.
pub type ScrubRules<'a> = &'a [(&'a str, &'a str)];

pub struct Recorder {
    dir: PathBuf,
    seq: usize,
}

impl Recorder {
    /// Create the capture directory if it does not exist.
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir, seq: 0 })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write one exchange as three files: the request body, the response
    /// payload, and a human-readable hexdump pairing them.
    pub fn record(
        &mut self,
        request_type: &str,
        request: &[u8],
        response: &[u8],
        notes: &[(String, String)],
        rules: ScrubRules<'_>,
    ) -> io::Result<()> {
        self.seq += 1;
        let stem = format!("{:02}-{}", self.seq, request_type.to_ascii_lowercase());

        let request = scrub_bytes(request, rules);
        let response = scrub_bytes(response, rules);

        fs::write(self.dir.join(format!("{stem}.request.bin")), &request)?;
        fs::write(self.dir.join(format!("{stem}.response.bin")), &response)?;

        let mut meta = String::new();
        meta.push_str(&format!("X-RequestType: {request_type}\n"));
        for (k, v) in notes {
            meta.push_str(&format!("{k}: {}\n", scrub_str(v, rules)));
        }
        meta.push_str(&format!("\nrequest  {} bytes\n", request.len()));
        meta.push_str(&hexdump(&request));
        meta.push_str(&format!("\nresponse {} bytes\n", response.len()));
        meta.push_str(&hexdump(&response));
        fs::write(self.dir.join(format!("{stem}.meta.txt")), meta)?;
        Ok(())
    }
}

fn scrub_str(s: &str, rules: ScrubRules<'_>) -> String {
    let mut out = s.to_owned();
    for (needle, replacement) in rules {
        out = out.replace(needle, replacement);
    }
    out
}

/// Scrub a binary body. MAPI bodies carry both ASCII (the LegacyDN on
/// `Connect`) and UTF-16LE (strings in a `RopBuffer`), so a needle has to be
/// replaced in both encodings or half the occurrences survive. Replacements are
/// padded or truncated to the needle's length so no offset in the capture moves
/// — a transcript whose byte offsets shifted would be actively misleading.
fn scrub_bytes(body: &[u8], rules: ScrubRules<'_>) -> Vec<u8> {
    let mut out = body.to_vec();
    for (needle, replacement) in rules {
        out = replace_all(&out, needle.as_bytes(), replacement.as_bytes());
        out = replace_all(&out, &utf16le(needle), &utf16le(replacement));
    }
    out
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn replace_all(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return haystack.to_vec();
    }
    // Length-preserving: pad with the replacement's last byte, or truncate.
    let mut fixed = replacement.to_vec();
    let pad = *replacement.last().unwrap_or(&b'x');
    fixed.resize(needle.len(), pad);

    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if i + needle.len() <= haystack.len() && &haystack[i..i + needle.len()] == needle {
            out.extend_from_slice(&fixed);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

fn hexdump(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (i, chunk) in bytes.chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02X}")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if (0x20..0x7F).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        out.push_str(&format!(
            "{:08X}  {:<47}  |{ascii}|\n",
            i * 16,
            hex.join(" ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_ascii_and_utf16_occurrences_alike() {
        let mut body = b"dn=/o=Dev/cn=x".to_vec();
        body.extend_from_slice(&utf16le("Dev"));
        let rules: &[(&str, &str)] = &[("Dev", "Lab")];

        let out = scrub_bytes(&body, rules);
        assert!(!out.windows(3).any(|w| w == b"Dev"));
        assert!(out.windows(3).any(|w| w == b"Lab"));
        // The UTF-16 copy was scrubbed too, not just the ASCII one.
        assert!(
            !out.windows(6).any(|w| w == utf16le("Dev").as_slice()),
            "a UTF-16LE occurrence survived scrubbing"
        );
    }

    /// A transcript is read by byte offset, so scrubbing must not move
    /// anything — a shifted offset is worse than an unscrubbed name.
    #[test]
    fn scrubbing_preserves_length() {
        let body = b"aaaa-SECRET-bbbb".to_vec();
        for replacement in ["x", "REDACTED-MUCH-LONGER", "yy"] {
            let out = scrub_bytes(&body, &[("SECRET", replacement)]);
            assert_eq!(
                out.len(),
                body.len(),
                "replacement {replacement:?} moved offsets"
            );
        }
    }

    #[test]
    fn empty_and_oversized_needles_are_inert() {
        let body = b"short".to_vec();
        assert_eq!(scrub_bytes(&body, &[("", "x")]), body);
        assert_eq!(
            scrub_bytes(&body, &[("much longer than the body", "x")]),
            body
        );
    }

    #[test]
    fn hexdump_renders_offsets_and_ascii_gutter() {
        let dump = hexdump(&[0x41, 0x42, 0x00, 0xFF]);
        assert!(dump.starts_with("00000000  41 42 00 FF"));
        assert!(dump.trim_end().ends_with("|AB..|"));
    }

    #[test]
    fn hexdump_of_nothing_is_nothing() {
        assert_eq!(hexdump(&[]), "");
    }
}
