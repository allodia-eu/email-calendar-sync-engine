// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

use super::{EasClient, EasError, MOVE_ITEMS_MAX_PER_COMMAND, command_chunks, expect_root};
use crate::{
    commands,
    types::{
        ConversationMoveRequest, ConversationMoveResult, EmptyFolderContentsRequest,
        EmptyFolderContentsResult, GetItemEstimateRequest, GetItemEstimateResult,
        ItemOperationsFetchRequest, ItemOperationsFetchResult, PingRequest, PingResult,
        SearchRequest, SearchResult,
    },
};

const PAGE_ITEM_OPS: u8 = 20;
const IO_ITEMOPERATIONS: u8 = 0x05;
const PAGE_PING: u8 = 13;
const PING_PING: u8 = 0x05;
const GIE_ROOT: u8 = 0x05;
const PAGE_SEARCH: u8 = 15;
const SR_SEARCH: u8 = 0x05;
/// Move (page 5) root token for MoveItems — [MS-ASWBXML] §2.1.2.1.6.
const PAGE_MOVE: u8 = 5;
const MV_MOVE_ITEMS: u8 = 0x05;
/// MeetingResponse (page 8) root token — [MS-ASWBXML] §2.1.2.1.9.
const PAGE_MREQ: u8 = 8;
const MREQ_MEETING_RESPONSE: u8 = 0x07;

/// Per-request timeout for a Ping: the requested heartbeat plus a 60s margin
/// for response delivery/parse time. The client-wide reqwest default is 120s
/// (`EasClient::new`), which otherwise kills every server-held ping at 120s
/// while the tuned heartbeat can reach 480s (cap 1680s) — reqwest's
/// per-request `RequestBuilder::timeout` overrides that default. Saturating:
/// a u32::MAX heartbeat must not overflow the margin add.
fn ping_request_timeout(heartbeat_secs: u32) -> std::time::Duration {
    std::time::Duration::from_secs(u64::from(heartbeat_secs.saturating_add(60)))
}

/// MS-ASPing: status 5 = requested heartbeat out of range; the response's
/// HeartbeatInterval carries a supported value → retry once with it.
fn ping_retry_interval(status: &str, server_interval: Option<u32>) -> Option<u32> {
    if status == "5" { server_interval } else { None }
}

impl EasClient {
    /// ItemOperations — fetch an attachment or item by server id. When
    /// `req.accept_multipart` is set the request carries
    /// `MS-ASAcceptMultiPart: T` and a multipart response ([MS-ASCMD]
    /// §2.2.1.10.1) is accepted and resolved inline before parsing, so the
    /// result shape is identical either way.
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub async fn item_operations(
        &mut self,
        req: &ItemOperationsFetchRequest,
    ) -> Result<ItemOperationsFetchResult, EasError> {
        let tree = commands::build_item_operations_request(req);
        let resp = self
            .send_command_ex_opts("ItemOperations", &tree, false, None, req.accept_multipart)
            .await?
            .ok_or_else(|| EasError::Transport("empty response body".into()))?;
        expect_root(&resp, PAGE_ITEM_OPS, IO_ITEMOPERATIONS)?;
        Ok(commands::parse_item_operations_response(&resp)?)
    }

    /// ItemOperations → EmptyFolderContents ([MS-ASCMD] §4.14.4): deletes
    /// EVERY item in the named folder server-side (and, when
    /// `req.delete_sub_folders` is set, the folder's subfolders too). A
    /// standalone user-facing command, so — like `settings_user_information`
    /// — it goes through the normal retry path.
    ///
    /// DESTRUCTIVE ACTION: this wipes a folder's contents on the server and
    /// cannot be undone from the client. Callers MUST confirm with the user
    /// before invoking. Errors carry protocol status codes only — no folder
    /// or item data is interpolated into logs or error messages.
    ///
    /// `result.status` is the effective status (top-level itemoperations
    /// Status, overridden by the EmptyFolderContents-level Status when
    /// present — the parser's more-specific-wins rule). Non-1 is surfaced
    /// as a typed CommandStatus error, mirroring the Settings family.
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub async fn empty_folder_contents(
        &mut self,
        req: &EmptyFolderContentsRequest,
    ) -> Result<EmptyFolderContentsResult, EasError> {
        let tree = commands::build_empty_folder_contents_request(req);
        let resp = self.send_command("ItemOperations", &tree).await?;
        expect_root(&resp, PAGE_ITEM_OPS, IO_ITEMOPERATIONS)?;
        let result = commands::parse_empty_folder_contents_response(&resp)?;
        if result.status != 1 {
            return Err(EasError::CommandStatus {
                status: result.status,
                message: format!(
                    "ItemOperations EmptyFolderContents failed: {}",
                    commands::common_status_message(result.status).unwrap_or("unknown status code")
                ),
            });
        }
        Ok(result)
    }

