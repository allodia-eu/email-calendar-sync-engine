//! What a failed calendar or contact `/set` tells its caller: unconfirmed where the change may
//! have been made, and the server's own class everywhere else, which means it was not.
//!
//! Through the real client where the transport decides (a silent server, a status, a body), and
//! through canned answers where the answer's content does. The session is fetched in real time
//! and the clock paused once it is (`engine_http::test_server` has why).

use std::time::Duration;

use async_trait::async_trait;
use engine_core::{
    contact::{ContactCard, ContactDraft},
    error::FailureClass,
    ids::{AddressBookId, CalendarId, ContactId, Uid},
    membership::Memberships,
    time::{CalendarDateTime, UtcDateTime},
};
use engine_http::test_server::{SilentServer, held_until, reply};
use engine_provider::{CalendarWrites, ContactsProvider, EventDraft, ProviderError};
use serde_json::json;

use crate::{
    Credentials, JmapConfig, JmapProvider,
    error::JmapError,
    executor::Executor,
    provider::provider_test_support::{FakeExecutor, account, provider},
    request::{Request, Response},
    session::Session,
};

/// Longer than any bound under test, so only a missing bound reaches it.
const GUARD: Duration = Duration::from_hours(1);

const SESSION: &str = r#"{"capabilities":{"urn:ietf:params:jmap:core":{},"urn:ietf:params:jmap:calendars":{},"urn:ietf:params:jmap:contacts":{}},"primaryAccounts":{"urn:ietf:params:jmap:calendars":"c","urn:ietf:params:jmap:contacts":"c"},"accounts":{"c":{"accountCapabilities":{"urn:ietf:params:jmap:calendars":{},"urn:ietf:params:jmap:contacts":{}}}},"apiUrl":"https://mail.test.local/jmap/"}"#;

/// A provider whose session `server` served first, connected in real time.
async fn connected(server: &SilentServer) -> JmapProvider {
    let config = JmapConfig::new(server.url(), Credentials::basic("alice", "pw"))
        .with_session_path("/jmap/session");
    JmapProvider::connect(config).await.expect("session")
}

/// A server that serves the session and then `replies`, then nothing.
async fn session_then(replies: &[String]) -> SilentServer {
    let mut script = vec![reply("200 OK", "application/json", SESSION)];
    script.extend_from_slice(replies);
    SilentServer::start(script).await
}

fn draft() -> EventDraft {
    let at =
        |hour: u8| CalendarDateTime::utc(format!("2026-08-01T{hour:02}:00:00").parse().unwrap());
    EventDraft::new(
        CalendarId::try_from("b").unwrap(),
        Uid::new("evt-1@test.local").unwrap(),
        "Planning",
        at(9),
        at(10),
        UtcDateTime::new(2026, 7, 14, 10, 0, 0).unwrap(),
    )
}

fn card_draft() -> ContactDraft {
    let book = AddressBookId::try_from("book-1").unwrap();
    ContactDraft {
        address_book: book.clone(),
        card: ContactCard::new(
            ContactId::try_from("ignored").unwrap(),
            Memberships::of_one(book),
        ),
    }
}

#[tokio::test]
async fn a_set_whose_reply_never_comes_may_have_been_applied() {
    for verb in ["CalendarEvent/set", "ContactCard/set"] {
        let server = session_then(&[]).await;
        let provider = connected(&server).await;
        tokio::time::pause();

        let write = async {
            if verb == "CalendarEvent/set" {
                provider.create_event(&account(), &draft()).await.map(drop)
            } else {
                provider
                    .create_contact(&account(), &card_draft())
                    .await
                    .map(drop)
            }
        };
        let outcome = held_until(server.has_received(2), tokio::time::timeout(GUARD, write))
            .await
            .expect("bounded");

        let err = outcome.expect_err("a silent server is a failure");
        assert!(err.requires_confirmation(), "{verb}: {err}");
        assert!(!err.is_retryable(), "{verb}: {err}");
        assert_eq!(
            server.received(),
            2,
            "{verb}: the session, then the write once"
        );
        tokio::time::resume();
    }
}

/// An executor whose request never reached a server: nothing listens on the port.
struct Refused(FakeExecutor);

#[async_trait]
impl Executor for Refused {
    async fn execute(&self, _request: &Request) -> Result<Response, JmapError> {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("free port")
            .port();
        let refused = engine_http::client(&engine_tls::TlsClientConfig::default())
            .build()
            .expect("client")
            .post(format!("http://127.0.0.1:{port}/jmap/"))
            .send()
            .await
            .expect_err("nothing listens there");
        Err(refused.into())
    }

    async fn download(&self, _url: &str) -> Result<Vec<u8>, JmapError> {
        unreachable!("a create downloads nothing")
    }

