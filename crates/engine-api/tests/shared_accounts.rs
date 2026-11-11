//! The shared-account onboarding step, as a host drives it: through `engine_api` alone.
//!
//! Every name a host needs is imported from `engine_api` — the fake adapter is the only
//! thing that reaches into `engine_provider`, because an adapter is what a host does *not*
//! write. A type missing from the facade's re-exports therefore fails this file to compile,
//! which is the check `facade_exports.rs` makes for the calendar intents.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use engine_api::{
    ApiError, Capabilities, Engine, FailureClass, Mailbox, MailboxAccess, Provider, SearchDomain,
    SharedAccount, SharedAccountId, SharedAccounts, StoreDomains, SyncError,
};
use engine_provider::{CalendarWrites, ConnectionInfo, MailboxWrites, ProviderResult};

/// An enumerable adapter that counts how often it is asked, so a test can show a refused
/// address never reached it.
#[derive(Default)]
struct Enumerable {
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for Enumerable {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(
            Capabilities::none()
                .with_mail()
                .with_shared_accounts(SharedAccounts::Enumerable),
        )
    }

    async fn list_shared_accounts(&self) -> ProviderResult<Vec<SharedAccount>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![
            SharedAccount::new(
                SharedAccountId::try_from("f").unwrap(),
                "Support",
                StoreDomains::of(SearchDomain::Mail).with(SearchDomain::Calendar),
            )
            .with_address("support@example.test"),
        ])
    }
}

impl CalendarWrites for Enumerable {}
impl MailboxWrites for Enumerable {}

#[tokio::test]
async fn a_host_lists_and_resolves_through_the_facade() {
    let engine = Engine::open_in_memory().unwrap();
    // Held the way a host that picks its adapter at runtime holds one.
    let provider: Box<dyn Provider> = Box::new(Enumerable::default());

    assert_eq!(
        provider.connection_info().capabilities.shared_accounts(),
        SharedAccounts::Enumerable
    );
    let listed = engine.list_shared_accounts(&provider).await.unwrap();
    assert_eq!(listed.len(), 1);
    // What a host dials for the store: every domain it offers, named with facade types.
    assert_eq!(
        listed[0].domains.iter().collect::<Vec<_>>(),
        [SearchDomain::Mail, SearchDomain::Calendar]
    );

    let resolved = engine
        .resolve_shared_account(&provider, "Support@Example.Test")
        .await
        .unwrap();
    let handle: &SharedAccountId = &resolved.handle;
    assert_eq!(handle, &listed[0].handle);

    let missing = engine
        .resolve_shared_account(&provider, "nobody@example.test")
        .await
        .unwrap_err();
    // Terminal, so a host asks the user to check the address rather than retrying — and
    // it can tell that from the class, without parsing the message.
    let ApiError::Sync(SyncError::Provider(err)) = missing else {
        panic!("expected the provider's answer, got {missing:?}");
    };
    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn an_address_that_is_not_one_never_reaches_the_provider() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = Enumerable::default();

    for bad in [
        "support",
        "support@",
        "a b@example.test",
        "x@example.test\n",
    ] {
        let err = engine
            .resolve_shared_account(&provider, bad)
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::InvalidInput(_)), "{bad:?}: {err:?}");
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

/// What a host writes before offering "move out of this folder": the per-folder right,
/// read off the synced mailbox with nothing but facade names.
fn may_offer_a_move_out(folder: &Mailbox) -> bool {
    folder.access.may_remove_items
}

#[test]
fn a_host_can_read_what_it_may_do_in_a_folder() {
    let _: fn(&Mailbox) -> bool = may_offer_a_move_out;
    assert!(MailboxAccess::owner().may_remove_items);
    let read_only = MailboxAccess::reader();
    assert!(read_only.may_read_items && !read_only.may_remove_items);
}
