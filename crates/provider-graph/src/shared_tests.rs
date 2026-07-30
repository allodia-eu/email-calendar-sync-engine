//! The shared-mailbox probe: its request shape, and the classification of every response
//! shape a real tenant produced.
//!
//! Each error body here is a **captured** one (scrubbed per `tests/fixtures/README.md`), and
//! the set is the point: probing every mailbox of a real tenant yielded three different
//! `404` codes and no `403` on this route at all, so "not shared with you" is
//! indistinguishable from "does not exist". The classifier has to be right about that, not
//! about what the documentation implies.

use engine_core::error::FailureClass;
use engine_provider::{Provider, SharedMailboxes};
use serde_json::Value;

use super::*;
use crate::{
    GraphProvider,
    test_support::{fake_client, fake_client_fallible, json},
};

const PROBE_OK: &str = include_str!("../tests/fixtures/mail/shared_mailbox_probe.json");
const INVALID_USER: &str = include_str!("../tests/fixtures/error/shared_mailbox_invalid_user.json");
const NOT_ENABLED: &str = include_str!("../tests/fixtures/error/shared_mailbox_not_enabled.json");
const NO_INBOX: &str = include_str!("../tests/fixtures/error/shared_mailbox_no_inbox.json");
const ACCESS_DENIED: &str =
    include_str!("../tests/fixtures/error/shared_mailbox_access_denied.json");

const SHARED: &str = "shared@example.test";

/// The class the probe reports for a route answering `status` with `body`.
async fn class_for(status: u16, body: &str) -> FailureClass {
    let client = fake_client_fallible(vec![("/mailFolders/inbox", Err((status, json(body))))]);
    resolve(&client, SHARED).await.unwrap_err().class()
}

#[tokio::test]
async fn a_reachable_mailbox_resolves_to_a_handle_that_reopens_it() {
    let client = fake_client(vec![("/mailFolders/inbox", json(PROBE_OK))]);
    let resolved = resolve(&client, SHARED).await.expect("200 means access");
    // On Graph the address *is* the reopening handle — it is what `/users/{…}` takes.
    assert_eq!(resolved.handle.as_str(), SHARED);
    assert_eq!(resolved.address.as_deref(), Some(SHARED));
    // Never `personal`: this verb answers about a mailbox other than the one signed in.
    assert!(!resolved.personal);
}

#[tokio::test]
async fn every_captured_not_found_shape_is_terminal() {
    // Three codes, one meaning: this cannot be opened as a mailbox. The third
    // (`ErrorItemNotFound`, "Default folder Inbox not found") is the subtle one — the
    // principal resolves and is reachable, it simply is not a mailbox — and reading it as
    // anything retryable would have a host poll forever.
    for body in [INVALID_USER, NOT_ENABLED, NO_INBOX] {
        assert_eq!(
            class_for(404, body).await,
            FailureClass::Permanent,
            "{body}"
        );
    }
}

#[tokio::test]
async fn a_grant_shortfall_is_an_auth_failure_not_a_missing_mailbox() {
    // `403 ErrorAccessDenied` — captured from `/users/{other}/mailboxSettings` with the
    // scope granted — means the credential's grant does not cover the request. Reporting it
    // as `Permanent` would tell a host the mailbox does not exist when a re-consent is all
    // that is missing.
    assert_eq!(
        class_for(403, ACCESS_DENIED).await,
        FailureClass::Authentication
    );
}

#[tokio::test]
async fn a_transient_failure_is_never_reported_as_a_missing_mailbox() {
    // The probe's whole output is a status code, which makes it unusually easy to read a
    // throttle or an outage as a negative answer. It must not.
    assert_eq!(
        class_for(
            429,
            r#"{"error":{"code":"TooManyRequests","message":"throttled"}}"#
        )
        .await,
        FailureClass::RateLimited
    );
    assert_eq!(
        class_for(
            503,
            r#"{"error":{"code":"ServiceUnavailable","message":"try later"}}"#
        )
        .await,
        FailureClass::Retryable
    );
    assert_eq!(
        class_for(
            401,
            r#"{"error":{"code":"InvalidAuthenticationToken","message":"expired"}}"#
        )
        .await,
        FailureClass::Authentication
    );
}

