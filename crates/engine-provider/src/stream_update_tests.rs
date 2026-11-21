use std::collections::BTreeSet;

use engine_core::{
    ids::{MailboxId, MessageId, ProviderKey},
    mail::{MailStateChange, Message},
    membership::Memberships,
    sync::{MailboxWindows, SyncState, SyncUpdate, SyncWindow},
    time::{CalendarDate, UtcDateTime},
};

use crate::EmailChunk;

fn mailbox(id: &str) -> MailboxId {
    MailboxId::try_from(id).unwrap()
}

fn key(value: &str) -> ProviderKey {
    ProviderKey::new(value).unwrap()
}

/// A message `id` filed in `folder`, received at `received` (an RFC 3339 instant) or undated.
fn message(id: &str, folder: &str, received: Option<&str>) -> Message {
    let mut message = Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(mailbox(folder)),
    );
    message.received_at = received.map(|at| at.parse::<UtcDateTime>().unwrap());
    message
}

fn since(year: i32, month: u8, day: u8) -> SyncWindow {
    SyncWindow::since(CalendarDate::new(year, month, day).unwrap())
}

fn seen(id: &str) -> MailStateChange {
    MailStateChange::keywords(key(id), BTreeSet::new())
}

fn ids(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .map(|message| message.id.key().as_str())
        .collect()
}

#[test]
fn an_additive_chunk_drops_arrivals_outside_the_window_and_keeps_the_rest() {
    let chunk = EmailChunk::additive(
        vec![
            message("old", "inbox", Some("2025-01-10T09:00:00Z")),
            message("new", "inbox", Some("2026-05-10T09:00:00Z")),
            message("undated", "inbox", None),
        ],
        vec![key("gone")],
        None,
        SyncState::new("n2"),
    )
    .with_patched(vec![seen("new")]);
    let windows = MailboxWindows::new(since(2026, 4, 1));
    let mut present = BTreeSet::new();

    let (update, advance_to) = chunk.into_update(&windows, &mut present);

    let SyncUpdate::Delta {
        changed,
        patched,
        removed,
    } = update
    else {
        panic!("an additive chunk applies as a delta");
    };
    assert_eq!(ids(&changed), ["new", "undated"]);
    // The patch's key also arrived whole, and the whole object is the later word.
    assert_eq!(patched, []);
    assert_eq!(removed, [key("gone")]);
    assert_eq!(advance_to, Some(SyncState::new("n2")));
    assert_eq!(present, BTreeSet::new());
}

#[test]
fn an_additive_chunk_keeps_a_patch_for_a_key_that_did_not_arrive_whole() {
    let chunk = EmailChunk::additive(vec![], vec![], None, SyncState::new("n3"))
        .with_patched(vec![seen("elsewhere")]);

    let (update, _) = chunk.into_update(&MailboxWindows::default(), &mut BTreeSet::new());

    let SyncUpdate::Delta { patched, .. } = update else {
        panic!("an additive chunk applies as a delta");
    };
    assert_eq!(patched, [seen("elsewhere")]);
}

#[test]
fn a_deepened_mailbox_admits_older_mail_than_the_account_window() {
    let chunk = EmailChunk::additive(
        vec![
            message("sent-old", "sent", Some("2025-01-10T09:00:00Z")),
            message("inbox-old", "inbox", Some("2025-01-10T09:00:00Z")),
        ],
        vec![],
        None,
        SyncState::new("n4"),
    );
    let windows = MailboxWindows::new(since(2026, 4, 1)).deepen(mailbox("sent"), since(2024, 1, 1));

    let (update, _) = chunk.into_update(&windows, &mut BTreeSet::new());

    let SyncUpdate::Delta { changed, .. } = update else {
        panic!("an additive chunk applies as a delta");
    };
    assert_eq!(ids(&changed), ["sent-old"]);
}

#[test]
fn reconcile_pages_are_not_filtered_and_the_last_tombstones_against_every_page() {
    let windows = MailboxWindows::new(since(2026, 4, 1));
    let mut present = BTreeSet::new();
    let first = EmailChunk::reconcile_page(
        vec![message("old", "inbox", Some("2025-01-10T09:00:00Z"))],
        vec![key("old")],
        Some(2),
    )
    .with_patched(vec![seen("elsewhere")]);

    let (update, advance_to) = first.into_update(&windows, &mut present);

    // The adapter enumerated the window with its server's date rules; a second rule here could
    // only disagree and drop a message whose key stays present.
    let SyncUpdate::Delta {
        changed, patched, ..
    } = update
    else {
        panic!("an intermediate reconcile page applies additively");
    };
    assert_eq!(ids(&changed), ["old"]);
    assert_eq!(patched, [seen("elsewhere")]);
    assert_eq!(advance_to, None);
    assert_eq!(present, BTreeSet::from([key("old")]));

    let last = EmailChunk::reconcile_last(
        vec![message("new", "inbox", Some("2026-05-10T09:00:00Z"))],
        vec![key("new")],
        Some(2),
        SyncState::new("v1;n9"),
    )
    .with_patched(vec![seen("elsewhere")]);

    let (update, advance_to) = last.into_update(&windows, &mut present);

    assert_eq!(
        update,
        SyncUpdate::snapshot(
            vec![message("new", "inbox", Some("2026-05-10T09:00:00Z"))],
            BTreeSet::from([key("old"), key("new")]),
        ),
        "the snapshot is the whole present set and carries no partials"
    );
    assert_eq!(advance_to, Some(SyncState::new("v1;n9")));
    assert_eq!(
        present,
        BTreeSet::new(),
        "the last page hands the present set over"
    );
}

#[test]
fn an_unbounded_window_admits_everything() {
    let chunk = EmailChunk::additive(
        vec![message("ancient", "inbox", Some("1999-01-10T09:00:00Z"))],
        vec![],
        None,
        SyncState::new("n1"),
    );

    let (update, _) = chunk.into_update(&MailboxWindows::default(), &mut BTreeSet::new());

    let SyncUpdate::Delta { changed, .. } = update else {
        panic!("an additive chunk applies as a delta");
    };
    assert_eq!(ids(&changed), ["ancient"]);
}
