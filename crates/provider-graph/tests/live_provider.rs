//! Gated live provider-level checks against a real Microsoft Graph account: folder
//! role resolution, message normalization, and the snapshot → delta cursor cycle
//! through the real HTTP client.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set (an OAuth bearer access token, e.g.
//! from `tools/graph-oauth`), so the offline `cargo test --workspace` stays green.
//! There is no CI harness for this (no live Microsoft account in CI); run it
//! locally:
//!
//! ```sh
//! cargo run --manifest-path tools/graph-oauth/Cargo.toml -- refresh
//! GRAPH_ACCESS_TOKEN="$(python3 -c "import json;print(json.load(open('tools/graph-oauth/.local/tokens.json'))['access_token'])")" \
//!   cargo test -p provider-graph --test live_provider -- --nocapture
//! ```

use std::collections::BTreeSet;

use engine_core::{
    calendar::Event,
    ids::{AccountId, CalendarId, MailboxId, MessageIdHeader, Uid},
    mail::{EmailAddress, MailboxRole, Message, SystemKeyword},
    membership::Memberships,
    sync::SyncUpdate,
    time::{CalendarDate, CalendarDateTime, LocalDateTime, TimeZoneId},
};
use engine_provider::{
    Draft, EventDeletion, EventDraft, EventEdit, EventPatch, MailEdit, PatchTarget, Provider,
};
use provider_graph::{CalendarWindow, GraphCalendarProvider, GraphClient, GraphProvider};

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The throwaway test account's own address — every live send is addressed to it (the
/// account sends only to itself), so nothing leaves the mailbox.
const SELF_ADDRESS: &str = "allodia-e2e@outlook.com";

/// The bearer token, or `None` to skip the gated test.
fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// A provider bound to the inbox (Graph accepts the well-known alias in the URL).
fn provider(token: String) -> GraphProvider {
    let client =
        GraphClient::connect(token, &engine_tls::TlsClientConfig::bundled()).expect("client");
    GraphProvider::new(client, MailboxId::try_from("inbox").unwrap())
}

