//! The [`Provider`] implementation, wiring CalDAV discovery and `sync-collection`
//! into the engine's generic calendar sync.
//!
//! Like an [`ImapProvider`](provider_imap), a `CalDavProvider` is **bound to one
//! calendar collection** for events ([`event_scope`](Provider::event_scope) is
//! that collection's [`DavCollection`](engine_core::sync::SyncScope::DavCollection)),
//! while [`sync_calendars`](Provider::sync_calendars) lists *all* of the account's
//! calendars under the per-account
//! [`DavCollectionList`](engine_core::sync::SyncScope::DavCollectionList) container
//! scope. The collection list is re-snapshotted each pass (no list cursor),
//! exactly as IMAP re-`LIST`s its folders. The cross-collection fan-out (drive
//! every calendar) is the later orchestrator's job. The provider advertises
//! [`Capabilities::calendars`] **and** [`Capabilities::calendar_writes`] — it both
//! reads/syncs and writes (`PUT`/`DELETE`) over the same HTTP transport (`write`);
//! the mail methods keep their unsupported defaults.

use std::sync::Arc;

use async_trait::async_trait;
use engine_core::{
    calendar::{Calendar, Event},
    ids::{AccountId, CalendarId, DavCollectionId, EventId, Uid},
    sync::{SyncScope, SyncState, SyncUpdate},
};
use engine_provider::{
    Capabilities, ConnectObserver, ConnectStep, ConnectionInfo, EventDeletion, EventDraft,
    EventEdit, EventRsvp, EventWrite, EventWriteReceipt, IgnoreConnectSteps, Provider,
    ProviderError, ProviderResult, RsvpControls, ScopeSync, WriteGuard,
};
use engine_tls::TlsClientConfig;

use crate::{
    discovery,
    error::CalDavError,
    href::{bind_collection, encode_path_segment, ensure_bound_present},
    transport::{Credentials, DavClient, DavExecutor},
};

/// What a CalDAV RSVP can and cannot control.
///
/// The answer rides the same conditional `PUT` as any other write, so it keeps the enforced
/// `If-Match` guard. Neither surrounding control is ours: an RFC 6638 auto-schedule server
/// emits the iTIP `REPLY` itself the moment the `PARTSTAT` changes, and iCalendar has no
/// per-attendee note to carry one in. Declared once, and used both to advertise and to
/// enforce, so the two can never disagree.
const CALDAV_RSVP: RsvpControls = RsvpControls {
    comment: false,
    suppress_notification: false,
    guard: WriteGuard::Enforced,
};

/// Connection settings for a CalDAV account.
#[derive(Clone)]
pub struct CalDavConfig {
    /// The server origin, e.g. `https://dav.example.com`.
    pub base_url: String,
    /// How to authenticate.
    pub credentials: Credentials,
    /// The URL to begin discovery at; defaults to the RFC 6764 well-known path.
    pub discovery_path: String,
    /// The calendar collection to bind events to — a name under the calendar home
    /// (e.g. `default`) or an absolute collection path.
    pub calendar: String,
    /// The TLS trust policy for this account, shared with every other provider
    /// (`docs/agent-guidance/tls.md`). Defaults to the hermetic bundled roots;
    /// override with [`CalDavConfig::with_tls`].
    pub tls: TlsClientConfig,
    /// Private, unlike its siblings: a `dyn` observer is neither `Debug` nor
    /// meaningfully inspectable, so it is set through
    /// [`CalDavConfig::with_connect_observer`] and read only by
    /// [`CalDavProvider::connect`].
    connect_observer: Option<Arc<dyn ConnectObserver>>,
}

impl core::fmt::Debug for CalDavConfig {
    /// Hand-written because the observer is a `dyn` trait object; the credentials
    /// redact themselves (`Credentials`).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CalDavConfig")
            .field("base_url", &self.base_url)
            .field("credentials", &self.credentials)
            .field("discovery_path", &self.discovery_path)
            .field("calendar", &self.calendar)
            .field("tls", &self.tls)
            .field("connect_observer", &self.connect_observer.is_some())
            .finish()
    }
}

