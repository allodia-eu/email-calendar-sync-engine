//! The reply import: where the target holds none, again for nothing, never over another, and a
//! reply compared as the target holds it.

use engine_core::{
    time::UtcDateTime,
    vacation::{AutoReply, ReplyAudience, VacationWindow},
    version::VacationVersion,
};

use super::*;
use crate::import::{
    RulesImporter,
    rules::tests::{Target, account},
};

fn away() -> VacationResponse {
    VacationResponse::default().turned_on().with_reply(
        AutoReply::default()
            .with_subject("Away")
            .with_text("Back soon"),
    )
}

#[tokio::test]
async fn a_reply_is_imported_where_the_target_holds_none() {
    let target = Target::with_own(Vec::new());
    let receipt = target.import_vacation(&account(), &away()).await.unwrap();
    assert_eq!(receipt.version, Some(VacationVersion::new("v2")));
    let replies = target.replies.lock().unwrap().clone();
    assert_eq!(replies, [(Some(VacationVersion::new("v1")), away())]);
}

#[tokio::test]
async fn the_same_reply_again_writes_nothing() {
    let mut target = Target::with_own(Vec::new());
    target.reply = away().with_version(VacationVersion::new("v7"));
    let receipt = target.import_vacation(&account(), &away()).await.unwrap();
    assert_eq!(receipt.version, Some(VacationVersion::new("v7")));
    assert_eq!(receipt.rules, RulesEffect::Untouched);
    assert!(target.replies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_reply_of_the_targets_own_is_never_replaced() {
    // On, or off with words of its own: either is the target's.
    for held in [
        away().with_reply(AutoReply::default().with_text("Other")),
        away().turned_off(),
        VacationResponse::default().with_window(
            VacationWindow::default()
                .with_start(UtcDateTime::parse_rfc3339("2026-01-01T00:00:00Z").unwrap()),
        ),
        VacationResponse::default().with_audience(ReplyAudience::Contacts),
    ] {
        let mut target = Target::with_own(Vec::new());
        target.reply = held;
        let err = target
            .import_vacation(&account(), &away())
            .await
            .unwrap_err();
        assert!(err.is_item_unavailable(), "{err:?}");
        assert!(target.replies.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_reply_with_a_part_the_target_cannot_hold_is_refused_whole() {
    let target = Target::with_own(Vec::new());
    let html = away().with_outside_reply(AutoReply::default().with_text("Outside"));
    let err = target.import_vacation(&account(), &html).await.unwrap_err();
    assert!(err.is_item_unavailable());
    assert!(target.replies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_reply_reads_as_the_one_imported_whatever_its_line_ends_and_its_window_fraction() {
    // As a Sieve target holds it: CRLF line ends, one closing the text, the window to whole
    // seconds inward.
    let at = |text: &str| UtcDateTime::parse_rfc3339(text).unwrap();
    let imported = VacationResponse::default()
        .turned_on()
        .with_window(
            VacationWindow::default()
                .with_start(at("2030-01-02T09:00:00.250Z"))
                .with_end(at("2030-01-09T17:30:00.750Z")),
        )
        .with_reply(
            AutoReply::default()
                .with_subject("Away")
                .with_text("Line one\nLine two"),
        );
    let mut target = Target::with_own(Vec::new());
    target.reply = VacationResponse::default()
        .turned_on()
        .with_window(
            VacationWindow::default()
                .with_start(at("2030-01-02T09:00:01Z"))
                .with_end(at("2030-01-09T17:30:00Z")),
        )
        .with_reply(
            AutoReply::default()
                .with_subject("Away")
                .with_text("Line one\r\nLine two\r\n"),
        )
        .with_version(VacationVersion::new("v9"));
    let receipt = target.import_vacation(&account(), &imported).await.unwrap();
    assert_eq!(receipt.version, Some(VacationVersion::new("v9")));
    assert!(target.replies.lock().unwrap().is_empty(), "nothing written");

    // A different line is a different reply.
    target.reply.reply = AutoReply::default()
        .with_subject("Away")
        .with_text("Line one\r\nLine three\r\n");
    let err = target
        .import_vacation(&account(), &imported)
        .await
        .unwrap_err();
    assert!(err.is_item_unavailable());
}
