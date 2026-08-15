//! Gated live provider-level checks against a real Google account: Gmail label role
//! resolution, the snapshot → history-delta cursor cycle, and raw-source fetch — all
//! through the real HTTP client, so the actual request shapes are exercised (the offline
//! fakes serve canned bytes regardless of the request — `AGENTS.md`).
//!
//! Skips unless `GOOGLE_ACCESS_TOKEN` is set (an OAuth bearer access token, e.g. from
//! `tools/google-oauth`), so the offline `cargo test --workspace` stays green. There is
//! no CI harness for this (no live Google account in CI); run it locally:
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_provider -- --nocapture
//! ```
//!
//! The tests share one live account and cargo runs them concurrently, so the mutating
//! ones (send/edit) create and delete their own throwaway messages, and the read ones
//! tolerate a concurrently-deleted pick. Google also answers an occasional transient
//! `500 backendError`; cleanup is best-effort. Pass `-- --test-threads=1` for a fully
//! serialized run if a flake ever slips through.

use engine_core::{
    ids::{AccountId, MailboxId, MessageId, MessageIdHeader},
    mail::{EmailAddress, MailboxRole, Message, SystemKeyword},
    membership::Memberships,
    sync::SyncUpdate,
};
use engine_provider::{Draft, MailEdit, Provider};
use provider_google::{GmailProvider, GoogleClient};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The test account's own address — every live send is self-addressed, so nothing leaves
/// the mailbox.
const SELF_ADDRESS: &str = "allodia.e2e@gmail.com";

/// The bearer token, or `None` to skip the gated test.
fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn provider(token: String) -> GmailProvider {
    let client =
        GoogleClient::connect(token, &engine_tls::TlsClientConfig::bundled()).expect("client");
    GmailProvider::new(client)
}