impl CalDavConfig {
    /// Settings with the RFC 6764 well-known discovery path and the `default`
    /// calendar.
    #[must_use]
    pub fn new(base_url: impl Into<String>, credentials: Credentials) -> Self {
        Self {
            base_url: base_url.into(),
            credentials,
            discovery_path: "/.well-known/caldav".to_owned(),
            calendar: "default".to_owned(),
            tls: TlsClientConfig::default(),
            connect_observer: None,
        }
    }

    /// Binds events to a different calendar collection (a home-relative name or an
    /// absolute path).
    #[must_use]
    pub fn with_calendar(mut self, calendar: impl Into<String>) -> Self {
        self.calendar = calendar.into();
        self
    }

    /// Overrides the discovery starting path.
    #[must_use]
    pub fn with_discovery_path(mut self, path: impl Into<String>) -> Self {
        self.discovery_path = path.into();
        self
    }

    /// Sets the TLS trust policy (the host builds one and shares it across the
    /// account's providers).
    #[must_use]
    pub fn with_tls(mut self, tls: TlsClientConfig) -> Self {
        self.tls = tls;
        self
    }

    /// Observes the connect phase: one [`ConnectStep::Redirected`] per hop discovery
    /// follows itself, then [`ConnectStep::Discovered`] naming the calendar home.
    ///
    /// No TLS step (reqwest never exposes the negotiated version,
    /// `docs/agent-guidance/tls.md`) and no auth step — CalDAV has no discrete
    /// authentication exchange; credentials ride on each `PROPFIND`.
    ///
    /// The observer rides on the config, so a host that rebuilds this provider after a
    /// dropped session observes the redial too. `Arc` so one host observer can be
    /// shared across the account's providers.
    #[must_use]
    pub fn with_connect_observer(mut self, observer: Arc<dyn ConnectObserver>) -> Self {
        self.connect_observer = Some(observer);
        self
    }
}

/// The opaque cursor the per-account calendar-list scope persists. Like IMAP's
/// folder-list sentinel, it is a fixed, non-empty token: the list is re-discovered
/// as a snapshot each pass (no real delta cursor), but an *empty* state must not be
/// used — elsewhere empty means "no cursor / full resync", a meaning this scope
/// must not overload.
const CALENDAR_LIST_CURSOR: &str = "caldav-calendar-list";

/// The CalDAV provider adapter (calendar read/sync).
///
/// The bound collection is held once as a [`DavCollectionId`]; the membership
/// [`CalendarId`] and the transport href are derived from it, so the three views
/// of one href cannot drift.
pub struct CalDavProvider {
    executor: Box<dyn DavExecutor>,
    capabilities: Capabilities,
    home_href: String,
    collection: DavCollectionId,
}

impl core::fmt::Debug for CalDavProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CalDavProvider")
            .field("home_href", &self.home_href)
            .field("collection", &self.collection.as_str())
            .finish_non_exhaustive()
    }
}

impl CalDavProvider {
    /// Connects to a CalDAV server, discovering the calendar home and binding to
    /// the configured collection for events.
    ///
    /// # Errors
    ///
    /// Returns [`CalDavError`] on a bad URL, a transport/HTTP failure, or a
    /// discovery response with no calendar home.
    ///
    /// Trust comes from [`CalDavConfig::tls`] (`docs/agent-guidance/tls.md`).
    /// Discovery reports its progress to [`CalDavConfig::with_connect_observer`]'s
    /// observer, if one is configured.
    pub async fn connect(config: CalDavConfig) -> Result<Self, CalDavError> {
        let observer: &dyn ConnectObserver = config
            .connect_observer
            .as_deref()
            .unwrap_or(&IgnoreConnectSteps);
        let client = DavClient::new(&config.base_url, config.credentials, &config.tls)?;
        Self::with_executor(
            Box::new(client),
            &config.discovery_path,
            &config.calendar,
            observer,
        )
        .await
    }