#[tokio::test]
async fn live_mail_folders_resolve_roles() {
    let Some(token) = token() else {
        eprintln!("skipping live_mail_folders_resolve_roles: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let mailboxes = provider(token)
        .sync_mailboxes(&account(), None)
        .await
        .expect("sync folders");
    assert!(mailboxes.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &mailboxes.update else {
        panic!("expected a folder snapshot");
    };
    // Roles resolve by well-known-alias id despite localized display names.
    let roles: BTreeSet<MailboxRole> = objects.iter().filter_map(|m| m.role.clone()).collect();
    assert!(roles.contains(&MailboxRole::Inbox), "inbox role resolved");
    assert!(roles.contains(&MailboxRole::Sent), "sent role resolved");
    // Every folder is top-level (parent nulled against msgfolderroot).
    assert!(objects.iter().all(|m| m.parent.is_none()));
}

#[tokio::test]
async fn live_message_snapshot_then_delta() {
    let Some(token) = token() else {
        eprintln!("skipping live_message_snapshot_then_delta: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    // The initial pass is a full snapshot of the inbox.
    let snapshot = provider
        .sync_email(&account(), None)
        .await
        .expect("snapshot");
    assert!(snapshot.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &snapshot.update else {
        panic!("expected a message snapshot");
    };
    // The deterministic seed message is present and fully normalized.
    let subjects: BTreeSet<&str> = objects
        .iter()
        .filter_map(|m| m.envelope.subject.as_deref())
        .collect();
    assert!(
        subjects.contains("Fixture: first message"),
        "seed subject missing; got {subjects:?}"
    );
    // Graph mail is single-folder, so every membership has exactly one collection.
    assert!(objects.iter().all(|m| m.mailboxes.len().get() == 1));

    // A delta from the fresh cursor is a delta (not a snapshot).
    let delta = provider
        .sync_email(&account(), Some(&snapshot.next_cursor))
        .await
        .expect("delta");
    assert!(!delta.is_snapshot());
}

#[tokio::test]
async fn live_send_preserves_message_id_end_to_end() {
    let Some(token) = token() else {
        eprintln!("skipping live_send_preserves_message_id_end_to_end: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    // A unique, self-addressed draft: the whole point of the MIME send is that our
    // pre-generated Message-ID reaches the wire verbatim, so we prove it does by finding
    // it come back. Uniqueness comes from the wall clock (a test may read it).
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let message_id = MessageIdHeader::new(format!("graph-live-{unique}@allodia-e2e.test")).unwrap();
    let me = EmailAddress::new(SELF_ADDRESS);
    let draft = Draft::new(
        message_id.clone(),
        me.clone(),
        vec![me],
        format!("provider-graph MIME send probe {unique}"),
        "Sent by the provider-graph live test; safe to delete.",
    );

    // `submit_email` returns a receipt with a Message-ID-derived placeholder key (Graph
    // answers 202 with no id) echoing the Message-ID for reconciliation.
    let receipt = provider
        .submit_email(&account(), &draft)
        .await
        .expect("submit_email");
    assert_eq!(receipt.message_id, message_id);
    assert_eq!(
        receipt.email_key.as_str(),
        format!("sent:graph-live-{unique}@allodia-e2e.test")
    );

    // The self-addressed copy lands back in the Inbox; poll for our exact Message-ID to
    // prove Graph preserved it (it is bracket-stripped in the projection, as we generated it).
    let mut found = false;
    for _ in 0..15 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let snapshot = provider
            .sync_email(&account(), None)
            .await
            .expect("resync inbox");
        let SyncUpdate::Snapshot { objects, .. } = &snapshot.update else {
            continue;
        };
        if objects.iter().any(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id.as_str())
        }) {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "the sent Message-ID {} never appeared in the inbox — Graph did not preserve it",
        message_id.as_str()
    );
}

/// Sends a unique self-addressed message and polls the inbox until it appears, returning
/// the synced [`Message`] (with its immutable-id provider key). Panics if it never lands.
async fn send_and_await_inbox(provider: &GraphProvider, message_id: &MessageIdHeader) -> Message {
    let me = EmailAddress::new(SELF_ADDRESS);
    let draft = Draft::new(
        message_id.clone(),
        me.clone(),
        vec![me],
        "provider-graph write probe",
        "Sent by the provider-graph live write test; safe to delete.",
    );
    provider
        .submit_email(&account(), &draft)
        .await
        .expect("submit_email");

    for _ in 0..15 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let snapshot = provider.sync_email(&account(), None).await.expect("resync");
        let SyncUpdate::Snapshot { objects, .. } = snapshot.update else {
            continue;
        };
        if let Some(msg) = objects.into_iter().find(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id.as_str())
        }) {
            return msg;
        }
    }
    panic!("the sent message never appeared in the inbox");
}

/// Re-syncs the inbox and finds the message with `message_id`, if still present.
async fn find_in_inbox(provider: &GraphProvider, message_id: &MessageIdHeader) -> Option<Message> {
    let snapshot = provider.sync_email(&account(), None).await.expect("resync");
    let SyncUpdate::Snapshot { objects, .. } = snapshot.update else {
        return None;
    };
    objects.into_iter().find(|m| {
        m.envelope
            .message_id
            .iter()
            .any(|id| id.as_str() == message_id.as_str())
    })
}

#[tokio::test]
async fn live_mail_edit_marks_moves_and_deletes() {
    let Some(token) = token() else {
        eprintln!("skipping live_mail_edit_marks_moves_and_deletes: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token.clone());

    // Land a fresh, unique self-addressed message in the inbox to edit.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let message_id = MessageIdHeader::new(format!("graph-edit-{unique}@allodia-e2e.test")).unwrap();
    let message = send_and_await_inbox(&provider, &message_id).await;
    let key = message.id.key().clone();
    assert!(
        !message.has_system_keyword(SystemKeyword::Seen),
        "a freshly delivered message is unread"
    );

    // Mark it read + flagged; a re-sync must reflect both keyword changes.
    provider
        .edit_mail(
            &account(),
            &MailEdit::SetKeywords {
                target: key.clone(),
                add: [
                    engine_core::mail::Keyword::system(SystemKeyword::Seen),
                    engine_core::mail::Keyword::system(SystemKeyword::Flagged),
                ]
                .into(),
                remove: std::collections::BTreeSet::new(),
            },
        )
        .await
        .expect("set keywords");
    let edited = find_in_inbox(&provider, &message_id)
        .await
        .expect("the message is still in the inbox after a keyword edit");
    assert!(edited.has_system_keyword(SystemKeyword::Seen), "now read");
    assert!(
        edited.has_system_keyword(SystemKeyword::Flagged),
        "now flagged"
    );

    // Move it to Archive; a re-sync of the inbox must no longer find it.
    let archive = archive_folder_id(&provider).await;
    provider
        .edit_mail(&account(), &MailEdit::move_to(key.clone(), archive))
        .await
        .expect("move to archive");
    assert!(
        find_in_inbox(&provider, &message_id).await.is_none(),
        "the moved message left the inbox"
    );

    // Immutable ids survive the move, so the same key deletes it (from Archive) to clean up.
    provider
        .edit_mail(&account(), &MailEdit::delete(key))
        .await
        .expect("permanent delete");
    // (A repeat delete is NOT retried: Graph answers a re-delete of a purged message with
    // `403 ErrorCannotDeleteObject`, not the clean `404` idempotency keys on. That 404
    // path is offline-tested; the outbox's NeedsConfirmation owns the ambiguous retry.)
}

/// Resolves the account's Archive folder id from a folder-list sync.
async fn archive_folder_id(provider: &GraphProvider) -> MailboxId {
    let folders = provider
        .sync_mailboxes(&account(), None)
        .await
        .expect("sync folders");
    let SyncUpdate::Snapshot { objects, .. } = folders.update else {
        panic!("expected a folder snapshot");
    };
    objects
        .into_iter()
        .find(|m| m.role == Some(MailboxRole::Archive))
        .expect("an archive folder")
        .id
}

// ---------------------------------------------------------------------------
// Calendar (gated live)
// ---------------------------------------------------------------------------

fn calendar_window() -> CalendarWindow {
    CalendarWindow::new(
        CalendarDate::new(2026, 8, 1).unwrap(),
        CalendarDate::new(2026, 11, 1).unwrap(),
    )
}

fn amsterdam() -> TimeZoneId {
    TimeZoneId::iana("Europe/Amsterdam").unwrap()
}

/// A calendar provider bound to `calendar`, reading times in Europe/Amsterdam.
fn calendar_provider(token: &str, calendar: CalendarId) -> GraphCalendarProvider {
    let client =
        GraphClient::connect(token, &engine_tls::TlsClientConfig::bundled()).expect("client");
    GraphCalendarProvider::new(client, calendar, calendar_window(), amsterdam())
}

fn zoned(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse::<LocalDateTime>().unwrap(),
        zone: amsterdam(),
    }
}

/// A minimal event carrying the identity + revision a write receipt reports, so a
/// follow-up patch/delete can guard on the ETag the create/patch returned.
fn base_from(
    receipt_event: &CalendarId,
    id: &str,
    uid: &Uid,
    revisions: engine_core::version::RevisionTokens,
) -> Event {
    let mut event = Event::new(
        engine_core::ids::EventId::try_from(id).unwrap(),
        uid.clone(),
        Memberships::of_one(receipt_event.clone()),
        zoned("2026-09-01T10:00:00"),
    );
    event.revisions = revisions;
    event
}

#[tokio::test]
async fn live_calendar_lists_syncs_and_writes() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendar_lists_syncs_and_writes: GRAPH_ACCESS_TOKEN unset");
        return;
    };

    // List calendars and find the default (the binding used for events + writes).
    let placeholder = CalendarId::try_from("placeholder").unwrap();
    let calendars = calendar_provider(&token, placeholder)
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars");
    let SyncUpdate::Snapshot { objects, .. } = &calendars.update else {
        panic!("expected a calendar snapshot");
    };
    let default = objects
        .iter()
        .find(|c| c.is_default)
        .expect("a default calendar");
    let calendar_id = default.id.clone();

    let provider = calendar_provider(&token, calendar_id.clone());

    // A snapshot of the calendar's events: masters + singles, each zoned in the display
    // zone (proving the Prefer: outlook.timezone request), recurrence mapped for a series.
    let events = provider
        .sync_events(&account(), None)
        .await
        .expect("sync events");
    assert!(events.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &events.update else {
        panic!("expected an event snapshot");
    };
    assert!(
        objects.iter().all(|e| matches!(
            e.start,
            CalendarDateTime::Zoned { .. } | CalendarDateTime::Date(_)
        )),
        "every event is zoned or all-day (never a bare UTC instant)"
    );
    // A delta from the fresh cursor is a delta, not a snapshot.
    let delta = provider
        .sync_events(&account(), Some(&events.next_cursor))
        .await
        .expect("delta");
    assert!(!delta.is_snapshot());

    // Create → patch → delete a throwaway event, guarding each write on the returned ETag.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let uid = Uid::new(format!("live-cal-{unique}@allodia-e2e.test")).unwrap();
    let draft = EventDraft::new(
        calendar_id.clone(),
        uid.clone(),
        "provider-graph live write probe",
        zoned("2026-09-15T10:00:00"),
        zoned("2026-09-15T10:30:00"),
        "2026-07-18T10:00:00Z".parse().unwrap(),
    )
    .location("Room Z")
    .description("safe to delete");

    let created = provider
        .create_event(&account(), &draft)
        .await
        .expect("create_event");
    assert!(
        created.revisions.etag.is_some(),
        "Graph returns an ETag on create"
    );

    // Rename it (a whole-series patch), guarded by the create's ETag.
    let base = base_from(
        &calendar_id,
        created.event.as_str(),
        &created.uid,
        created.revisions.clone(),
    );
    let edit = EventEdit::new(
        &base,
        PatchTarget::Series,
        EventPatch::new("2026-07-18T10:05:00Z".parse().unwrap())
            .summary("live write probe (renamed)"),
    );
    let patched = provider
        .patch_event(&account(), &base, &edit)
        .await
        .expect("patch_event");
    assert!(patched.revisions.etag.is_some());
    assert_ne!(
        patched.revisions.etag, created.revisions.etag,
        "a patch advances the ETag"
    );

    // Delete it, guarded by the patch's ETag.
    let base = base_from(
        &calendar_id,
        patched.event.as_str(),
        &patched.uid,
        patched.revisions.clone(),
    );
    provider
        .delete_event(&account(), &EventDeletion::of(&base))
        .await
        .expect("delete_event");
    // (A repeat delete of the just-deleted event is NOT retried here: Graph answers a
    // re-delete with `400 ErrorInvalidRequest` — the item has moved to Deleted Items — not
    // the clean `404` the idempotent path keys on. The 404 idempotency is offline-tested;
    // the outbox's NeedsConfirmation path covers the genuinely-ambiguous retry.)
}
