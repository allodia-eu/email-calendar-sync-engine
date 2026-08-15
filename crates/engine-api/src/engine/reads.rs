//! The read and search surface on `Engine`: per-account search, the mailbox/message
//! and calendar/event lists, and the windowed and thread-oriented message reads.

use std::collections::{HashMap, HashSet};

use engine_core::{
    calendar::{Calendar, Event},
    ids::{AccountId, ProviderKey, ThreadId},
    mail::{Mailbox, Message, ThreadRef},
    sync::{ObjectKind, SearchDomain, SyncScope},
    time::Horizon,
};
use engine_search::{CalendarQuery, MailQuery, SearchResults};
use engine_store::{MailListRow, MailSelector, MessageBodyStore, OccurrenceRow, StoreRead};
use serde_json::Value;

use super::decode_error;
use crate::{ApiError, Engine};

impl Engine {
    /// Searches one account's mail with the textual DSL (`from:a subject:"q report"
    /// before:2026-01-01`), returning ranked object keys and the answer's coverage.
    /// Runs over the account's mail scopes, enumerated from the store rather than
    /// hard-coded, so the facade stays provider-agnostic.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Query`] if `query` is malformed, or [`ApiError::Store`]
    /// on a backend failure.
    pub async fn search_mail(
        &self,
        account: &AccountId,
        query: &str,
        limit: usize,
    ) -> Result<SearchResults, ApiError> {
        let query = MailQuery::parse(query)?;
        let scopes = self.scopes_in(account, SearchDomain::Mail).await?;
        Ok(self.store.search_mail(&scopes, &query, limit).await?)
    }

