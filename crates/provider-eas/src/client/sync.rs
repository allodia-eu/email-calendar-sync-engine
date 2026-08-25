// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

use super::{EasClient, EasError, SYNC_MAX_COMMANDS_PER_REQUEST, command_chunks, expect_root};
use crate::{
    commands,
    types::{
        FolderCreateRequest, FolderDeleteRequest, FolderSyncResult, FolderUpdateRequest,
        SyncRequest, SyncResult,
    },
    wbxml::WbxmlElement,
};

const PAGE_FOLDER: u8 = 7;
const PAGE_AIRSYNC: u8 = 0;
/// AirSync (page 0) `Sync` root token. Used by the Sync response `expect_root`
/// check so a non-Sync response (server error page, OWA redirect, etc.) is
/// surfaced as `UnexpectedRoot` rather than a confusing parse failure.
const AS_SYNC: u8 = 0x05;
const FH_FOLDER_SYNC: u8 = 0x16;
const FH_FOLDER_CREATE: u8 = 0x13;
const FH_FOLDER_DELETE: u8 = 0x14;
const FH_FOLDER_UPDATE: u8 = 0x15;

/// Build the no-changes SyncResult for an empty Sync response body.
/// Exchange returns an EMPTY HTTP body for Sync when the collection has
/// nothing to report (Android EasSync.java:225 treats empty as OK), so this
/// is a success result — NOT an error. CRITICAL: the sync key is the
/// REQUEST's key, not Default's empty string — an empty key would corrupt
/// the engine's persisted cursor and force a full re-sync. SyncResult's
/// manual Default already yields status 1 (success) and empty item lists.
fn no_changes_result(request_key: &str) -> SyncResult {
    SyncResult {
        sync_key: request_key.to_string(),
        ..Default::default()
    }
}

impl EasClient {
    /// FolderSync — full folder hierarchy sync.
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub async fn folder_sync(&mut self, sync_key: &str) -> Result<FolderSyncResult, EasError> {
        let req = commands::build_folder_sync_request(sync_key);
        let resp = self.send_command("FolderSync", &req).await?;
        expect_root(&resp, PAGE_FOLDER, FH_FOLDER_SYNC)?;
        let result = commands::parse_folder_sync_response(&resp)?;
        if result.status != 1 {
            return Err(EasError::CommandStatus {
                status: result.status,
                message: format!(
                    "FolderSync failed: {}",
                    commands::folder_sync_status_message(result.status)
                ),
            });
        }
        // Cache the hierarchy key — folder ops must echo it back per MS-ASCMD.
        self.hierarchy_sync_key.clone_from(&result.sync_key);
        Ok(result)
    }

    /// Sync — single-collection item sync.
    ///
    /// The response is parsed through the class-aware seam (M8 Task 4): with
    /// `req.class == "Calendar"` the Add/Change items surface as typed
    /// [`crate::calendar::CalendarEventProps`] on `SyncResult::calendar_added` /
    /// `calendar_updated`; every other class keeps the Email-shaped
    /// `added` / `updated` path bit-for-bit. The REQUEST builder is
    /// class-agnostic either way (`build_sync_request` emits no Class
    /// element in 14.0+).
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub async fn sync(&mut self, req: &SyncRequest) -> Result<SyncResult, EasError> {
        let tree = commands::build_sync_request(req, &self.config.protocol_version);
        // allow_empty=true: Exchange returns an EMPTY HTTP body for Sync when
        // the collection has nothing to report (Android EasSync.java:225
        // treats empty as OK). None → the no-changes result, which MUST carry
        // the request's sync key (Default's empty key would corrupt the
        // engine's cursor). Some(resp) → the normal parse path.
        match self.send_command_ex("Sync", &tree, true, None).await? {
            None => Ok(no_changes_result(&req.sync_key)),
            Some(resp) => {
                // Verify the server returned a Sync element. A non-Sync root
                // typically means the server returned an error page or OWA
                // redirect that the transport layer couldn't detect — surface
                // it rather than attempt a misleading parse.
                expect_root(&resp, PAGE_AIRSYNC, AS_SYNC)?;
                Ok(commands::parse_sync_response_for_class(&resp, &req.class)?)
            }
        }
    }

