//! The email streaming primitive: a pull [`Stream`] of [`EmailChunk`]s.
//!
//! This is the shape every mail adapter produces for a sync pass. It **decouples
//! two knobs a single page size used to conflate** (`store-and-sync.md`):
//!
//! - **fetch batching** — how many objects the adapter pulls per network round trip (an IMAP `UID
//!   FETCH` window, a JMAP `Email/get` page, a Graph `$top`);
//! - **streaming granularity** — how many objects it emits per [`EmailChunk`], the unit the
//!   orchestrator commits and reports.
//!
//! A large batch with a small chunk gives *both* few round trips *and*
//! row-as-it-arrives commits: an IMAP adapter parses `FETCH` responses off the wire
//! and yields a chunk every `chunk_size` messages **within** one batched fetch, so a
//! host surfaces mail before the whole batch has downloaded.
//!
//! Each chunk also carries how the orchestrator must apply it — a
//! [`PassMode`] (additive-and-resumable versus reconciling) and an optional
//! [`advance_to`](EmailChunk::advance_to) checkpoint cursor — so a killed cold sync
//! resumes from where it stopped rather than restarting (`store-and-sync.md`).

use std::collections::BTreeSet;

use engine_core::{
    ids::ProviderKey,
    mail::Message,
    sync::{SyncState, SyncUpdate},
};
use futures_core::stream::Stream;

use crate::{ProviderError, ProviderResult, ScopeSync};

/// How the orchestrator must apply a pass's chunks — set by the adapter, constant
/// across every chunk of one pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassMode {
    /// Apply each chunk **additively** (upserts plus explicit `removed` keys) and
    /// advance the cursor to each chunk's [`advance_to`](EmailChunk::advance_to)
    /// checkpoint. Never tombstones.
    ///
    /// The first (cold) sync — nothing is stored yet, so there is nothing to
    /// tombstone — and every steady-state delta. Because the cursor advances
    /// mid-pass, a crash resumes from the last committed checkpoint instead of
    /// re-downloading from the start.
    Additive,
    /// Apply each chunk additively but **hold** the cursor, accumulating the
    /// `present` id set across chunks; the final chunk (the one carrying
    /// [`advance_to`](EmailChunk::advance_to)) tombstones every local row absent
    /// from that accumulated set and advances the cursor in one commit.
    ///
    /// A reconciling re-snapshot — a `UIDVALIDITY` reset, a JMAP
    /// `cannotCalculateChanges`, or a non-QRESYNC flag/expunge reconcile — where
    /// local rows exist and must be reconciled against the server's current set. It
    /// is not checkpoint-resumable (a crash re-runs the pass), which is acceptable
    /// because it is the rare path.
    Reconcile,
}

impl PassMode {
    /// Whether a pass in this mode tombstones absent rows at its end.
    #[must_use]
    pub fn tombstones(self) -> bool {
        matches!(self, Self::Reconcile)
    }
}

/// One incrementally-delivered slice of an email sync pass.
///
/// Small enough to commit and surface on its own (the streaming granularity). All
/// chunks of one pass share [`mode`](EmailChunk::mode) and [`total`](EmailChunk::total);
/// [`advance_to`](EmailChunk::advance_to) marks the cursor to persist.
///
/// (`Message` is `PartialEq` but not `Eq`, so neither is this.)
#[derive(Debug, Clone, PartialEq)]
pub struct EmailChunk {
    /// How the orchestrator applies this pass (constant across chunks).
    pub mode: PassMode,
    /// Messages created or updated in this chunk (upserts).
    pub changed: Vec<Message>,
    /// Keys explicitly removed in this chunk — a delta's destroyed ids, or a
    /// QRESYNC `VANISHED` set. Applied inline in either mode; empty when removals
    /// come only from [`PassMode::Reconcile`] tombstoning.
    pub removed: Vec<ProviderKey>,
    /// For [`PassMode::Reconcile`], the ids **this chunk covers**, accumulated by
    /// the orchestrator to drive end-of-pass tombstoning. Empty in
    /// [`PassMode::Additive`].
    pub present: Vec<ProviderKey>,
    /// The total objects in the pass, if the adapter can compute it (the progress
    /// denominator). Stable across chunks; `None` when unknown (a typical delta).
    pub total: Option<usize>,
    /// The cursor to persist after applying this chunk, or `None` to hold it:
    ///
    /// - [`PassMode::Additive`]: `Some(checkpoint)` on **every** chunk so a crash resumes from
    ///   here; the last chunk's checkpoint is the pass's final cursor.
    /// - [`PassMode::Reconcile`]: `None` on every chunk except the **last**, which carries the
    ///   final cursor and triggers tombstoning.
    pub advance_to: Option<SyncState>,
}

