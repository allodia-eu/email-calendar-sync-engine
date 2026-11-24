//! The automatic replies read canonically and written as the model says, against the scrubbed
//! capture (`automatic_replies_disabled.json`) and a scheduled shape written after Graph's
//! reference in the capture's shape (`automatic_replies_scheduled.json`).

use engine_core::{
    error::FailureClass,
    ids::AccountId,
    time::UtcDateTime,
    vacation::{AutoReply, ReplyAudience, RulesEffect, VacationResponse, VacationWindow},
    version::VacationVersion,
};
use engine_provider::{Provider, Vacation, VacationPrecondition, WriteGuard};
use serde_json::{Value, json};

use super::{reply, setting, vocabulary};
use crate::{
    GraphProvider,
    test_support::{FakeRoute, RequestLog, fake_client_logged, json},
};

const DISABLED: &str = include_str!("../tests/fixtures/rules/automatic_replies_disabled.json");
const SHARED: &str = include_str!("../tests/fixtures/rules/automatic_replies_shared.json");
const SCHEDULED: &str = include_str!("../tests/fixtures/rules/automatic_replies_scheduled.json");

fn at(text: &str) -> UtcDateTime {
    UtcDateTime::parse_rfc3339(text).unwrap()
}

fn provider(fixture: &str, extra: Vec<(&str, FakeRoute)>) -> (GraphProvider, RequestLog) {
    let mut routes = extra;
    routes.push(("automaticRepliesSetting", Ok(json(fixture))));
    routes.push(("/mailboxSettings", Ok(Value::Null)));
    let (client, _, log) = fake_client_logged(routes);
    let folder = engine_core::ids::MailboxId::try_from("inbox").unwrap();
    (GraphProvider::new(client, folder), log)
}

fn account() -> AccountId {
    AccountId::try_from("a").unwrap()
}

fn patches(log: &RequestLog) -> Vec<Value> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|(method, ..)| *method == "PATCH")
        .map(|(_, _, body)| body.clone().unwrap())
        .collect()
}

#[test]
fn the_captured_reply_reads_off_with_two_html_messages_to_everyone() {
    let read = reply(&json(DISABLED)).unwrap();
    assert!(!read.enabled);
    // A schedule kept beside `disabled` is not a window the reply runs in.
    assert!(read.window.is_open());
    assert_eq!(read.audience, ReplyAudience::Everyone);
    assert!(
        read.reply
            .html
            .as_deref()
            .unwrap()
            .contains("senders inside")
    );
    assert!(read.reply.text.is_none() && read.reply.subject.is_none());
    let outside = read
        .outside_reply
        .as_ref()
        .expect("a different external message");
    assert!(outside.html.as_deref().unwrap().contains("senders outside"));
    assert!(read.version.is_some());
    // The context names the mailbox, and is not kept.
    assert!(
        !read
            .raw
            .as_ref()
            .unwrap()
            .as_str()
            .contains("odata.context")
    );
    // A shared mailbox with no messages reads as an empty reply.
    let shared = reply(&json(SHARED)).unwrap();
    assert!(shared.reply.is_empty() && shared.outside_reply.is_none());
}

#[test]
fn a_scheduled_reply_reads_its_zoned_ends_as_instants() {
    let read = reply(&json(SCHEDULED)).unwrap();
    assert!(read.enabled);
    // 09:00 in Western Europe in July is 07:00 UTC; the end was written in UTC already.
    assert_eq!(read.window.start, Some(at("2026-07-01T07:00:00Z")));
    assert_eq!(read.window.end, Some(at("2026-07-15T17:30:00.5Z")));
    assert_eq!(read.audience, ReplyAudience::OrganizationAndContacts);
    // The same message for outside senders is no outside reply.
    assert!(read.outside_reply.is_none());
    assert!(read.replies_at(at("2026-07-10T12:00:00Z")));
}