    async fn upload(&self, _url: &str, _type: &str, _bytes: &[u8]) -> Result<String, JmapError> {
        unreachable!("a create uploads nothing")
    }

    fn session(&self) -> &Session {
        self.0.session()
    }
}

#[tokio::test]
async fn a_set_that_never_reached_the_server_was_not_applied() {
    let executor = Refused(FakeExecutor::new(Vec::new()));
    let err = ProviderError::from(
        crate::calendar_write::create_event(&executor, "c", &draft())
            .await
            .expect_err("refused"),
    );
    assert!(err.is_retryable(), "{err}");
    assert!(!err.requires_confirmation(), "{err}");
}

/// A gateway's `502`/`504` says the server behind it gave no answer, and a `200` nobody can
/// read is no answer either; every other status is the server's own refusal.
#[tokio::test]
async fn only_an_answer_nobody_gave_leaves_a_set_unconfirmed() {
    for (status, body, unconfirmed) in [
        ("502 Bad Gateway", "{}", true),
        ("504 Gateway Timeout", "{}", true),
        ("200 OK", "not json", true),
        ("200 OK", r#"{"methodResponses":[]}"#, true),
        ("500 Internal Server Error", "{}", false),
        ("503 Service Unavailable", "{}", false),
        ("403 Forbidden", "{}", false),
    ] {
        let server = session_then(&[reply(status, "application/json", body)]).await;
        let provider = connected(&server).await;
        let err = provider
            .create_event(&account(), &draft())
            .await
            .expect_err(status);
        assert_eq!(
            err.requires_confirmation(),
            unconfirmed,
            "{status} {body}: {err}"
        );
    }
}

fn answer(method: &str, arguments: &serde_json::Value) -> serde_json::Value {
    json!({ "methodResponses": [[method, arguments, "0"]] })
}

/// RFC 8620 §3.6.2: `serverFail` "made no changes to the server's state", while
/// `serverPartialFail` means some of them were made.
#[tokio::test]
async fn a_partial_failure_is_unconfirmed_and_a_stated_refusal_is_not() {
    for (arguments, unconfirmed, class) in [
        (json!({ "type": "serverPartialFail" }), true, None),
        (
            json!({ "type": "serverFail" }),
            false,
            Some(FailureClass::Retryable),
        ),
        (
            json!({ "type": "forbidden" }),
            false,
            Some(FailureClass::Permanent),
        ),
    ] {
        let p = provider(vec![answer("error", &arguments)]);
        let err = p
            .create_event(&account(), &draft())
            .await
            .expect_err("refused");
        assert_eq!(
            err.requires_confirmation(),
            unconfirmed,
            "{arguments}: {err}"
        );
        if let Some(class) = class {
            assert_eq!(err.class(), class, "{arguments}: {err}");
        }
    }
    let refused = answer(
        "CalendarEvent/set",
        &json!({ "notCreated": { "new": { "type": "invalidProperties" } } }),
    );
    let err = provider(vec![refused])
        .create_event(&account(), &draft())
        .await
        .expect_err("refused");
    assert!(!err.requires_confirmation(), "{err}");
    let partial = answer(
        "CalendarEvent/set",
        &json!({ "notCreated": { "new": { "type": "serverPartialFail" } } }),
    );
    let err = provider(vec![partial])
        .create_event(&account(), &draft())
        .await
        .expect_err("partial");
    assert!(err.requires_confirmation(), "{err}");
}

#[tokio::test]
async fn a_contact_set_whose_answer_does_not_say_is_unconfirmed() {
    for arguments in [
        json!({ "created": {} }),
        json!({ "created": { "new": { "id": "" } } }),
    ] {
        let p = provider(vec![answer("ContactCard/set", &arguments)]);
        let err = p
            .create_contact(&account(), &card_draft())
            .await
            .expect_err("no id");
        assert!(err.requires_confirmation(), "{arguments}: {err}");
    }
    let refused = answer(
        "ContactCard/set",
        &json!({ "notCreated": { "new": { "type": "forbidden" } } }),
    );
    let err = provider(vec![refused])
        .create_contact(&account(), &card_draft())
        .await
        .expect_err("refused");
    assert!(!err.requires_confirmation(), "{err}");
}

#[test]
fn an_unconfirmed_set_is_never_retried_and_says_why() {
    let err = ProviderError::from(JmapError::status(504, "").after_write());
    assert!(err.requires_confirmation());
    assert!(!err.is_retryable());
    assert!(err.to_string().contains("may have been applied"), "{err}");
    assert!(matches!(
        JmapError::status(502, "").after_write().after_write(),
        JmapError::Unconfirmed(inner) if matches!(*inner, JmapError::Status { .. })
    ));
    // What the request never reached cannot have been applied.
    assert!(!matches!(
        JmapError::Session("no apiUrl".into()).after_write(),
        JmapError::Unconfirmed(_)
    ));
}