impl EmailChunk {
    /// An additive chunk (cold backfill or delta): upserts + explicit removals,
    /// checkpointing the cursor to `checkpoint`.
    #[must_use]
    pub fn additive(
        changed: Vec<Message>,
        removed: Vec<ProviderKey>,
        total: Option<usize>,
        checkpoint: SyncState,
    ) -> Self {
        Self {
            mode: PassMode::Additive,
            changed,
            removed,
            present: Vec::new(),
            total,
            advance_to: Some(checkpoint),
        }
    }

    /// An additive chunk that **holds** the cursor (upserts + explicit removals, no
    /// checkpoint) — for an adapter whose backfill is not cheaply resumable
    /// mid-pass (JMAP/Graph, fast HTTP paging), so intermediate chunks are visible
    /// but a final marker chunk carries the cursor. A crash re-runs the pass.
    #[must_use]
    pub fn additive_held(
        changed: Vec<Message>,
        removed: Vec<ProviderKey>,
        total: Option<usize>,
    ) -> Self {
        Self {
            mode: PassMode::Additive,
            changed,
            removed,
            present: Vec::new(),
            total,
            advance_to: None,
        }
    }

    /// An intermediate reconcile chunk: upserts, carrying the `present` ids it
    /// covers, holding the cursor (the final chunk tombstones and advances).
    #[must_use]
    pub fn reconcile_page(
        changed: Vec<Message>,
        present: Vec<ProviderKey>,
        total: Option<usize>,
    ) -> Self {
        Self {
            mode: PassMode::Reconcile,
            changed,
            removed: Vec::new(),
            present,
            total,
            advance_to: None,
        }
    }

    /// The final reconcile chunk: upserts, its `present` ids, and the cursor to
    /// advance to — the orchestrator tombstones absent rows against the accumulated
    /// present set on this commit.
    #[must_use]
    pub fn reconcile_last(
        changed: Vec<Message>,
        present: Vec<ProviderKey>,
        total: Option<usize>,
        cursor: SyncState,
    ) -> Self {
        Self {
            mode: PassMode::Reconcile,
            changed,
            present,
            removed: Vec::new(),
            total,
            advance_to: Some(cursor),
        }
    }

    /// Whether this is the final chunk of a [`PassMode::Reconcile`] pass (the one
    /// that tombstones). Always `false` in [`PassMode::Additive`], where the stream
    /// simply ends after the last checkpointed chunk.
    #[must_use]
    pub fn is_reconcile_final(&self) -> bool {
        self.mode == PassMode::Reconcile && self.advance_to.is_some()
    }
}

/// Splits one **fully-fetched** page into intermediate content chunks of at most
/// `chunk_size` messages each (`0` = a single chunk) — for an adapter that fetches a
/// page whole over HTTP and re-chunks it for incremental commit (JMAP/Graph, where
/// the round trip is atomic so there is no wire-level streaming to exploit).
///
/// Every returned chunk **holds** the cursor (`advance_to == None`); the caller emits
/// a final marker chunk carrying the cursor after the last page. Metadata rides on
/// the **first** sub-chunk so the orchestrator accumulates it exactly once: the
/// page's `removed` keys and (for `Reconcile`) its `present` ids ride there. `total`
/// (constant across the pass) rides on every chunk so a determinate progress bar
/// shows from the first commit.
#[must_use]
pub fn split_page(
    mode: PassMode,
    changed: Vec<Message>,
    removed: Vec<ProviderKey>,
    present: Vec<ProviderKey>,
    total: Option<usize>,
    chunk_size: usize,
) -> Vec<EmailChunk> {
    let make =
        |batch: Vec<Message>, removed: Vec<ProviderKey>, present: Vec<ProviderKey>| match mode {
            PassMode::Additive => EmailChunk::additive_held(batch, removed, total),
            PassMode::Reconcile => EmailChunk {
                mode: PassMode::Reconcile,
                changed: batch,
                removed,
                present,
                total,
                advance_to: None,
            },
        };
    if changed.is_empty() {
        // A page with only removals/present (e.g. an empty-arrivals delta that still
        // destroyed keys) still needs one chunk to carry them.
        return if removed.is_empty() && present.is_empty() {
            Vec::new()
        } else {
            vec![make(Vec::new(), removed, present)]
        };
    }
    let step = if chunk_size == 0 {
        changed.len()
    } else {
        chunk_size
    };
    let mut chunks = Vec::new();
    let mut removed = Some(removed);
    let mut present = Some(present);
    let mut iter = changed.into_iter().peekable();
    while iter.peek().is_some() {
        let batch: Vec<Message> = iter.by_ref().take(step.max(1)).collect();
        // The metadata rides on the first sub-chunk only.
        chunks.push(make(
            batch,
            removed.take().unwrap_or_default(),
            present.take().unwrap_or_default(),
        ));
    }
    chunks
}

