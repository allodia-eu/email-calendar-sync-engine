//! `Connect` and `Disconnect` request types ([MS-OXCMAPIHTTP] §2.2.4.1, §2.2.4.3).
//!
//! The auxiliary buffer is sent **empty**. [MS-OXCRPC] §3.1.4.1 says the server
//! fails only when `cbAuxIn` is *between 1 and 7* — i.e. "you claimed an aux
//! buffer too short to hold an RPC_HEADER_EXT". Zero is explicitly outside that
//! band, and the MIT reference client sends zero on Connect, Execute and
//! Disconnect alike. That defers the whole [MS-OXCRPC] aux-block layer.
//!
//! There is also no version negotiation to implement: `EcDoConnectEx` carries
//! `rgwClientVersion` as an RPC parameter, but the MAPI/HTTP `Connect` body has
//! no version fields at all. The client version rides the `X-ClientApplication`
//! header and nothing else.

use crate::cursor::{Reader, Writer};

/// `Connect`'s `Flags` is `EcDoConnectEx`'s `ulFlags`, where `0x00000000` means
/// "requests connection **without** administrator privilege" ([MS-OXCRPC]
/// §3.1.4.1).
///
/// This is a trap worth naming: the MIT reference client sets this field to
/// `0x00000001` under a comment about *not compressing the ROP response* — but
/// that is the meaning of `Flags` on **Execute**, not on Connect. On Connect,
/// bit 0 requests *administrator privilege*. Sending 1 here made a live Gromox
/// answer `ErrorCode 0x000003F2` (`ecLoginPerm`) for an ordinary mailbox user,
/// which reads like an auth failure and is not one. Two different fields, same
/// name, opposite meanings.
pub const FLAG_NO_ADMIN_PRIVILEGE: u32 = 0x0000_0000;

/// Windows-1252. Requesting `PtypString` (Unicode) columns later means the code
/// page never actually decides how text comes back.
pub const CP_WINDOWS_1252: u32 = 1252;

/// en-US, for both sorting and everything else.
pub const LCID_EN_US: u32 = 0x0000_0409;

#[derive(Debug, Clone)]
pub struct ConnectRequest {
    pub user_dn: String,
    pub flags: u32,
    pub code_page: u32,
    pub lcid_sort: u32,
    pub lcid_string: u32,
}

impl ConnectRequest {
    /// `user_dn` is the mailbox's legacyExchangeDN, which Autodiscover returns
    /// as `<User><LegacyDN>`. Do not construct it by hand.
    pub fn new(user_dn: impl Into<String>) -> Self {
        Self {
            user_dn: user_dn.into(),
            flags: FLAG_NO_ADMIN_PRIVILEGE,
            code_page: CP_WINDOWS_1252,
            lcid_sort: LCID_EN_US,
            lcid_string: LCID_EN_US,
        }
    }

    /// Field order is UserDn, Flags, DefaultCodePage, LcidSort, LcidString,
    /// AuxiliaryBufferSize, AuxiliaryBuffer ([MS-OXCMAPIHTTP] §2.2.4.1.1).
    pub fn serialize(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.ascii_z(&self.user_dn)
            .u32(self.flags)
            .u32(self.code_page)
            .u32(self.lcid_sort)
            .u32(self.lcid_string)
            .u32(0); // AuxiliaryBufferSize — deliberately empty
        w.finish()
    }
}

/// [MS-OXCMAPIHTTP] §2.2.4.1.2. `StatusCode` is the transport's verdict and
/// `ErrorCode` is `EcDoConnectEx`'s return value; both must be zero.
///
/// `ErrorCode` is a precise oracle while bringing a client up:
/// `0x80070005` ecAccessDenied (bad auth, or an empty UserDn),
/// `0x000003EB` ecUnknownUser (wrong UserDn — framing is fine),
/// `0x80040111` ecLoginFailure (mailbox not provisioned),
/// `0x80040115` ecRpcFailed (malformed aux buffer).
#[derive(Debug, Clone)]
pub struct ConnectResponse {
    pub status_code: u32,
    pub error_code: u32,
    pub polls_max: u32,
    pub retry_count: u32,
    pub retry_delay: u32,
    pub dn_prefix: String,
    pub display_name: String,
}