    /// Builds a provider over an arbitrary executor (the live client, or a fake in
    /// tests), running discovery through it and reporting each step to `observer`.
    pub(crate) async fn with_executor(
        executor: Box<dyn DavExecutor>,
        discovery_path: &str,
        calendar: &str,
        observer: &dyn ConnectObserver,
    ) -> Result<Self, CalDavError> {
        let home_href =
            discovery::discover_home(executor.as_ref(), discovery_path, observer).await?;
        // The calendar home is where every collection — including the bound one — is
        // resolved from: the endpoint this connect settled on.
        observer.step(&ConnectStep::discovered(&home_href));
        // Whether the server schedules for itself is a property of *this* server, not of
        // CalDAV — RFC 4791 is calendar access and RFC 6638 a separate layer on top — so it
        // is asked rather than inferred from the fact that a PARTSTAT can be written
        // (`discovery.rs`).
        let scheduling = discovery::discover_scheduling(executor.as_ref(), &home_href).await?;
        let collection = bind_collection(&home_href, calendar)?;
        // CalDAV *enforces* the lost-update guard: a stale `If-Match` is a `412` on
        // every server we have driven (proven live against both harness servers). It is
        // the transport that can actually promise it — contrast JMAP, which cannot.
        let capabilities = Capabilities::none()
            .with_calendars()
            .with_calendar_writes(WriteGuard::Enforced)
            .with_calendar_rsvp(CALDAV_RSVP);
        Ok(Self {
            executor,
            capabilities: if scheduling {
                capabilities.with_calendar_scheduling()
            } else {
                capabilities
            },
            home_href,
            collection,
        })
    }

    /// Rebinds this provider to a different calendar collection **without** re-running
    /// discovery — the calendar home is unchanged, only the bound collection moves.
    /// Consumes `self` to reuse the existing executor (a host that lists calendars,
    /// then picks one, avoids a second discovery round trip).
    ///
    /// # Errors
    ///
    /// Returns [`CalDavError`] if `calendar` does not form a valid collection href.
    pub fn rebind(self, calendar: &str) -> Result<Self, CalDavError> {
        let collection = bind_collection(&self.home_href, calendar)?;
        Ok(Self { collection, ..self })
    }

    /// The href of the calendar collection events are bound to.
    #[must_use]
    pub fn collection_href(&self) -> &str {
        self.collection.as_str()
    }

    /// Mints the resource href for a **new** event in the bound collection:
    /// `<collection>/<uid>.ics`, the universal CalDAV convention (RFC 4791 §5.3.2 lets the
    /// client choose the resource name). The `uid` is percent-encoded as a single path
    /// segment, so an unusual `UID` still yields a valid href.
    ///
    /// [`create_event`](Provider::create_event) mints this itself, so a host does not need
    /// it — a create states an [`EventDraft`] and learns the resulting id from the receipt.
    /// It stays public for the operations that address a resource *before* it has been
    /// synced: pre-cleaning a throwaway event, or the iMIP RSVP path
    /// ([`imip`](crate::imip)). A patch or delete of a synced event reuses its stored
    /// [`Event::id`](engine_core::calendar::Event::id).
    ///
    /// # Errors
    ///
    /// Returns [`CalDavError`] if the resolved href is not a valid event key (the
    /// bound collection href and the non-empty suffix make this unreachable in
    /// practice, but the construction is fallible like the collection binding).
    pub fn event_href(&self, uid: &Uid) -> Result<EventId, CalDavError> {
        let href = format!(
            "{}{}.ics",
            self.collection.as_str(),
            encode_path_segment(uid.as_str())
        );
        EventId::try_from(href.as_str())
            .map_err(|e| CalDavError::protocol(format!("bad event href {href:?}: {e}")))
    }