    /// ItemOperations → Move ([MS-ASCMD] §4.25): moves a whole conversation
    /// to another folder. When `req.move_always` is set, ALL FUTURE messages
    /// of the conversation are moved too — a persistent server-side behavior
    /// the caller MUST surface to the user before invoking. A standalone
    /// user-facing command, so — like `empty_folder_contents` — it goes
    /// through the normal retry path.
    ///
    /// `req.conversation_id` is an opaque server blob carried verbatim
    /// (never decoded). `result.status` is the effective status (top-level
    /// itemoperations Status, overridden by the Move-level Status when
    /// present — the parser's more-specific-wins rule). Non-1 is surfaced
    /// as a typed CommandStatus error, mirroring the Settings family.
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub async fn conversation_move(
        &mut self,
        req: &ConversationMoveRequest,
    ) -> Result<ConversationMoveResult, EasError> {
        let tree = commands::build_conversation_move_request(req);
        let resp = self.send_command("ItemOperations", &tree).await?;
        expect_root(&resp, PAGE_ITEM_OPS, IO_ITEMOPERATIONS)?;
        let result = commands::parse_conversation_move_response(&resp)?;
        if result.status != 1 {
            return Err(EasError::CommandStatus {
                status: result.status,
                message: format!(
                    "ItemOperations Move failed: {}",
                    commands::common_status_message(result.status).unwrap_or("unknown status code")
                ),
            });
        }
        Ok(result)
    }

    /// MoveItems — move one or more messages between folders
    /// ([MS-ASCMD] §2.2.1.12). `moves` is a batch of
    /// `(src_msg_id, src_fld_id, dst_fld_id)` tuples (wire ServerIds / folder
    /// ServerIds). Returns per-Move `(Status, DstMsgId)` pairs merged in
    /// request order on full success.
    ///
    /// Batch size: [MS-ASCMD] §3.1.5.10 SHOULD-limits a MoveItems request to
    /// `MOVE_ITEMS_MAX_PER_COMMAND` Move elements, so batches larger than
    /// the limit are split into sequential ≤1000-move commands and the
    /// per-move results merged in order (`command_chunks` covers the input
    /// contiguously, keeping merged results aligned with the request tuples).
    ///
    /// Batch-failure policy (fail-fast): the FIRST per-Move result that
    /// `commands::move_status_succeeded` rejects in ANY chunk surfaces as
    /// `EasError::CommandStatus` (decoded by `move_items_status_message` —
    /// [MS-ASCMD] 2.2.3.177.10), and LATER CHUNKS ARE NOT SENT — the batch is
    /// already partially applied server-side at that point; continuing would
    /// only widen the partial-batch window, and the caller reconciles via the
    /// next sync rounds either way. Every chunk's outcome is logged; nothing
    /// is swallowed. Note the MoveItems status table is INVERTED versus every
    /// other command: **3 is the success code** (returned with a valid
    /// DstMsgId — Exchange 15.2 live evidence, F10-2), and 1 means "invalid
    /// source collection/item ID".
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub async fn move_items(
        &mut self,
        moves: &[(String, String, String)],
    ) -> Result<Vec<(u32, Option<String>)>, EasError> {
        let chunks = command_chunks(moves, MOVE_ITEMS_MAX_PER_COMMAND);
        if chunks.is_empty() {
            // No moves → no round-trip (the production caller already
            // short-circuits on empty batches; this keeps the method total
            // rather than sending an empty MoveItems command).
            log::debug!("EAS MoveItems: empty batch — no round-trip");
            return Ok(Vec::new());
        }
        let total_chunks = chunks.len();
        let mut results: Vec<(u32, Option<String>)> = Vec::with_capacity(moves.len());
        for (chunk_idx, chunk) in chunks.iter().enumerate() {
            let chunk_no = chunk_idx + 1;
            match self.move_items_chunk(chunk_no, total_chunks, chunk).await {
                Ok(chunk_results) => {
                    log::debug!(
                        "EAS MoveItems chunk {chunk_no}/{total_chunks}: {} moves succeeded",
                        chunk_results.len()
                    );
                    // Chunks cover the input contiguously in order, so this
                    // keeps the merged pairs aligned with the request tuples.
                    results.extend(chunk_results);
                }
                Err(e) => {
                    // Fail-fast: later chunks are NOT sent after a failure.
                    let done: usize = chunks[..chunk_idx].iter().map(|c| c.len()).sum();
                    log::warn!(
                        "EAS MoveItems aborted at chunk {chunk_no}/{total_chunks}: {e} — {done} moves already applied in earlier chunks, the failing chunk carried {}; remaining chunks NOT sent (fail-fast avoids widening the partial-batch window)",
                        chunk.len()
                    );
                    return Err(e);
                }
            }
        }
        Ok(results)
    }

