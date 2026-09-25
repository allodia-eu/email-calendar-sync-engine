//! Shared-mailbox discovery and binding, over the **captured** session of an account that
//! two other stores are shared with (`tests/fixtures/session_shared_accounts.json`, taken
//! from the Stalwart harness as `alice@test.local`).
//!
//! The fixture is what makes these worth having: a hand-written session would carry the
//! same assumptions the code does, while this one carries the server's answer — including
//! the finding the design rests on, that a share whose only mailbox is read-only reports
//! its *account* as writable.

use std::sync::Arc;

use engine_core::{error::FailureClass, mail::MailboxAccess, sync::SyncUpdate};
use engine_provider::SharedMailboxes;
use serde_json::{Value, json};

use super::{provider_test_support::*, *};
use crate::{
    executor::Executor,
    session::{Session, SessionUrlPolicy},
};

/// The account ids in the captured session. The server assigns them and they are *not*
/// stable across a fresh bootstrap, so the live suite resolves them by name; only here,
/// where the fixture pins them, are they spelled out.
const ALICE: &str = "c";
const SUPPORT: &str = "f";
const BOB: &str = "d";

fn session_doc() -> Value {
    fixture("session_shared_accounts.json")
}

fn bound(handle: Option<&str>, responses: Vec<Value>) -> (JmapProvider, Arc<FakeExecutor>) {
    let exec = Arc::new(FakeExecutor::bound_to(&session_doc(), handle, responses));
    (JmapProvider::with_executor(Box::new(exec.clone())), exec)
}

#[tokio::test]
async fn the_session_lists_the_mailboxes_shared_with_the_credential_and_not_its_own() {
    let (provider, exec) = bound(None, vec![]);
    assert_eq!(
        provider.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::Enumerable
    );
    let listed = provider.list_shared_mailboxes().await.unwrap();
    let handles: Vec<&str> = listed.iter().map(|m| m.handle.as_str()).collect();
    // The group mailbox and bob's read-only share — alice's own account is not there, so a
    // host renders this list as "add another mailbox" without filtering it.
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert!(
        handles.contains(&SUPPORT) && handles.contains(&BOB),
        "{handles:?}"
    );
    assert!(!handles.contains(&ALICE));
    let support = listed
        .iter()
        .find(|m| m.handle.as_str() == SUPPORT)
        .unwrap();
    assert_eq!(support.address.as_deref(), Some("support@test.local"));
    assert_eq!(support.name, "support@test.local");
    // No request: the accounts map arrived with the session connect already fetched.
    assert_eq!(exec.request_count(), 0);
}

#[tokio::test]
async fn an_account_name_that_is_not_an_address_is_offered_as_a_label_only() {
    // RFC 8620 makes `name` "usually" an address, not always: a display name must not
    // come back as something a host would send mail to, or match a typed address.
    let mut doc = session_doc();
    doc["accounts"][BOB]["name"] = json!("Bob's Mailbox");
    let exec = Arc::new(FakeExecutor::bound_to(&doc, None, vec![]));
    let provider = JmapProvider::with_executor(Box::new(exec));
    let listed = provider.list_shared_mailboxes().await.unwrap();
    let bob = listed.iter().find(|m| m.handle.as_str() == BOB).unwrap();
    assert_eq!(
        (bob.name.as_str(), bob.address.as_deref()),
        ("Bob's Mailbox", None)
    );
    let err = provider
        .resolve_shared_mailbox("bob@test.local")
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn an_address_resolves_through_the_listing_and_the_own_account_never_does() {
    let (provider, _) = bound(None, vec![]);
    let support = provider
        .resolve_shared_mailbox("Support@Test.Local")
        .await
        .unwrap();
    assert_eq!(support.handle.as_str(), SUPPORT);

    // The credential's own address is in the session too, but it is not a shared mailbox:
    // resolving it must not hand a host a second binding to the account it already has.
    for not_shared in ["alice@test.local", "nobody@test.local"] {
        assert_eq!(
            provider
                .resolve_shared_mailbox(not_shared)
                .await
                .unwrap_err()
                .class(),
            FailureClass::Permanent,
            "{not_shared}"
        );
    }
}

#[tokio::test]
async fn a_bound_client_addresses_the_share_and_reads_its_rights_off_each_folder() {
    let (provider, exec) = bound(
        Some(BOB),
        vec![fixture("mailbox_get_shared_read_only_response.json")],
    );
    // Bob's account reports `isReadOnly: false` although the one mailbox it exposes grants
    // read alone — so writes stay advertised at the account level, as the RFC flag says,
    // and the folder is where the truth is.
    assert!(provider.connection_info().capabilities.mail_writes());

    let sync = provider.sync_mailboxes(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("a first sync is a snapshot");
    };
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].access, MailboxAccess::reader());
    // And the call went to bob's account, not the credential's primary one.
    let requests = exec.requests.lock().unwrap();
    assert_eq!(requests[0]["methodCalls"][0][1]["accountId"], json!(BOB));
}

#[tokio::test]
async fn discovery_does_not_depend_on_what_the_client_is_bound_to() {
    // What a credential can reach is a fact about the credential: a client bound to one
    // share lists the same shares as an unbound one.
    let (unbound, _) = bound(None, vec![]);
    let (bound_to_support, _) = bound(Some(SUPPORT), vec![]);
    assert_eq!(
        unbound.list_shared_mailboxes().await.unwrap(),
        bound_to_support.list_shared_mailboxes().await.unwrap()
    );
}

#[test]
fn a_handle_the_session_does_not_list_fails_at_connect() {
    let base = reqwest::Url::parse("http://127.0.0.1:18080").unwrap();
    let stale = engine_core::ids::SharedMailboxId::try_from("zzz").unwrap();
    let err = Session::parse(
        &session_doc(),
        &base,
        SessionUrlPolicy::RebaseToConnection,
        Some(&stale),
    )
    .unwrap_err();
    assert!(err.to_string().contains("lost access"), "{err}");
}

#[tokio::test]
async fn a_domain_the_shared_account_does_not_expose_is_not_advertised() {
    // Stalwart gives every account the same `accountCapabilities`, so this case needs a
    // trimmed session: RFC 8620 defines the set per account precisely so it *can* differ,
    // and claiming a domain the session says is absent would be a wrong promise.
    let mut doc = session_doc();
    let caps = doc["accounts"][BOB]["accountCapabilities"]
        .as_object_mut()
        .unwrap();
    caps.retain(|urn, _| urn == "urn:ietf:params:jmap:mail");
    let exec = FakeExecutor::bound_to(&doc, Some(BOB), vec![]);
    let caps = exec.session().capabilities();
    assert!(caps.mail() && !caps.calendars() && !caps.contacts() && !caps.submission());
    let err = exec.session().calendar_account_id().unwrap_err();
    assert!(
        err.to_string().contains("does not expose calendar"),
        "{err}"
    );
    // Discovery stays: the credential can still find the other shares.
    assert_eq!(caps.shared_mailboxes(), SharedMailboxes::Enumerable);
}

#[test]
fn a_session_without_an_accounts_map_offers_no_discovery() {
    // A server that omits the map (against RFC 8620 §1.6.2) is still usable for the
    // credential's own mail; it just has no list to read, and says so.
    let mut doc = session_doc();
    doc.as_object_mut().unwrap().remove("accounts");
    let exec = FakeExecutor::bound_to(&doc, None, vec![]);
    assert!(exec.session().capabilities().mail());
    assert_eq!(
        exec.session().capabilities().shared_mailboxes(),
        SharedMailboxes::Unsupported
    );
    assert!(exec.session().accounts().is_empty());
}