    /// The [`CalendarId`] of the bound collection (same href as
    /// [`collection_href`](Self::collection_href), a distinct id type).
    ///
    /// This is the calendar an [`EventDraft`] must name to be created here — a draft
    /// targeting any other is refused rather than silently written to this one.
    #[must_use]
    pub fn calendar_id(&self) -> CalendarId {
        // The collection href already validated as a provider key when bound.
        CalendarId::new(self.collection.key().clone())
    }
}

#[async_trait]
impl Provider for CalDavProvider {
    /// The fixed calendar read/write capabilities plus the transport's negotiated HTTP
    /// version. The TLS version is always `None` — reqwest exposes only the peer
    /// certificate, never the negotiated protocol version (`docs/agent-guidance/tls.md`).
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            http_version: self.executor.http_version(),
            ..ConnectionInfo::new(self.capabilities)
        }
    }

    fn calendar_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::DavCollectionList {
            account: account.clone(),
        }
    }

    fn event_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::DavCollection {
            account: account.clone(),
            collection: self.collection.clone(),
        }
    }

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        // The collection list is re-discovered as a snapshot each pass (no list
        // cursor), so the store tombstones any calendar that has disappeared.
        let mut calendars =
            discovery::list_calendars(self.executor.as_ref(), &self.home_href).await?;
        // Guarantee the bound collection is represented, so events synced under it
        // never reference a calendar the container snapshot omits (a collection
        // bound outside the home would otherwise be absent here).
        ensure_bound_present(&mut calendars, &self.calendar_id());
        let present = calendars.iter().map(|c| c.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(calendars, present),
            SyncState::new(CALENDAR_LIST_CURSOR),
        ))
    }

    async fn sync_events(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        Ok(crate::sync::sync_events(
            self.executor.as_ref(),
            self.collection.as_str(),
            &self.calendar_id(),
            cursor,
        )
        .await?)
    }

    /// Mints the href from the draft's `UID` inside the **bound** collection, then `PUT`s
    /// the built document there.
    ///
    /// A draft naming a different calendar is refused rather than silently written to the
    /// bound one: this provider is collection-bound (`rebind` moves it), so honouring the
    /// draft's calendar would mean writing the event where the host did not ask.
    async fn create_event(
        &self,
        _account: &AccountId,
        draft: &EventDraft,
    ) -> ProviderResult<EventWriteReceipt> {
        if draft.calendar != self.calendar_id() {
            return Err(ProviderError::invalid_state(format!(
                "draft targets calendar {:?}, but this provider is bound to {:?}; rebind first",
                draft.calendar.as_str(),
                self.collection.as_str()
            )));
        }
        let href = self.event_href(&draft.uid)?;
        Ok(crate::write::create_event(self.executor.as_ref(), href, draft).await?)
    }

    async fn patch_event(
        &self,
        _account: &AccountId,
        base: &Event,
        edit: &EventEdit,
    ) -> ProviderResult<EventWriteReceipt> {
        Ok(crate::write::patch_event(self.executor.as_ref(), base, edit).await?)
    }

    async fn put_event(
        &self,
        _account: &AccountId,
        write: &EventWrite,
    ) -> ProviderResult<EventWriteReceipt> {
        Ok(crate::write::put_event(self.executor.as_ref(), write).await?)
    }

    async fn rsvp_event(
        &self,
        _account: &AccountId,
        base: &Event,
        rsvp: &EventRsvp,
    ) -> ProviderResult<EventWriteReceipt> {
        CALDAV_RSVP.accept(rsvp)?;
        Ok(crate::write::rsvp_event(self.executor.as_ref(), base, rsvp).await?)
    }

    async fn delete_event(
        &self,
        _account: &AccountId,
        deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        Ok(crate::write::delete_event(self.executor.as_ref(), deletion).await?)
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "imip_flow_tests.rs"]
mod imip_flow_tests;