    /// One ≤[`MOVE_ITEMS_MAX_PER_COMMAND`]-move MoveItems round-trip: build,
    /// send, parse, and gate the per-move statuses. The chunk numbers are
    /// embedded in the surfaced `CommandStatus` message so a mid-batch
    /// failure is diagnosable from the caller's log alone.
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    async fn move_items_chunk(
        &mut self,
        chunk_no: usize,
        total_chunks: usize,
        moves: &[(String, String, String)],
    ) -> Result<Vec<(u32, Option<String>)>, EasError> {
        let tree = commands::build_move_items_request(moves);
        let resp = self.send_command("MoveItems", &tree).await?;
        expect_root(&resp, PAGE_MOVE, MV_MOVE_ITEMS)?;
        let results = commands::parse_move_items_response(&resp)?;
        // F10-2: per [MS-ASCMD] 2.2.3.177.10 the MoveItems SUCCESS status is
        // 3, not 1 — Exchange 15.2 returns per-Move Status 3 WITH a valid
        // DstMsgId and performs the move (IMAP-verified 2026-08-02). Log at
        // debug (this fires on every successful 15.2 move — warn would spam);
        // the gate below tolerates it (see `move_status_succeeded`).
        for (status, dst_msg_id) in &results {
            if *status == 3 && dst_msg_id.is_some() {
                log::debug!(
                    "EAS MoveItems: per-Move Status 3 with DstMsgId {dst_msg_id:?} — per [MS-ASCMD] 2.2.3.177.10, 3 is the MoveItems SUCCESS code (not 1); the server performed the move and returned the new item id. Treating as success."
                );
            }
        }
        if let Some(status) = commands::first_failing_move_status(&results) {
            return Err(EasError::CommandStatus {
                status,
                message: format!(
                    "MoveItems failed (chunk {chunk_no}/{total_chunks}): {}",
                    commands::move_items_status_message(status)
                ),
            });
        }
        Ok(results)
    }

    /// MeetingResponse — accept/tentative/decline a meeting request
    /// ([MS-ASCMD] §2.2.1.11). `collection_id` is the folder holding the
    /// invite email; `request_id` is that EMAIL message's wire ServerId;
    /// `user_response` is "1" (accept) / "2" (tentative) / "3" (decline).
    /// `instance_id`, when Some, names ONE instance of a recurring meeting
    /// (its original UTC start time, [MS-ASCAL]-format — §2.2.3.92.1); None
    /// applies the response to every instance. `send_response` emits the
    /// empty `<SendResponse/>` element asking a 16.0/16.1 server to email
    /// the organizer (the token is unregistered on older protocol versions —
    /// the IPC layer gates it). A non-1 Result Status surfaces as
    /// `EasError::CommandStatus` (decoded by
    /// `meeting_response_status_message` — [MS-ASCMD] 2.2.3.177.9).
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub async fn meeting_response(
        &mut self,
        collection_id: &str,
        request_id: &str,
        user_response: &str,
        instance_id: Option<&str>,
        send_response: bool,
    ) -> Result<u32, EasError> {
        let tree = commands::build_meeting_response_request(
            collection_id,
            request_id,
            user_response,
            instance_id,
            send_response,
        );
        let resp = self.send_command("MeetingResponse", &tree).await?;
        expect_root(&resp, PAGE_MREQ, MREQ_MEETING_RESPONSE)?;
        let status = commands::parse_meeting_response_response(&resp)?;
        if status != 1 {
            return Err(EasError::CommandStatus {
                status,
                message: format!(
                    "MeetingResponse failed: {}",
                    commands::meeting_response_status_message(status)
                ),
            });
        }
        Ok(status)
    }

    /// GetItemEstimate — count of pending items for a collection.
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub async fn get_item_estimate(
        &mut self,
        req: &GetItemEstimateRequest,
    ) -> Result<GetItemEstimateResult, EasError> {
        let tree = commands::build_get_item_estimate_request(req);
        let resp = self.send_command("GetItemEstimate", &tree).await?;
        // Root page for GetItemEstimate is 6; root token 0x05.
        expect_root(&resp, 6, GIE_ROOT)?;
        Ok(commands::parse_get_item_estimate_response(&resp)?)
    }

    /// Search — mailbox or GAL search ([MS-ASCMD] §2.2.1.16).
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub async fn search(&mut self, req: &SearchRequest) -> Result<SearchResult, EasError> {
        let tree = commands::build_search_request(req);
        let resp = self.send_command("Search", &tree).await?;
        expect_root(&resp, PAGE_SEARCH, SR_SEARCH)?;
        Ok(commands::parse_search_response(&resp)?)
    }

