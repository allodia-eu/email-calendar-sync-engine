//! The account's sender identities: reading the names a provider holds, and changing
//! one where the provider lets the account holder.
//!
//! Neither call touches the store. A sender name is a **host preference**, not synced
//! PIM state: the `From` a send carries is assembled from the caller's
//! [`Draft`](engine_provider::Draft), so the host's own copy is what reaches the wire
//! and the server's copy is a courtesy to the account's other clients. What the engine
//! owns here is the protocol, nothing else.

use engine_provider::{Provider, SenderIdentity, SenderIdentityId};
use engine_sync::SyncError;

use crate::{ApiError, Engine};

/// The longest sender name that will be sent to a provider.
///
/// Not a protocol limit — RFC 5322 bounds a header's *line* length, not a display
/// name — but a bound on what a host can put on the wire in one field. Long enough
/// that no real name approaches it, short enough that a paste of a whole document
/// never reaches a server.
const MAX_SENDER_NAME: usize = 128;

impl Engine {
    /// The addresses `account` may send as, with the name the provider holds for each.
    ///
    /// **Read
    /// [`Capabilities::sender_identities`](engine_provider::Capabilities::sender_identities)
    /// first.** It is `None` on a provider with no identity object at all (IMAP/SMTP),
    /// where the name is the host's alone and there is nothing here to read.
    ///
    /// The order is the provider's and carries no meaning: find the account's own
    /// identity by matching an address (`engine_core::scheduling::addresses_match`),
    /// never by taking the first entry.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] wrapping the provider failure — including the refusal
    /// of a provider whose credential is short the scope its settings API needs, which
    /// no capability can predict.
    pub async fn sender_identities<P: Provider>(
        &self,
        provider: &P,
        account: &engine_core::ids::AccountId,
    ) -> Result<Vec<SenderIdentity>, ApiError> {
        provider
            .sender_identities(account)
            .await
            .map_err(|err| ApiError::Sync(SyncError::Provider(err)))
    }

    /// Changes the name the provider holds for `identity`.
    ///
    /// **Read
    /// [`IdentityControls`](engine_provider::IdentityControls) first**: on a read-only
    /// directory the edit belongs to an administrator, and offering it is offering an
    /// edit that cannot land.
    ///
    /// `name` is rejected here, before any request, when it carries a control character
    /// or is longer than 128 characters. A control character is the header-injection shape
    /// the RFC 5322 assembler already refuses, and refusing it at the *setting* is the
    /// difference between a settings field that says no and a mailbox that cannot send.
    /// An empty `name` is allowed and clears the provider's copy.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::InvalidInput`] for a name that cannot go in a header, and
    /// [`ApiError::Sync`] if the provider refuses the change — which it may do without
    /// warning, since no capability promises a given server implements the write.
    pub async fn set_sender_name<P: Provider>(
        &self,
        provider: &P,
        account: &engine_core::ids::AccountId,
        identity: &SenderIdentityId,
        name: &str,
    ) -> Result<(), ApiError> {
        validate_sender_name(name)?;
        provider
            .set_sender_name(account, identity, name)
            .await
            .map_err(|err| ApiError::Sync(SyncError::Provider(err)))
    }
}

/// Rejects a name that cannot safely become a `From` display name.
fn validate_sender_name(name: &str) -> Result<(), ApiError> {
    if name.chars().count() > MAX_SENDER_NAME {
        return Err(ApiError::InvalidInput(format!(
            "sender name is longer than {MAX_SENDER_NAME} characters"
        )));
    }
    if let Some(bad) = name.chars().find(|ch| ch.is_control()) {
        // Named by codepoint, never echoed: a CR or LF pasted into a settings field is
        // the header-injection shape, and reflecting it into an error message carries it
        // onward into whatever renders that message.
        return Err(ApiError::InvalidInput(format!(
            "sender name contains a control character (U+{:04X})",
            bad as u32
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_name_is_accepted() {
        validate_sender_name("Alice Smith").unwrap();
        // Non-ASCII is ordinary: the assembler encodes it as an RFC 2047 word.
        validate_sender_name("Ada Lovelace-Byron, Café").unwrap();
    }

    #[test]
    fn an_empty_name_is_accepted_because_it_clears_the_providers_copy() {
        validate_sender_name("").unwrap();
    }

    #[test]
    fn a_newline_is_refused_at_the_setting_not_at_the_send() {
        // The header-injection shape. The assembler refuses it too, but by then the user
        // has a mailbox that cannot send and no idea why.
        for injected in ["Alice\r\nBcc: eve@example.com", "Alice\nX: y", "Alice\rX"] {
            let err = validate_sender_name(injected).unwrap_err();
            assert!(matches!(err, ApiError::InvalidInput(_)), "{injected:?}");
        }
    }

    #[test]
    fn the_refusal_never_echoes_the_offending_bytes() {
        // An error message travels: into a log, into a dialog, into whatever renders it.
        // Echoing the injected value would carry the payload along with the complaint.
        let err = validate_sender_name("Alice\r\nBcc: eve@example.com").unwrap_err();
        let ApiError::InvalidInput(message) = err else {
            panic!("expected invalid input");
        };
        assert!(!message.contains("eve@example.com"), "{message}");
        assert!(!message.contains('\r'), "{message}");
    }

    #[test]
    fn a_name_longer_than_the_cap_is_refused() {
        validate_sender_name(&"a".repeat(MAX_SENDER_NAME)).unwrap();
        assert!(validate_sender_name(&"a".repeat(MAX_SENDER_NAME + 1)).is_err());
    }

    #[test]
    fn the_cap_counts_characters_not_bytes() {
        // A name of accented characters is not half as long as an ASCII one.
        validate_sender_name(&"é".repeat(MAX_SENDER_NAME)).unwrap();
    }
}
