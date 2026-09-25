//! The shared-mailbox probe: its request shape, the own-mailbox refusal, and the
//! classification of every answer a real tenant gave.
//!
//! Each body is a captured one, scrubbed per `tests/fixtures/README.md`. The set is the
//! point: a real tenant answered three different `404` codes and no `403` on this route,
//! so "not shared with you" cannot be told from "does not exist", and the classifier has to
//! be right about that rather than about what the documentation implies.

use engine_core::error::FailureClass;
use engine_provider::{Provider, SharedMailboxes};
use serde_json::{Value, json};

use super::*;
use crate::{
    GraphProvider, MailboxPrincipal,
    test_support::{FakeRoute, fake_client, fake_client_fallible, json},
};

const PROBE_OK: &str = include_str!("../tests/fixtures/mail/shared_mailbox_probe.json");
const OWN_INBOX: &str = include_str!("../tests/fixtures/mail/own_inbox_probe.json");
const INVALID_USER: &str = include_str!("../tests/fixtures/error/shared_mailbox_invalid_user.json");
const NOT_ENABLED: &str = include_str!("../tests/fixtures/error/shared_mailbox_not_enabled.json");
const NO_INBOX: &str = include_str!("../tests/fixtures/error/shared_mailbox_no_inbox.json");
const ACCESS_DENIED: &str =
    include_str!("../tests/fixtures/error/shared_mailbox_access_denied.json");

const SHARED: &str = "shared@example.test";
/// The probe's exact URL tail, so a request rooted anywhere else misses every route.
const SHARED_ROUTE: &str = "/users/shared@example.test/mailFolders/inbox?$select=id";
const OWN_ROUTE: &str = "/me/mailFolders/inbox?$select=id";

/// A client whose shared probe answers `shared` and whose own-Inbox probe answers `own`.
fn client(shared: FakeRoute, own: FakeRoute) -> GraphClient {
    fake_client_fallible(vec![(SHARED_ROUTE, shared), (OWN_ROUTE, own)])
}

fn failing(status: u16, body: &str) -> FakeRoute {
    Err((status, json(body)))
}

async fn class_when_probe_answers(status: u16, body: &str) -> FailureClass {
    let client = client(failing(status, body), Ok(json(OWN_INBOX)));
    resolve(&client, SHARED).await.unwrap_err().class()
}

#[tokio::test]
async fn a_mailbox_the_credential_can_open_resolves_to_a_handle_that_binds_it() {
    let client = client(Ok(json(PROBE_OK)), Ok(json(OWN_INBOX)));
    let resolved = resolve(&client, SHARED).await.expect("200 means access");
    // On Graph the address *is* what binds a client — `/users/{…}` takes it — so the handle
    // is the address, and `MailboxPrincipal::shared` turns it straight back into a root.
    assert_eq!(resolved.handle.as_str(), SHARED);
    assert_eq!(resolved.address.as_deref(), Some(SHARED));
    assert_eq!(
        MailboxPrincipal::shared(&resolved.handle).unwrap().root(),
        "/users/shared@example.test"
    );
}

#[tokio::test]
async fn the_credentials_own_mailbox_is_refused_under_any_address() {
    // `/users/{own address}` answers 200 — so does an alias of it — and a host would onboard
    // the signed-in mailbox a second time. The Inbox id is what gives it away: observed
    // identical through `/me` and through `/users/{own address}` in any case.
    let own_under_another_name = json(&PROBE_OK.replace("folder-shared-inbox", "folder-inbox"));
    let client = client(Ok(own_under_another_name), Ok(json(OWN_INBOX)));
    let err = resolve(&client, SHARED).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(err.detail().contains("own mailbox"), "{err}");
}

#[tokio::test]
async fn a_credential_with_no_mailbox_of_its_own_still_resolves_shared_ones() {
    // A licensed user without Exchange Online has no Inbox of their own; nothing they
    // resolve can then be their own, so the check stands down rather than failing.
    let client = client(Ok(json(PROBE_OK)), failing(404, NOT_ENABLED));
    assert!(resolve(&client, SHARED).await.is_ok());
}

#[tokio::test]
async fn a_throttled_own_mailbox_check_is_not_reported_as_an_answer() {
    // The own-mailbox read can be refused like any other; that says nothing about the
    // mailbox being resolved, so it keeps its class instead of passing or failing it.
    let throttle = r#"{"error":{"code":"TooManyRequests","message":"throttled"}}"#;
    let client = client(Ok(json(PROBE_OK)), failing(429, throttle));
    assert_eq!(
        resolve(&client, SHARED).await.unwrap_err().class(),
        FailureClass::RateLimited
    );
}