    /// Sync — client-side `Commands > Change` upsync for a single collection
    /// (flag mutations etc.). Returns a [`commands::SyncChangeOutcome`]: the
    /// rotated sync key, the collection status, and any server-side Commands
    /// piggybacked onto the response ([MS-ASSYNC] §2.2.2).
    ///
    /// The collection sync key is passed in — `EasClient` deliberately does
    /// NOT cache collection keys (the engine's `eas_sync_state` cursor store
    /// owns them); the caller decides whether to persist the returned key on
    /// success (and must consume the piggybacked changes first if it does).
    ///
    /// Batch size: [MS-ASCMD] §3.1.5.10 SHOULD-limits the Sync command
    /// elements (Add + Change + Delete + Fetch) to
    /// `SYNC_MAX_COMMANDS_PER_REQUEST` per request, so batches larger than
    /// the limit are split into sequential ≤200-change chunks. Each chunk's
    /// response rotates the collection sync key — the rotated key is the
    /// REQUIRED request key of the next Sync — so the chunks thread it:
    /// K0 → chunk 1 → K1 → chunk 2 → … The returned outcome carries the LAST
    /// chunk's key and status, with the piggybacked server commands of ALL
    /// chunks merged in chunk order (discarding them while adopting the
    /// rotated key would silently diverge from the server — see
    /// `parse_sync_change_response`).
    ///
    /// Failure is fail-fast: a non-1 collection status (e.g. 3 = invalid
    /// sync key) or transport error in chunk N surfaces as
    /// `EasError::CommandStatus` / the transport error with the chunk number
    /// logged, and chunks N+1.. are NOT sent. The caller (set_flags) never
    /// persists the rotated key on ANY path, so a mid-batch failure leaves
    /// the persisted cursor at the pre-upsync key — the next downsync
    /// re-syncs from there and self-heals; no impossible state is recorded.
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub async fn sync_changes(
        &mut self,
        collection_id: &str,
        sync_key: &str,
        changes: &[commands::EasChange],
    ) -> Result<commands::SyncChangeOutcome, EasError> {
        let chunks = command_chunks(changes, SYNC_MAX_COMMANDS_PER_REQUEST);
        if chunks.is_empty() {
            // No changes → no round-trip, key unchanged (the production
            // caller already short-circuits on empty batches; this keeps the
            // method total rather than sending an empty Sync command).
            log::debug!(
                "EAS Sync Change: empty batch for collection {collection_id} — no round-trip, sync key unchanged"
            );
            return Ok(commands::SyncChangeOutcome {
                status: 1,
                new_key: sync_key.to_string(),
                ..Default::default()
            });
        }
        let total_chunks = chunks.len();
        let mut merged = commands::SyncChangeOutcome {
            status: 1,
            new_key: sync_key.to_string(),
            ..Default::default()
        };
        let mut current_key = sync_key.to_string();
        for (chunk_idx, chunk) in chunks.iter().enumerate() {
            let chunk_no = chunk_idx + 1;
            let outcome = match self
                .sync_changes_chunk(chunk_no, total_chunks, collection_id, &current_key, chunk)
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    // Fail-fast: later chunks are NOT sent after a failure —
                    // the batch is already partially applied server-side and
                    // continuing would only widen the partial window.
                    let done: usize = chunks[..chunk_idx].iter().map(|c| c.len()).sum();
                    log::warn!(
                        "EAS Sync Change upsync aborted at chunk {chunk_no}/{total_chunks} for collection {collection_id}: {e} — {done} changes already applied in earlier chunks, the failing chunk carried {}; remaining chunks NOT sent (fail-fast). The persisted cursor is untouched (set_flags never persists the rotated key), so the next downsync re-syncs from the pre-upsync key and self-heals.",
                        chunk.len()
                    );
                    return Err(e);
                }
            };
            log::debug!(
                "EAS Sync Change chunk {chunk_no}/{total_chunks} for collection {collection_id}: {} changes upsynced, sync key rotated{}",
                chunk.len(),
                if outcome.has_piggybacked() {
                    " (response carries piggybacked server commands)"
                } else {
                    ""
                }
            );
            // Thread the rotated key into the next chunk and merge this
            // chunk's piggybacked server commands in chunk order.
            current_key = outcome.new_key.clone();
            merged.piggybacked_added.extend(outcome.piggybacked_added);
            merged
                .piggybacked_updated
                .extend(outcome.piggybacked_updated);
            merged
                .piggybacked_deleted
                .extend(outcome.piggybacked_deleted);
            merged.new_key = outcome.new_key;
            merged.status = outcome.status;
        }
        Ok(merged)
    }

    /// One ≤[`SYNC_MAX_COMMANDS_PER_REQUEST`]-change Sync round-trip. The
    /// chunk numbers are embedded in the surfaced `CommandStatus` message so
    /// a mid-batch failure is diagnosable from the caller's log alone.
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    async fn sync_changes_chunk(
        &mut self,
        chunk_no: usize,
        total_chunks: usize,
        collection_id: &str,
        sync_key: &str,
        changes: &[commands::EasChange],
    ) -> Result<commands::SyncChangeOutcome, EasError> {
        let tree = commands::build_sync_change_request(collection_id, sync_key, changes);
        let resp = self.send_command("Sync", &tree).await?;
        expect_root(&resp, PAGE_AIRSYNC, AS_SYNC)?;
        let outcome = commands::parse_sync_change_response(&resp)?;
        if outcome.status != 1 {
            return Err(EasError::CommandStatus {
                status: outcome.status,
                message: format!(
                    "Sync Change failed (chunk {chunk_no}/{total_chunks}): {}",
                    commands::common_status_message(outcome.status)
                        .unwrap_or("collection status not success")
                ),
            });
        }
        Ok(outcome)
    }

    /// FolderCreate — create a new folder under a parent.
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub async fn folder_create(
        &mut self,
        req: &FolderCreateRequest,
    ) -> Result<(u32, Option<String>), EasError> {
        let key = self.hierarchy_key().to_string();
        let tree = commands::build_folder_create_request(req, &key);
        let resp = self.send_command("FolderCreate", &tree).await?;
        expect_root(&resp, PAGE_FOLDER, FH_FOLDER_CREATE)?;
        self.adopt_folder_op_sync_key(&resp);
        Ok(commands::parse_folder_op_response(&resp)?)
    }

    /// FolderDelete — delete a folder by server id.
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub async fn folder_delete(
        &mut self,
        req: &FolderDeleteRequest,
    ) -> Result<(u32, Option<String>), EasError> {
        let key = self.hierarchy_key().to_string();
        let tree = commands::build_folder_delete_request(req, &key);
        let resp = self.send_command("FolderDelete", &tree).await?;
        expect_root(&resp, PAGE_FOLDER, FH_FOLDER_DELETE)?;
        self.adopt_folder_op_sync_key(&resp);
        Ok(commands::parse_folder_op_response(&resp)?)
    }

    /// FolderUpdate — rename or move a folder.
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    pub async fn folder_update(
        &mut self,
        req: &FolderUpdateRequest,
    ) -> Result<(u32, Option<String>), EasError> {
        let key = self.hierarchy_key().to_string();
        let tree = commands::build_folder_update_request(req, &key);
        let resp = self.send_command("FolderUpdate", &tree).await?;
        expect_root(&resp, PAGE_FOLDER, FH_FOLDER_UPDATE)?;
        self.adopt_folder_op_sync_key(&resp);
        Ok(commands::parse_folder_op_response(&resp)?)
    }

    /// Adopt the hierarchy SyncKey a folder-op response carries ([MS-ASCMD]
    /// 2.2.3.181.1): every FolderCreate/Update/Delete advances the hierarchy
    /// key, and the next folder op must send the new one — without this a
    /// create→delete sequence goes out with a stale key (live evidence:
    /// eas_folder_debug 2026-08-02, delete with pre-create key → status 110).
    ///
    /// # Errors
    ///
    /// Returns `EasError`: `Transport`/`HttpStatus` when the HTTP round-trip fails,
    /// `Wbxml` when the response bytes do not decode, and `CommandStatus` when the
    /// server answers a non-success status.
    fn adopt_folder_op_sync_key(&mut self, resp: &WbxmlElement) {
        if let Some(key) = commands::folder_op_response_sync_key(resp)
            && !key.is_empty()
        {
            self.hierarchy_sync_key = key;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EasConfig;

    /// [MS-ASCMD] 2.2.3.181.1: a folder-op response carries the NEW hierarchy
    /// SyncKey; the client must adopt it or the next folder op goes out stale.
    #[test]
    fn folder_op_response_sync_key_is_adopted() {
        let mut client = EasClient::new(EasConfig::default());
        client.hierarchy_sync_key = "1".to_string(); // as if FolderSync ran
        let resp = WbxmlElement::container(
            PAGE_FOLDER,
            FH_FOLDER_CREATE,
            vec![
                WbxmlElement::text(PAGE_FOLDER, 0x0C, "1"),  // Status
                WbxmlElement::text(PAGE_FOLDER, 0x12, "2"),  // SyncKey
                WbxmlElement::text(PAGE_FOLDER, 0x08, "52"), // ServerId
            ],
        );
        client.adopt_folder_op_sync_key(&resp);
        assert_eq!(client.hierarchy_key(), "2");
    }

    /// Error responses may omit SyncKey — the cached key must survive.
    #[test]
    fn folder_op_response_without_sync_key_keeps_existing_key() {
        let mut client = EasClient::new(EasConfig::default());
        client.hierarchy_sync_key = "1".to_string();
        let resp = WbxmlElement::container(
            PAGE_FOLDER,
            FH_FOLDER_CREATE,
            vec![WbxmlElement::text(PAGE_FOLDER, 0x0C, "10")], // Status only
        );
        client.adopt_folder_op_sync_key(&resp);
        assert_eq!(client.hierarchy_key(), "1");
    }

    // ---- Sync empty body = no changes (hotfix 2026-08-03) ----
    //
    // Exchange returns an EMPTY HTTP body for Sync when the collection has
    // nothing to report (Android EasSync.java:225 treats empty as OK).
    // `no_changes_result` is the pure decision: build the no-changes
    // SyncResult that PRESERVES the request's sync key (Default's empty key
    // would corrupt the engine's cursor).

    #[test]
    fn no_changes_result_preserves_request_key() {
        let r = no_changes_result("sync-key-42");
        assert_eq!(r.sync_key, "sync-key-42");
    }

    #[test]
    fn no_changes_result_is_success_with_no_items() {
        let r = no_changes_result("k");
        assert_eq!(r.status, 1, "no-changes must read as success");
        assert!(r.added.is_empty());
        assert!(r.updated.is_empty());
        assert!(r.deleted_server_ids.is_empty());
        assert!(!r.more_available);
    }

    #[test]
    fn no_changes_result_empty_key_stays_empty() {
        let r = no_changes_result("");
        assert_eq!(r.sync_key, "");
        assert_eq!(r.status, 1);
    }
}
