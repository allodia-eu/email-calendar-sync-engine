//! Shared-mailbox discovery and binding, over the **captured** session of an account that
//! two other stores are shared with (`tests/fixtures/session_shared_accounts.json`, taken
//! from the Stalwart harness as `alice@test.local`).
//!
//! The fixture is what makes these worth having: a hand-written session would carry the
//! same assumptions the code does, while this one carries the server's answer — including
//! the finding the design rests on, that a share whose only mailbox is read-only reports
//! its *account* as writable.

use std::sync::Arc;

use engine_core::{
    error::FailureClass,
    mail::MailboxAccess,
    sync::{SearchDomain, SyncUpdate},
};
use engine_provider::SharedAccounts;
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
const SUPPORT: &str = "g";
const DAVE: &str = "f";
const BOB: &str = "d";

fn session_doc() -> Value {
    fixture("session_shared_accounts.json")
}

fn bound(handle: Option<&str>, responses: Vec<Value>) -> (JmapProvider, Arc<FakeExecutor>) {
    let exec = Arc::new(FakeExecutor::bound_to(&session_doc(), handle, responses));
    (JmapProvider::with_executor(Box::new(exec.clone())), exec)
}

fn probe() -> Value {
    fixture("shared_accounts_probe_response.json")
}

fn domains(listed: &[engine_provider::SharedAccount], handle: &str) -> Vec<SearchDomain> {
    listed
        .iter()
        .find(|m| m.handle.as_str() == handle)
        .unwrap_or_else(|| panic!("{handle} is not listed: {listed:?}"))
        .domains
        .iter()
        .collect()
}