impl ConnectResponse {
    pub fn parse(body: &[u8]) -> crate::cursor::Result<Self> {
        let mut r = Reader::new(body);
        let status_code = r.u32()?;
        // A failure body is StatusCode + AuxiliaryBufferSize only; the fields
        // the success layout promises are simply absent (§2.2.4.1.3).
        if status_code != 0 {
            return Ok(Self {
                status_code,
                error_code: 0,
                polls_max: 0,
                retry_count: 0,
                retry_delay: 0,
                dn_prefix: String::new(),
                display_name: String::new(),
            });
        }
        Ok(Self {
            status_code,
            error_code: r.u32()?,
            polls_max: r.u32()?,
            retry_count: r.u32()?,
            retry_delay: r.u32()?,
            dn_prefix: r.ascii_z()?,
            display_name: r.utf16_z()?,
        })
    }

    pub fn ok(&self) -> bool {
        self.status_code == 0 && self.error_code == 0
    }

    pub fn error_name(&self) -> &'static str {
        match self.error_code {
            0x0000_0000 => "ecSuccess",
            0x0000_03EB => "ecUnknownUser",
            0x0000_03F2 => "ecLoginPerm",
            0x8004_0111 => "ecLoginFailure",
            0x8004_0110 => "ecVersionMismatch",
            0x8004_0115 => "ecRpcFailed",
            0x8007_0005 => "ecAccessDenied",
            _ => "unknown",
        }
    }
}

/// §2.2.4.3.1 — an auxiliary buffer and nothing else.
pub fn disconnect_request() -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(0);
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vector, hand-computed from §2.2.4.1.1. Locking the byte layout
    /// offline is what makes a live failure mean "the server disagrees" rather
    /// than "we cannot serialize".
    #[test]
    fn connect_request_matches_the_spec_layout() {
        let req = ConnectRequest::new("/o=X");
        let bytes = req.serialize();

        #[rustfmt::skip]
        let expected = vec![
            b'/', b'o', b'=', b'X', 0x00,   // UserDn, null-terminated ASCII
            0x00, 0x00, 0x00, 0x00,         // Flags = 0 (no admin privilege)
            0xE4, 0x04, 0x00, 0x00,         // DefaultCodePage = 1252
            0x09, 0x04, 0x00, 0x00,         // LcidSort = 0x0409
            0x09, 0x04, 0x00, 0x00,         // LcidString = 0x0409
            0x00, 0x00, 0x00, 0x00,         // AuxiliaryBufferSize = 0
        ];
        assert_eq!(bytes, expected);
        // 4 ASCII + NUL + five u32s.
        assert_eq!(bytes.len(), 5 + 20);
    }

    #[test]
    fn connect_response_round_trips() {
        let mut w = Writer::new();
        w.u32(0).u32(0).u32(60_000).u32(3).u32(1_000);
        w.ascii_z("/o=Gromox default/ou=Exchange Administrative Group");
        for u in "Alice Example".encode_utf16() {
            w.u16(u);
        }
        w.u16(0);
        w.u32(0);

        let resp = ConnectResponse::parse(&w.finish()).unwrap();
        assert!(resp.ok());
        assert_eq!(resp.polls_max, 60_000);
        assert_eq!(resp.retry_count, 3);
        assert_eq!(resp.display_name, "Alice Example");
        assert_eq!(resp.error_name(), "ecSuccess");
    }

    /// §2.2.4.1.3: a failure body stops after StatusCode. Parsing it with the
    /// success layout is the classic way to turn a clear server error into a
    /// confusing truncation error.
    #[test]
    fn failure_body_is_truncated_and_still_parses() {
        let mut w = Writer::new();
        w.u32(0x0000_000A).u32(0);
        let resp = ConnectResponse::parse(&w.finish()).unwrap();
        assert!(!resp.ok());
        assert_eq!(resp.status_code, 0x0000_000A);
    }

    #[test]
    fn hostile_response_bodies_never_panic() {
        for body in [
            &b""[..],
            &[0x00][..],
            &[0x00, 0x00, 0x00, 0x00][..],
            &[0xFF; 7][..],
        ] {
            let _ = ConnectResponse::parse(body);
        }
    }

    #[test]
    fn disconnect_is_just_an_empty_aux_buffer() {
        assert_eq!(disconnect_request(), vec![0, 0, 0, 0]);
    }
}
