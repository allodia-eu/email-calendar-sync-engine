//! What a failed `PUT` or `DELETE` tells its caller: unconfirmed where the write may have been
//! applied, and the server's own class everywhere else, which means it was not.
//!
//! Through the real transport against `engine_http::test_server`, for a calendar event and for
//! a card. The card writes run on a provider whose discovery is replayed, so the only exchange
//! on the socket is the write.

use std::time::Duration;

use async_trait::async_trait;
use engine_core::{
    contact::{ContactCard, ContactDraft},
    error::FailureClass,
    ids::{AddressBookId, CalendarId, ContactId, EventId, ProviderKey, Uid},
    membership::Memberships,
    time::{CalendarDateTime, UtcDateTime},
};
use engine_http::test_server::{SilentServer, held_until, reply, trickling};
use engine_provider::{
    ContactsProvider, EventDeletion, EventDraft, IgnoreConnectSteps, ProviderError,
};
use tokio::net::TcpListener;

use crate::{
    CardDavProvider, Credentials,
    carddav_tests::{BOOKS, PRINCIPAL, account},
    error::CalDavError,
    response::HttpResponse,
    test_support::{Replay, ok, options},
    transport::{DavClient, DavExecutor, DavMethod, WriteRequest},
    write,
};

/// Longer than any bound under test, so only a missing bound reaches it.
const GUARD: Duration = Duration::from_hours(1);