#[tokio::test]
async fn the_listing_asks_each_shared_account_what_it_holds() {
    let (provider, exec) = bound(None, vec![probe()]);
    assert_eq!(
        provider.connection_info().capabilities.shared_accounts(),
        SharedAccounts::Enumerable
    );
    let listed = provider.list_shared_accounts().await.unwrap();
    // Every shared account claims mail and calendars (STALWART-1); asked, bob holds mail
    // alone, dave a calendar alone, and the group both. Alice's own account is not there,
    // so a host renders this list as "add another account" without filtering it.
    assert_eq!(listed.len(), 3, "{listed:?}");
    assert_eq!(domains(&listed, BOB), [SearchDomain::Mail]);
    assert_eq!(domains(&listed, DAVE), [SearchDomain::Calendar]);
    assert_eq!(
        domains(&listed, SUPPORT),
        [SearchDomain::Mail, SearchDomain::Calendar]
    );
    assert!(!listed.iter().any(|m| m.handle.as_str() == ALICE));
    let support = listed
        .iter()
        .find(|m| m.handle.as_str() == SUPPORT)
        .unwrap();
    assert_eq!(support.address.as_deref(), Some("support@test.local"));
    assert_eq!(support.name, "support@test.local");

    // One request, asking each claimed domain of each shared account for no objects.
    assert_eq!(exec.request_count(), 1);
    let request = exec.requests.lock().unwrap()[0].clone();
    let calls: Vec<(String, String)> = request["methodCalls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| {
            assert_eq!(call[1]["ids"], json!([]), "{call}");
            (
                call[0].as_str().unwrap().to_owned(),
                call[1]["accountId"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let expected: Vec<(String, String)> = [BOB, DAVE, SUPPORT]
        .iter()
        .flat_map(|id| {
            [
                ("Mailbox/get".to_owned(), (*id).to_owned()),
                ("Calendar/get".to_owned(), (*id).to_owned()),
            ]
        })
        .collect();
    assert_eq!(calls, expected);
    let using = request["using"].to_string();
    assert!(
        using.contains("jmap:mail") && using.contains("jmap:calendars"),
        "{using}"
    );
}

#[tokio::test]
async fn an_account_that_answers_for_neither_domain_is_not_listed() {
    let mut answer = probe();
    // Bob's mail refuses too: nothing in his store answers, so there is nothing to open.
    answer["methodResponses"][0] = json!(["error", { "type": "accountNotSupportedByMethod" }, "0"]);
    let (provider, _) = bound(None, vec![answer]);
    let listed = provider.list_shared_accounts().await.unwrap();
    assert!(
        !listed.iter().any(|m| m.handle.as_str() == BOB),
        "{listed:?}"
    );
    assert_eq!(listed.len(), 2);
}

#[tokio::test]
async fn a_domain_the_credential_is_forbidden_is_not_held_and_the_rest_still_lists() {
    // RFC 8620 §3.6.2's `forbidden` for one domain of one share says that domain is not this
    // credential's to read, not that the server failed: the share's mail still lists, and so
    // does every other store.
    let mut answer = probe();
    answer["methodResponses"][1] = json!(["error", { "type": "forbidden" }, "1"]);
    let (provider, _) = bound(None, vec![answer]);
    let listed = provider.list_shared_accounts().await.unwrap();
    assert_eq!(listed.len(), 3, "{listed:?}");
    assert_eq!(domains(&listed, BOB), [SearchDomain::Mail]);
}

#[tokio::test]
async fn the_stores_keep_the_order_the_session_lists_them_in() {
    let (provider, _) = bound(None, vec![probe()]);
    let handles: Vec<String> = provider
        .list_shared_accounts()
        .await
        .unwrap()
        .iter()
        .map(|m| m.handle.as_str().to_owned())
        .collect();
    let session: Vec<String> = Session::parse(
        &session_doc(),
        &reqwest::Url::parse("http://127.0.0.1:18080").unwrap(),
        SessionUrlPolicy::RebaseToConnection,
        None,
    )
    .unwrap()
    .accounts()
    .iter()
    .filter(|a| !a.personal)
    .map(|a| a.id.clone())
    .collect();
    assert_eq!(handles, session);
}

#[tokio::test]
async fn a_probe_that_fails_for_another_reason_fails_the_listing() {
    // Only "this account does not hold that" means absent. A server error is not an empty
    // store, or a blip would drop a store from the list a host shows.
    let mut answer = probe();
    answer["methodResponses"][3] = json!(["error", { "type": "serverFail" }, "3"]);
    let (provider, _) = bound(None, vec![answer]);
    provider.list_shared_accounts().await.unwrap_err();
}

#[tokio::test]
async fn only_the_domains_an_account_claims_and_the_server_serves_are_asked() {
    let mut doc = session_doc();
    // A server without calendars: the probe asks for mail alone.
    doc["capabilities"]
        .as_object_mut()
        .unwrap()
        .remove("urn:ietf:params:jmap:calendars");
    // And an account not claiming mail is not asked about it.
    doc["accounts"][DAVE]["accountCapabilities"]
        .as_object_mut()
        .unwrap()
        .remove("urn:ietf:params:jmap:mail");
    let answer = json!({ "methodResponses": [
        ["Mailbox/get", { "accountId": BOB, "list": [] }, "0"],
        ["Mailbox/get", { "accountId": SUPPORT, "list": [] }, "1"],
    ]});
    let exec = Arc::new(FakeExecutor::bound_to(&doc, None, vec![answer]));
    let provider = JmapProvider::with_executor(Box::new(exec.clone()));
    let listed = provider.list_shared_accounts().await.unwrap();
    assert_eq!(domains(&listed, BOB), [SearchDomain::Mail]);
    assert_eq!(domains(&listed, SUPPORT), [SearchDomain::Mail]);
    assert_eq!(listed.len(), 2, "{listed:?}");
    let request = exec.requests.lock().unwrap()[0].clone();
    let methods = request["methodCalls"].to_string();
    assert!(!methods.contains("Calendar/get"), "{methods}");
    assert!(!request["using"].to_string().contains("calendars"));
}

#[tokio::test]
async fn a_session_with_no_shared_account_lists_nothing_without_asking() {
    let mut doc = session_doc();
    doc["accounts"]
        .as_object_mut()
        .unwrap()
        .retain(|id, _| id == ALICE);
    let exec = Arc::new(FakeExecutor::bound_to(&doc, None, vec![]));
    let provider = JmapProvider::with_executor(Box::new(exec.clone()));
    assert_eq!(provider.list_shared_accounts().await.unwrap(), []);
    assert_eq!(exec.request_count(), 0);
}

#[tokio::test]
async fn an_account_name_that_is_not_an_address_is_offered_as_a_label_only() {
    // RFC 8620 makes `name` "usually" an address, not always: a display name must not
    // come back as something a host would send mail to, or match a typed address.
    let mut doc = session_doc();
    doc["accounts"][BOB]["name"] = json!("Bob's Mailbox");
    let exec = Arc::new(FakeExecutor::bound_to(&doc, None, vec![probe(), probe()]));
    let provider = JmapProvider::with_executor(Box::new(exec));
    let listed = provider.list_shared_accounts().await.unwrap();
    let bob = listed.iter().find(|m| m.handle.as_str() == BOB).unwrap();
    assert_eq!(
        (bob.name.as_str(), bob.address.as_deref()),
        ("Bob's Mailbox", None)
    );
    let err = provider
        .resolve_shared_account("bob@test.local")
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn an_address_resolves_through_the_listing_and_the_own_account_never_does() {
    let (provider, _) = bound(None, vec![probe(), probe(), probe()]);
    let support = provider
        .resolve_shared_account("Support@Test.Local")
        .await
        .unwrap();
    assert_eq!(support.handle.as_str(), SUPPORT);

    // The credential's own address is in the session too, but it is not a shared mailbox:
    // resolving it must not hand a host a second binding to the account it already has.
    for not_shared in ["alice@test.local", "nobody@test.local"] {
        assert_eq!(
            provider
                .resolve_shared_account(not_shared)
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
        vec![fixture("mailbox_snapshot_shared_read_only_response.json")],
    );
    // The one mailbox Bob's account exposes grants read alone, and Stalwart v1 says so on the
    // account too (`isReadOnly: true`; v0.16 said `false`), so writes are not advertised. The
    // folder still carries the rights: an account mixing a writable folder with a read-only
    // one reports `isReadOnly: false` (observed on v1.0.2), so the flag cannot stand in for
    // them (modeling.md).
    assert!(!provider.connection_info().capabilities.mail_writes());
    // v1 also stops listing `submission` on that account (the group share keeps it), so a
    // client bound to it offers no send rather than one the server would refuse.
    assert!(!provider.connection_info().capabilities.submission());

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
async fn a_bound_client_on_a_writable_share_keeps_writes_and_send() {
    // The other direction of the read-only case above, from the same captured session: the
    // group account, every folder of which alice may write, says `isReadOnly: false` and
    // lists `submission`, so neither is withheld.
    let (provider, _) = bound(Some(SUPPORT), vec![]);
    let caps = provider.connection_info().capabilities;
    assert!(caps.mail_writes());
    assert!(caps.submission());
}

#[tokio::test]
async fn discovery_does_not_depend_on_what_the_client_is_bound_to() {
    // What a credential can reach is a fact about the credential: a client bound to one
    // share lists the same shares as an unbound one.
    let (unbound, _) = bound(None, vec![probe()]);
    let (bound_to_support, _) = bound(Some(SUPPORT), vec![probe()]);
    assert_eq!(
        unbound.list_shared_accounts().await.unwrap(),
        bound_to_support.list_shared_accounts().await.unwrap()
    );
}

#[test]
fn a_handle_the_session_does_not_list_fails_at_connect() {
    let base = reqwest::Url::parse("http://127.0.0.1:18080").unwrap();
    let stale = engine_core::ids::SharedAccountId::try_from("zzz").unwrap();
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
    // RFC 8620 defines `accountCapabilities` per account precisely so the set *can* differ,
    // and claiming a domain the session says is absent would be a wrong promise. Bob's
    // share already lacks `submission` in the captured session, so the trim is applied to
    // the group account, which lists every domain: each assertion below then holds only
    // because the narrowing read the trimmed set.
    let mut doc = session_doc();
    let caps = doc["accounts"][SUPPORT]["accountCapabilities"]
        .as_object_mut()
        .unwrap();
    caps.retain(|urn, _| urn == "urn:ietf:params:jmap:mail");
    let exec = FakeExecutor::bound_to(&doc, Some(SUPPORT), vec![]);
    let caps = exec.session().capabilities();
    assert!(caps.mail() && !caps.calendars() && !caps.contacts() && !caps.submission());
    let err = exec.session().calendar_account_id().unwrap_err();
    assert!(
        err.to_string().contains("does not expose calendar"),
        "{err}"
    );
    // Discovery stays: the credential can still find the other shares.
    assert_eq!(caps.shared_accounts(), SharedAccounts::Enumerable);
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
        exec.session().capabilities().shared_accounts(),
        SharedAccounts::Unsupported
    );
    assert!(exec.session().accounts().is_empty());
}

#[tokio::test]
async fn the_questions_are_split_at_the_servers_call_limit() {
    let mut doc = session_doc();
    doc["capabilities"]["urn:ietf:params:jmap:core"]["maxCallsInRequest"] = json!(4);
    // The six questions of the captured probe, answered four and then two.
    let all = probe()["methodResponses"].as_array().unwrap().clone();
    let renumber = |answers: &[Value]| {
        let answers: Vec<Value> = answers
            .iter()
            .enumerate()
            .map(|(n, answer)| json!([answer[0], answer[1], n.to_string()]))
            .collect();
        json!({ "methodResponses": answers })
    };
    let exec = Arc::new(FakeExecutor::bound_to(
        &doc,
        None,
        vec![renumber(&all[..4]), renumber(&all[4..])],
    ));
    let provider = JmapProvider::with_executor(Box::new(exec.clone()));
    let listed = provider.list_shared_accounts().await.unwrap();
    assert_eq!(exec.request_count(), 2);
    assert_eq!(listed.len(), 3, "{listed:?}");
    assert_eq!(
        domains(&listed, SUPPORT),
        [SearchDomain::Mail, SearchDomain::Calendar]
    );
}

/// In a bound store the calendar user is the store's owner (`engine_provider::calendar_user`).
/// `ParticipantIdentity/get` answers with the *user's* identities on any account (pinned in
/// `smoke_shared_stores.rs`), so the bound answer keeps those that are the owner's.
mod calendar_user {
    use engine_provider::{CalendarUserAddresses, CalendarWrites};

    use super::*;

    const USER: &str = "alice@test.local";

    async fn answer(doc: &Value, handle: &str) -> CalendarUserAddresses {
        let identities = fixture("participant_identity_shared_response.json");
        let exec = Arc::new(FakeExecutor::bound_to(doc, Some(handle), vec![identities]));
        let provider = JmapProvider::with_executor(Box::new(exec.clone()));
        let answer = provider.calendar_user_addresses(&account()).await.unwrap();
        let (_, method, arguments) = exec.sole_call();
        assert_eq!(method, "ParticipantIdentity/get");
        assert_eq!(arguments["accountId"], json!(handle));
        answer
    }

    #[tokio::test]
    async fn a_store_the_user_schedules_in_answers_with_its_owners_address_alone() {
        let got = answer(&session_doc(), SUPPORT).await;
        assert_eq!(
            got,
            CalendarUserAddresses::Known(vec!["support@test.local".to_owned()])
        );
        assert!(got.is_for_store("support@test.local", USER));
    }

    #[tokio::test]
    async fn a_store_the_user_cannot_schedule_as_answers_nobody() {
        // Dave's calendar is shared read-only, and no identity of alice's is his.
        let got = answer(&session_doc(), DAVE).await;
        assert_eq!(got, CalendarUserAddresses::NotEnabled);
        assert!(got.is_for_store("dave@test.local", USER));
    }

    #[tokio::test]
    async fn a_store_whose_owner_has_no_address_cannot_be_answered_for() {
        let mut doc = session_doc();
        doc["accounts"][SUPPORT]["name"] = json!("Support Team");
        assert_eq!(answer(&doc, SUPPORT).await, CalendarUserAddresses::Unknown);
    }
}
