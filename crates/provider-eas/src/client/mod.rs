// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.
//
// EAS HTTP client. Wraps `reqwest` to send WBXML POST requests to an Exchange
// ActiveSync endpoint and parse WBXML responses. Each command (FolderSync,
// Sync, SendMail, etc.) has its own high-level method in the family modules
// (sync / compose / items / settings / provision), delegating to the pure
// marshalers in `commands` and riding the transport/retry layers below.

pub use options::pick_protocol_version;

mod compose;
mod http;
mod items;
mod options;
mod provision;
mod redaction;
mod retry;
mod session;
mod settings;
mod sync;
mod transport;

use crate::{
    types::EasConfig,
    wbxml::{WbxmlElement, WbxmlError},
};

// ---- Command size limits ([MS-ASCMD] §3.1.5.10 "Limiting Size of Command
// Requests") ----
//
// Clients SHOULD limit the number of elements per command request; servers
// SHOULD enforce the tabulated limits and return the documented error status
// when exceeded (Exchange 2010 SP2 UR6 and later do — §3.1.5.10 product
// notes <27>/<28>). We chunk client-side so we never trip a server-side
// rejection. ItemOperations (≤100 ops), MeetingResponse (≤100 Requests), and
// GetItemEstimate (≤1000 collections) need no chunking here: their builders
// emit exactly one unit per call (see the single-op verification in the
// task-5 report).

/// [MS-ASCMD] §3.1.5.10: a MoveItems request SHOULD carry at most 1000 Move
/// elements (spec minimum 1). A server enforcing the limit answers MoveItems
/// Status 4 (§2.2.3.177.10) — `move_items` splits larger batches into
/// sequential ≤1000-move commands instead of risking that rejection.
const MOVE_ITEMS_MAX_PER_COMMAND: usize = 1000;

/// [MS-ASCMD] §3.1.5.10: the sum of the Add + Change + Delete + Fetch
/// elements in a Sync request SHOULD be at most 200 (spec minimum 1). The
/// upsync only emits Change elements (`build_sync_change_request`), so the
/// change count IS the element count; `sync_changes` splits larger batches
/// into sequential ≤200-change commands, threading the rotated sync key.
const SYNC_MAX_COMMANDS_PER_REQUEST: usize = 200;

