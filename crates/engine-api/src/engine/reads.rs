//! The read and search surface on `Engine`: per-account search, the mailbox/message
//! and calendar/event lists, and the windowed and thread-oriented message reads.

use std::{cmp::Reverse, collections::HashSet};

use engine_core::{
    calendar::{Calendar, Event},
    ids::{AccountId, ProviderKey, ThreadId},
    mail::{Mailbox, Message},
    sync::{ObjectKind, SearchDomain, SyncScope},
    time::UtcDateTime,
};
use engine_search::{CalendarQuery, MailQuery, SearchResults};
use engine_store::StoreRead;
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
        Ok(messages)
    }

    /// One account's newest `limit` messages by date — the windowed message list a host
    /// renders, ranked by `received_at`/`sent_at` (newest first) via the scalar mail index so
    /// **only the chosen `limit` payloads are deserialized**, not the whole mailbox. Messages
    /// with no known date sort last (entering the window only if dated ones don't fill it).
    /// Pair with [`Engine::thread_messages`] to pull a shown conversation's older members and
    /// [`Engine::messages_by_keys`] to resolve a specific message the window omits.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn messages_windowed(
        &self,
        account: &AccountId,
        limit: usize,
    ) -> Result<Vec<Message>, ApiError> {
        // Rank every mail scope's index entries by date (cheap — keys + dates, no payloads),
        // keep the newest `limit`, then deserialize just those.
        let mut ranked: Vec<(SyncScope, ProviderKey, Option<UtcDateTime>)> = Vec::new();
        for scope in self.mail_scopes(account).await? {
            for (key, date, _thread) in self.store.scope_mail_index(&scope).await? {
                ranked.push((scope.clone(), key, date));
            }
        }
        // Newest first. `Option<UtcDateTime>` orders `None` below any `Some`, so `Reverse` sinks
        // undated messages to the end — they enter the window only if dated ones leave room.
        ranked.sort_by_key(|(_, _, date)| Reverse(*date));
        ranked.truncate(limit);
        let mut messages = Vec::with_capacity(ranked.len());
        for (scope, key, _) in &ranked {
            if let Some(payload) = self.store.object_payload(scope, key).await? {
                messages.push(serde_json::from_value(payload).map_err(|err| decode_error(&err))?);
            }
        }
        Ok(messages)
    }

    /// Every message on one thread within an account — **all** of its members regardless of
    /// any date window, so a windowed list can still expand a conversation into its full
    /// history (a years-old reply included). Resolved through the mail index's `thread_id`, so
    /// only the thread's own members are read and decoded.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn thread_messages(
        &self,
        account: &AccountId,
        thread_id: &str,
    ) -> Result<Vec<Message>, ApiError> {
        let mut messages = Vec::new();
        for scope in self.mail_scopes(account).await? {
            for (key, _date, thread) in self.store.scope_mail_index(&scope).await? {
                if thread.as_ref().map(ThreadId::as_str) == Some(thread_id)
                    && let Some(payload) = self.store.object_payload(&scope, &key).await?
                {
                    messages
                        .push(serde_json::from_value(payload).map_err(|err| decode_error(&err))?);
                }
            }
        }
        Ok(messages)
    }

    /// Every message that belongs to **any** of the given `threads` within an account, except the
    /// ones whose key is in `exclude` — the batched counterpart of [`Engine::thread_messages`] for
    /// completing a **whole windowed list's** conversations in one pass. It scans the mail index
    /// **once**, not once per thread (which would be `O(threads × mailbox)` — pathological for a
    /// large mailbox), so a host pulls every shown conversation's out-of-window members
    /// (`exclude` = the keys already in the window, so they aren't re-read and re-decoded) in a
    /// single pass. Empty `threads` returns nothing without touching the store.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn thread_members(
        &self,
        account: &AccountId,
        threads: &HashSet<String>,
        exclude: &HashSet<String>,
    ) -> Result<Vec<Message>, ApiError> {
        if threads.is_empty() {
            return Ok(Vec::new());
        }
        let mut messages = Vec::new();
        for scope in self.mail_scopes(account).await? {
            for (key, _date, thread) in self.store.scope_mail_index(&scope).await? {
                let in_thread = thread
                    .as_ref()
                    .is_some_and(|thread| threads.contains(thread.as_str()));
                if in_thread
                    && !exclude.contains(key.as_str())
                    && let Some(payload) = self.store.object_payload(&scope, &key).await?
                {
                    messages
                        .push(serde_json::from_value(payload).map_err(|err| decode_error(&err))?);
                }
            }
        }
        Ok(messages)
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

    /// Lists one account's events — the synced calendar event objects (the projected
    /// envelope; recurrence materializes into occurrences in the store) across the
    /// account's calendar scopes. For the agenda/event list; pair with
    /// [`Engine::search_calendar`] for filtered or ranked views.
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

    /// The normalized payload of every object of `kind` across the account's scopes,
    /// enumerated and filtered by [`SyncScope::object_kind`] — so the facade never
    /// hard-codes or branches on which scopes a provider uses. One batch read per scope
    /// (no per-key round trip).
    async fn objects_of(
        &self,
        account: &AccountId,
        kind: ObjectKind,
    ) -> Result<Vec<Value>, ApiError> {
        let scopes = self.store.account_scopes(account.clone()).await?;
        let mut payloads = Vec::new();
        for scope in scopes
            .into_iter()
            .filter(|scope| scope.object_kind() == Some(kind))
        {
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
}
