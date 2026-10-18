//! What a failed calendar or contact write tells its caller: unconfirmed where the write may
//! have been applied, and the server's own class everywhere else, which means it was not.
//!
//! Through the real transport against `engine_http::test_server`.

use std::time::Duration;

use engine_core::{
    contact::{ContactCard, ContactDraft},
    error::FailureClass,
    ids::{AccountId, AddressBookId, CalendarId, ContactId, EventId, Uid},
    membership::Memberships,
    time::{CalendarDateTime, UtcDateTime},
};
use engine_http::test_server::{SilentServer, held_until, reply};
use engine_provider::{ContactsProvider, EventDeletion, EventDraft, ProviderError};
use tokio::net::TcpListener;

use crate::{
    GoogleClient, GoogleContactProvider,
    test_support::{retry, tls},
};

/// Longer than any bound under test, so only a missing bound reaches it.
const GUARD: Duration = Duration::from_hours(1);

fn account() -> AccountId {
    AccountId::try_from("account-1").unwrap()
}

fn client(base: &str) -> GoogleClient {
    GoogleClient::with_base("token", base, tls(), &retry()).expect("client")
}

fn draft() -> EventDraft {
    let at =
        |hour: u8| CalendarDateTime::utc(format!("2026-08-01T{hour:02}:00:00").parse().unwrap());
    EventDraft::new(
        CalendarId::try_from("cal1").unwrap(),
        Uid::new("evt-1@test.local").unwrap(),
        "Planning",
        at(9),
        at(10),
        UtcDateTime::new(2026, 7, 14, 10, 0, 0).unwrap(),
    )
}

async fn create(base: &str) -> Result<EventId, ProviderError> {
    crate::cal_write::create_event(&client(base), "cal1", &draft())
        .await
        .map(|receipt| receipt.event)
}

async fn delete(base: &str) -> Result<(), ProviderError> {
    let deletion = EventDeletion::unconditional(
        EventId::try_from("e1").unwrap(),
        Uid::new("evt-1@test.local").unwrap(),
    );
    crate::cal_write::delete_event(&client(base), "cal1", &deletion).await
}

fn card_draft() -> ContactDraft {
    let book = AddressBookId::try_from("google-connections").unwrap();
    ContactDraft {
        address_book: book.clone(),
        card: ContactCard::new(
            ContactId::try_from("ignored").unwrap(),
            Memberships::of_one(book),
        ),
    }
}

async fn create_contact(base: &str) -> Result<ContactId, ProviderError> {
    GoogleContactProvider::connections(client(base))
        .create_contact(&account(), &card_draft())
        .await
        .map(|receipt| receipt.contact)
}

#[tokio::test(start_paused = true)]
async fn a_write_whose_reply_never_comes_may_have_been_applied() {
    for verb in ["create event", "delete event", "create contact"] {
        let server = SilentServer::start(Vec::new()).await;
        let write = async {
            match verb {
                "create event" => create(server.url()).await.map(drop),
                "delete event" => delete(server.url()).await,
                _ => create_contact(server.url()).await.map(drop),
            }
        };
        let outcome = held_until(server.has_received(1), tokio::time::timeout(GUARD, write))
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

    for outcome in [
        tokio::time::timeout(GUARD, create(&base))
            .await
            .expect("bounded")
            .map(drop),
        tokio::time::timeout(GUARD, create_contact(&base))
            .await
            .expect("bounded")
            .map(drop),
    ] {
        let err = outcome.expect_err("an unconnected write is not a success");
        assert!(err.is_retryable(), "{err}");
        assert!(!err.requires_confirmation(), "{err}");
    }
    drop(listener);
}

/// A gateway's `502`/`504` says the server behind it gave no answer, and a success that does
/// not say what it made is no answer either; every other status is the server's own refusal.
#[tokio::test]
async fn only_an_answer_nobody_gave_leaves_a_write_unconfirmed() {
    for (status, body, class) in [
        ("502 Bad Gateway", "{}", None),
        ("504 Gateway Timeout", "{}", None),
        ("201 Created", "not json", None),
        ("201 Created", "", None),
        ("201 Created", "{}", None),
        (
            "500 Internal Server Error",
            "{}",
            Some(FailureClass::Retryable),
        ),
        (
            "503 Service Unavailable",
            "{}",
            Some(FailureClass::Retryable),
        ),
        ("403 Forbidden", "{}", Some(FailureClass::Permanent)),
        (
            "412 Precondition Failed",
            "{}",
            Some(FailureClass::Conflict),
        ),
    ] {
        for contact in [false, true] {
            let server = SilentServer::start(vec![reply(status, "application/json", body)]).await;
            let err = if contact {
                create_contact(server.url()).await.map(drop)
            } else {
                create(server.url()).await.map(drop)
            }
            .expect_err(status);
            let what = format!("{status} {body:?} (contact: {contact}): {err}");
            assert_eq!(err.requires_confirmation(), class.is_none(), "{what}");
            if let Some(class) = class {
                assert_eq!(err.class(), class, "{what}");
            }
        }
    }
}

#[tokio::test]
async fn a_delete_the_server_refused_or_already_made_is_answered_as_such() {
    let gone = SilentServer::start(vec![reply("404 Not Found", "application/json", "{}")]).await;
    delete(gone.url()).await.expect("already gone is done");
    let refused = SilentServer::start(vec![reply("403 Forbidden", "application/json", "{}")]).await;
    let err = delete(refused.url()).await.expect_err("refused");
    assert!(!err.requires_confirmation(), "{err}");
    let gateway =
        SilentServer::start(vec![reply("504 Gateway Timeout", "application/json", "{}")]).await;
    let err = delete(gateway.url()).await.expect_err("no answer");
    assert!(err.requires_confirmation(), "{err}");
}
