//! A ManageSieve service over a scripted server, for the offline tests of the rules and the reply.

use std::{
    fmt::Write as _,
    sync::{Arc, Mutex},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::{
    response::Capabilities,
    service::{ManageSieve, SieveDial},
    session::Session,
};
use crate::{
    mock::{MockStream, Recorded, written},
    pool::other::Budget,
};

/// What Stalwart sends once TLS is up.
pub(crate) const GREETING: &[u8] =
    include_bytes!("../../tests/fixtures/managesieve/stalwart_greeting_tls.txt");

/// A budget of one slot, so a test sees a slot held for the whole session.
pub(crate) struct OneSlot(pub(crate) Arc<Semaphore>);

impl Budget for OneSlot {
    fn reserve(&self) -> futures_util::future::BoxFuture<'_, OwnedSemaphorePermit> {
        Box::pin(async {
            Arc::clone(&self.0)
                .acquire_owned()
                .await
                .expect("never closed")
        })
    }
}

/// A service whose one session answers `answers` after the TLS greeting, and what it was sent.
pub(crate) fn scripted(answers: &[&[u8]]) -> (ManageSieve<MockStream>, Arc<Mutex<Vec<Recorded>>>) {
    let mut script = GREETING.to_vec();
    for answer in answers {
        script.extend_from_slice(answer);
    }
    let sessions: Arc<Mutex<Vec<Recorded>>> = Arc::default();
    let record = Arc::clone(&sessions);
    let dial: SieveDial<MockStream> = Arc::new(move || {
        let (stream, recorded) = MockStream::new(script.clone());
        record.lock().unwrap().push(recorded);
        Box::pin(Session::open(stream))
    });
    let greeting = Capabilities {
        sieve: vec!["vacation".into()],
        ..Capabilities::default()
    };
    let budget = Arc::new(OneSlot(Arc::new(Semaphore::new(1))));
    (ManageSieve::new(dial, budget, &greeting), sessions)
}

/// Everything the client sent, across its sessions.
pub(crate) fn sent(sessions: &Mutex<Vec<Recorded>>) -> String {
    sessions.lock().unwrap().iter().map(written).collect()
}

/// A `LISTSCRIPTS` answer listing `scripts` (name, active).
pub(crate) fn listing(scripts: &[(&str, bool)]) -> Vec<u8> {
    let mut out = String::new();
    for (name, active) in scripts {
        let _ = write!(
            out,
            "\"{name}\"{}\r\n",
            if *active { " ACTIVE" } else { "" }
        );
    }
    out.push_str("OK\r\n");
    out.into_bytes()
}

/// A `GETSCRIPT` answer holding `text`.
pub(crate) fn script(text: &str) -> Vec<u8> {
    format!("{{{}}}\r\n{text}\r\nOK\r\n", text.len()).into_bytes()
}

/// The capabilities once signed in, with `extensions` listed.
pub(crate) fn capability(extensions: &str) -> Vec<u8> {
    format!("\"SIEVE\" \"{extensions}\"\r\n\"MAXREDIRECTS\" \"4\"\r\nOK\r\n").into_bytes()
}

/// Stalwart's own reply script, as its JMAP `VacationResponse/set` wrote it and `GETSCRIPT`
/// answered on the harness (2026-11-24).
pub(crate) const STALWART_REPLY: &str = "require [\"vacation\", \"relational\", \"date\"];\r\n\r\nif \
    currentdate :value \"ge\" \"iso8601\" \"2030-01-02T09:00:00Z\" {\r\nif currentdate :value \
    \"le\" \"iso8601\" \"2030-01-09T17:30:00Z\" {\r\nvacation :mime :subject \"Away\" \
    \"Content-Type: text/plain; charset=\\\"utf-8\\\"\r\nContent-Transfer-Encoding: 7bit\r\n\r\n\
    I am away.\";\r\n}\r\n}\r\n";
