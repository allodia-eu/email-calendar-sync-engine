//! Reading and writing the account's sender identities, end to end through the fake
//! executor.
//!
//! The fake answers canned bytes whatever it is sent, so every test here also asserts
//! the **request** it produced (`sole_call`): the response alone would pass over an
//! `Identity/set` that patched the wrong property or named the wrong capability URN.
//! Whether Stalwart accepts what we send is what `tests/live_jmap_identity.rs` proves.

use engine_core::{error::FailureClass, ids::AccountId};
use engine_provider::{IdentityControls, Provider, SenderIdentityId};
use serde_json::{Value, json};

use super::{provider_test_support::*, *};

/// The real `Identity/get` the harness returned for `alice@test.local` (captured live).
const GET_RESPONSE: &str = include_str!("../tests/fixtures/identity_get_response.json");
/// The real `Identity/set` acknowledgement for a rename (captured live): `updated` keyed
/// by the id, valued `null`.
const SET_RESPONSE: &str = include_str!("../tests/fixtures/identity_set_response.json");
/// The real `SetError` the harness returned for an id it does not know (captured live).
const SET_NOT_FOUND: &str = include_str!("../tests/fixtures/identity_set_notfound_response.json");

fn doc(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap()
}

#[tokio::test]
async fn the_captured_get_parses_into_the_accounts_own_identity() {
    // The bytes Stalwart actually returned, so the normalizer is tested against the
    // shape a real server sends rather than one this repo invented — `replyTo`, `bcc`
    // and the two signature properties included.
    let provider = provider(vec![doc(GET_RESPONSE)]);

    let identities = provider.sender_identities(&account()).await.unwrap();

    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].address.email, "alice@test.local");
    assert_eq!(identities[0].address.name.as_deref(), Some("Alice Tester"));
}

#[tokio::test]
async fn the_captured_set_acknowledgement_is_read_as_applied() {
    // `updated: { "<id>": null }` is what a rename really answers. Reading only the
    // object-valued shape would turn every successful rename into a failure.
    let provider = provider(vec![doc(SET_RESPONSE)]);

    provider
        .set_sender_name(&account(), &SenderIdentityId::new("b"), "Alice Tester")
        .await
        .unwrap();
}

#[tokio::test]
async fn an_unknown_identity_is_a_conflict_the_caller_can_recover_from() {
    // The real refusal for an id the server does not hold is `notFound`, which is a
    // conflict — re-read the identities and retry — and **not** the same answer as a
    // server that will not implement the write at all. Keeping the two apart is what
    // lets a host retry the first and stop asking about the second.
    let provider = provider(vec![doc(SET_NOT_FOUND)]);

    let err = provider
        .set_sender_name(&account(), &SenderIdentityId::new("nosuchid"), "X")
        .await
        .unwrap_err();

    assert_eq!(err.class(), FailureClass::Conflict);
}

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

#[tokio::test]
async fn identities_read_the_name_beside_the_address() {
    let provider = provider(vec![json!({
        "methodResponses": [["Identity/get", {
            "accountId": "c",
            "list": [
                { "id": "id-primary", "name": "Alice Smith", "email": "alice@example.com" },
                { "id": "id-alias", "name": "", "email": "sales@example.com" }
            ]
        }, "0"]]
    })]);

    let identities = provider.sender_identities(&account()).await.unwrap();

    assert_eq!(identities.len(), 2);
    assert_eq!(identities[0].id, SenderIdentityId::new("id-primary"));
    assert_eq!(identities[0].address.email, "alice@example.com");
    assert_eq!(identities[0].address.name.as_deref(), Some("Alice Smith"));
    // An empty `name` is the server holding nothing, not a name that happens to be
    // blank: it must reach the host as `None`, which is what makes "ask the user" and
    // "we already know" distinguishable.
    assert_eq!(identities[1].address.name, None);
}

#[tokio::test]
async fn identities_are_read_under_the_submission_capability() {
    let (provider, exec) = recording(vec![json!({
        "methodResponses": [["Identity/get", { "list": [] }, "0"]]
    })]);

    provider.sender_identities(&account()).await.unwrap();

    let (using, method, args) = exec.sole_call();
    assert_eq!(method, "Identity/get");
    // `Identity` is defined by the submission spec (RFC 8621 §6), so a request that
    // named only the mail URN would be refused by a conforming server.
    assert!(
        using.iter().any(|u| u == capability::SUBMISSION),
        "{using:?}"
    );
    // The submission account, not the mail account: they are separate `primaryAccounts`
    // entries and only coincide on the servers we happen to test against.
    assert_eq!(args["accountId"], "c");
}

#[tokio::test]
async fn setting_a_name_patches_only_the_name() {
    let (provider, exec) = recording(vec![json!({
        "methodResponses": [["Identity/set", {
            "accountId": "c",
            "updated": { "id-primary": null }
        }, "0"]]
    })]);

    provider
        .set_sender_name(
            &account(),
            &SenderIdentityId::new("id-primary"),
            "Alice Smith",
        )
        .await
        .unwrap();

    let (using, method, args) = exec.sole_call();
    assert_eq!(method, "Identity/set");
    assert!(using.iter().any(|u| u == capability::SUBMISSION));
    // Only `name`: an update naming `email` would ask the server to change which
    // address the account sends as, which is not what a rename means.
    let patch = &args["update"]["id-primary"];
    assert_eq!(patch["name"], "Alice Smith");
    assert_eq!(patch.as_object().unwrap().len(), 1, "{patch}");
}

#[tokio::test]
async fn an_acknowledgement_carrying_server_set_properties_still_counts_as_applied() {
    // RFC 8620 §5.3: `updated[id]` is `null` when the server changed nothing else, and
    // an object when it did. Both are acknowledgements; reading only the `null` shape
    // would turn a normalizing server into a spurious failure.
    let provider = provider(vec![json!({
        "methodResponses": [["Identity/set", {
            "updated": { "id-primary": { "name": "Alice Smith" } }
        }, "0"]]
    })]);

    provider
        .set_sender_name(
            &account(),
            &SenderIdentityId::new("id-primary"),
            "  Alice Smith  ",
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_refused_set_is_permanent_not_a_silent_success() {
    // A server that will not let the account holder rename answers `forbidden`. Nothing
    // in the session advertises that in advance — a captured `Identity/get` carries
    // `mayDelete` and no "may write" of any kind (`engine_provider::identity`) — so the
    // refusal has to arrive as an error the host can act on rather than as a no-op.
    let provider = provider(vec![json!({
        "methodResponses": [["Identity/set", {
            "notUpdated": { "id-primary": { "type": "forbidden" } }
        }, "0"]]
    })]);

    let err = provider
        .set_sender_name(&account(), &SenderIdentityId::new("id-primary"), "Alice")
        .await
        .unwrap_err();

    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn an_id_the_server_never_mentions_is_not_a_success() {
    let provider = provider(vec![json!({
        "methodResponses": [["Identity/set", { "updated": { "someone-else": null } }, "0"]]
    })]);

    assert!(
        provider
            .set_sender_name(&account(), &SenderIdentityId::new("id-primary"), "Alice")
            .await
            .is_err()
    );
}

#[test]
fn a_submission_session_advertises_writable_identities() {
    // The capability rides the submission URN because `Identity` is defined there. A
    // session without it must not advertise an editor.
    let provider = provider(vec![]);
    let controls = provider
        .connection_info()
        .capabilities
        .sender_identities()
        .expect("a submission session has identities");
    assert_eq!(controls, IdentityControls::Writable);
}