#[test]
fn each_status_and_audience_reads_as_its_cell_says() {
    let mut doc = json(SCHEDULED);
    doc["status"] = json!("alwaysEnabled");
    doc["externalAudience"] = json!("none");
    doc["externalReplyMessage"] = json!("<p>other</p>");
    let read = reply(&doc).unwrap();
    assert!(read.enabled && read.window.is_open());
    // Nobody outside gets a reply: its own message is no outside reply.
    assert_eq!(read.audience, ReplyAudience::Organization);
    assert!(read.outside_reply.is_none());
    doc["externalAudience"] = json!("somethingNew");
    assert_eq!(reply(&doc).unwrap().audience, ReplyAudience::Organization);
    doc["status"] = json!("unknownFutureValue");
    assert!(reply(&doc).is_err());
    doc["status"] = json!("scheduled");
    doc["scheduledStartDateTime"] = json!({"dateTime": "not a time", "timeZone": "UTC"});
    assert!(reply(&doc).is_err());
    assert!(reply(&json!([])).is_err());
}

#[test]
fn a_reply_is_written_as_the_setting_that_says_it() {
    let window = VacationWindow::default()
        .with_start(at("2030-01-01T00:00:00Z"))
        .with_end(at("2030-01-02T12:30:00.25Z"));
    let on = VacationResponse::default()
        .turned_on()
        .with_window(window)
        .with_reply(AutoReply::default().with_html("<p>in</p>"))
        .with_audience(ReplyAudience::OrganizationAndContacts)
        .with_outside_reply(AutoReply::default().with_text("Out & <about>\nback soon"));
    assert_eq!(
        setting(&on),
        json!({
            "status": "scheduled",
            "scheduledStartDateTime": {"dateTime": "2030-01-01T00:00:00", "timeZone": "UTC"},
            "scheduledEndDateTime": {"dateTime": "2030-01-02T12:30:00.2500000", "timeZone": "UTC"},
            "externalAudience": "contactsOnly",
            "internalReplyMessage": "<p>in</p>",
            "externalReplyMessage": "Out &amp; &lt;about&gt;<br>back soon"
        })
    );
    // Off, it keeps its window; with no outside reply, outside senders get the same one.
    let off = on.clone().turned_off();
    let mut off = off;
    off.outside_reply = None;
    off.audience = ReplyAudience::Everyone;
    let written = setting(&off);
    assert_eq!(written["status"], "disabled");
    assert!(written.get("scheduledEndDateTime").is_some());
    assert_eq!(written["externalReplyMessage"], "<p>in</p>");
    assert_eq!(written["externalAudience"], "all");
    let always = VacationResponse::default().turned_on();
    let written = setting(&always);
    assert_eq!(written["status"], "alwaysEnabled");
    assert_eq!(written["internalReplyMessage"], "");
    assert!(written.get("scheduledStartDateTime").is_none());
}

#[test]
fn what_graph_writes_reads_back_the_same() {
    let read = reply(&json(SCHEDULED)).unwrap();
    let again = reply(&setting(&read)).unwrap();
    assert_eq!(
        (again.enabled, again.window, again.audience, &again.reply),
        (read.enabled, read.window, read.audience, &read.reply)
    );
}

#[tokio::test]
async fn a_guarded_write_reads_again_and_refuses_a_reply_that_moved() {
    let (graph, log) = provider(DISABLED, vec![]);
    let read = graph.read_vacation(&account()).await.unwrap();
    let new = read.clone().turned_on();
    let mut stale = read.clone();
    stale.version = Some(VacationVersion::new("older"));
    let err = graph
        .set_vacation(&account(), VacationPrecondition::IfUnchanged(&stale), &new)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
    let mut built = read.clone();
    built.version = None;
    let err = graph
        .set_vacation(&account(), VacationPrecondition::IfUnchanged(&built), &new)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert_eq!(patches(&log).len(), 0);
    let receipt = graph
        .set_vacation(&account(), VacationPrecondition::IfUnchanged(&read), &new)
        .await
        .unwrap();
    assert_eq!(receipt.rules, RulesEffect::Untouched);
    assert!(receipt.version.is_some());
    let sent = patches(&log);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0]["automaticRepliesSetting"]["status"],
        "alwaysEnabled"
    );
}

