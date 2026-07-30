//! The shared-mailbox discovery seam at the [`Provider`] boundary: the rejecting
//! defaults, and that a boxed adapter's overrides survive dynamic dispatch.
//!
//! In its own file so `tests.rs` stays under the line limit.

use async_trait::async_trait;
use engine_core::{error::FailureClass, ids::SharedMailboxId};

use crate::{
    Capabilities, ConnectionInfo, Provider, ProviderError, ProviderResult, SharedMailbox,
    SharedMailboxes,
};

fn handle(value: &str) -> SharedMailboxId {
    SharedMailboxId::try_from(value).unwrap()
}

/// An adapter that says nothing about shared mailboxes — the default for every existing
/// provider, and what a Gmail-shaped adapter stays at.
struct NoDiscovery;

impl Provider for NoDiscovery {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail())
    }
}

/// An enumerable adapter (the JMAP/IMAP shape): it lists its own store alongside the ones
/// shared with it, and resolves an address against that list.
struct Enumerable;

#[async_trait]
impl Provider for Enumerable {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(
            Capabilities::none()
                .with_mail()
                .with_shared_mailboxes(SharedMailboxes::Enumerable),
        )
    }

    async fn list_shared_mailboxes(&self) -> ProviderResult<Vec<SharedMailbox>> {
        Ok(vec![
            SharedMailbox::new(handle("c"), "alice@test.local")
                .with_address("alice@test.local")
                .as_personal(),
            SharedMailbox::new(handle("f"), "support@test.local")
                .with_address("support@test.local"),
        ])
    }

    async fn resolve_shared_mailbox(&self, address: &str) -> ProviderResult<SharedMailbox> {
        self.list_shared_mailboxes()
            .await?
            .into_iter()
            .find(|mailbox| mailbox.address.as_deref() == Some(address))
            .ok_or_else(|| ProviderError::permanent("no such mailbox"))
    }
}

#[tokio::test]
async fn both_verbs_default_to_rejecting() {
    // A mail adapter with no such mechanism rejects both, so a capability-checking
    // caller — one that read `shared_mailboxes()` first — never relies on the default.
    let provider = NoDiscovery;
    assert_eq!(
        provider.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::Unsupported
    );
    assert_eq!(
        provider.list_shared_mailboxes().await.unwrap_err().class(),
        FailureClass::InvalidState
    );
    assert_eq!(
        provider
            .resolve_shared_mailbox("support@test.local")
            .await
            .unwrap_err()
            .class(),
        FailureClass::InvalidState
    );
}

#[tokio::test]
async fn a_boxed_adapter_delegates_both_verbs() {
    // Both methods have default bodies that *succeed at returning an error*, so a
    // forward the blanket impl forgot would not fail to compile — it would quietly
    // answer "cannot enumerate" for an adapter that enumerates perfectly well.
    let boxed: Box<dyn Provider> = Box::new(Enumerable);
    assert_eq!(
        boxed.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::Enumerable
    );

    let listed = boxed.list_shared_mailboxes().await.expect("enumerable");
    assert_eq!(listed.len(), 2);
    // The credential's own store is reported alongside the shared ones, flagged rather
    // than omitted, so a host renders one list without special-casing the signed-in
    // account.
    assert!(listed[0].personal);
    assert!(!listed[1].personal);

    let resolved = boxed
        .resolve_shared_mailbox("support@test.local")
        .await
        .expect("resolvable");
    assert_eq!(resolved.handle, handle("f"));

    // An address the server does not know is `Permanent`: nothing about the credential
    // would make it resolve.
    assert_eq!(
        boxed
            .resolve_shared_mailbox("nobody@test.local")
            .await
            .unwrap_err()
            .class(),
        FailureClass::Permanent
    );
}