    /// Ping — block up to heartbeat_interval waiting for changes.
    ///
    /// Per-request timeout: the client-wide reqwest default is 120s, which
    /// otherwise kills every server-held ping at 120s while the tuned
    /// heartbeat can reach 480s (cap 1680s) — log-verified as transport
    /// failures causing strikes → drop to poll. Both the initial request and
    /// the status-5 retry pass `heartbeat + 60s` via `ping_request_timeout`
    /// (the retry uses the server-adopted interval).
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub async fn ping(&mut self, req: &PingRequest) -> Result<PingResult, EasError> {
        let started = std::time::Instant::now();
        let collections: Vec<&str> = req
            .monitored_collections
            .iter()
            .map(|c| c.collection_id.as_str())
            .collect();
        // INFO-level wire summary: the ping request/response is observable in
        // the app log without DEBUG builds (full WBXML hex stays at DEBUG).
        log::info!(
            "EAS Ping → heartbeat={}s collections={:?}",
            req.heartbeat_interval,
            collections
        );
        let tree = commands::build_ping_request(req);
        let timeout = Some(ping_request_timeout(req.heartbeat_interval));
        let resp = self.send_command_timed("Ping", &tree, timeout).await?;
        expect_root(&resp, PAGE_PING, PING_PING)?;
        let result = commands::parse_ping_response(&resp)?;
        log::info!(
            "EAS Ping ← held={:.1}s status={} heartbeat_interval={:?} folders={:?}",
            started.elapsed().as_secs_f64(),
            result.status,
            result.heartbeat_interval,
            result.folders
        );
        if let Some(interval) = ping_retry_interval(&result.status, result.heartbeat_interval) {
            log::info!(
                "EAS Ping status 5 — retrying once with server heartbeat {interval}s (was {}s)",
                req.heartbeat_interval
            );
            let retry_req = PingRequest {
                heartbeat_interval: interval,
                monitored_collections: req.monitored_collections.clone(),
            };
            let retry_tree = commands::build_ping_request(&retry_req);
            let retry_timeout = Some(ping_request_timeout(interval));
            let retry_started = std::time::Instant::now();
            let retry_resp = self
                .send_command_timed("Ping", &retry_tree, retry_timeout)
                .await?;
            expect_root(&retry_resp, PAGE_PING, PING_PING)?;
            let mut retry_result = commands::parse_ping_response(&retry_resp)?;
            log::info!(
                "EAS Ping ← (retry) held={:.1}s status={} heartbeat_interval={:?} folders={:?}",
                retry_started.elapsed().as_secs_f64(),
                retry_result.status,
                retry_result.heartbeat_interval,
                retry_result.folders
            );
            // Surface the adopted server interval so the engine's ping loop
            // can tune + persist it (the retry's own response carries no
            // HeartbeatInterval element unless it too is a status 5).
            retry_result.adopted_heartbeat = Some(interval);
            return Ok(retry_result);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Ping per-request timeout (hotfix 2026-08-03) ----
    //
    // `EasClient::new` sets a client-wide 120s reqwest timeout, but the tuned
    // ping heartbeat can reach 480s (cap 1680s) — every server-held ping died
    // client-side at exactly 120s as a transport failure, causing strikes and
    // a drop to poll. Ping now passes a per-request timeout of heartbeat + 60s
    // margin (reqwest's `RequestBuilder::timeout` overrides the client
    // default); every other command keeps the global default.

    #[test]
    fn ping_request_timeout_is_heartbeat_plus_margin() {
        // Normal tuned heartbeat: 480s + 60s margin.
        assert_eq!(ping_request_timeout(480), std::time::Duration::from_mins(9));
    }

    #[test]
    fn ping_request_timeout_zero_heartbeat_is_margin_only() {
        assert_eq!(ping_request_timeout(0), std::time::Duration::from_mins(1));
    }

    #[test]
    fn ping_request_timeout_saturates_at_u32_max() {
        // u32::MAX + 60 must not overflow — saturating_add keeps u32::MAX.
        assert_eq!(
            ping_request_timeout(u32::MAX),
            std::time::Duration::from_secs(u64::from(u32::MAX))
        );
    }

    // ---- F2: Ping heartbeat-interval retry DECISION ----

    #[test]
    fn ping_retry_interval_only_on_status_5_with_value() {
        assert_eq!(ping_retry_interval("5", Some(60)), Some(60));
        assert_eq!(ping_retry_interval("5", None), None);
        assert_eq!(ping_retry_interval("2", Some(60)), None);
        assert_eq!(ping_retry_interval("1", None), None);
    }
}
