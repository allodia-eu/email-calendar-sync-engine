//! What a message read over IMAP is, in the Gmail API's terms.
//!
//! The IMAP pass and the API pass write the same rows, so a message must come out of either
//! one identical: the same key, the same thread, the same memberships and keywords. This module
//! rebuilds the label ids the API would have listed for a message from what IMAP says of it,
//! and then hands them to the very functions the API pass uses ([`memberships_of`],
//! [`keywords_from_labels`]), so the two cannot disagree about what a label means.
//!
//! IMAP states a message's labels in three places, and none of them is the API's ids:
//!
//! - `X-GM-LABELS` names a system label with a backslash (`\Inbox`) and a label of the user's by
//!   its name, which the API's `labels.list` maps to its id.
//! - The flags carry read and starred: no `\Seen` is the API's `UNREAD`, `\Flagged` its `STARRED`.
//! - Spam and Trash are folders of their own, outside All Mail, so the folder a message was read
//!   from is its `SPAM` or `TRASH` label.
//!
//! The inbox categories are in none of them, and need not be: the API pass drops them too
//! ([`crate::normalize::is_unfiled_label`]).

use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    ids::{MessageId, ProviderKey, ThreadId},
    mail::{MailState, MailStateChange, Message, ThreadRef},
};
use provider_imap::{GmailFolder, GmailMessage, GmailState};
use serde_json::Value;

use crate::{
    error::GoogleError,
    json::opt_str,
    named_labels::NamedLabels,
    normalize::{keywords_from_labels, memberships_of, parse_rfc2822},
    transport::GoogleClient,
};

/// The API id of each system label IMAP names with a backslash.
const SYSTEM: [(&str, &str); 8] = [
    ("\\Inbox", "INBOX"),
    ("\\Sent", "SENT"),
    ("\\Draft", "DRAFT"),
    ("\\Important", "IMPORTANT"),
    ("\\Starred", "STARRED"),
    ("\\Spam", "SPAM"),
    ("\\Trash", "TRASH"),
    ("\\Chat", "CHAT"),
];

/// The account's label ids by name, from `labels.list`.
#[derive(Debug, Clone, Default)]
pub(crate) struct LabelIds(BTreeMap<String, String>);

impl LabelIds {
    /// Reads the account's labels (1 quota unit).
    pub(crate) async fn read(client: &GoogleClient) -> Result<Self, GoogleError> {
        let doc = client.get(&client.url("/gmail/v1/users/me/labels")).await?;
        Ok(Self::from_list(
            doc.get("labels")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice),
        ))
    }

    pub(crate) fn from_list(labels: &[Value]) -> Self {
        Self(
            labels
                .iter()
                .filter_map(|label| {
                    Some((
                        opt_str(label, "name")?.to_owned(),
                        opt_str(label, "id")?.to_owned(),
                    ))
                })
                .collect(),
        )
    }
}

/// The label ids the API would list for `state`, read from `folder`.
pub(crate) fn label_ids(state: &GmailState, folder: GmailFolder, ids: &LabelIds) -> Vec<String> {
    let mut labels = BTreeSet::new();
    for label in &state.labels {
        let id = SYSTEM
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(label))
            .map(|(_, id)| (*id).to_owned())
            .or_else(|| {
                (!label.starts_with('\\'))
                    .then(|| ids.0.get(label).cloned())
                    .flatten()
            });
        labels.extend(id);
    }
    let has_flag = |flag: &str| state.flags.iter().any(|f| f.eq_ignore_ascii_case(flag));
    if !has_flag("\\Seen") {
        labels.insert("UNREAD".to_owned());
    }
    if has_flag("\\Flagged") {
        labels.insert("STARRED".to_owned());
    }
    match folder {
        GmailFolder::Spam => {
            labels.insert("SPAM".to_owned());
        }
        GmailFolder::Trash => {
            labels.insert("TRASH".to_owned());
        }
        GmailFolder::All => {}
    }
    labels.into_iter().collect()
}

/// The key the API gives a message: its Gmail id in hex.
pub(crate) fn key_of(gmail_id: u64) -> Result<ProviderKey, GoogleError> {
    ProviderKey::new(format!("{gmail_id:x}"))
        .map_err(|e| GoogleError::protocol(format!("gmail id: {e}")))
}

/// A message read over IMAP, as the API pass would have written it.
pub(crate) fn message_of(
    read: GmailMessage,
    folder: GmailFolder,
    ids: &LabelIds,
    named: &NamedLabels,
) -> Result<Message, GoogleError> {
    let labels = label_ids(&read.state, folder, ids);
    let mut message = read.message;
    message.id = MessageId::new(key_of(read.state.gmail_id)?);
    message.mailboxes = memberships_of(&labels)?;
    message.keywords = keywords_from_labels(&labels);
    let thread = ThreadId::try_from(format!("{:x}", read.state.thread_id).as_str())
        .map_err(|e| GoogleError::protocol(format!("gmail thread id: {e}")))?;
    message.thread = Some(ThreadRef::provider_assigned(thread));
    message.sent_at = read.date.as_deref().and_then(parse_rfc2822);
    named.apply(&mut message);
    Ok(message)
}

/// A message's state read over IMAP, as the API pass's history delta would have written it.
pub(crate) fn state_change_of(
    state: &GmailState,
    folder: GmailFolder,
    ids: &LabelIds,
    named: &NamedLabels,
) -> Result<MailStateChange, GoogleError> {
    let labels = label_ids(state, folder, ids);
    let mut change = MailStateChange::new(
        key_of(state.gmail_id)?,
        MailState::with_keywords(keywords_from_labels(&labels)).filed_in(memberships_of(&labels)?),
    );
    named.apply_state(&mut change.state);
    Ok(change)
}

#[cfg(test)]
#[path = "imap_labels_tests.rs"]
mod tests;
