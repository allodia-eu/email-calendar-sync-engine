//! Applying a [`MailEdit`] to an already-synced Graph message.
//!
//! Graph models mail state as typed properties, not a keyword set (`graph.md`), so the
//! three provider-neutral edits (`modeling.md`) map onto three *different* Graph shapes:
//!
//! - [`MailEdit::SetKeywords`] → `PATCH /messages/{id}` toggling `isRead` (`$seen`) and
//!   `flag.flagStatus` (`$flagged`) — the only two writable keyword-like properties Graph exposes.
//!   Any other keyword is **rejected**, never silently dropped: `$draft` is read-only, and Graph
//!   categories are a different concept.
//! - [`MailEdit::MoveTo`] → `POST /messages/{id}/move { destinationId }`. Immutable ids are stable
//!   across a move (live-verified — the moved copy keeps its id), so the receipt key is the
//!   unchanged target and the next sync of the destination folder reconciles the new membership,
//!   exactly like JMAP.
//! - [`MailEdit::Delete`] → `POST /messages/{id}/permanentDelete` — a hard delete, the neutral
//!   irreversible-delete contract (a Trash move is `MoveTo(trash)`, not this). An already-gone
//!   message (`404`) is idempotent success; the ambiguous re-delete Graph answers with `403
//!   ErrorCannotDeleteObject` (the item is in Purges) propagates, left to the outbox's
//!   `NeedsConfirmation` — the same shape as the calendar delete.
//!
//! Mail edits carry no ETag guard (the [`MailEdit`] shape has none), so — like IMAP
//! `UID STORE` and JMAP `Email/set` — every write is unconditional (no `If-Match`).

use std::collections::BTreeSet;

use engine_core::{
    ids::{MailboxId, ProviderKey},
    mail::{Keyword, SystemKeyword},
};
use engine_provider::{MailEdit, MailEditReceipt, ProviderError, ProviderResult};
use serde_json::{Map, Value, json};

use crate::{error::GraphError, transport::GraphClient};

/// Applies `edit` to its target message, returning a receipt carrying the (immutable,
/// so unchanged) target key.
///
/// # Errors
///
/// A classified [`ProviderError`]: [`InvalidState`] for a keyword Graph cannot express;
/// otherwise the underlying write's status classification (a `410`→resync, `429`→rate
/// limit, `5xx`→retryable, other 4xx→permanent).
///
/// [`InvalidState`]: engine_core::error::FailureClass::InvalidState
pub(crate) async fn edit_mail(
    client: &GraphClient,
    edit: &MailEdit,
) -> ProviderResult<MailEditReceipt> {
    let target = edit.target();
    match edit {
        MailEdit::SetKeywords { add, remove, .. } => {
            set_keywords(client, target, add, remove).await
        }
        MailEdit::MoveTo { destination, .. } => move_to(client, target, destination).await,
        MailEdit::Delete { .. } => delete(client, target).await,
    }
}

/// `PATCH /messages/{id}` toggling `isRead`/`flag` from the neutral keyword sets. An empty
/// patch (both sides empty) is a no-op — no request, receipt resolves the pending op.
async fn set_keywords(
    client: &GraphClient,
    target: &ProviderKey,
    add: &BTreeSet<Keyword>,
    remove: &BTreeSet<Keyword>,
) -> ProviderResult<MailEditReceipt> {
    let body = keyword_patch(add, remove)?;
    if body.is_empty() {
        return Ok(MailEditReceipt::new(target.clone()));
    }
    client
        .patch(
            &client.url(&format!("/messages/{}", target.as_str())),
            "application/json",
            None,
            serde_json::to_vec(&Value::Object(body)).map_err(GraphError::from)?,
        )
        .await?;
    Ok(MailEditReceipt::new(target.clone()))
}

/// Builds the `PATCH /messages/{id}` body from the add/remove keyword sets, mapping `$seen`
/// to `isRead` and `$flagged` to `flag.flagStatus` and rejecting any keyword Graph has no
/// property for (never a partial, false-success write).
fn keyword_patch(
    add: &BTreeSet<Keyword>,
    remove: &BTreeSet<Keyword>,
) -> ProviderResult<Map<String, Value>> {
    let mut body = Map::new();
    for keyword in add {
        apply_keyword(&mut body, keyword, true)?;
    }
    for keyword in remove {
        apply_keyword(&mut body, keyword, false)?;
    }
    Ok(body)
}

/// Sets (`set`) or clears the one Graph property a system keyword maps to. `$seen`→`isRead`
/// bool; `$flagged`→`flag.flagStatus` (`flagged`/`notFlagged`). Any other keyword is
/// rejected: Graph exposes no writable property for it (`$draft` is read-only, categories
/// are a separate concept), so applying it would be a silent no-op the caller reads as done.
fn apply_keyword(
    body: &mut Map<String, Value>,
    keyword: &Keyword,
    set: bool,
) -> ProviderResult<()> {
    match keyword.as_system() {
        Some(SystemKeyword::Seen) => {
            body.insert("isRead".to_owned(), json!(set));
        }
        Some(SystemKeyword::Flagged) => {
            let status = if set { "flagged" } else { "notFlagged" };
            body.insert("flag".to_owned(), json!({ "flagStatus": status }));
        }
        _ => {
            return Err(ProviderError::invalid_state(format!(
                "Graph mail can write only the $seen and $flagged keywords; got {}",
                keyword.as_str()
            )));
        }
    }
    Ok(())
}

/// `POST /messages/{id}/move { destinationId }`. Immutable ids survive a move, so the
/// receipt carries the unchanged target key and the destination folder reconciles the new
/// membership on its next sync (the JMAP shape, not IMAP's new-key-on-move).
async fn move_to(
    client: &GraphClient,
    target: &ProviderKey,
    destination: &MailboxId,
) -> ProviderResult<MailEditReceipt> {
    let body = json!({ "destinationId": destination.as_str() });
    client
        .post(
            &client.url(&format!("/messages/{}/move", target.as_str())),
            "application/json",
            serde_json::to_vec(&body).map_err(GraphError::from)?,
        )
        .await?;
    Ok(MailEditReceipt::new(target.clone()))
}

/// `POST /messages/{id}/permanentDelete` — an irreversible hard delete. An already-gone
/// message (`404`) is idempotent success, mirroring the calendar delete; any other status
/// propagates (the ambiguous re-delete is `403`, left to the outbox).
async fn delete(client: &GraphClient, target: &ProviderKey) -> ProviderResult<MailEditReceipt> {
    match client
        .post(
            &client.url(&format!("/messages/{}/permanentDelete", target.as_str())),
            "application/json",
            Vec::new(),
        )
        .await
    {
        Ok(_) | Err(GraphError::Status { status: 404, .. }) => {
            Ok(MailEditReceipt::new(target.clone()))
        }
        Err(other) => Err(other.into()),
    }
}

#[cfg(test)]
#[path = "mutate_tests.rs"]
mod tests;