#[tokio::test]
async fn live_labels_resolve_roles() {
    let Some(token) = token() else {
        eprintln!("skipping live_labels_resolve_roles: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let sync = provider(token)
        .sync_mailboxes(&account(), None)
        .await
        .expect("sync labels");
    assert!(sync.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("expected a label snapshot");
    };
    let roles: std::collections::BTreeSet<MailboxRole> =
        objects.iter().filter_map(|m| m.role.clone()).collect();
    assert!(roles.contains(&MailboxRole::Inbox), "inbox role");
    assert!(roles.contains(&MailboxRole::Sent), "sent role");
    // The synthetic All Mail home is always present.
    assert!(roles.contains(&MailboxRole::All), "all-mail role");
    // Keyword-only labels never become mailboxes.
    assert!(!objects.iter().any(|m| m.id.as_str() == "STARRED"));
}

#[tokio::test]
async fn live_snapshot_then_delta_cycle() {
    let Some(token) = token() else {
        eprintln!("skipping live_snapshot_then_delta_cycle: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    // A first sync is a reconciling snapshot; capture the account historyId cursor.
    let snapshot = provider
        .sync_email(&account(), None)
        .await
        .expect("snapshot");
    assert!(snapshot.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &snapshot.update else {
        panic!("expected a snapshot");
    };
    assert!(!objects.is_empty(), "the test account has messages");
    // Every snapshot message carries a provider-assigned thread and an envelope.
    assert!(objects.iter().all(|m| m.thread.is_some()));
    let cursor = snapshot.next_cursor.clone();
    assert!(cursor.as_str().chars().all(|c| c.is_ascii_digit()));

    // An immediate delta from that cursor must not error and must advance (or hold) the
    // cursor — proving the real history.list request shape is accepted.
    let delta = provider
        .sync_email(&account(), Some(&cursor))
        .await
        .expect("delta");
    assert!(!delta.is_snapshot(), "a delta from a live cursor");
    let _ = &delta.next_cursor;
}

// NOTE: the aged-out-cursor → `HistoryExpired` → snapshot-restart recovery is proven
// offline (`fetch::tests::delta_page_maps_a_404_to_history_expired`,
// `provider::tests::sync_email_restarts_...`). It has no live test: a `startHistoryId`
// only 404s once it ages out of Gmail's retained history window, which a fresh test
// account never reaches (an id of "1" is still *inside* the window there and returns a
// valid — large — delta, not a 404). See `tests/fixtures/README.md`.

#[tokio::test]
async fn live_message_source_round_trips() {
    let Some(token) = token() else {
        eprintln!("skipping live_message_source_round_trips: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);
    let snapshot = provider
        .sync_email(&account(), None)
        .await
        .expect("snapshot");
    let SyncUpdate::Snapshot { objects, .. } = &snapshot.update else {
        panic!("expected a snapshot");
    };
    // Try messages until one yields a source: the mutating live tests run concurrently on
    // this shared account, so any single pick can be deleted out from under us (a 404).
    let mut raw = None;
    for message in objects {
        if let Ok(source) = provider.fetch_message_source(&account(), message).await {
            raw = Some(source);
            break;
        }
    }
    let raw = raw.expect("at least one message's source is fetchable");
    // The decoded source is a real RFC 5322 message with headers and a blank-line body.
    let text = String::from_utf8_lossy(raw.as_bytes());
    assert!(
        text.contains("\r\n\r\n") || text.contains("\n\n"),
        "has a body"
    );
    assert!(
        text.to_ascii_lowercase().contains("from:"),
        "has a From header"
    );
}

/// Best-effort cleanup of a throwaway message: Google occasionally answers a transient
/// `500 backendError`, so retry once, then give up (a lingering test message is harmless
/// and must not fail the assertion under test).
async fn cleanup(provider: &GmailProvider, key: engine_core::ids::ProviderKey) {
    for attempt in 0..2 {
        match provider
            .edit_mail(&account(), &MailEdit::delete(key.clone()))
            .await
        {
            Ok(_) => return,
            Err(e) if attempt == 0 => {
                eprintln!("cleanup delete retrying after: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            Err(e) => eprintln!("cleanup delete gave up (leaving throwaway): {e}"),
        }
    }
}

/// A self-addressed draft with a caller-generated Message-ID.
fn live_draft(marker: &str) -> Draft {
    let message_id = MessageIdHeader::new(format!("gmail-live-{marker}@example.test")).unwrap();
    Draft::new(
        message_id,
        EmailAddress::new(SELF_ADDRESS),
        vec![EmailAddress::new(SELF_ADDRESS)],
        format!("Live send {marker}"),
        "Live submission body.",
    )
}

#[tokio::test]
async fn live_send_returns_a_real_id_and_gmail_rewrites_the_message_id() {
    let Some(token) = token() else {
        eprintln!("skipping live_send_...: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);
    // A pid-based marker keeps concurrent runs from colliding without needing a clock.
    let marker = format!("p{}", std::process::id());
    let draft = live_draft(&marker);

    let receipt = provider
        .submit_email(&account(), &draft)
        .await
        .expect("send");
    // Gmail returns the sent message's real id immediately (unlike SMTP/Graph sendMail).
    let id = receipt.email_key.clone();
    assert_eq!(id.as_str().len(), 16, "a 16-hex Gmail message id: {id:?}");

    // Fetch the sent copy's raw source and confirm the real-behavior finding: Gmail
    // *rewrote* the Message-ID (our example.test id is gone, replaced by a gmail one), so
    // reconcile-by-Message-ID would fail — the returned id is authoritative instead.
    let sent = Message::new(
        MessageId::new(id.clone()),
        Memberships::of_one(MailboxId::try_from("SENT").unwrap()),
    );
    let raw = provider
        .fetch_message_source(&account(), &sent)
        .await
        .expect("raw of sent");
    let text = String::from_utf8_lossy(raw.as_bytes());
    assert!(
        !text.contains(&format!("gmail-live-{marker}@example.test")),
        "Gmail rewrites the caller's Message-ID on send"
    );

    // Clean up the throwaway sent+inbox copies (permanent delete, enabled by full scope).
    cleanup(&provider, id).await;
}

#[tokio::test]
async fn live_edit_mail_mark_read_and_flag_are_accepted() {
    let Some(token) = token() else {
        eprintln!("skipping live_edit_mail_...: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);
    let marker = format!("edit-p{}", std::process::id());
    // Send a throwaway to operate on, then exercise every edit verb's real request shape.
    let receipt = provider
        .submit_email(&account(), &live_draft(&marker))
        .await
        .expect("send");
    let key = receipt.email_key;

    provider
        .edit_mail(&account(), &MailEdit::mark_seen(key.clone(), true))
        .await
        .expect("mark read");
    provider
        .edit_mail(&account(), &MailEdit::set_flagged(key.clone(), true))
        .await
        .expect("flag");
    // Move to a real label (a membership replacement over the live API).
    provider
        .edit_mail(
            &account(),
            &MailEdit::move_to(key.clone(), MailboxId::try_from("IMPORTANT").unwrap()),
        )
        .await
        .expect("move to a label");
    // Clean up.
    cleanup(&provider, key).await;
}

#[tokio::test]
async fn live_archive_to_all_mail_leaves_the_inbox_and_is_accepted_by_gmail() {
    let Some(token) = token() else {
        eprintln!("skipping live_archive_...: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);
    let marker = format!("archive-p{}", std::process::id());
    // A self-addressed send lands in INBOX (and SENT), so there is an inbox membership to
    // leave. This is the shape the product archives.
    let receipt = provider
        .submit_email(&account(), &live_draft(&marker))
        .await
        .expect("send");
    let key = receipt.email_key;
    // Prove the precondition before asserting on the change: without an INBOX membership to
    // begin with, "it left the inbox" is a check that cannot fail. Gmail answers a
    // self-addressed send with `labelIds: ["UNREAD","SENT","INBOX"]`.
    assert!(
        labels_of(&provider, &key)
            .await
            .contains(&"INBOX".to_owned()),
        "the throwaway starts in the inbox"
    );

    // Archive = MoveTo the synthetic All-Mail id. Gmail has no Archive label, and
    // `ALL_MAIL` is an id the adapter reserves — sending it as a *label* is a 400
    // `invalidArgument`, which is exactly what this asserts does not happen. Only a live
    // call can catch it: the offline fakes answer canned bytes whatever we send.
    provider
        .edit_mail(
            &account(),
            &MailEdit::move_to(key.clone(), MailboxId::try_from("ALL_MAIL").unwrap()),
        )
        .await
        .expect("archive to All Mail is accepted by Gmail");

    // And it really left the inbox — read the labels back rather than trusting the 200.
    let labels = labels_of(&provider, &key).await;
    assert!(
        !labels.contains(&"INBOX".to_owned()),
        "archived message left the inbox, got {labels:?}"
    );
    // The Sent copy is untouched — `SENT` is system-managed and preserved across a move,
    // so archiving a conversation never hides the user's own reply.
    assert!(
        labels.contains(&"SENT".to_owned()),
        "sent copy survives: {labels:?}"
    );

    cleanup(&provider, key).await;
}

/// The current label membership of `key`, read back through a snapshot (a message's labels
/// *are* its membership). Used to assert an edit's effect against the server rather than
/// against the `200` it answered with.
async fn labels_of(provider: &GmailProvider, key: &engine_core::ids::ProviderKey) -> Vec<String> {
    let snapshot = provider
        .sync_email(&account(), None)
        .await
        .expect("snapshot");
    let SyncUpdate::Snapshot { objects, .. } = &snapshot.update else {
        panic!("expected a snapshot");
    };
    objects
        .iter()
        .find(|message| message.id.key() == key)
        .expect("the message is in the account")
        .mailboxes
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect()
}

#[tokio::test]
async fn live_a_label_change_comes_back_as_state_not_a_whole_message() {
    // Gmail's history record carries the message's *resulting* `labelIds`, and in Gmail that
    // set is the whole of a message's mutable half — labels are both its keywords (`UNREAD`,
    // `STARRED`) and its filing (`INBOX`, and every folder-like label). So a label change is
    // answered by the delta page itself: it arrives as a state change, and nothing re-fetches
    // the message to move a flag.
    //
    // Only a live call proves the *shape* Gmail actually returns. The offline fakes answer
    // canned bytes whatever we send, so a history record that stopped carrying `labelIds`
    // would pass there and fail here.
    let Some(token) = token() else {
        eprintln!("skipping live_a_label_change_...: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);
    let marker = format!("state-p{}", std::process::id());
    let receipt = provider
        .submit_email(&account(), &live_draft(&marker))
        .await
        .expect("send");
    let key = receipt.email_key;

    // Take the cursor *after* the send, so the delta below carries the label change and not
    // the arrival — an id in `messagesAdded` is a new message and is fetched whole, which is
    // the other branch.
    let cursor = provider
        .sync_email(&account(), None)
        .await
        .expect("snapshot")
        .next_cursor;

    provider
        .edit_mail(&account(), &MailEdit::mark_seen(key.clone(), true))
        .await
        .expect("mark read");

    let delta = provider
        .sync_email(&account(), Some(&cursor))
        .await
        .expect("delta after the label change");
    let SyncUpdate::Delta {
        changed, patched, ..
    } = &delta.update
    else {
        panic!("expected a delta");
    };

    assert!(
        !changed.iter().any(|m| m.id.key() == &key),
        "a label change is not a whole object: it would rewrite the stored payload"
    );
    let state = patched
        .iter()
        .find(|c| c.key == key)
        .expect("the label change came back as a state change");
    assert!(
        state
            .state
            .keywords
            .iter()
            .any(|k| k.as_system() == Some(SystemKeyword::Seen)),
        "and it carries the resulting keywords, got {:?}",
        state.state.keywords
    );
    // Gmail files by label, so a state change must carry the filing too — otherwise an
    // archive, which is the same kind of event, would be silently lost.
    let filing = state
        .state
        .mailboxes
        .as_ref()
        .expect("Gmail files in place, so the change says where");
    assert!(
        filing.contains(&MailboxId::try_from("INBOX").unwrap()),
        "the self-addressed send is still in the inbox, got {filing:?}"
    );

    cleanup(&provider, key).await;
}
