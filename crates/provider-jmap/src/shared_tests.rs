//! Shared-mailbox discovery over the **real captured session** of an account that has
//! been granted access to two other stores (`tests/fixtures/session_shared_accounts.json`,
//! captured from the Stalwart harness as `alice@test.local`).
//!
//! The fixture is what makes these tests worth having: a hand-written session would encode
//! the same assumptions the code does, whereas this one carries the server's real answer —
//! including the finding that shapes the design, that a share whose only mailbox is
//! read-only still reports the *account* as writable.

use engine_core::{error::FailureClass, mail::MailboxAccess};
use engine_provider::SharedMailboxes;
use serde_json::Value;

use super::{provider_test_support::*, *};

/// The account ids in the captured fixture. Server-assigned and *not* stable across a fresh
/// bootstrap, so they are resolved by name at run time everywhere except here, where the
/// fixture pins them.
const ALICE: &str = "c";
const SUPPORT_GROUP: &str = "f";
const READ_ONLY_SHARE: &str = "d";

fn session_doc() -> Value {
    fixture("session_shared_accounts.json")
}

fn bound(selected: Option<&str>) -> JmapProvider {
    JmapProvider::with_executor(Box::new(FakeExecutor::from_session_selecting(
        &session_doc(),
        selected,
        vec![],
    )))
}

#[tokio::test]
async fn the_session_enumerates_every_store_the_credential_can_open() {
    let provider = bound(None);
    assert_eq!(
        provider.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::Enumerable
    );

    let listed = provider.list_shared_mailboxes().await.expect("enumerable");
    let by_address = |address: &str| {
        listed
            .iter()
            .find(|m| m.address.as_deref() == Some(address))
            .unwrap_or_else(|| panic!("no entry for {address}: {listed:?}"))
    };

    // Three stores: alice's own, the credential-less `support` group mailbox, and bob's
    // INBOX shared read-only.
    assert_eq!(listed.len(), 3);
    let own = by_address("alice@test.local");
    assert!(
        own.personal,
        "the signed-in account must be flagged personal"
    );
    assert_eq!(own.handle.as_str(), ALICE);
    assert!(!by_address("support@test.local").personal);
    assert_eq!(
        by_address("support@test.local").handle.as_str(),
        SUPPORT_GROUP
    );
    assert!(!by_address("bob@test.local").personal);
}

#[tokio::test]
async fn resolving_by_address_yields_the_handle_that_reopens_the_store() {
    let provider = bound(None);
    let resolved = provider
        .resolve_shared_mailbox("support@test.local")
        .await
        .expect("the session lists it");
    assert_eq!(resolved.handle.as_str(), SUPPORT_GROUP);
    assert!(!resolved.personal);

    // Case-insensitive, because an address a user types is not case-normalized and the
    // domain half never is (RFC 5321 §2.3.11).
    assert_eq!(
        provider
            .resolve_shared_mailbox("SUPPORT@TEST.LOCAL")
            .await
            .expect("case-insensitive")
            .handle,
        resolved.handle
    );

    // An address the session does not list is `Permanent`: a JMAP session lists precisely
    // the accounts the credential may open, so absent means absent, not withheld.
    assert_eq!(
        provider
            .resolve_shared_mailbox("nobody@test.local")
            .await
            .unwrap_err()
            .class(),
        FailureClass::Permanent
    );
}

#[tokio::test]
async fn binding_to_a_share_addresses_that_account_not_the_primary() {
    // Unbound, mail calls carry the `primaryAccounts` entry, as they always have.
    let unbound = bound(None);
    assert_eq!(unbound.executor.session().mail_account_id().unwrap(), ALICE);

    // Bound, they carry the share's id — the only change binding makes, which is what
    // "a shared mailbox is just another account" has to mean in practice.
    let shared = bound(Some(SUPPORT_GROUP));
    assert_eq!(
        shared.executor.session().mail_account_id().unwrap(),
        SUPPORT_GROUP
    );
    // The submission account follows the binding too, so a send *from* the shared mailbox
    // is not silently submitted as the signed-in user.
    assert_eq!(
        shared.executor.session().submission_account_id().unwrap(),
        SUPPORT_GROUP
    );
}