#[tokio::test]
async fn an_address_that_could_restructure_the_url_is_refused_before_any_request() {
    // The route below would answer 200 to anything, so reaching it at all is the failure.
    // This matters more than it looks: `../me` percent-encodes to `..%2Fme`, and Graph was
    // observed *decoding that and resolving it to the signed-in user's own mailbox* — a 200,
    // and a "shared mailbox" a host would happily onboard. Encoding is not the boundary;
    // this is.
    let client = fake_client(vec![("/mailFolders/inbox", json(PROBE_OK))]);
    for hostile in [
        "../me",
        "..%2Fme",
        "a@b.test/../../me",
        "a@b.test?$select=id",
        "a@b.test\\me",
        "\"Name\" <a@b.test>",
        "a b@c.test",
    ] {
        let err = resolve(&client, hostile).await.unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent, "{hostile}");
    }
    // So is anything that is not an address at all.
    for malformed in ["", "no-at-sign", "@domain.test", "local@", "a@b@c.test"] {
        assert_eq!(
            resolve(&client, malformed).await.unwrap_err().class(),
            FailureClass::Permanent,
            "{malformed}"
        );
    }
    // But a real address still resolves — including the shapes that look alarming and are
    // not: an Entra guest UPN's `#EXT#`, and a tagged local part.
    for ok in [
        SHARED,
        "user_partner.example#EXT#@tenant.onmicrosoft.com",
        "first.last+tag@company.org",
    ] {
        assert!(resolve(&client, ok).await.is_ok(), "{ok}");
    }
}

#[tokio::test]
async fn the_probe_is_one_request_to_the_named_mailboxs_inbox() {
    // One request, and no `mailboxSettings` second call — that route answers 403 for any
    // mailbox but the signed-in one, so a kind lookup could only ever fail (`graph.md`).
    let client = fake_client(vec![("/mailFolders/inbox", json(PROBE_OK))]);
    resolve(&client, SHARED).await.expect("resolves");

    // And the URL is rooted at the *probed* address, not at the client's own principal —
    // the client here is bound to `/me`, so a lost principal would silently probe the
    // signed-in mailbox and report success for any address at all.
    let principal = MailboxPrincipal::user(SHARED);
    assert_eq!(
        client.principal_url(&principal, PROBE_PATH),
        "https://graph.test/users/shared@example.test/mailFolders/inbox?$select=id"
    );
}

#[tokio::test]
async fn the_provider_advertises_by_address_and_cannot_enumerate() {
    let provider = GraphProvider::new(
        fake_client(vec![("/mailFolders/inbox", json(PROBE_OK))]),
        engine_core::ids::MailboxId::try_from("folder-inbox").unwrap(),
    );
    // Graph exposes no route that lists the mailboxes shared with a credential, and the
    // capability says exactly that — so a host asks the user to type an address.
    assert_eq!(
        provider.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::ByAddress
    );
    // The enumeration verb therefore stays at its rejecting default.
    assert_eq!(
        provider.list_shared_mailboxes().await.unwrap_err().class(),
        FailureClass::InvalidState
    );
    // While the by-address verb answers.
    assert_eq!(
        provider
            .resolve_shared_mailbox(SHARED)
            .await
            .expect("resolves")
            .handle
            .as_str(),
        SHARED
    );
}

#[test]
fn the_captured_bodies_carry_the_codes_the_tenant_returned() {
    // Pins the fixtures themselves: if one is ever re-captured and loses its `code`, the
    // classifier tests above would still pass on the status alone while the documented
    // contract quietly stopped being about anything observed.
    let code = |body: &str| {
        json(body)["error"]["code"]
            .as_str()
            .expect("the standard Graph error envelope")
            .to_owned()
    };
    assert_eq!(code(INVALID_USER), "ErrorInvalidUser");
    assert_eq!(code(NOT_ENABLED), "MailboxNotEnabledForRESTAPI");
    assert_eq!(code(NO_INBOX), "ErrorItemNotFound");
    assert_eq!(code(ACCESS_DENIED), "ErrorAccessDenied");
    // And the positive probe is exactly the one field `$select=id` asked for.
    let ok: Value = json(PROBE_OK);
    assert!(ok["id"].is_string());
}