/// Error returned by any EAS operation. Combines transport, WBXML, and
/// protocol-level errors (status codes).
#[derive(Debug, thiserror::Error)]
pub enum EasError {
    /// The HTTP request itself failed (DNS, TLS, timeout).
    #[error("HTTP transport error: {0}")]
    Transport(String),
    /// Non-200 HTTP response. `retry_after` carries the parsed `Retry-After`
    /// header (delta-seconds form, converted to absolute epoch) when the server
    /// sent one alongside a 429/503; `None` otherwise (including the HTTP-date
    /// form, which we do not parse — caller falls back to a default window).
    /// The EAS source promotes a 429/503 HttpStatus with `retry_after` to
    /// `SourceError::RateLimited`.
    /// `x_ms_location` carries the `X-MS-Location` response header on HTTP
    /// 451 ([MS-ASHTTP] §2.2.1.1.2.4) — the full URL of the server the
    /// mailbox moved to. The retry layer adopts it (hop-capped) and retries;
    /// `None` for every other status and for a 451 that omits the header.
    #[error("HTTP {status}: {body}")]
    HttpStatus {
        /// The HTTP status code.
        status: u16,
        /// (Possibly truncated) response body for diagnostics.
        body: String,
        /// Parsed `Retry-After` (delta-seconds → absolute epoch) on 429/503.
        retry_after: Option<i64>,
        /// `X-MS-Location` target on a 451 mailbox-moved response.
        x_ms_location: Option<String>,
    },
    /// The WBXML codec rejected the request/response bytes.
    #[error("WBXML codec error: {0}")]
    Wbxml(#[from] WbxmlError),
    /// The response's root element is not the command's expected top tag.
    #[error("unexpected response root: page {page} token {token}")]
    UnexpectedRoot {
        /// The root element's code page.
        page: u8,
        /// The root element's token.
        token: u8,
    },
    /// The server answered an in-body command status other than success
    /// ([MS-ASCMD] §2.2.3.177).
    #[error("command status {status}: {message}")]
    CommandStatus {
        /// The in-body status code.
        status: u32,
        /// Human-readable mapping of the code.
        message: String,
    },
    /// Client-side request validation failure — the request was rejected
    /// before ANY network I/O (distinct from `CommandStatus`, which means
    /// the SERVER actively rejected a request that went out).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Authentication failure: the OAuth token provider could not produce an
    /// access token (IdP/network failure, or the refresh grant is dead and the
    /// user must re-authenticate). Surfaces to the caller so it can prompt the
    /// user to re-authenticate. Basic auth never produces this error —
    /// `EasAuth::refresh()` is a no-op for Basic. (Absorbs the temporary
    /// `auth::AuthError`; kylins' old `AuthRefreshFailed` maps here.)
    #[error("authentication failed: {0}")]
    Auth(String),
}

impl From<reqwest::Error> for EasError {
    fn from(e: reqwest::Error) -> Self {
        EasError::Transport(e.to_string())
    }
}

/// Split a batch of per-element command units into slices no larger than the
/// [MS-ASCMD] §3.1.5.10 SHOULD-limit for one wire command. The chunks cover
/// the input contiguously and in order (backing the callers' result-merge
/// ordering contract). Empty input yields zero chunks — the callers then
/// send nothing. Pure / no I/O: the unit-testable boundary behind the
/// `move_items` / `sync_changes` chunked send loops (same split as
/// `redirect_hop_decision`).
fn command_chunks<T>(items: &[T], max_per_command: usize) -> Vec<&[T]> {
    items.chunks(max_per_command).collect()
}

/// High-level EAS client. Cheap to clone (just wraps a `reqwest::Client` and config).
#[derive(Clone, Debug)]
pub struct EasClient {
    config: EasConfig,
    http: reqwest::Client,
    /// Folder-hierarchy sync key from the last successful FolderSync.
    /// Folder ops (Create/Update/Delete) must send it per MS-ASCMD; empty
    /// until the first FolderSync, then `hierarchy_key()` yields "0".
    hierarchy_sync_key: String,
    /// The last HTTP 451 `X-MS-Location` redirect target adopted during a
    /// command round ([MS-ASHTTP] §3.1.5.2). `None` until a redirect is
    /// adopted; `EasSource::persist_eas_url_if_changed` reads it after a
    /// successful round to persist the adopted endpoint into
    /// `accounts.eas_url`, mirroring the policy-key persistence.
    adopted_url: Option<String>,
}

impl EasClient {
    /// Build a client for one account: configures the shared `reqwest::Client`
    /// (timeouts, optional invalid-cert acceptance, user agent) from `config`.
    pub fn new(config: EasConfig) -> Self {
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_mins(2))
            .danger_accept_invalid_certs(config.accept_invalid_certs);
        // User-Agent
        let ua = config.user_agent.clone();
        if !ua.is_empty() {
            builder = builder.user_agent(&ua);
        }
        let http = builder.build().unwrap_or_else(|_| reqwest::Client::new());
        Self {
            config,
            http,
            hierarchy_sync_key: String::new(),
            adopted_url: None,
        }
    }
}

fn expect_root(root: &WbxmlElement, page: u8, token: u8) -> Result<(), EasError> {
    if root.page == page && root.token == token {
        Ok(())
    } else {
        Err(EasError::UnexpectedRoot {
            page: root.page,
            token: root.token,
        })
    }
}

