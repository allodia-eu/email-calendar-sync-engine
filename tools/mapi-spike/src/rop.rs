//! Individual ROPs. CP3 needs exactly one: `RopLogon` ([MS-OXCSTOR] §2.2.1.1).
//!
//! Responses are decoded by **reading each `RopId` off the stream**, never
//! positionally against the request list. Two reasons: `RopRelease` returns no
//! response on success, so counts do not line up; and the server may substitute
//! `RopBackoff` (0xF9) or `RopBufferTooSmall` (0xFF) for a response you did ask
//! for. A failing ROP's response is also truncated right after `ReturnValue`,
//! so that field is read first and a non-zero value stops the decode.

use crate::cursor::{Reader, Result, Writer};

pub const ROP_RELEASE: u8 = 0x01;
pub const ROP_OPEN_FOLDER: u8 = 0x02;
pub const ROP_GET_HIERARCHY_TABLE: u8 = 0x04;
pub const ROP_GET_CONTENTS_TABLE: u8 = 0x05;
pub const ROP_SET_COLUMNS: u8 = 0x12;
pub const ROP_QUERY_ROWS: u8 = 0x15;
pub const ROP_BACKOFF: u8 = 0xF9;
pub const ROP_LOGON: u8 = 0xFE;
pub const ROP_BUFFER_TOO_SMALL: u8 = 0xFF;

/// `LogonFlags`: private mailbox rather than public folders.
pub const LOGON_PRIVATE: u8 = 0x01;

/// `OpenFlags`: `USE_PER_MDB_REPLID_MAPPING`. What Outlook sends for a normal
/// private-mailbox logon.
pub const OPEN_USE_PER_MDB_REPLID_MAPPING: u32 = 0x0100_0000;

/// The 13 fixed folder slots in a private-mailbox logon response, in wire
/// order ([MS-OXCSTOR] §2.2.1.1.3). Index 4 being the Inbox is what lets this
/// spike skip `RopGetReceiveFolder` *and* all EntryID parsing.
pub const FOLDER_NAMES: [&str; 13] = [
    "Mailbox Root",
    "Deferred Action",
    "Spooler Queue",
    "IPM subtree",
    "Inbox",
    "Outbox",
    "Sent Items",
    "Deleted Items",
    "Common Views",
    "Schedule",
    "Search",
    "Views",
    "Shortcuts",
];

pub const FOLDER_IPM_SUBTREE: usize = 3;
pub const FOLDER_INBOX: usize = 4;

/// `RopLogon` request ([MS-OXCSTOR] §2.2.1.1.1).
pub fn logon_request(output_handle_index: u8, essdn: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(ROP_LOGON)
        .u8(0x00) // LogonId
        .u8(output_handle_index)
        .u8(LOGON_PRIVATE)
        .u32(OPEN_USE_PER_MDB_REPLID_MAPPING)
        .u32(0x0000_0000) // StoreState
        // EssdnSize counts the null terminator.
        .u16((essdn.len() + 1) as u16)
        .ascii_z(essdn);
    w.finish()
}

#[derive(Debug, Clone)]
pub struct LogonResponse {
    pub output_handle_index: u8,
    pub return_value: u32,
    pub logon_flags: u8,
    pub folder_ids: Vec<u64>,
    pub mailbox_guid: [u8; 16],
    pub replica_id: u16,
}

impl LogonResponse {
    pub fn folder(&self, slot: usize) -> Option<u64> {
        self.folder_ids.get(slot).copied()
    }
}

/// What a decoded ROP response can be. Only the shapes CP3/CP4 need are
/// modelled; anything else is kept as its raw id so an unexpected response is
/// reported rather than silently skipped.
#[derive(Debug)]
pub enum RopResponse {
    Logon(LogonResponse),
    /// A ROP that failed: the body stopped after `ReturnValue`.
    Failed {
        rop_id: u8,
        return_value: u32,
    },
    /// `MaxRopOut` was too small; `size_needed` says how much to ask for.
    BufferTooSmall {
        size_needed: u16,
    },
    Backoff,
}

/// Decode one response off the stream, dispatching on its `RopId`.
pub fn decode_one(r: &mut Reader<'_>) -> Result<RopResponse> {
    let rop_id = r.u8()?;
    match rop_id {
        ROP_BUFFER_TOO_SMALL => {
            let size_needed = r.u16()?;
            Ok(RopResponse::BufferTooSmall { size_needed })
        }
        ROP_BACKOFF => Ok(RopResponse::Backoff),
        ROP_LOGON => {
            let output_handle_index = r.u8()?;
            let return_value = r.u32()?;
            if return_value != 0 {
                return Ok(RopResponse::Failed {
                    rop_id,
                    return_value,
                });
            }
            let logon_flags = r.u8()?;
            let mut folder_ids = Vec::with_capacity(13);
            for _ in 0..13 {
                folder_ids.push(r.u64()?);
            }
            let _response_flags = r.u8()?;
            let mut mailbox_guid = [0u8; 16];
            mailbox_guid.copy_from_slice(r.bytes(16)?);
            let replica_id = r.u16()?;
            let _replica_guid = r.bytes(16)?;
            // LogonTime is a `LogonTime` struct: Seconds, Minutes, Hour,
            // DayOfWeek, Day, Month (1 byte each) + Year (u16) = **8 bytes**.
            // Counting it as 13 overruns by 5 and turns a perfectly good
            // response into a truncation error — the total for a private
            // mailbox is exactly 166 bytes, which is how this was caught.
            let _logon_time = r.bytes(8)?;
            let _gwart_time = r.bytes(8)?;
            let _store_state = r.u32()?;
            Ok(RopResponse::Logon(LogonResponse {
                output_handle_index,
                return_value,
                logon_flags,
                folder_ids,
                mailbox_guid,
                replica_id,
            }))
        }
        other => {
            // Every other ROP response begins the same way, so a generic
            // ReturnValue read is enough to report it honestly.
            let _handle_index = r.u8()?;
            let return_value = r.u32()?;
            Ok(RopResponse::Failed {
                rop_id: other,
                return_value,
            })
        }
    }
}

