//! Writing a message's row: the whole-object upsert, and the narrower writes that move only
//! part of it.
//!
//! Split from the generic derived-row machinery in [`super`] because these are the mail table's
//! own shape, and because a message row is written three ways — a whole object, a state-only
//! change, a thread assignment — that must agree on their columns. All three live here, so that
//! agreement is visible in one file rather than asserted across two.

use engine_core::{
    ids::{MessageIdHeader, ThreadId},
    search_index::{MailRow, MailStateRow, MailThreadRow, MembershipKind},
    version::{ChangeKey, ETag},
};
use engine_store::Result;
use rusqlite::Transaction;

use crate::{convert, sql};

/// Upserts one message row: everything a whole object knows, both halves in one statement.
///
/// The provider sent the object, so it is authoritative about the content columns *and* the state
/// ones. A state-only change writes the narrower [`apply_state_change`] instead.
pub(crate) fn upsert_message(
    tx: &Transaction<'_>,
    scope_key: &str,
    account: &str,
    row: &MailRow,
) -> Result<()> {
    sql::execute(
        tx,
        "INSERT INTO message (scope_key, provider_key, account, thread_id, message_id, date_utc,
                              flags, has_attachment, from_name, from_addr, subject, preview,
                              last_modified, etag, change_key, mod_seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT(scope_key, provider_key) DO UPDATE SET
             account = excluded.account,
             thread_id = excluded.thread_id,
             message_id = excluded.message_id,
             date_utc = excluded.date_utc,
             flags = excluded.flags,
             has_attachment = excluded.has_attachment,
             from_name = excluded.from_name,
             from_addr = excluded.from_addr,
             subject = excluded.subject,
             preview = excluded.preview,
             last_modified = excluded.last_modified,
             etag = excluded.etag,
             change_key = excluded.change_key,
             mod_seq = excluded.mod_seq",
        rusqlite::params![
            scope_key,
            row.key.as_str(),
            account,
            row.thread_id.as_ref().map(ThreadId::as_str),
            row.message_id.as_ref().map(MessageIdHeader::as_str),
            row.date_utc.map(convert::instant_to_text),
            i64::from(row.flags.bits()),
            i64::from(row.has_attachment),
            row.from_name.as_deref(),
            row.from_addr.as_deref(),
            row.subject.as_deref(),
            row.preview.as_deref(),
            row.last_modified.map(convert::instant_to_text),
            row.revisions.etag.as_ref().map(ETag::as_str),
            row.revisions.change_key.as_ref().map(ChangeKey::as_str),
            row.revisions
                .mod_seq
                .as_ref()
                .and_then(|m| i64::try_from(m.get()).ok()),
        ],
    )?;
    Ok(())
}

/// Writes one thread assignment: the message row's `thread_id`, and nothing else.
///
/// An `UPDATE` for the same reason as a state change: an assignment names a thread, not a
/// message, so it cannot file a row for one the store does not hold. The derivation pass reads
/// payloads to rebuild the reference graph, and rewriting those would carry every other column
/// along with the one column it decided.
pub(super) fn assign_thread(
    tx: &Transaction<'_>,
    scope_key: &str,
    row: &MailThreadRow,
) -> Result<()> {
    sql::execute(
        tx,
        "UPDATE message SET thread_id = ?3 WHERE scope_key = ?1 AND provider_key = ?2",
        (scope_key, row.key.as_str(), row.thread_id.as_str()),
    )?;
    Ok(())
}

/// Writes one state-only change: the message row's state columns, that message's
/// `keyword`-kind memberships, and — only when the provider files in place — its
/// `mailbox`-kind ones.
///
/// Deliberately an `UPDATE`, not an upsert — a state change carries no subject, sender or
/// date, so an insert would file a blank row for a message the store has never seen. A change
/// for an unknown key is a no-op: the message is out of the synced window, and the pass that
/// admits it will bring its state with it.
///
/// Each membership replace is scoped to its own `kind`, which is what makes a partial write
/// safe. Clearing every kind here — the shape [`replace_memberships`] uses, where the batch
/// carries a whole projection — would drop a message out of its folder on a mark-read.
///
/// **Filing is written only when `mailboxes` is `Some`.** `None` is not "no mailboxes"; it means
/// the provider files through identity (an IMAP move mints a new UID, a Graph move a new id), so
/// it has nothing to say about this axis and the rows it would otherwise clear are the only
/// record of which folder the message is in.
///
/// `schedule_tag` has no column: it is CalDAV scheduling state, which a message can never carry.
pub(super) fn apply_state_change(
    tx: &Transaction<'_>,
    scope_key: &str,
    row: &MailStateRow,
) -> Result<()> {
    // One statement: the flags and the revision tokens are the same message's state, and they
    // move together whenever a provider reports one.
    sql::execute(
        tx,
        "UPDATE message
            SET flags = ?3, last_modified = ?4, etag = ?5, change_key = ?6, mod_seq = ?7
          WHERE scope_key = ?1 AND provider_key = ?2",
        rusqlite::params![
            scope_key,
            row.key.as_str(),
            i64::from(row.flags.bits()),
            row.last_modified.map(convert::instant_to_text),
            row.revisions.etag.as_ref().map(ETag::as_str),
            row.revisions.change_key.as_ref().map(ChangeKey::as_str),
            row.revisions
                .mod_seq
                .as_ref()
                .and_then(|m| i64::try_from(m.get()).ok()),
        ],
    )?;
    replace_kind(
        tx,
        scope_key,
        row.key.as_str(),
        MembershipKind::Keyword,
        &row.keywords,
    )?;
    if let Some(mailboxes) = &row.mailboxes {
        replace_kind(
            tx,
            scope_key,
            row.key.as_str(),
            MembershipKind::Mailbox,
            mailboxes,
        )?;
    }
    Ok(())
}

/// Replaces one message's memberships **of a single kind**, leaving every other kind standing.
fn replace_kind(
    tx: &Transaction<'_>,
    scope_key: &str,
    provider_key: &str,
    kind: MembershipKind,
    values: &[String],
) -> Result<()> {
    let kind = convert::membership_kind_text(kind);
    sql::execute(
        tx,
        "DELETE FROM membership WHERE scope_key = ?1 AND provider_key = ?2 AND kind = ?3",
        (scope_key, provider_key, kind),
    )?;
    for value in values {
        sql::execute(
            tx,
            "INSERT INTO membership (scope_key, provider_key, kind, value)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(scope_key, provider_key, kind, value) DO NOTHING",
            (scope_key, provider_key, kind, value.as_str()),
        )?;
    }
    Ok(())
}