#[tokio::test]
async fn the_account_read_only_flag_is_not_the_signal_a_caller_needs() {
    // Bound to the read-only share, the advertised capabilities still say writable —
    // because `isReadOnly` on the *account* is false even though the one mailbox it
    // exposes grants read alone. This is not a bug being papered over: it is the observed
    // server behaviour, and the reason a host must consult `Mailbox::access` before
    // offering a write rather than trusting the account or its capabilities.
    let shared = bound(Some(READ_ONLY_SHARE));
    let caps = shared.connection_info().capabilities;
    assert!(
        caps.mail() && caps.mail_writes(),
        "the account-level flag reports this share as writable: {caps:?}"
    );
}

#[tokio::test]
async fn a_bound_share_advertises_only_what_that_account_exposes() {
    // Stalwart reports the *same* full `accountCapabilities` for every account it lists,
    // shares included (see the captured fixture), so against it this narrowing is a no-op.
    // It is kept because RFC 8620 §1.6.2 defines the set per account precisely so it *can*
    // differ, and claiming a domain the session denies would be a wrong promise. Driven
    // here by a session trimmed to what a narrowing server would send.
    let mut doc = session_doc();
    let share = doc["accounts"][READ_ONLY_SHARE]["accountCapabilities"]
        .as_object_mut()
        .expect("accountCapabilities");
    share.retain(|urn, _| urn == capability::MAIL || urn == capability::CORE);

    let shared = JmapProvider::with_executor(Box::new(FakeExecutor::from_session_selecting(
        &doc,
        Some(READ_ONLY_SHARE),
        vec![],
    )));
    let caps = shared.connection_info().capabilities;
    assert!(caps.mail(), "the share does expose mail");
    assert!(
        !caps.calendars() && !caps.contacts(),
        "capabilities must narrow to the bound account: {caps:?}"
    );
    // And asking for a domain it does not expose fails locally, naming the reason, rather
    // than as a method error from a server that was asked for something the session
    // already ruled out.
    let err = shared.executor.session().calendar_account_id().unwrap_err();
    assert!(err.to_string().contains("does not expose calendar"));
}

#[tokio::test]
async fn binding_to_an_account_the_session_does_not_list_fails_at_connect() {
    let base = reqwest::Url::parse("http://127.0.0.1:18080").unwrap();
    let err = Session::parse(
        &session_doc(),
        &base,
        crate::SessionUrlPolicy::RebaseToConnection,
        Some("no-such-account"),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("lists no account"),
        "a revoked or foreign handle must fail on connect: {err}"
    );
}

#[test]
fn a_read_only_share_reports_read_only_rights_on_its_mailbox_not_its_account() {
    // The whole reason rights live on the mailbox. Both halves come from the same live
    // server: the account says writable, the mailbox says read-only.
    let accounts = crate::session_accounts::parse_accounts(&session_doc());
    let share = accounts
        .iter()
        .find(|a| a.id == READ_ONLY_SHARE)
        .expect("the fixture lists the read-only share");
    assert!(!share.personal);
    assert!(
        !share.read_only,
        "Stalwart reports this share's *account* as writable — if that ever changes, the \
         per-mailbox-rights rationale in modeling.md is worth revisiting"
    );

    let result = fixture("mailbox_get_shared_read_only.json");
    let mailboxes: Vec<_> = result["list"]
        .as_array()
        .expect("list")
        .iter()
        .map(|m| mailbox_from_json(m).expect("normalizes"))
        .collect();
    // One mailbox, and it grants read and nothing else.
    assert_eq!(mailboxes.len(), 1);
    assert_eq!(
        mailboxes[0].role,
        Some(engine_core::mail::MailboxRole::Inbox)
    );
    assert_eq!(mailboxes[0].access, MailboxAccess::reader());
}
