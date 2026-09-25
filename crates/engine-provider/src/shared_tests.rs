//! The shared-mailbox discovery seam: the neutral types, the rejecting defaults, the
//! list-backed default for resolving, and that a boxed adapter keeps its overrides.

use async_trait::async_trait;
use engine_core::{error::FailureClass, ids::SharedMailboxId};

use super::*;
use crate::{CalendarWrites, ConnectionInfo, ProviderError};

fn handle(value: &str) -> SharedMailboxId {
    SharedMailboxId::try_from(value).unwrap()
}

/// An adapter that says nothing about shared mailboxes — every existing adapter's
/// starting point, and where one with no mechanism (Gmail) stays.
struct NoMechanism;

impl Provider for NoMechanism {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail())
    }
}

impl CalendarWrites for NoMechanism {}

/// The JMAP/IMAP shape: it lists, and implements nothing else — resolving comes from the
/// default. `fail_listing` stands in for the transport failing mid-discovery.
struct Lists {
    fail_listing: bool,
}

#[async_trait]
impl Provider for Lists {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(
            Capabilities::none()
                .with_mail()
                .with_shared_mailboxes(SharedMailboxes::Enumerable),
        )
    }

    async fn list_shared_mailboxes(&self) -> ProviderResult<Vec<SharedMailbox>> {
        if self.fail_listing {
            return Err(ProviderError::retryable("the connection dropped"));
        }
        Ok(vec![
            SharedMailbox::new(handle("f"), "Support").with_address("support@test.local"),
            // A store with no address — an IMAP server whose logins are bare names.
            SharedMailbox::new(handle("Other Users/bob"), "bob"),
        ])
    }
}

impl CalendarWrites for Lists {}

/// The Graph shape: it cannot list, so it answers for an address itself.
struct AnswersByAddress;

#[async_trait]
impl Provider for AnswersByAddress {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_shared_mailboxes(SharedMailboxes::ByAddress))
    }

    async fn resolve_shared_mailbox(&self, address: &str) -> ProviderResult<SharedMailbox> {
        Ok(SharedMailbox::new(handle(address), address).with_address(address))
    }
}

impl CalendarWrites for AnswersByAddress {}

/// Mirrors how `engine-api` reaches the verbs: through a generic bound. Calling
/// `boxed.list_shared_mailboxes()` directly would auto-deref to the adapter inside and
/// pass with the `Box` forward deleted, so every box test goes through here.
async fn list_as_engine_would<P: Provider>(provider: &P) -> ProviderResult<Vec<SharedMailbox>> {
    provider.list_shared_mailboxes().await
}

async fn resolve_as_engine_would<P: Provider>(
    provider: &P,
    address: &str,
) -> ProviderResult<SharedMailbox> {
    provider.resolve_shared_mailbox(address).await
}

#[test]
fn a_store_starts_addressless_and_takes_an_address_when_given_one() {
    let bare = SharedMailbox::new(handle("f"), "Support");
    assert_eq!(bare.handle.as_str(), "f");
    assert_eq!(bare.name, "Support");
    // A label is not an address: nothing is inferred from the name.
    assert!(bare.address.is_none());
    assert!(!bare.answers_to("Support"));

    let addressed = bare.with_address("support@test.local");
    assert_eq!(addressed.address.as_deref(), Some("support@test.local"));
}

#[test]
fn an_address_matches_regardless_of_ascii_case() {
    let store = SharedMailbox::new(handle("f"), "Support").with_address("support@test.local");
    assert!(store.answers_to("support@test.local"));
    assert!(store.answers_to("Support@Test.Local"));
    assert!(!store.answers_to("support@test.local.example"));
    assert!(!store.answers_to("upport@test.local"));
}

