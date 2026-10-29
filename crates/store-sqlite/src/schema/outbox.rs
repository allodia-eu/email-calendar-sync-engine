//! Schema steps for the outbox's own tables. v20, the payload beside the row.

/// Migration v20: an op's payload moves out of `pending_op` into a table of its own, kept only
/// while something may still dispatch the op.
///
/// SQLite rewrites a whole record on any `UPDATE` of it, so a payload kept in the row is written
/// again by every claim, hand-over, lease renewal and outcome. A draft carries its attachments:
/// in the row, a claim and a parked failure of a 2 MB one write about 4 MB. Beside it, the payload
/// is written once, when the op is queued.
///
/// A settled row stays, because it is the idempotency record, but nothing reads its payload again:
/// a claim takes only a `Pending` op, and the queue read lists only what is outstanding. So the
/// payload goes when the op reaches a state no verb leaves for a dispatch: the trigger's `WHEN` is
/// the negation of `engine_store::stays_listed`, the one rule for both. A send that settled
/// `Failed` keeps its payload, because the user may send it again; a send awaiting confirmation
/// keeps it, because "not delivered" queues it again. Kept for ever, settled payloads are most of
/// a store that saves drafts with attachments.
///
/// The fill moves the payloads the same rule keeps. Dropping the column rewrites `pending_op`
/// once; the pages it frees are reused before the file grows, and given back at the next
/// `vacuum`.
pub(crate) const V20: &str = "\
CREATE TABLE pending_op_payload (
    op_id   INTEGER PRIMARY KEY REFERENCES pending_op (id) ON DELETE CASCADE,
    payload TEXT    NOT NULL
) STRICT;
INSERT INTO pending_op_payload (op_id, payload)
    SELECT id, payload FROM pending_op
     WHERE state IN ('Pending', 'InFlight', 'NeedsConfirmation')
        OR (state = 'Failed' AND kind IS 'MailSubmit');
ALTER TABLE pending_op DROP COLUMN payload;
CREATE TRIGGER pending_op_settled AFTER UPDATE OF state ON pending_op
WHEN NOT (NEW.state IN ('Pending', 'InFlight', 'NeedsConfirmation')
          OR (NEW.state = 'Failed' AND NEW.kind IS 'MailSubmit'))
BEGIN
    DELETE FROM pending_op_payload WHERE op_id = NEW.id;
END;
";