#[tokio::test]
async fn a_reply_graph_cannot_hold_is_refused_before_anything_is_sent() {
    let (graph, log) = provider(DISABLED, vec![]);
    for unheld in [
        VacationResponse::default().with_reply(AutoReply::default().with_subject("s")),
        VacationResponse::default()
            .with_reply(AutoReply::default().with_text("t").with_html("<p>t</p>")),
        VacationResponse::default().with_audience(ReplyAudience::Contacts),
        VacationResponse::default()
            .with_window(VacationWindow::default().with_start(at("2030-01-01T00:00:00Z"))),
    ] {
        let err = graph
            .set_vacation(&account(), VacationPrecondition::Unconditional, &unheld)
            .await
            .unwrap_err();
        assert!(err.is_item_unavailable(), "{unheld:?}");
    }
    assert_eq!(log.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn an_unconditional_write_sends_without_reading_first_and_a_refusal_is_classified() {
    let (graph, log) = provider(
        DISABLED,
        vec![(
            "/mailboxSettings",
            Err((
                403,
                json!({"error": {"code": "ErrorAccessDenied", "message": "m"}}),
            )),
        )],
    );
    let err = graph
        .set_vacation(
            &account(),
            VacationPrecondition::Unconditional,
            &VacationResponse::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert_eq!(log.lock().unwrap().len(), 1, "the PATCH alone");
}

#[test]
fn graph_offers_the_reply_with_its_vocabulary_and_no_server_guard() {
    let (graph, _) = provider(DISABLED, vec![]);
    assert!(graph.vacation().is_some());
    assert_eq!(graph.vacation_write_guard(), WriteGuard::Absent);
    let held = vocabulary();
    let asked = futures_util::FutureExt::now_or_never(Vacation::vocabulary(&graph, &account()));
    assert_eq!(asked.unwrap().unwrap(), held);
    assert!(held.has_outside_reply() && !held.has_subject());
    let shown = format!("{:?}", reply(&json(DISABLED)).unwrap());
    assert!(!shown.contains("Example reply"), "{shown}");
}

fn scheduled(start: &str, end: &str, zone: &str) -> Value {
    let mut doc = json(SCHEDULED);
    doc["scheduledStartDateTime"] = json!({"dateTime": start, "timeZone": zone});
    doc["scheduledEndDateTime"] = json!({"dateTime": end, "timeZone": zone});
    doc
}

#[test]
fn a_wall_time_dst_skips_or_repeats_never_widens_the_window() {
    // Western Europe, 2026: 02:00 to 03:00 is skipped on 29 March and repeated on 25 October.
    // The start takes the later instant the wall time can name, the end the earlier one.
    let zone = "W. Europe Standard Time";
    let gap = reply(&scheduled(
        "2026-03-29T02:30:00.0000000",
        "2026-03-29T02:30:00.0000000",
        zone,
    ))
    .unwrap();
    // Both ends name the same skipped time: the window holds no instant, so no message gets a
    // reply, rather than one an hour long.
    assert!(gap.window.is_empty());
    let read = reply(&scheduled(
        "2026-03-29T02:30:00.0000000",
        "2026-10-25T02:30:00.0000000",
        zone,
    ))
    .unwrap();
    assert_eq!(read.window.start, Some(at("2026-03-29T01:30:00Z")));
    assert_eq!(read.window.end, Some(at("2026-10-25T00:30:00Z")));
    let fold = reply(&scheduled(
        "2026-10-25T02:30:00.0000000",
        "2026-10-26T00:00:00.0000000",
        zone,
    ))
    .unwrap();
    assert_eq!(fold.window.start, Some(at("2026-10-25T01:30:00Z")));
}

#[test]
fn a_schedule_in_a_zone_the_engine_cannot_resolve_is_refused_not_guessed() {
    let err = reply(&scheduled(
        "2026-07-01T09:00:00.0000000",
        "2026-07-02T09:00:00.0000000",
        "Mars Standard Time",
    ))
    .unwrap_err();
    assert!(err.is_item_unavailable(), "{err}");
    assert_eq!(err.class(), FailureClass::Permanent);
    // A reply that is not scheduled does not read its schedule at all.
    let mut off = scheduled("x", "y", "Mars Standard Time");
    off["status"] = json!("disabled");
    assert!(reply(&off).is_ok());
}

#[test]
fn an_external_message_the_model_does_not_hold_is_not_sent() {
    // Nobody outside gets a reply, and the model holds no outside message: the one stored stays.
    let inside_only = VacationResponse::default()
        .turned_on()
        .with_reply(AutoReply::default().with_html("<p>in</p>"))
        .with_audience(ReplyAudience::Organization);
    let written = setting(&inside_only);
    assert_eq!(written["externalAudience"], "none");
    assert!(written.get("externalReplyMessage").is_none(), "{written}");
}