/// Human-readable names for the error codes this spike actually provokes.
pub fn ec_name(code: u32) -> &'static str {
    match code {
        0x0000_0000 => "ecSuccess",
        0x0000_0002 => "ecNotFound",
        0x0000_03EB => "ecUnknownUser",
        0x0000_03F2 => "ecLoginPerm",
        0x8004_0111 => "ecLoginFailure",
        0x8004_0115 => "ecRpcFailed",
        0x8007_0005 => "ecAccessDenied",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logon_request_layout() {
        let bytes = logon_request(0, "/o=X");
        #[rustfmt::skip]
        let expected = vec![
            0xFE,                   // RopId
            0x00,                   // LogonId
            0x00,                   // OutputHandleIndex
            0x01,                   // LogonFlags = Private
            0x00, 0x00, 0x00, 0x01, // OpenFlags = USE_PER_MDB_REPLID_MAPPING (LE)
            0x00, 0x00, 0x00, 0x00, // StoreState
            0x05, 0x00,             // EssdnSize = 4 + NUL
            b'/', b'o', b'=', b'X', 0x00,
        ];
        assert_eq!(bytes, expected);
    }

    /// EssdnSize must count the terminator, or the server reads past the name.
    #[test]
    fn essdn_size_includes_the_null_terminator() {
        let dn = "/o=Gromox default/cn=alice";
        let bytes = logon_request(0, dn);
        let size = u16::from_le_bytes([bytes[12], bytes[13]]) as usize;
        assert_eq!(size, dn.len() + 1);
        assert_eq!(bytes.len(), 14 + dn.len() + 1);
    }

    fn sample_logon_response(return_value: u32) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(ROP_LOGON).u8(0).u32(return_value);
        if return_value != 0 {
            return w.finish();
        }
        w.u8(LOGON_PRIVATE);
        for i in 0..13u64 {
            w.u64(0x0100_0000_0000_0000 | i);
        }
        w.u8(0); // ResponseFlags
        w.bytes(&[0xAB; 16]); // MailboxGuid
        w.u16(1); // ReplId
        w.bytes(&[0xCD; 16]); // ReplGuid
        w.bytes(&[0; 8]); // LogonTime (Seconds..Month + Year u16)
        w.bytes(&[0; 8]); // GwartTime
        w.u32(0); // StoreState
        w.finish()
    }

    /// The observed size of a real Gromox private-mailbox logon response. This
    /// is the assertion that caught LogonTime being 8 bytes rather than 13.
    #[test]
    fn private_mailbox_logon_response_is_exactly_166_bytes() {
        assert_eq!(sample_logon_response(0).len(), 166);
    }

    #[test]
    fn decodes_thirteen_folder_ids_with_inbox_at_slot_four() {
        let buf = sample_logon_response(0);
        let mut r = Reader::new(&buf);
        let RopResponse::Logon(logon) = decode_one(&mut r).unwrap() else {
            panic!("expected a logon response");
        };
        assert_eq!(logon.folder_ids.len(), 13);
        assert_eq!(logon.replica_id, 1);
        assert_eq!(logon.folder(FOLDER_INBOX), Some(0x0100_0000_0000_0004));
        assert_eq!(FOLDER_NAMES[FOLDER_INBOX], "Inbox");
        assert_eq!(FOLDER_NAMES[FOLDER_IPM_SUBTREE], "IPM subtree");
        assert!(r.is_empty(), "the whole response must be consumed");
    }

    /// The failure shape is the one that breaks naive decoders: everything the
    /// success layout promises is simply absent.
    #[test]
    fn failed_rop_stops_after_return_value() {
        let buf = sample_logon_response(0x0000_03EB);
        let mut r = Reader::new(&buf);
        match decode_one(&mut r).unwrap() {
            RopResponse::Failed {
                rop_id,
                return_value,
            } => {
                assert_eq!(rop_id, ROP_LOGON);
                assert_eq!(ec_name(return_value), "ecUnknownUser");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn buffer_too_small_carries_the_size_needed() {
        let mut w = Writer::new();
        w.u8(ROP_BUFFER_TOO_SMALL).u16(4096);
        let mut r = Reader::new(w.finish().leak());
        match decode_one(&mut r).unwrap() {
            RopResponse::BufferTooSmall { size_needed } => assert_eq!(size_needed, 4096),
            other => panic!("expected BufferTooSmall, got {other:?}"),
        }
    }

    #[test]
    fn hostile_rop_streams_never_panic() {
        for buf in [
            &b""[..],
            &[0xFE][..],
            &[0xFE, 0x00][..],
            &[0xFE, 0x00, 0x00, 0x00][..],
            &[0xFF; 40][..],
        ] {
            let mut r = Reader::new(buf);
            let _ = decode_one(&mut r);
        }
    }
}
