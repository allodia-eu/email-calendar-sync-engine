//! Gated live **write** of the automatic replies. Not run by default, nor by a token alone: it
//! changes the mailbox, so it needs `GRAPH_RULES_WRITES=1` beside a delegated `GRAPH_ACCESS_TOKEN`
//! with `MailboxSettings.ReadWrite`.
//!
//! It writes only where the reply is off (`status: disabled`), keeps the setting exactly as read,
//! writes it once as a reply scheduled for 2099, and only once that has settled (five matching
//! reads a minute apart: two quick `mailboxSettings` writes have landed out of order before,
//! `graph.md`) `PATCH`es the kept setting back verbatim; the setting must settle on it the same way
//! and still match it five minutes later. It prints only statuses and counts of differing fields.

mod live_writes_support;

use std::time::Duration;

use engine_core::{
    error::FailureClass,
    time::UtcDateTime,
    vacation::{AutoReply, VacationWindow},
};
use engine_provider::{Provider, VacationPrecondition};
use live_writes_support::{account, failed, provider, send, settled_by, token};
use serde_json::{Value, json};

const SETTING: &str = "/me/mailboxSettings/automaticRepliesSetting";

/// The reply's setting as Graph answers it, without the context that names the mailbox.
async fn setting(token: &str) -> Value {
    let (status, mut setting) = send(token, reqwest::Method::GET, SETTING, None).await;
    assert!(status == 200, "reading the reply answered {status}");
    setting.as_object_mut().unwrap().remove("@odata.context");
    setting
}

/// How many of `kept`'s fields `now` does not hold as they were, either way.
fn differing(kept: &Value, now: &Value) -> usize {
    let (Some(kept), Some(now)) = (kept.as_object(), now.as_object()) else {
        return usize::MAX;
    };
    kept.keys()
        .chain(now.keys().filter(|key| !kept.contains_key(*key)))
        .filter(|key| kept.get(*key) != now.get(*key))
        .count()
}

/// Five matching reads a minute apart: a `mailboxSettings` write has landed out of order before.
async fn settled_setting(token: &str, what: &str, holds: impl Fn(&Value) -> bool) -> Value {
    settled_by(what, 5, Duration::from_secs(60), || setting(token), holds).await
}

#[tokio::test]
async fn live_a_reply_scheduled_for_2099_is_written_settled_and_the_setting_restored_exactly() {
    let Some(token) = token() else {
        eprintln!("skipping: GRAPH_RULES_WRITES=1 and GRAPH_ACCESS_TOKEN not both set");
        return;
    };
    let kept = setting(&token).await;
    assert!(
        kept["status"] == "disabled",
        "unexpected state: the reply is not off; nothing was written"
    );
    let graph = provider(&token);
    let before = graph
        .vacation()
        .unwrap()
        .read_vacation(&account())
        .await
        .unwrap_or_else(|err| failed("read", &err));
    let run = tokio::spawn(reply_cycle(token.clone(), before)).await;
    // Put the setting back exactly as it was read, once the write above has settled (or failed),
    // wait until the restore has settled in turn, and read once more five minutes later.
    let (restored, _) = send(
        &token,
        reqwest::Method::PATCH,
        "/me/mailboxSettings",
        Some(json!({ "automaticRepliesSetting": kept })),
    )
    .await;
    settled_setting(&token, "the restored setting", |now| {
        differing(&kept, now) == 0
    })
    .await;
    tokio::time::sleep(Duration::from_secs(300)).await;
    let changed = differing(&kept, &setting(&token).await);
    eprintln!("restore: {restored}; fields differing five minutes after it settled: {changed}");
    if let Err(panic) = run {
        std::panic::resume_unwind(panic.into_panic());
    }
    assert!(
        (200..300).contains(&restored),
        "restore answered {restored}"
    );
    assert!(changed == 0, "{changed} fields differ from the snapshot");
}

async fn reply_cycle(token: String, before: engine_core::vacation::VacationResponse) {
    let graph = provider(&token);
    let reply = graph.vacation().unwrap();
    let at = |text: &str| UtcDateTime::parse_rfc3339(text).unwrap();
    let window = VacationWindow::default()
        .with_start(at("2099-01-01T09:00:00Z"))
        .with_end(at("2099-01-02T17:00:00Z"));
    let new = before
        .clone()
        .turned_on()
        .with_window(window)
        .with_reply(AutoReply::default().with_html("<p>Engine live test reply</p>"));
    let receipt = reply
        .set_vacation(&account(), VacationPrecondition::IfUnchanged(&before), &new)
        .await
        .unwrap_or_else(|err| failed("write", &err));
    // Graph settles the write later: the receipt names no version.
    assert_eq!(receipt.version, None);
    settled_setting(&token, "the written reply", |now| {
        now["status"] == "scheduled"
            && now["scheduledStartDateTime"]["dateTime"]
                .as_str()
                .is_some_and(|t| t.starts_with("2099-01-01T09:00:00"))
            && now["internalReplyMessage"]
                .as_str()
                .is_some_and(|m| m.contains("Engine live test reply"))
    })
    .await;
    let stale = reply
        .set_vacation(&account(), VacationPrecondition::IfUnchanged(&before), &new)
        .await
        .err()
        .unwrap_or_else(|| panic!("a stale write was taken"));
    assert_eq!(stale.class(), FailureClass::Conflict);
}