#[test]
fn no_mechanism_is_the_default_and_independent_of_the_other_capabilities() {
    assert_eq!(SharedMailboxes::default(), SharedMailboxes::Unsupported);
    assert_eq!(
        Capabilities::default().shared_mailboxes(),
        SharedMailboxes::Unsupported
    );
    // A mail adapter can have no mechanism, and one with a mechanism need not be a mail
    // adapter at all: the promise is about the credential.
    let gmail_shaped = Capabilities::none().with_mail().with_mail_writes();
    assert_eq!(
        gmail_shaped.shared_mailboxes(),
        SharedMailboxes::Unsupported
    );
    let only_discovery = Capabilities::none().with_shared_mailboxes(SharedMailboxes::ByAddress);
    assert_eq!(
        only_discovery.shared_mailboxes(),
        SharedMailboxes::ByAddress
    );
    assert!(!only_discovery.mail());
}

#[tokio::test]
async fn an_adapter_with_no_mechanism_rejects_both_verbs() {
    let provider = NoMechanism;
    for err in [
        provider.list_shared_mailboxes().await.unwrap_err(),
        provider
            .resolve_shared_mailbox("support@test.local")
            .await
            .unwrap_err(),
    ] {
        assert_eq!(err.class(), FailureClass::InvalidState, "{err}");
    }
}

#[tokio::test]
async fn resolving_on_a_listing_adapter_reads_the_list() {
    let provider = Lists {
        fail_listing: false,
    };
    let found = provider
        .resolve_shared_mailbox("SUPPORT@test.local")
        .await
        .expect("listed, in another case");
    assert_eq!(found.handle.as_str(), "f");

    // Absent from the list the server gave is terminal: nothing about retrying would make
    // the credential able to open it.
    let missing = provider
        .resolve_shared_mailbox("nobody@test.local")
        .await
        .unwrap_err();
    assert_eq!(missing.class(), FailureClass::Permanent);
    // An addressless store cannot be found by its label.
    assert_eq!(
        provider
            .resolve_shared_mailbox("bob")
            .await
            .unwrap_err()
            .class(),
        FailureClass::Permanent
    );
}

#[tokio::test]
async fn a_failed_listing_is_not_reported_as_a_missing_mailbox() {
    // The easy mistake: every error on the way to "not in the list" reads as "not shared".
    // A dropped connection is retryable and must stay so, or a host tells the user the
    // mailbox does not exist when the network blinked.
    let provider = Lists { fail_listing: true };
    let err = provider
        .resolve_shared_mailbox("support@test.local")
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Retryable);
}

#[tokio::test]
async fn an_adapter_that_answers_by_address_does_not_fall_back_to_listing() {
    // The list-backed default is for adapters that list; one that cannot is asked directly,
    // and its enumeration stays at the rejecting default.
    let provider = AnswersByAddress;
    assert_eq!(
        provider.list_shared_mailboxes().await.unwrap_err().class(),
        FailureClass::InvalidState
    );
    assert!(
        provider
            .resolve_shared_mailbox("shared@example.test")
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_boxed_adapter_keeps_both_overrides() {
    // Both verbs have defaults that *compile* without a forward, so a forgotten one would
    // not fail the build: the box would silently answer the default. For the lister that
    // is "cannot enumerate"; for the by-address adapter it is the list-backed resolve,
    // which rejects — so each case below fails with its forward removed.
    let lister: Box<dyn Provider> = Box::new(Lists {
        fail_listing: false,
    });
    assert_eq!(list_as_engine_would(&lister).await.unwrap().len(), 2);

    let by_address: Box<dyn Provider> = Box::new(AnswersByAddress);
    let resolved = resolve_as_engine_would(&by_address, "shared@example.test")
        .await
        .expect("the box must reach the adapter's own resolve");
    assert_eq!(resolved.handle.as_str(), "shared@example.test");
}

#[tokio::test]
async fn a_boxed_adapter_with_no_mechanism_still_rejects() {
    let boxed: Box<dyn Provider> = Box::new(NoMechanism);
    assert_eq!(
        list_as_engine_would(&boxed).await.unwrap_err().class(),
        FailureClass::InvalidState
    );
    assert_eq!(
        resolve_as_engine_would(&boxed, "support@test.local")
            .await
            .unwrap_err()
            .class(),
        FailureClass::InvalidState
    );
}
