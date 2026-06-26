//! Offline tests for [`edit_mail`], driven over a scripted mock stream.

use super::edit_mail;
use crate::mock::{MockStream, script, written};
use crate::transport::Connection;
use engine_core::error::FailureClass;
use engine_core::ids::{MailboxId, ProviderKey};
use engine_core::mail::{Keyword, SystemKeyword};
use engine_provider::MailEdit;
use std::collections::BTreeSet;

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";
/// A `SELECT` whose `UIDVALIDITY` matches the keys below (`v7`).
const SELECT_V7: &str = "* 3 EXISTS\r\n* OK [UIDVALIDITY 7] v\r\na2 OK [READ-WRITE] done\r\n";

/// The provider key for INBOX UID 42 under UIDVALIDITY 7.
fn target() -> ProviderKey {
    ProviderKey::new("imap:v7:u42@INBOX").unwrap()
}

/// Opens a connection over `server`, consuming the greeting + login.
async fn logged_in(server: Vec<u8>) -> (Connection<MockStream>, crate::mock::Recorded) {
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    (conn, recorded)
}

#[tokio::test]
async fn mark_read_selects_then_stores_seen() {
    let server = script(&[GREETING, LOGIN_OK, SELECT_V7, "a3 OK STORE done\r\n"]);
    let (mut conn, recorded) = logged_in(server).await;

    let edit = MailEdit::mark_seen(target(), true);
    let receipt = edit_mail(&mut conn, &edit).await.unwrap();
    assert_eq!(receipt.message_key, target());

    let sent = written(&recorded);
    assert!(sent.contains("a2 SELECT \"INBOX\""), "{sent}");
    assert!(
        sent.contains("a3 UID STORE 42 +FLAGS.SILENT (\\Seen)"),
        "{sent}"
    );
}