#[tokio::test]
async fn every_captured_not_found_shape_is_terminal() {
    // Three codes, one meaning. The third (`ErrorItemNotFound`, "Default folder Inbox not
    // found") is the subtle one — the principal resolves, it just is not a mailbox — and
    // reading it as retryable would have a host poll forever.
    for body in [INVALID_USER, NOT_ENABLED, NO_INBOX] {
        assert_eq!(
            class_when_probe_answers(404, body).await,
            FailureClass::Permanent,
            "{body}"
        );
    }
}

#[tokio::test]
async fn a_grant_shortfall_is_an_auth_failure_not_a_missing_mailbox() {
    // Reporting `403` as `Permanent` would tell a host the mailbox does not exist when a
    // re-consent to the shared scopes is all that is missing.
    assert_eq!(
        class_when_probe_answers(403, ACCESS_DENIED).await,
        FailureClass::Authentication
    );
}

#[tokio::test]
async fn a_transient_failure_is_never_reported_as_a_missing_mailbox() {
    // The probe's whole answer is a status code, which makes it unusually easy to read an
    // outage as a negative. It must not be.
    let cases = [
        (429, "TooManyRequests", FailureClass::RateLimited),
        (503, "ServiceUnavailable", FailureClass::Retryable),
        (
            401,
            "InvalidAuthenticationToken",
            FailureClass::Authentication,
        ),
    ];
    for (status, code, class) in cases {
        let body = json!({ "error": { "code": code, "message": "…" } }).to_string();
        assert_eq!(
            class_when_probe_answers(status, &body).await,
            class,
            "{code}"
        );
    }
}

#[tokio::test]
async fn an_address_that_could_restructure_the_url_is_refused_before_any_request() {
    // No routes at all: any request would fail as "no fake route", so the refusal has to
    // name the address as the problem to show it never got that far.
    let client = fake_client(vec![]);
    for hostile in [
        "../me",
        "a@b.example/../../me",
        "a@b.example\\me",
        "local-only",
    ] {
        let err = resolve(&client, hostile).await.unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent, "{hostile}");
        assert!(
            err.detail().contains("invalid mailbox address"),
            "{hostile}: {err}"
        );
    }
}

#[tokio::test]
async fn the_own_mailbox_check_asks_me_even_from_a_client_bound_elsewhere() {
    // A client bound to one shared mailbox resolving another: the probe is rooted at the
    // *probed* address and the own-mailbox check at `/me` — neither at this client's own
    // principal, which has no route here and would fail the call if either used it.
    let client = client(Ok(json(PROBE_OK)), Ok(json(OWN_INBOX)))
        .with_principal(MailboxPrincipal::user("elsewhere@example.test").unwrap());
    assert!(resolve(&client, SHARED).await.is_ok());
}

#[tokio::test]
async fn the_provider_can_resolve_by_address_and_cannot_list() {
    let provider = GraphProvider::new(
        client(Ok(json(PROBE_OK)), Ok(json(OWN_INBOX))),
        engine_core::ids::MailboxId::try_from("folder-inbox").unwrap(),
    );
    assert_eq!(
        provider.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::ByAddress
    );
    assert_eq!(
        provider.list_shared_mailboxes().await.unwrap_err().class(),
        FailureClass::InvalidState
    );
    let resolved = provider.resolve_shared_mailbox(SHARED).await.unwrap();
    assert_eq!(resolved.handle.as_str(), SHARED);
}

#[test]
fn the_captured_bodies_carry_the_codes_the_tenant_returned() {
    // Pins the fixtures: were one re-captured without its `code`, the classifier tests
    // would still pass on the status alone while no longer being about anything observed.
    let code = |body: &str| json(body)["error"]["code"].as_str().unwrap().to_owned();
    assert_eq!(code(INVALID_USER), "ErrorInvalidUser");
    assert_eq!(code(NOT_ENABLED), "MailboxNotEnabledForRESTAPI");
    assert_eq!(code(NO_INBOX), "ErrorItemNotFound");
    assert_eq!(code(ACCESS_DENIED), "ErrorAccessDenied");
    let ok: Value = json(PROBE_OK);
    assert!(ok["id"].is_string());
    assert_ne!(ok["id"], json(OWN_INBOX)["id"]);
}
