//! Carrying engine-derived fields across a provider's rewrite of a message.
//!
//! An apply replaces a message's whole stored payload with the one the provider just
//! sent. Two fields on that payload are not the provider's to send:
//!
//! - `thread` — the engine derives it from the reference graph; only JMAP has a server-side
//!   equivalent.
//! - `preview` — IMAP has no server snippet, so a pass that streams metadata alone carries none.
//!
//! So marking one message read erased both: the message left its conversation until
//! the next full derivation pass put it back, and its list row lost its snippet.
//!
//! This restores them from what the store already holds, for exactly the messages
//! that arrived without them. A provider that *does* supply a thread is authoritative
//! and is never overridden. A message body is immutable, so a stored preview is
//! always still true of the key it was stored under.
//!
//! It is a stopgap for a whole-payload write. Once an apply touches the columns the
//! provider supplied and nothing else, there is nothing left to restore and this goes.

use engine_core::{mail::Message, sync::SyncScope};
use engine_store::StoreRead;

use crate::SyncError;

/// Fills each message's missing `thread`/`preview` from its stored copy.
///
/// Only messages arriving without one are read back, so a provider that supplies both
/// costs nothing, and a message the store has never seen costs one indexed miss.
pub(crate) async fn restore<S: StoreRead>(
    store: &S,
    scope: &SyncScope,
    changed: &mut [Message],
) -> Result<(), SyncError> {
    for message in changed {
        let wants_thread = message.thread.is_none();
        let wants_preview = is_blank(message.preview.as_deref());
        if !wants_thread && !wants_preview {
            continue;
        }
        let Some(payload) = store.object_payload(scope, message.id.key()).await? else {
            continue;
        };
        // Decoded as a whole `Message` rather than by picking two JSON fields out by
        // name: the field names are serde's to choose, and a rename would silently
        // stop restoring anything.
        let stored: Message =
            serde_json::from_value(payload).map_err(|err| SyncError::decode(&err))?;
        if wants_thread {
            message.thread = stored.thread;
        }
        if wants_preview && !is_blank(stored.preview.as_deref()) {
            message.preview = stored.preview;
        }
    }
    Ok(())
}

/// Whether a preview carries nothing — absent and empty are the same "not supplied".
fn is_blank(preview: Option<&str>) -> bool {
    preview.is_none_or(str::is_empty)
}

#[cfg(test)]
mod tests {
    use super::is_blank;

    #[test]
    fn an_empty_preview_counts_as_not_supplied() {
        // A provider that maps a message it fetched no body for lands `Some("")` just
        // as often as `None`; treating only `None` as missing would leave those rows
        // blank for the rest of the sync.
        assert!(is_blank(None));
        assert!(is_blank(Some("")));
        assert!(!is_blank(Some("Quarterly review — please confirm")));
    }
}
