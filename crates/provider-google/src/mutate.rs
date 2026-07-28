//! Applying a [`MailEdit`] to a Gmail message via `messages.modify`/`trash`/`delete`.
//!
//! Gmail's mutation surface is **label deltas** (`addLabelIds`/`removeLabelIds`), so the
//! three neutral edits map as:
//!
//! - [`MailEdit::SetKeywords`] → a label delta, with the keyword axis translated to Gmail's state
//!   labels: `$seen` is the **absence** of `UNREAD` (so setting `$seen` *removes* `UNREAD`, and
//!   clearing it *adds* `UNREAD` — an inversion), and `$flagged` is `STARRED`. Keywords Gmail has
//!   no label for are skipped.
//! - [`MailEdit::MoveTo`] → the neutral "ends up in exactly the destination" contract (matching
//!   JMAP's `mailboxIds` replacement). Because Gmail is multi-membership and `modify` takes deltas,
//!   the current place labels are fetched and all of them (bar the destination, the keyword-state
//!   labels, and the system labels `modify` cannot touch) are removed while the destination is
//!   added. A move to `TRASH` uses the dedicated `messages.trash` (the idiomatic, single-call
//!   trash), and a move to the synthetic All-Mail id is the **archive** — Gmail has no Archive
//!   label, so it removes the place labels and adds none.
//! - [`MailEdit::Delete`] → `messages.delete`, a **permanent** delete past Trash, which the full
//!   `mail.google.com` scope enables.

use std::collections::BTreeSet;

use engine_core::{
    ids::{MailboxId, ProviderKey},
    mail::{Keyword, SystemKeyword},
};
use engine_provider::{MailEdit, MailEditReceipt, ProviderResult};

use crate::{error::GoogleError, fetch, normalize::ALL_MAIL_ID, transport::GoogleClient};

/// The Gmail system label id for the Trash place — a `MoveTo` here uses `messages.trash`.
const TRASH_LABEL: &str = "TRASH";

/// System labels `messages.modify` refuses to add/remove (they are system-managed), plus
/// the keyword-state labels a *move* must preserve (read/flag state, not a place). Kept
/// out of a `MoveTo` replacement's remove set so the modify is not rejected and the
/// message's state survives the move.
const UNTOUCHABLE_ON_MOVE: &[&str] = &["SENT", "DRAFT", "CHAT", "UNREAD", "STARRED"];

/// Applies `edit` to its target message, returning the receipt the outbox records.
///
/// # Errors
///
/// A classified [`ProviderError`](engine_provider::ProviderError): a stale target is a
/// [`Conflict`](engine_core::error::FailureClass::Conflict); auth/rate-limit/retryable
/// map from the HTTP status.
pub(crate) async fn edit(
    client: &GoogleClient,
    edit: &MailEdit,
) -> ProviderResult<MailEditReceipt> {
    let key = edit.target();
    match edit {
        MailEdit::SetKeywords { add, remove, .. } => {
            let (add_labels, remove_labels) = keyword_label_delta(add, remove);
            modify(client, key, &add_labels, &remove_labels).await?;
        }
        MailEdit::MoveTo { destination, .. } => move_to(client, key, destination).await?,
        MailEdit::Delete { .. } => {
            // Permanent delete (past Trash), enabled by the full mail.google.com scope.
            client
                .delete(&client.url(&messages_path(key)), None)
                .await?;
        }
    }
    Ok(MailEditReceipt::new(key.clone()))
}

/// Moves `key` to `destination`: `messages.trash` for the Trash label, otherwise a
/// replacement `modify` that leaves the message in exactly the destination (bar
/// preserved state/system labels).
///
/// A move to the synthetic All-Mail id is the **archive**, and is the one destination
/// that is added to *nothing*: [`ALL_MAIL_ID`] is an id this adapter reserves for the
/// mailbox it synthesizes ([`crate::normalize::all_mail_mailbox`]) because Gmail exposes
/// no label for All Mail — an archived message simply carries no place label. Sending it
/// back as a label would be a `400 invalidArgument` on a name Gmail has never heard of,
/// so archiving is the removals alone.
async fn move_to(
    client: &GoogleClient,
    key: &ProviderKey,
    destination: &MailboxId,
) -> Result<(), GoogleError> {
    if destination.as_str() == TRASH_LABEL {
        client
            .post(
                &client.url(&format!("{}/trash", messages_path(key))),
                "application/json",
                b"{}".to_vec(),
            )
            .await?;
        return Ok(());
    }
    let label = destination.as_str();
    let current = fetch::message_labels(client, key).await?;
    let remove: Vec<String> = current
        .into_iter()
        .filter(|current| current != label && !UNTOUCHABLE_ON_MOVE.contains(&current.as_str()))
        .collect();
    let remove_refs: Vec<&str> = remove.iter().map(String::as_str).collect();
    let add: &[&str] = if label == ALL_MAIL_ID {
        &[]
    } else {
        std::slice::from_ref(&label)
    };
    modify(client, key, add, &remove_refs).await
}

/// `POST`s `messages.modify` with the given label deltas (a no-op when both are empty).
async fn modify(
    client: &GoogleClient,
    key: &ProviderKey,
    add: &[&str],
    remove: &[&str],
) -> Result<(), GoogleError> {
    let body = serde_json::json!({
        "addLabelIds": add,
        "removeLabelIds": remove,
    });
    let body = serde_json::to_vec(&body).map_err(GoogleError::from)?;
    client
        .post(
            &client.url(&format!("{}/modify", messages_path(key))),
            "application/json",
            body,
        )
        .await?;
    Ok(())
}

/// The label deltas for a keyword change, translating the keyword axis to Gmail's state
/// labels — `$seen` inverts against `UNREAD`; `$flagged` is `STARRED`; other keywords
/// have no Gmail label and are skipped.
fn keyword_label_delta(
    add: &BTreeSet<Keyword>,
    remove: &BTreeSet<Keyword>,
) -> (Vec<&'static str>, Vec<&'static str>) {
    let mut add_labels = Vec::new();
    let mut remove_labels = Vec::new();
    for keyword in add {
        match keyword.as_system() {
            // Marking read = removing UNREAD.
            Some(SystemKeyword::Seen) => remove_labels.push("UNREAD"),
            Some(SystemKeyword::Flagged) => add_labels.push("STARRED"),
            _ => {}
        }
    }
    for keyword in remove {
        match keyword.as_system() {
            // Marking unread = adding UNREAD.
            Some(SystemKeyword::Seen) => add_labels.push("UNREAD"),
            Some(SystemKeyword::Flagged) => remove_labels.push("STARRED"),
            _ => {}
        }
    }
    (add_labels, remove_labels)
}

/// The message resource path for `key` (`/gmail/v1/users/me/messages/{id}`).
fn messages_path(key: &ProviderKey) -> String {
    format!("/gmail/v1/users/me/messages/{}", key.as_str())
}

#[cfg(test)]
#[path = "mutate_tests.rs"]
mod tests;