#[tokio::test]
async fn mark_unread_stores_a_minus_seen() {
    let server = script(&[GREETING, LOGIN_OK, SELECT_V7, "a3 OK STORE done\r\n"]);
    let (mut conn, recorded) = logged_in(server).await;

    let edit = MailEdit::mark_seen(target(), false);
    edit_mail(&mut conn, &edit).await.unwrap();
    assert!(
        written(&recorded).contains("a3 UID STORE 42 -FLAGS.SILENT (\\Seen)"),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn flag_stores_a_plus_flagged() {
    let server = script(&[GREETING, LOGIN_OK, SELECT_V7, "a3 OK STORE done\r\n"]);
    let (mut conn, recorded) = logged_in(server).await;

    let edit = MailEdit::set_flagged(target(), true);
    edit_mail(&mut conn, &edit).await.unwrap();
    assert!(
        written(&recorded).contains("a3 UID STORE 42 +FLAGS.SILENT (\\Flagged)"),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn set_keywords_with_both_sides_issues_two_stores() {
    let server = script(&[
        GREETING,
        LOGIN_OK,
        SELECT_V7,
        "a3 OK STORE done\r\n",
        "a4 OK STORE done\r\n",
    ]);
    let (mut conn, recorded) = logged_in(server).await;

    let edit = MailEdit::SetKeywords {
        target: target(),
        add: BTreeSet::from([Keyword::system(SystemKeyword::Seen)]),
        remove: BTreeSet::from([Keyword::system(SystemKeyword::Flagged)]),
    };
    edit_mail(&mut conn, &edit).await.unwrap();

    let sent = written(&recorded);
    assert!(
        sent.contains("a3 UID STORE 42 +FLAGS.SILENT (\\Seen)"),
        "{sent}"
    );
    assert!(
        sent.contains("a4 UID STORE 42 -FLAGS.SILENT (\\Flagged)"),
        "{sent}"
    );
}

#[tokio::test]
async fn set_keywords_with_both_sides_empty_is_a_select_only_no_op() {
    // No STORE is issued; only the SELECT guard runs, and a receipt resolves the op.
    let server = script(&[GREETING, LOGIN_OK, SELECT_V7]);
    let (mut conn, recorded) = logged_in(server).await;

    let edit = MailEdit::SetKeywords {
        target: target(),
        add: BTreeSet::new(),
        remove: BTreeSet::new(),
    };
    let receipt = edit_mail(&mut conn, &edit).await.unwrap();
    assert_eq!(receipt.message_key, target());
    assert!(
        !written(&recorded).contains("UID STORE"),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn move_issues_a_quoted_uid_move() {
    let server = script(&[GREETING, LOGIN_OK, SELECT_V7, "a3 OK MOVE done\r\n"]);
    let (mut conn, recorded) = logged_in(server).await;

    let edit = MailEdit::move_to(target(), MailboxId::try_from("Archive").unwrap());
    let receipt = edit_mail(&mut conn, &edit).await.unwrap();
    // The receipt carries the source key (the destination copy reconciles on sync).
    assert_eq!(receipt.message_key, target());
    assert!(
        written(&recorded).contains("a3 UID MOVE 42 \"Archive\""),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn delete_stores_deleted_then_uid_expunges() {
    let server = script(&[
        GREETING,
        LOGIN_OK,
        SELECT_V7,
        "a3 OK STORE done\r\n",
        "a4 OK EXPUNGE done\r\n",
    ]);
    let (mut conn, recorded) = logged_in(server).await;

    let edit = MailEdit::delete(target());
    let receipt = edit_mail(&mut conn, &edit).await.unwrap();
    assert_eq!(receipt.message_key, target());

    let sent = written(&recorded);
    assert!(
        sent.contains("a3 UID STORE 42 +FLAGS.SILENT (\\Deleted)"),
        "{sent}"
    );
    assert!(sent.contains("a4 UID EXPUNGE 42"), "{sent}");
}

#[tokio::test]
async fn uidvalidity_mismatch_is_a_conflict() {
    // The mailbox now reports UIDVALIDITY 99, but the key was synthesized under 7:
    // every prior key is stale, so the edit is a Conflict (re-sync, then retry) —
    // never a blind write against a renumbered UID space.
    let select_v99 = "* 3 EXISTS\r\n* OK [UIDVALIDITY 99] v\r\na2 OK [READ-WRITE] done\r\n";
    let server = script(&[GREETING, LOGIN_OK, select_v99]);
    let (mut conn, recorded) = logged_in(server).await;

    let edit = MailEdit::mark_seen(target(), true);
    let err = edit_mail(&mut conn, &edit).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
    // No STORE is attempted once the guard fails.
    assert!(
        !written(&recorded).contains("UID STORE"),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn an_unparseable_target_key_is_invalid_state() {
    // A foreign/garbage key never reaches the wire — it is rejected before SELECT.
    let server = script(&[GREETING, LOGIN_OK]);
    let (mut conn, recorded) = logged_in(server).await;

    let edit = MailEdit::mark_seen(ProviderKey::new("jmap:Mxyz").unwrap(), true);
    let err = edit_mail(&mut conn, &edit).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
    assert!(
        !written(&recorded).contains("SELECT"),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn a_move_destination_with_crlf_is_rejected_before_the_wire() {
    // A mailbox id is only validated non-empty, so a destination could carry CR/LF;
    // since `quote` escapes only "/\, admitting it would inject a second command.
    // The edit must be rejected before any IMAP command reaches the wire.
    let server = script(&[GREETING, LOGIN_OK, SELECT_V7]);
    let (mut conn, recorded) = logged_in(server).await;

    let evil = MailboxId::try_from("Archive\r\na9 DELETE INBOX").unwrap();
    let edit = MailEdit::move_to(target(), evil);
    let err = edit_mail(&mut conn, &edit).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
    // The SELECT may run, but no UID MOVE (and certainly no injected command) is sent.
    assert!(
        !written(&recorded).contains("UID MOVE") && !written(&recorded).contains("DELETE"),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn a_custom_keyword_passes_through_as_a_bare_atom() {
    let server = script(&[GREETING, LOGIN_OK, SELECT_V7, "a3 OK STORE done\r\n"]);
    let (mut conn, recorded) = logged_in(server).await;

    let edit = MailEdit::SetKeywords {
        target: target(),
        add: BTreeSet::from([Keyword::new("project-x").unwrap()]),
        remove: BTreeSet::new(),
    };
    edit_mail(&mut conn, &edit).await.unwrap();
    assert!(
        written(&recorded).contains("a3 UID STORE 42 +FLAGS.SILENT (project-x)"),
        "{}",
        written(&recorded)
    );
}
