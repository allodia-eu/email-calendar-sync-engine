//! An etag-less delta entry is a state change only when it names nothing but state: one naming
//! content (importance, a draft's subject) is read whole, so the change reaches the store.
//!
//! Exchange has been observed answering an importance or subject change with a full entry
//! (`message_importance.json`'s capture) and an `isRead` change both ways
//! (`messages_delta_changed.json` is the lightweight form). These pin the classification for the
//! lightweight form of a content change, which the docs allow and nothing guarantees against.

use engine_core::{ids::MailboxId, mail::Importance, sync::SyncState};
use serde_json::{Value, json};

use super::{SyncKind, messages_page};
use crate::{
    normalize_state::names_only_state,
    test_support::{fake_client, json as fixture},
};

const IMPORTANCE: &str = include_str!("../tests/fixtures/mail/message_importance.json");

fn lightweight(changed: &Value) -> Value {
    let mut entry = json!({ "@odata.type": "#microsoft.graph.message", "id": "message-1" });
    entry
        .as_object_mut()
        .unwrap()
        .extend(changed.as_object().unwrap().clone());
    entry
}

#[test]
fn an_entry_naming_only_state_is_a_state_change() {
    assert!(names_only_state(&lightweight(&json!({ "isRead": true }))));
    assert!(names_only_state(&lightweight(
        &json!({ "flag": { "flagStatus": "flagged" }, "parentFolderId": "folder-inbox" })
    )));
    assert!(names_only_state(&json!({
        "@odata.type": "#microsoft.graph.message",
        "id": "message-3",
        "isRead": true,
        "parentFolderId": "folder-inbox"
    })));
}

#[test]
fn an_entry_naming_content_or_anything_unknown_is_read_whole() {
    for changed in [
        json!({ "importance": "low" }),
        json!({ "subject": "Renamed" }),
        json!({ "isRead": true, "importance": "high" }),
        json!({ "aPropertyGraphAddsLater": 1 }),
    ] {
        assert!(!names_only_state(&lightweight(&changed)), "{changed}");
    }
    assert!(!names_only_state(&json!(["not", "an", "object"])));
}

#[tokio::test]
async fn a_lightweight_importance_change_reaches_the_message() {
    let cursor = SyncState::new("https://graph.test/me/mailFolders/folder-drafts/delta-token-1");
    let page = json!({
        "@odata.deltaLink": "https://graph.test/me/mailFolders/folder-drafts/delta-token-2",
        "value": [lightweight(&json!({ "importance": "high" }))],
    });
    // Only the delta and the whole-message read are served: a state read would find no route.
    let client = fake_client(vec![
        ("delta-token-1", page),
        (
            "/messages/message-1?$select=id,internetMessageId",
            fixture(IMPORTANCE),
        ),
    ]);
    let drafts = MailboxId::try_from("folder-drafts").unwrap();
    let page = messages_page(&client, &drafts, Some(&cursor), None, None, &[])
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Delta);
    assert!(
        page.patched.is_empty(),
        "a content change is not a state change"
    );
    assert_eq!(page.changed.len(), 1);
    assert_eq!(page.changed[0].envelope.importance, Importance::High);
}
