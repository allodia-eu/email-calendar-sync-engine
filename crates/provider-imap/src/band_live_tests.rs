//! Gated live: the `MESSAGELIMIT` fallback with both bounds and a UID bound, through its own
//! entry point ([`Connection::uid_search_dated`]), against every configured harness server.
//!
//! No harness server advertises `MESSAGELIMIT`, so the session is told one of 2 after it
//! connects, and the fallback's own requests (`UID FETCH <span> (UID INTERNALDATE)`, and the
//! `PARTIAL` pages where the server offers RFC 9394) go to a real server that serves them. The
//! server's own `UID SEARCH UID 1:n SINCE … BEFORE …` over the same folder is the answer it must
//! match, so neither side's reading of the dates is taken on trust. Yahoo, the server that cuts
//! searches off, runs the same code through `live_imap_password.rs` when the owner runs it.
//!
//! Each run plants five dated messages in a folder of its own, deleted afterwards; on Stalwart
//! as the scratch account `carol@`. Skips per server when its address variable is unset.

use std::time::{SystemTime, UNIX_EPOCH};

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::MessageMetadata,
    membership::Memberships,
    raw::RawMime,
    time::UtcDateTime,
};
use engine_provider::{
    Credential, MailboxEdit, MailboxWrites,
    import::{MailImporter, MessageImport},
};

use crate::{
    ImapAccount, ImapConfig,
    capability::{Extension, Negotiated},
    transport_search::DatedSearch,
};

const SERVERS: [(&str, &str, &str, &str); 3] = [
    (
        "stalwart",
        "STALWART_IMAP_ADDR",
        "carol@test.local",
        "harness-carol-pw",
    ),
    (
        "dovecot-rev1",
        "DOVECOT_REV1_IMAP_ADDR",
        "alice@test.local",
        "dovecot-alice-pw",
    ),
    (
        "dovecot-rev2",
        "DOVECOT_REV2_IMAP_ADDR",
        "alice@test.local",
        "dovecot-alice-pw",
    ),
];

const PLANTED: [&str; 5] = [
    "2025-12-31T23:59:59Z",
    "2026-01-01T00:00:00Z",
    "2026-02-15T12:00:00Z",
    "2026-03-31T23:59:59Z",
    "2026-04-01T00:00:00Z",
];

#[tokio::test]
async fn the_message_limit_fallback_finds_what_the_servers_own_dated_search_finds() {
    for (label, var, user, password) in SERVERS {
        let Ok(addr) = std::env::var(var) else {
            eprintln!("skipping the message-limit fallback on {label}: {var} unset");
            continue;
        };
        let host = addr.rsplit_once(':').map_or("localhost", |(host, _)| host);
        let config = ImapConfig::new(addr.as_str(), host, Credential::password(user, password));
        let imap = ImapAccount::connect(
            &config,
            engine_tls::TlsClientConfig::dangerous_accept_any().connector(),
        )
        .await
        .unwrap_or_else(|err| panic!("connect to {label}: {err}"));
        let account = AccountId::try_from("live-band-limit").unwrap();
        let run = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let admin = imap.provider(MailboxId::try_from("INBOX").unwrap());
        let folder = admin
            .edit_mailbox(
                &account,
                &MailboxEdit::Create {
                    name: format!("Limit {run}"),
                    parent: None,
                },
            )
            .await
            .unwrap_or_else(|err| panic!("create a folder on {label}: {err}"))
            .mailbox
            .expect("the create names its folder");
        let reader = imap.provider(folder.clone());
        let checked = futures_util::FutureExt::catch_unwind(core::panic::AssertUnwindSafe(
            compare(&reader, &account, &folder, label, run),
        ))
        .await;
        admin
            .edit_mailbox(&account, &MailboxEdit::Delete { target: folder })
            .await
            .unwrap_or_else(|err| panic!("delete the folder on {label}: {err}"));
        if let Err(failed) = checked {
            std::panic::resume_unwind(failed);
        }
    }
}

async fn compare(
    reader: &crate::ImapProvider<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
    account: &AccountId,
    folder: &MailboxId,
    label: &str,
    run: u128,
) {
    let batch: Vec<MessageImport> = PLANTED
        .iter()
        .enumerate()
        .map(|(n, at)| {
            let raw = RawMime::new(format!(
                "From: a@example.com\r\nSubject: limit {n}\r\nMessage-ID: <limit-{n}-{run}@example.com>\r\n\r\nx\r\n"
            ));
            MessageImport::new(raw, Memberships::of_one(folder.clone())).with_metadata(
                MessageMetadata::default().with_received_at(at.parse::<UtcDateTime>().unwrap()),
            )
        })
        .collect();
    for answer in reader.import_messages(account, &batch).await {
        answer.unwrap_or_else(|err| panic!("plant on {label}: {err}"));
    }

    let mut session = reader.session().await.expect("a session");
    let select = session.select(folder.as_str()).await.expect("select");
    let advertised = session.capabilities().await.expect("capabilities");
    let partial = Negotiated::from_capabilities(&advertised).has(Extension::Partial);
    let highest = select.uid_next.expect("UIDNEXT") - 3;
    let search = DatedSearch {
        since: Some("1-Jan-2026"),
        before: Some("1-Apr-2026"),
        highest: Some(highest),
    };
    let unbounded = DatedSearch {
        highest: None,
        ..search
    };
    let own = session
        .uid_search_dated(search)
        .await
        .expect("the server's search");
    let own_all = session.uid_search_dated(unbounded).await.expect("search");
    assert_eq!(own_all.len(), 3, "{label}: the server's days, {own_all:?}");
    assert_eq!(own.len(), 2, "{label}: the UID bound left the newest out");

    // Told of a limit of 2, the search returns as many as the limit and reads as cut off.
    let mut told = vec!["MESSAGELIMIT=2".to_owned()];
    if partial {
        told.push("PARTIAL".to_owned());
    }
    session.negotiated = Negotiated::from_capabilities(&told);
    let fallback = session
        .uid_search_dated(search)
        .await
        .expect("the fallback");
    let fallback_all = session
        .uid_search_dated(unbounded)
        .await
        .expect("the fallback");
    eprintln!("{label}: own {own_all:?}, fallback {fallback_all:?}, PARTIAL {partial}");
    assert_eq!(fallback, own, "{label}: with the UID bound");
    assert_eq!(fallback_all, own_all, "{label}: both bounds");
    // The session was told something the server never said; it goes no further.
    session.discard();
}
