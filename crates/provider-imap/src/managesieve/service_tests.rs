//! A session's slot is held for one deadline at most, however slowly the server answers.

use std::sync::Arc;

use engine_core::error::FailureClass;
use engine_provider::ProviderError;
use tokio::{io::AsyncWriteExt, sync::Semaphore};

use super::*;
use crate::managesieve::test_support::{GREETING, OneSlot};

#[tokio::test(start_paused = true)]
async fn a_server_answering_a_byte_at_a_time_gives_the_slot_back_by_the_deadline() {
    let slots = Arc::new(Semaphore::new(1));
    let budget = Arc::new(OneSlot(Arc::clone(&slots)));
    let (client, mut server) = tokio::io::duplex(64);
    let client = std::sync::Mutex::new(Some(client));
    let dial: SieveDial<tokio::io::DuplexStream> = Arc::new(move || {
        let stream = client.lock().unwrap().take().expect("one dial");
        Box::pin(Session::open(stream))
    });
    // Each byte well inside the reply bound, so no single wait ever times out.
    tokio::spawn(async move {
        for byte in GREETING.iter().cycle() {
            tokio::time::sleep(Duration::from_secs(20)).await;
            if server.write_all(&[*byte]).await.is_err() {
                return;
            }
        }
    });
    let sieve = ManageSieve::new(dial, budget, &Capabilities::default());
    let started = tokio::time::Instant::now();
    let err = sieve
        .with_session(async |session: &mut Session<tokio::io::DuplexStream>| {
            session.refresh_capabilities().await.map(|_| ())
        })
        .await
        .unwrap_err();
    assert_eq!(started.elapsed(), SESSION_DEADLINE);
    assert_eq!(ProviderError::from(err).class(), FailureClass::Retryable);
    assert_eq!(slots.available_permits(), 1, "the slot is given back");
}

#[tokio::test]
async fn no_logout_is_sent_on_a_stream_that_failed_and_one_is_on_a_sound_one() {
    let (sieve, sessions) = crate::managesieve::test_support::scripted(&[b"\"a\"\r\n"]);
    let err = sieve
        .with_session(async |session: &mut Session<crate::mock::MockStream>| {
            session.list_scripts().await
        })
        .await
        .unwrap_err();
    assert!(matches!(err, SieveError::Imap(_)));
    let sent = crate::managesieve::test_support::sent(&sessions);
    assert_eq!(
        sent, "LISTSCRIPTS\r\n",
        "the connection closed under it: no LOGOUT"
    );

    let (sieve, sessions) = crate::managesieve::test_support::scripted(&[b"OK\r\n", b"OK\r\n"]);
    let _ = sieve
        .with_session(async |session: &mut Session<crate::mock::MockStream>| {
            session.list_scripts().await
        })
        .await
        .unwrap();
    let sent = crate::managesieve::test_support::sent(&sessions);
    assert_eq!(sent, "LISTSCRIPTS\r\nLOGOUT\r\n");
}