    /// Searches one account's calendar events with the textual DSL (`calendar:work
    /// attendee:a@x after:2026-06-01`); `before:`/`after:` match the materialized
    /// occurrences, not just the master event (`calendar-semantics.md`).
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Query`] if `query` is malformed, or [`ApiError::Store`]
    /// on a backend failure.
    pub async fn search_calendar(
        &self,
        account: &AccountId,
        query: &str,
        limit: usize,
    ) -> Result<SearchResults, ApiError> {
        let query = CalendarQuery::parse(query)?;
        let scopes = self.scopes_in(account, SearchDomain::Calendar).await?;
        Ok(self.store.search_calendar(&scopes, &query, limit).await?)
    }

    /// Lists one account's mailboxes (folders/labels) — the synced mail collections
    /// across the account's mailbox scopes — for the host's folder sidebar.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn mailboxes(&self, account: &AccountId) -> Result<Vec<Mailbox>, ApiError> {
        let mut mailboxes = Vec::new();
        for payload in self.objects_of(account, ObjectKind::Mailbox).await? {
            mailboxes.push(serde_json::from_value(payload).map_err(|err| decode_error(&err))?);
        }
        Ok(mailboxes)
    }

    /// Lists one account's messages — the synced mail objects (envelope metadata;
    /// bodies are fetched on demand) across the account's mail scopes. For the message
    /// list; pair with [`Engine::search_mail`] for filtered or ranked views.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn messages(&self, account: &AccountId) -> Result<Vec<Message>, ApiError> {
        let mut messages = Vec::new();
        for payload in self.objects_of(account, ObjectKind::Message).await? {
            messages.push(serde_json::from_value(payload).map_err(|err| decode_error(&err))?);
        }
        self.compose(account, &mut messages).await?;
        Ok(messages)
    }

    /// Rebuilds each message's mutable state from its stored row.
    ///
    /// The stored payload is the message's **immutable half**
    /// ([`MailContent`](engine_core::mail::MailContent)) and carries none of its
    /// [`MailState`](engine_core::mail::MailState): not its keywords, not a derived thread, not
    /// the revision tokens that bump when that state moves. All of those change without the
    /// message's bytes changing, so their home is the `message` row and the membership junction.
    /// A `Message` decoded from a payload alone is therefore incomplete by construction, and this
    /// is what completes it.
    ///
    /// **Every path that turns stored mail back into a `Message` goes through here.** That is
    /// the guarantee: not that readers remember to refresh, but that a decoded payload is
    /// visibly missing the state until this has run.
    ///
    /// A message with no row — out of the synced window, or tombstoned between the two reads —
    /// keeps the empty state the payload gave it rather than being dropped, so a caller asking
    /// for specific keys still gets its content back.
    async fn compose(&self, account: &AccountId, messages: &mut [Message]) -> Result<(), ApiError> {
        if messages.is_empty() {
            return Ok(());
        }
        let keys: Vec<ProviderKey> = messages.iter().map(|m| m.id.key().clone()).collect();
        let rows: HashMap<ProviderKey, MailListRow> = self
            .store
            .list_mail(
                core::slice::from_ref(account),
                MailSelector::Keys(&keys),
                usize::MAX,
            )
            .await?
            .into_iter()
            .map(|row| (row.mail.key.clone(), row))
            .collect();
        for message in messages {
            let Some(row) = rows.get(message.id.key()) else {
                continue;
            };
            // A provider-assigned thread came back with the payload and stays as it is —
            // relabelling it derived would tell the derivation pass it may re-thread mail the
            // provider threaded. Only an absent one is filled from the row, and the row's can
            // only be the engine's own.
            if message.thread.is_none() {
                message.thread = row.mail.thread_id.clone().map(ThreadRef::derived);
            }
            message.keywords = row.keywords.iter().cloned().collect();
            message.revisions = row.mail.revisions.clone();
            message.last_modified = row.mail.last_modified;
        }
        Ok(())
    }

    /// The newest `limit` mail rows across `accounts`, newest first — **the read a mailbox list
    /// is built from**.
    ///
    /// It returns the projected [`MailListRow`], not the normalized object: sender, subject, date,
    /// flags, preview and folder membership, straight out of an ordered index. So a list costs the
    /// rows it shows rather than the mail it is drawn from, whether the account holds seven
    /// thousand messages or four hundred thousand. Reading a body or an attachment list still goes
    /// to the object, on demand, for the one message being opened.
    ///
    /// Several accounts in one call is the point: a unified inbox is one ordered answer, not one
    /// answer per account merged by the host. Messages with no known date sort last, entering the
    /// window only if dated ones do not fill it. Pair with [`Engine::mail_on_threads`] to pull a
    /// shown conversation's older members and [`Engine::mail_by_keys`] to resolve a specific
    /// message the window omits.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn mail_window(
        &self,
        accounts: &[AccountId],
        limit: usize,
    ) -> Result<Vec<MailListRow>, ApiError> {
        Ok(self
            .store
            .list_mail(accounts, MailSelector::Newest, limit)
            .await?)
    }

    /// The newest `limit` messages across `accounts` **without a cached body text** — the work
    /// list a host's background body-warming pass feeds through [`Engine::message_body`] so the
    /// synced window becomes readable (and searchable) offline. Ordered exactly like
    /// [`Engine::mail_window`].
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn mail_missing_body(
        &self,
        accounts: &[AccountId],
        limit: usize,
    ) -> Result<Vec<MailListRow>, ApiError> {
        Ok(self.store.mail_missing_body(accounts, limit).await?)
    }

    /// Every mail row on any of `threads` across `accounts` — **all** of each conversation's
    /// members regardless of any date window, so a windowed list can expand one into its full
    /// history (a years-old reply included).
    ///
    /// Resolved through the store's thread index, so the cost is the size of the conversations
    /// asked for and not the size of the mailbox they sit in. A malformed thread id is dropped
    /// rather than raised: it names no conversation, so it can only ever contribute nothing, and
    /// failing the whole read would turn one bad id into an empty list. Empty `threads` returns
    /// nothing without touching the store.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn mail_on_threads<'a>(
        &self,
        accounts: &[AccountId],
        threads: impl IntoIterator<Item = &'a str>,
    ) -> Result<Vec<MailListRow>, ApiError> {
        let threads = thread_ids(threads.into_iter());
        Ok(self
            .store
            .list_mail(accounts, MailSelector::Threads(&threads), usize::MAX)
            .await?)
    }

    /// The mail rows named by provider `keys` within an account — a targeted resolve for actions
    /// and search hits that reference messages a date window may not hold. Keys not found (moved,
    /// tombstoned) are simply absent.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn mail_by_keys(
        &self,
        account: &AccountId,
        keys: &[ProviderKey],
    ) -> Result<Vec<MailListRow>, ApiError> {
        Ok(self
            .store
            .list_mail(
                core::slice::from_ref(account),
                MailSelector::Keys(keys),
                usize::MAX,
            )
            .await?)
    }

    /// The messages named by provider `keys` within an account — a targeted resolve for
    /// actions and search hits that reference specific messages a date window may not hold,
    /// without loading the whole mailbox. Keys not found (moved, tombstoned) are simply absent;
    /// order is unspecified.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn messages_by_keys(
        &self,
        account: &AccountId,
        keys: &[ProviderKey],
    ) -> Result<Vec<Message>, ApiError> {
        let mut wanted: HashSet<ProviderKey> = keys.iter().cloned().collect();
        let mut messages = Vec::new();
        for scope in self.mail_scopes(account).await? {
            if wanted.is_empty() {
                break;
            }
            // A provider key is unique within an account and lives in one scope, so each
            // resolved key is dropped from `wanted` and never probed in a later scope.
            for key in wanted.iter().cloned().collect::<Vec<_>>() {
                if let Some(payload) = self.store.object_payload(&scope, &key).await? {
                    messages
                        .push(serde_json::from_value(payload).map_err(|err| decode_error(&err))?);
                    wanted.remove(&key);
                }
            }
        }
        self.compose(account, &mut messages).await?;
        Ok(messages)
    }

    /// The account's `Message`-kind sync scopes (its mail folders/labels), for the windowed and
    /// thread reads — mirrors [`Engine::objects_of`]'s scope filter without materializing any
    /// payloads.
    async fn mail_scopes(&self, account: &AccountId) -> Result<Vec<SyncScope>, ApiError> {
        Ok(self
            .store
            .account_scopes(account.clone())
            .await?
            .into_iter()
            .filter(|scope| scope.object_kind() == Some(ObjectKind::Message))
            .collect())
    }

    /// Lists one account's calendars (collections) — the synced calendar containers
    /// across the account's calendar scopes — for the host's calendar sidebar.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn calendars(&self, account: &AccountId) -> Result<Vec<Calendar>, ApiError> {
        let mut calendars = Vec::new();
        for payload in self.objects_of(account, ObjectKind::Calendar).await? {
            calendars.push(serde_json::from_value(payload).map_err(|err| decode_error(&err))?);
        }
        Ok(calendars)
    }

    /// One account's materialized occurrences overlapping `window`, ascending by
    /// `(start, end, event)` across every calendar the account syncs.
    ///
    /// **This is the read a calendar grid pages over, and [`Engine::events`] is not.**
    /// Recurrence materializes into occurrence rows, not the master event
    /// (`store-and-sync.md`), so a host that lays out `events()` renders a weekly
    /// meeting exactly once — at the series start. Each row points back at its master
    /// via [`OccurrenceRow::event`]; join it against `events()` for the title, calendar
    /// membership, and participants.
    ///
    /// Only what a [`sync_calendar`](Engine::sync_calendar) already expanded is here.
    /// Reading past the horizon it materialized returns *nothing*, and re-syncing does
    /// not backfill it (a delta with no changes derives no occurrences) — advance it
    /// with [`Engine::expand_horizon`] first, or the grid will confidently render an
    /// empty week.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn occurrences_in(
        &self,
        account: &AccountId,
        window: Horizon,
    ) -> Result<Vec<OccurrenceRow>, ApiError> {
        let mut occurrences = Vec::new();
        for scope in self.scopes_of(account, ObjectKind::Event).await? {
            occurrences.extend(self.store.scope_occurrences(&scope, window).await?);
        }
        // Each scope is sorted; the merge across an account's calendars is not.
        occurrences.sort_by(|a, b| {
            (a.start, a.end, &a.event, a.recurrence_id).cmp(&(
                b.start,
                b.end,
                &b.event,
                b.recurrence_id,
            ))
        });
        Ok(occurrences)
    }

    /// Lists one account's events — the synced calendar event objects (the projected
    /// envelope; recurrence materializes into occurrences in the store) across the
    /// account's calendar scopes. For the agenda/event list; pair with
    /// [`Engine::search_calendar`] for filtered or ranked views, or with
    /// [`Engine::occurrences_in`] to lay a recurring series out on a grid.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn events(&self, account: &AccountId) -> Result<Vec<Event>, ApiError> {
        let mut events = Vec::new();
        for payload in self.objects_of(account, ObjectKind::Event).await? {
            events.push(serde_json::from_value(payload).map_err(|err| decode_error(&err))?);
        }
        Ok(events)
    }

    /// The events named by provider `keys` within an account — a targeted resolve for the
    /// event-detail read and the grid's occurrence→master join, **without deserializing the
    /// whole calendar**. The calendar counterpart of [`Engine::messages_by_keys`], and the
    /// read to reach for whenever a caller wants a *known* handful of events rather than the
    /// account's entire event history: on a real diary [`Engine::events`] decodes every one
    /// of thousands of event payloads, where this decodes only the `keys` asked for.
    ///
    /// A provider key is unique within an account and lives in one calendar scope, so each
    /// resolved key is dropped from the wanted set and never probed in a later scope. Keys
    /// not found (moved, tombstoned) are simply absent; order is unspecified. Empty `keys`
    /// returns nothing without touching the store.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn events_by_keys(
        &self,
        account: &AccountId,
        keys: &[ProviderKey],
    ) -> Result<Vec<Event>, ApiError> {
        let mut wanted: HashSet<ProviderKey> = keys.iter().cloned().collect();
        let mut events = Vec::new();
        for scope in self.scopes_of(account, ObjectKind::Event).await? {
            if wanted.is_empty() {
                break;
            }
            for key in wanted.iter().cloned().collect::<Vec<_>>() {
                if let Some(payload) = self.store.object_payload(&scope, &key).await? {
                    events.push(serde_json::from_value(payload).map_err(|err| decode_error(&err))?);
                    wanted.remove(&key);
                }
            }
        }
        Ok(events)
    }

    /// The normalized payload of every object of `kind` across the account's scopes,
    /// enumerated and filtered by [`SyncScope::object_kind`] — so the facade never
    /// hard-codes or branches on which scopes a provider uses. One batch read per scope
    /// (no per-key round trip).
    async fn objects_of(
        &self,
        account: &AccountId,
        kind: ObjectKind,
    ) -> Result<Vec<Value>, ApiError> {
        let mut payloads = Vec::new();
        for scope in self.scopes_of(account, kind).await? {
            payloads.extend(
                self.store
                    .scope_objects(&scope)
                    .await?
                    .into_iter()
                    .map(|(_key, payload)| payload),
            );
        }
        Ok(payloads)
    }

    /// The account's scopes holding objects of `kind`, enumerated and filtered by
    /// [`SyncScope::object_kind`] — so the facade never hard-codes or branches on which
    /// scopes a provider uses (a calendar is one `DavCollection` per CalDAV collection,
    /// but a single JMAP `CalendarEvent` type).
    async fn scopes_of(
        &self,
        account: &AccountId,
        kind: ObjectKind,
    ) -> Result<Vec<SyncScope>, ApiError> {
        Ok(self
            .store
            .account_scopes(account.clone())
            .await?
            .into_iter()
            .filter(|scope| scope.object_kind() == Some(kind))
            .collect())
    }
}

/// Parses caller-supplied thread ids, dropping any that are not well-formed.
///
/// Hosts hold these as plain strings — they came out of a list row and go straight back in — so
/// validation belongs here rather than in every caller.
fn thread_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Vec<ThreadId> {
    let mut parsed: Vec<ThreadId> = ids.filter_map(|id| ThreadId::try_from(id).ok()).collect();
    parsed.sort();
    parsed.dedup();
    parsed
}
