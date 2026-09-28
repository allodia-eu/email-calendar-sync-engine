//! Finding the mail stores a credential can open besides its own — the onboarding step
//! of a shared mailbox.
//!
//! Neither call touches the store, like [`sender_identities`](Engine::sender_identities):
//! what a credential can reach is the server's to say and nothing the engine keeps. A
//! store found here becomes an account only when the host onboards it — under an
//! [`AccountId`](engine_core::ids::AccountId) of the host's choosing, with a provider bound
//! to the returned handle through the adapter that produced it — and from then on every
//! sync, read and search path treats it as the ordinary account it is (`providers.md`).

use engine_provider::{Provider, SharedMailbox};
use engine_sync::SyncError;

use crate::{ApiError, Engine};

/// The longest address accepted: RFC 5321 §4.5.3.1.3 bounds a path at 256 octets
/// including its angle brackets, which leaves 254 for the address itself.
const MAX_ADDRESS_OCTETS: usize = 254;

impl Engine {
    /// Every mail store `provider`'s credential can open besides the one it opens by
    /// default.
    ///
    /// **Read
    /// [`Capabilities::shared_mailboxes`](engine_provider::Capabilities::shared_mailboxes)
    /// first.** Only an [`Enumerable`](engine_provider::SharedMailboxes::Enumerable) adapter
    /// answers this; a [`ByAddress`](engine_provider::SharedMailboxes::ByAddress) one can only
    /// be asked about an address the user types, through
    /// [`resolve_shared_mailbox`](Self::resolve_shared_mailbox).
    ///
    /// The answer does not depend on which store `provider` is bound to, so any provider
    /// holding the credential will do.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] wrapping the provider failure, which is
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState) on an
    /// adapter that cannot list.
    pub async fn list_shared_mailboxes<P: Provider>(
        &self,
        provider: &P,
    ) -> Result<Vec<SharedMailbox>, ApiError> {
        provider
            .list_shared_mailboxes()
            .await
            .map_err(|err| ApiError::Sync(SyncError::Provider(err)))
    }

    /// The mail store at `address`, if `provider`'s credential can open it.
    ///
    /// `address` is what the user typed, so it is checked here before any request: it must
    /// be a single `local@domain` with neither half empty, at most 254 octets, and free of
    /// whitespace and control characters. An adapter may refuse more — Graph refuses the
    /// characters that would restructure the URL the address is spliced into — but nothing
    /// that fails this check reaches one.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::InvalidInput`] for something that is not an address, and
    /// [`ApiError::Sync`] wrapping the provider failure otherwise.
    /// [`FailureClass::Permanent`](engine_core::error::FailureClass::Permanent) there means
    /// *not a store this credential can open* — on some servers that includes one that
    /// exists but was never shared, because the server will not say which — so a host
    /// asks the user to check the address rather than retrying.
    pub async fn resolve_shared_mailbox<P: Provider>(
        &self,
        provider: &P,
        address: &str,
    ) -> Result<SharedMailbox, ApiError> {
        validate_address(address)?;
        provider
            .resolve_shared_mailbox(address)
            .await
            .map_err(|err| ApiError::Sync(SyncError::Provider(err)))
    }
}

/// Rejects input that cannot be a mailbox address, before it reaches a provider.
///
/// Deliberately a *shape* check rather than a grammar: RFC 5322's local part admits far
/// more than anyone types, and the server — not this function — is the authority on which
/// addresses exist. What it does refuse is what could only be a mistake or an attack: no
/// `@`, an empty half, more than one `@`, whitespace, control characters, or an oversized
/// paste. The refusal names what was wrong and never echoes the input, which travels into
/// logs and dialogs.
fn validate_address(address: &str) -> Result<(), ApiError> {
    let invalid = |why: &str| {
        Err(ApiError::InvalidInput(format!(
            "not a mailbox address: {why}"
        )))
    };
    if address.len() > MAX_ADDRESS_OCTETS {
        return invalid("longer than 254 octets");
    }
    if let Some(bad) = address
        .chars()
        .find(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(ApiError::InvalidInput(format!(
            "not a mailbox address: contains U+{:04X}",
            bad as u32
        )));
    }
    let Some((local, domain)) = address.split_once('@') else {
        return invalid("no `@`");
    };
    if local.is_empty() || domain.is_empty() {
        return invalid("nothing on one side of the `@`");
    }
    if domain.contains('@') {
        return invalid("more than one `@`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_address_is_accepted() {
        validate_address("support@example.test").unwrap();
        // A tagged local part and an internationalized one are both addresses.
        validate_address("first.last+tag@example.test").unwrap();
        validate_address("jörg@example.test").unwrap();
        // Entra guest UPNs really do carry `#`; whether a transport can put one in a URL is
        // that adapter's question, not this one's.
        validate_address("user_partner.example#EXT#@tenant.example").unwrap();
    }

    #[test]
    fn something_that_is_not_an_address_is_refused() {
        for bad in [
            "",
            "support",
            "@example.test",
            "support@",
            "a@b@example.test",
            "support @example.test",
            "\"Support\" <support@example.test>",
        ] {
            assert!(
                matches!(validate_address(bad), Err(ApiError::InvalidInput(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_paste_longer_than_any_address_is_refused() {
        let local = "a".repeat(MAX_ADDRESS_OCTETS - "@example.test".len());
        validate_address(&format!("{local}@example.test")).unwrap();
        assert!(validate_address(&format!("{local}a@example.test")).is_err());
    }

    #[test]
    fn the_refusal_names_the_codepoint_and_never_echoes_the_input() {
        let err = validate_address("support@example.test\r\nX-Injected: yes").unwrap_err();
        let ApiError::InvalidInput(message) = err else {
            panic!("expected invalid input");
        };
        assert!(message.contains("U+000D"), "{message}");
        assert!(!message.contains("X-Injected"), "{message}");
        assert!(!message.contains("example.test"), "{message}");
    }
}
