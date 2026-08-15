//! Writing a message's row: the whole-object upsert, and the narrower writes that move only
//! part of it.
//!
//! Split from the generic derived-row machinery in [`super`] because these are the mail table's
//! own shape, and because a message row is written three ways — a whole object, a state-only
//! change, a thread assignment — that must agree on their columns.

use engine_core::{
    ids::{MessageIdHeader, ThreadId},
    search_index::{MailRow, MailStateRow, MembershipKind},
    time::UtcDateTime,
    version::{ChangeKey, ETag, RevisionTokens},
};
use engine_store::Result;
use rusqlite::Transaction;

use crate::{convert, sql};

/// Upserts one message row. Shared by the apply path and the v9 backfill, so a migrated store and
/// a freshly synced one hold the same columns.
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

/// Writes a message row's revision tokens and modification time.
///
/// Shared by the state-only apply, the whole-object upsert and the v11 backfill, so a migrated
/// store and a freshly synced one hold the same columns. `schedule_tag` has no column: it is
/// CalDAV scheduling state, which a message can never carry.
fn write_message_state(
    tx: &Transaction<'_>,
    scope_key: &str,
    provider_key: &str,
    revisions: &RevisionTokens,
    last_modified: Option<UtcDateTime>,
) -> Result<()> {
    sql::execute(
        tx,
        "UPDATE message SET last_modified = ?3, etag = ?4, change_key = ?5, mod_seq = ?6
         WHERE scope_key = ?1 AND provider_key = ?2",
        rusqlite::params![
            scope_key,
            provider_key,
            last_modified.map(convert::instant_to_text),
            revisions.etag.as_ref().map(ETag::as_str),
            revisions.change_key.as_ref().map(ChangeKey::as_str),
            revisions
                .mod_seq
                .as_ref()
                .and_then(|m| i64::try_from(m.get()).ok()),
        ],
    )?;
    Ok(())
}

/// Writes one state-only change: the message row's state columns, and that message's
/// `keyword`-kind memberships.
///
/// Deliberately an `UPDATE`, not an upsert — a keyword change carries no subject, sender or
/// date, so an insert would file a blank row for a message the store has never seen. A change
/// for an unknown key is a no-op: the message is out of the synced window, and the pass that
/// admits it will bring its keywords with it.
///
/// The membership replace is scoped to `kind = 'keyword'`, so the message keeps the mailbox
/// memberships that decide which folders it appears in. Clearing every kind here — the shape
/// [`replace_memberships`] uses, where the batch carries a whole projection — would drop a
/// message out of its folder on a mark-read.
pub(super) fn apply_state_change(
    tx: &Transaction<'_>,
    scope_key: &str,
    row: &MailStateRow,
) -> Result<()> {
    sql::execute(
        tx,
        "UPDATE message SET flags = ?3 WHERE scope_key = ?1 AND provider_key = ?2",
        (scope_key, row.key.as_str(), i64::from(row.flags.bits())),
    )?;
    write_message_state(
        tx,
        scope_key,
        row.key.as_str(),
        &row.revisions,
        row.last_modified,
    )?;
    sql::execute(
        tx,
        "DELETE FROM membership WHERE scope_key = ?1 AND provider_key = ?2 AND kind = ?3",
        (
            scope_key,
            row.key.as_str(),
            convert::membership_kind_text(MembershipKind::Keyword),
        ),
    )?;
    for keyword in &row.keywords {
        sql::execute(
            tx,
            "INSERT INTO membership (scope_key, provider_key, kind, value)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(scope_key, provider_key, kind, value) DO NOTHING",
            (
                scope_key,
                row.key.as_str(),
                convert::membership_kind_text(MembershipKind::Keyword),
                keyword.as_str(),
            ),
        )?;
    }
    Ok(())
}