/// A boxed pull stream of one email pass's chunks — what
/// [`Provider::stream_email`](crate::Provider::stream_email) returns.
///
/// Pull-based, so the adapter's fetch advances only when the orchestrator polls for
/// the next chunk (after committing the previous one): natural backpressure, no
/// unbounded buffering. Each item is a `Result`, so a mid-pass fetch error surfaces
/// as one `Err` and ends the stream. The `'a` lifetime ties the stream to the
/// borrowed adapter and arguments (an IMAP adapter holds its connection guard for
/// the stream's life).
pub type EmailStream<'a> =
    core::pin::Pin<Box<dyn Stream<Item = crate::ProviderResult<EmailChunk>> + Send + 'a>>;

/// Drains an [`EmailStream`] into one combined [`ScopeSync`] — the whole-scope convenience
/// behind [`Provider::sync_email`](crate::Provider::sync_email)'s default.
///
/// It lives here rather than inline in the trait because it is an *implementation* of the
/// streaming contract this module defines, not part of the seam adapters implement.
///
/// # Errors
///
/// The first chunk error, or [`ProviderError::invalid_state`] if the stream ended without a
/// final cursor to advance to.
pub(crate) async fn drain_email(mut stream: EmailStream<'_>) -> ProviderResult<ScopeSync<Message>> {
    use futures_util::StreamExt;

    let mut changed = Vec::new();
    let mut removed = Vec::new();
    let mut present = BTreeSet::new();
    let mut mode = PassMode::Additive;
    let mut next_cursor: Option<SyncState> = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        mode = chunk.mode;
        changed.extend(chunk.changed);
        removed.extend(chunk.removed);
        present.extend(chunk.present);
        if let Some(cursor) = chunk.advance_to {
            next_cursor = Some(cursor);
        }
    }
    let next_cursor = next_cursor
        .ok_or_else(|| ProviderError::invalid_state("email stream ended without a final cursor"))?;
    // A reconcile pass tombstones against the accumulated present set; an additive pass (cold
    // backfill or delta) carries only explicit removals. For a first sync both are equivalent
    // (nothing local to tombstone).
    let update = match mode {
        PassMode::Reconcile => SyncUpdate::snapshot(changed, present),
        PassMode::Additive => SyncUpdate::delta(changed, removed),
    };
    Ok(ScopeSync::new(update, next_cursor))
}

#[cfg(test)]
mod tests {
    use engine_core::{
        ids::{MailboxId, MessageId},
        mail::Message,
        membership::Memberships,
    };

    use super::*;

    fn message(id: &str) -> Message {
        Message::new(
            MessageId::try_from(id).unwrap(),
            Memberships::of_one(MailboxId::try_from("a").unwrap()),
        )
    }

    fn key(value: &str) -> ProviderKey {
        ProviderKey::new(value).unwrap()
    }

    #[test]
    fn additive_chunk_checkpoints_and_never_tombstones() {
        let chunk = EmailChunk::additive(
            vec![message("m1")],
            vec![key("gone")],
            Some(10),
            SyncState::new("v1;n5;b3"),
        );
        assert_eq!(chunk.mode, PassMode::Additive);
        assert!(!chunk.mode.tombstones());
        assert!(!chunk.is_reconcile_final());
        assert_eq!(chunk.advance_to, Some(SyncState::new("v1;n5;b3")));
        assert!(chunk.present.is_empty());
    }

    #[test]
    fn reconcile_pages_hold_then_tombstone_on_the_last() {
        let intermediate =
            EmailChunk::reconcile_page(vec![message("m1")], vec![key("m1")], Some(2));
        assert_eq!(intermediate.mode, PassMode::Reconcile);
        assert!(intermediate.mode.tombstones());
        assert!(intermediate.advance_to.is_none());
        assert!(!intermediate.is_reconcile_final());

        let last = EmailChunk::reconcile_last(
            vec![message("m2")],
            vec![key("m2")],
            Some(2),
            SyncState::new("v1;n9"),
        );
        assert!(last.is_reconcile_final());
        assert_eq!(last.advance_to, Some(SyncState::new("v1;n9")));
    }
}
