//! Offline history-delta tests: how a `history.list` page's label changes become state
//! changes, and when a message is fetched whole instead.

use super::*;

/// One `labelsRemoved` history page in the captured shape (see `history_delta.json`):
/// `labelIds` is what moved, `message.labelIds` what the message was left holding.
fn labels_removed(moved: &[&str], resulting: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "history": [{
            "id": "1700",
            "labelsRemoved": [{
                "labelIds": moved,
                "message": { "id": "message-9", "threadId": "t9", "labelIds": resulting },
            }],
        }],
        "historyId": "1700",
    })
}

#[tokio::test]
async fn a_mark_read_is_answered_by_the_history_page_itself() {
    // `UNREAD` removed and nothing else: the page already carries the resulting label
    // set, so this costs no `messages.get` at all. No message route is registered, so a
    // re-fetch would error — the test passing is the proof.
    let client = fake_client(vec![(
        "/history?startHistoryId",
        labels_removed(&["UNREAD"], &["INBOX", "CATEGORY_PERSONAL"]),
    )]);
    let page = delta_page(
        &client,
        &SyncState::new("1681"),
        None,
        &NamedLabels::default(),
    )
    .await
    .unwrap();

    assert!(page.changed.is_empty(), "a mark-read rewrites no message");
    assert_eq!(page.patched.len(), 1);
    assert_eq!(page.patched[0].key.as_str(), "message-9");
    // Gmail's `UNREAD` is inverted, so its *absence* from the resulting set is `$seen`.
    assert!(
        page.patched[0]
            .state
            .keywords
            .iter()
            .any(|k| k.as_system() == Some(engine_core::mail::SystemKeyword::Seen))
    );
}

#[tokio::test]
async fn an_archive_is_a_state_change_that_carries_the_new_filing() {
    // Gmail files by label, so removing `INBOX` is an archive — a membership change. A state
    // change carries filing, so this needs no re-fetch either; what it must not do is lose the
    // move. No message route is registered, so a re-fetch would error.
    let client = fake_client(vec![(
        "/history?startHistoryId",
        labels_removed(&["INBOX"], &["UNREAD", "CATEGORY_PERSONAL"]),
    )]);
    let page = delta_page(
        &client,
        &SyncState::new("1681"),
        None,
        &NamedLabels::default(),
    )
    .await
    .unwrap();

    assert!(page.changed.is_empty(), "an archive rewrites no message");
    assert_eq!(page.patched.len(), 1);
    let filing = page.patched[0]
        .state
        .mailboxes
        .as_ref()
        .expect("Gmail files in place, so the change says where");
    assert!(!filing.contains(&MailboxId::try_from("INBOX").unwrap()));
    assert!(filing.contains(&MailboxId::try_from("CATEGORY_PERSONAL").unwrap()));
    // `UNREAD` is keyword state, never a place.
    assert!(!filing.contains(&MailboxId::try_from("UNREAD").unwrap()));
}

#[tokio::test]
async fn a_message_left_with_no_folder_label_is_filed_in_all_mail() {
    // An archived, uncategorized message carries no folder-like label at all, and the engine's
    // membership set may not be empty — the same synthetic home the whole-object path uses.
    let client = fake_client(vec![(
        "/history?startHistoryId",
        labels_removed(&["INBOX"], &[]),
    )]);
    let page = delta_page(
        &client,
        &SyncState::new("1681"),
        None,
        &NamedLabels::default(),
    )
    .await
    .unwrap();
    let filing = page.patched[0].state.mailboxes.as_ref().expect("filing");
    assert!(filing.contains(&MailboxId::try_from("ALL_MAIL").unwrap()));
}

#[tokio::test]
async fn an_absent_resulting_label_set_refetches_rather_than_guessing() {
    // A partial with no `message.labelIds` at all is not an empty label set. Reading it
    // as one would hand `keywords_from_labels` an empty list, whose missing `UNREAD`
    // means `$seen` — silently marking unread mail read.
    let client = fake_client(vec![
        (
            "/history?startHistoryId",
            serde_json::json!({
                "history": [{
                    "id": "1700",
                    "labelsRemoved": [{
                        "labelIds": ["UNREAD"],
                        "message": { "id": "message-9", "threadId": "t9" },
                    }],
                }],
                "historyId": "1700",
            }),
        ),
        (
            "/messages/message-9",
            serde_json::json!({ "id": "message-9", "threadId": "t9", "labelIds": ["INBOX"] }),
        ),
    ]);
    let page = delta_page(
        &client,
        &SyncState::new("1681"),
        None,
        &NamedLabels::default(),
    )
    .await
    .unwrap();

    assert!(page.patched.is_empty());
    assert_eq!(page.changed.len(), 1);
}

#[tokio::test]
async fn a_new_message_that_also_changed_labels_is_still_fetched_whole() {
    // The same id in `messagesAdded` and `labelsAdded`: we hold none of its content, so
    // the label half must not shortcut the fetch.
    let client = fake_client(vec![
        (
            "/history?startHistoryId",
            serde_json::json!({
                "history": [{
                    "id": "1700",
                    "messagesAdded": [{
                        "message": { "id": "message-9", "threadId": "t9", "labelIds": ["UNREAD"] },
                    }],
                    "labelsAdded": [{
                        "labelIds": ["STARRED"],
                        "message": {
                            "id": "message-9", "threadId": "t9",
                            "labelIds": ["UNREAD", "STARRED"],
                        },
                    }],
                }],
                "historyId": "1700",
            }),
        ),
        (
            "/messages/message-9",
            serde_json::json!({
                "id": "message-9", "threadId": "t9", "labelIds": ["UNREAD", "STARRED"],
            }),
        ),
    ]);
    let page = delta_page(
        &client,
        &SyncState::new("1681"),
        None,
        &NamedLabels::default(),
    )
    .await
    .unwrap();

    assert!(page.patched.is_empty());
    assert_eq!(page.changed.len(), 1);
}

#[tokio::test]
async fn delta_page_maps_a_404_to_history_expired() {
    // An aged-out startHistoryId (404) becomes the resync signal, not a plain permanent.
    let client = fake_client_fallible(vec![(
        "/history?startHistoryId",
        Err((404, json(HISTORY_GONE))),
    )]);
    let err = delta_page(&client, &SyncState::new("1"), None, &NamedLabels::default())
        .await
        .unwrap_err();
    assert!(matches!(err, GoogleError::HistoryExpired(_)));
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::NeedsResync
    );
}