fn client(base: &str) -> DavClient {
    DavClient::new(
        base,
        Credentials::Basic {
            username: "alice".to_owned(),
            password: "pw".to_owned(),
        },
        &engine_tls::TlsClientConfig::default(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client")
}

fn href() -> EventId {
    EventId::try_from("/dav/cal/alice/default/evt-1.ics").unwrap()
}

fn draft() -> EventDraft {
    let at =
        |hour: u8| CalendarDateTime::utc(format!("2026-06-25T{hour:02}:00:00").parse().unwrap());
    EventDraft::new(
        CalendarId::new(ProviderKey::new("/dav/cal/alice/default/").unwrap()),
        Uid::new("evt-1@test.local").unwrap(),
        "Planning",
        at(14),
        at(15),
        UtcDateTime::new(2026, 6, 20, 8, 0, 0).unwrap(),
    )
}

async fn create(base: &str) -> Result<engine_provider::EventWriteReceipt, ProviderError> {
    write::create_event(&client(base), href(), &draft())
        .await
        .map_err(ProviderError::from)
}

async fn delete(base: &str) -> Result<(), ProviderError> {
    let deletion = EventDeletion::unconditional(href(), Uid::new("evt-1@test.local").unwrap());
    write::delete_event(&client(base), None, &deletion)
        .await
        .map_err(ProviderError::from)
}

#[tokio::test(start_paused = true)]
async fn a_create_or_delete_whose_reply_never_comes_may_have_been_applied() {
    for verb in ["PUT", "DELETE"] {
        let server = SilentServer::start(Vec::new()).await;
        let run = async {
            if verb == "PUT" {
                create(server.url()).await.map(drop)
            } else {
                delete(server.url()).await
            }
        };
        let outcome = held_until(server.has_received(1), tokio::time::timeout(GUARD, run))
            .await
            .expect("bounded");

        let err = outcome.expect_err("a silent server is a failure");
        assert!(err.requires_confirmation(), "{verb}: {err}");
        assert!(!err.is_retryable(), "{verb}: {err}");
        assert_eq!(server.received(), 1, "{verb}: written once, never again");
    }
}

#[tokio::test(start_paused = true)]
async fn a_write_whose_connection_never_completes_was_not_applied() {
    // Bound and never accepted: the TCP connect completes in the kernel and nothing answers
    // the TLS hello, so nothing can have been sent.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("https://{}", listener.local_addr().expect("address"));

    let err = tokio::time::timeout(GUARD, create(&base))
        .await
        .expect("bounded")
        .expect_err("an unconnected write is not a success");

    assert!(err.is_retryable(), "{err}");
    assert!(!err.requires_confirmation(), "{err}");
    drop(listener);
}

#[tokio::test]
async fn a_refused_port_was_not_applied() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("free port")
        .port();
    let err = create(&format!("http://127.0.0.1:{port}"))
        .await
        .expect_err("nothing listens there");
    assert!(!err.requires_confirmation(), "{err}");
}

/// A gateway's `502`/`504` says the server behind it gave no answer; every other status is
/// the server's own, so the write did not happen.
#[tokio::test]
async fn only_a_gateway_with_no_answer_leaves_a_write_unconfirmed() {
    for (status, class) in [
        ("502 Bad Gateway", None),
        ("504 Gateway Timeout", None),
        ("500 Internal Server Error", Some(FailureClass::Retryable)),
        ("403 Forbidden", Some(FailureClass::Permanent)),
        ("412 Precondition Failed", Some(FailureClass::Conflict)),
    ] {
        let server = SilentServer::start(vec![reply(status, "text/plain", "")]).await;
        let err = create(server.url()).await.expect_err(status);
        assert_eq!(
            err.requires_confirmation(),
            class.is_none(),
            "{status}: {err}"
        );
        if let Some(class) = class {
            assert_eq!(err.class(), class, "{status}: {err}");
        }
    }
}

/// A write's status is its whole answer: the new `ETag` rides in a header, so a body the
/// server stops sending after a `201` changes nothing about what happened.
#[tokio::test(start_paused = true)]
async fn a_success_whose_body_never_finishes_is_still_a_success() {
    let head = "HTTP/1.1 201 Created\r\nETag: \"v1\"\r\nContent-Length: 50\r\n\r\n".to_owned();
    let base = trickling(head, "x".to_owned(), 1, Duration::from_secs(1)).await;

    let receipt = tokio::time::timeout(GUARD, create(&base))
        .await
        .expect("bounded")
        .expect("the server said it was created");

    assert_eq!(
        receipt
            .revisions
            .etag
            .as_ref()
            .map(engine_core::version::ETag::as_str),
        Some("\"v1\"")
    );
}

#[tokio::test(start_paused = true)]
async fn a_refusal_whose_body_never_finishes_is_still_a_refusal() {
    let head = "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 50\r\n\r\n".to_owned();
    let base = trickling(head, "x".to_owned(), 1, Duration::from_secs(1)).await;

    let err = tokio::time::timeout(GUARD, create(&base))
        .await
        .expect("bounded")
        .expect_err("refused");

    assert_eq!(err.class(), FailureClass::Conflict, "{err}");
    assert!(!err.requires_confirmation(), "{err}");
}

#[test]
fn an_unconfirmed_write_is_never_retried_and_says_why() {
    let err = ProviderError::from(CalDavError::status(504, "").after_write());
    assert!(err.requires_confirmation());
    assert!(!err.is_retryable());
    assert!(err.to_string().contains("may have been applied"), "{err}");
    // Already unconfirmed stays as it is rather than nesting.
    assert!(matches!(
        CalDavError::status(502, "").after_write().after_write(),
        CalDavError::Unconfirmed(inner) if matches!(*inner, CalDavError::Status { .. })
    ));
}

/// Discovery replayed, writes on the socket.
struct WritesLive {
    reads: Replay,
    live: DavClient,
}

#[async_trait]
impl DavExecutor for WritesLive {
    async fn send(
        &self,
        method: DavMethod,
        href: &str,
        depth: &str,
        body: String,
    ) -> Result<HttpResponse, CalDavError> {
        self.reads.send(method, href, depth, body).await
    }

    async fn send_write(&self, request: WriteRequest) -> Result<HttpResponse, CalDavError> {
        self.live.send_write(request).await
    }
}

async fn contacts(base: &str) -> CardDavProvider {
    let executor = WritesLive {
        reads: Replay::new(vec![
            ok(PRINCIPAL),
            options(Some("1, 3, addressbook")),
            ok(BOOKS),
        ]),
        live: client(base),
    };
    CardDavProvider::with_executor(
        Box::new(executor),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
    .await
    .expect("connected")
}

fn card_draft() -> ContactDraft {
    let book = AddressBookId::try_from("/dav/addressbooks/alice/default/").unwrap();
    let mut card = ContactCard::new(
        ContactId::try_from("card").unwrap(),
        Memberships::of_one(book.clone()),
    );
    card.uid = Some("ada@test.local".to_owned());
    ContactDraft {
        address_book: book,
        card,
    }
}

#[tokio::test(start_paused = true)]
async fn a_card_write_whose_reply_never_comes_may_have_been_applied() {
    let server = SilentServer::start(Vec::new()).await;
    let provider = contacts(server.url()).await;

    let outcome = held_until(
        server.has_received(1),
        tokio::time::timeout(GUARD, provider.create_contact(&account(), &card_draft())),
    )
    .await
    .expect("bounded");

    let err = outcome.expect_err("a silent server is a failure");
    assert!(err.requires_confirmation(), "{err}");
    assert_eq!(server.received(), 1);
}

#[tokio::test]
async fn a_card_write_the_server_refused_was_not_applied_and_a_gateway_leaves_it_open() {
    for (status, unconfirmed) in [
        ("412 Precondition Failed", false),
        ("500 Internal Server Error", false),
        ("504 Gateway Timeout", true),
    ] {
        let server = SilentServer::start(vec![reply(status, "text/plain", "")]).await;
        let provider = contacts(server.url()).await;
        let err = tokio::time::timeout(GUARD, provider.create_contact(&account(), &card_draft()))
            .await
            .expect("bounded")
            .expect_err(status);
        assert_eq!(err.requires_confirmation(), unconfirmed, "{status}: {err}");
    }
}