/// Minimal form-urlencoder for the handful of query string values we emit.
/// Avoids pulling in a `urlencoding` crate dependency.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands;

    #[test]
    fn urlencode_passes_alphanumeric() {
        assert_eq!(urlencode("abcXYZ123"), "abcXYZ123");
    }

    #[test]
    fn urlencode_escapes_special() {
        assert_eq!(urlencode("user@host"), "user%40host");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("kylins\\admin"), "kylins%5Cadmin");
    }

    #[test]
    fn urlencode_keeps_unreserved() {
        assert_eq!(urlencode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn urlencode_empty() {
        assert_eq!(urlencode(""), "");
    }

    // ---- Command size-limit chunking ([MS-ASCMD] §3.1.5.10) ----
    //
    // Pure chunk-boundary tests: `command_chunks` is the boundary both
    // `move_items` (≤MOVE_ITEMS_MAX_PER_COMMAND Move elements per request)
    // and `sync_changes` (≤SYNC_MAX_COMMANDS_PER_REQUEST Sync command
    // elements per request) split through before sending sequential wire
    // commands. The send loops themselves need a live server; the boundary
    // shapes and the input-ordering guarantee are unit-testable without one
    // (same split as the `redirect_hop_decision` precedent above).

    fn move_tuple(i: usize) -> (String, String, String) {
        (
            format!("msg:{i}"),
            "srcfld".to_string(),
            "dstfld".to_string(),
        )
    }

    fn flag_change(i: usize) -> commands::EasChange {
        commands::EasChange {
            server_id: format!("srv:{i}"),
            read: Some(true),
            starred: None,
        }
    }

    /// Spec constant guard: these ARE the [MS-ASCMD] §3.1.5.10 SHOULD-limits
    /// (MoveItems Move elements = 1000; Sync Add+Change+Delete+Fetch elements
    /// = 200). Changing either requires re-verifying against the spec table.
    #[test]
    fn size_limit_constants_match_msascmd_3_1_5_10() {
        assert_eq!(MOVE_ITEMS_MAX_PER_COMMAND, 1000);
        assert_eq!(SYNC_MAX_COMMANDS_PER_REQUEST, 200);
    }

    #[test]
    fn move_items_exactly_at_limit_is_one_chunk() {
        let moves: Vec<_> = (0..MOVE_ITEMS_MAX_PER_COMMAND).map(move_tuple).collect();
        let chunks = command_chunks(&moves, MOVE_ITEMS_MAX_PER_COMMAND);
        assert_eq!(
            chunks.len(),
            1,
            "exactly 1000 moves fit in a single MoveItems command"
        );
        assert_eq!(chunks[0].len(), MOVE_ITEMS_MAX_PER_COMMAND);
    }

    #[test]
    fn move_items_one_over_limit_splits_1000_plus_1() {
        let moves: Vec<_> = (0..=MOVE_ITEMS_MAX_PER_COMMAND).map(move_tuple).collect();
        let chunks = command_chunks(&moves, MOVE_ITEMS_MAX_PER_COMMAND);
        assert_eq!(chunks.len(), 2, "1001 moves must split into two commands");
        assert_eq!(chunks[0].len(), MOVE_ITEMS_MAX_PER_COMMAND);
        assert_eq!(chunks[1].len(), 1);
    }

    #[test]
    fn move_items_2500_splits_1000_1000_500() {
        let moves: Vec<_> = (0..2500).map(move_tuple).collect();
        let chunks = command_chunks(&moves, MOVE_ITEMS_MAX_PER_COMMAND);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 1000);
        assert_eq!(chunks[1].len(), 1000);
        assert_eq!(chunks[2].len(), 500);
    }

    #[test]
    fn move_items_empty_input_is_zero_chunks() {
        let moves: Vec<(String, String, String)> = Vec::new();
        let chunks = command_chunks(&moves, MOVE_ITEMS_MAX_PER_COMMAND);
        assert!(chunks.is_empty(), "no moves → no MoveItems commands");
    }

    #[test]
    fn sync_changes_exactly_at_limit_is_one_chunk() {
        let changes: Vec<_> = (0..SYNC_MAX_COMMANDS_PER_REQUEST)
            .map(flag_change)
            .collect();
        let chunks = command_chunks(&changes, SYNC_MAX_COMMANDS_PER_REQUEST);
        assert_eq!(
            chunks.len(),
            1,
            "exactly 200 changes fit in a single Sync command"
        );
        assert_eq!(chunks[0].len(), SYNC_MAX_COMMANDS_PER_REQUEST);
    }

    #[test]
    fn sync_changes_one_over_limit_splits_200_plus_1() {
        let changes: Vec<_> = (0..=SYNC_MAX_COMMANDS_PER_REQUEST)
            .map(flag_change)
            .collect();
        let chunks = command_chunks(&changes, SYNC_MAX_COMMANDS_PER_REQUEST);
        assert_eq!(chunks.len(), 2, "201 changes must split into two commands");
        assert_eq!(chunks[0].len(), SYNC_MAX_COMMANDS_PER_REQUEST);
        assert_eq!(chunks[1].len(), 1);
    }

    /// The result-merge ordering guarantee: chunks cover the input
    /// contiguously in request order, so merging per-chunk results
    /// chunk-by-chunk keeps the merged `(Status, DstMsgId)` pairs aligned
    /// with the request tuples (move_items' merge contract).
    #[test]
    fn command_chunks_preserves_input_order_and_coverage() {
        let moves: Vec<_> = (0..3001).map(move_tuple).collect();
        let chunks = command_chunks(&moves, MOVE_ITEMS_MAX_PER_COMMAND);
        assert_eq!(chunks.len(), 4, "3001 moves = 1000+1000+1000+1");
        let flattened: Vec<&(String, String, String)> =
            chunks.iter().flat_map(|c| c.iter()).collect();
        assert_eq!(flattened.len(), moves.len());
        for (i, m) in flattened.iter().enumerate() {
            assert_eq!(m.0, format!("msg:{i}"), "input order broken at index {i}");
        }
    }
}
