//! The [`Provider`] implementation, wiring CalDAV discovery and `sync-collection`
//! into the engine's generic calendar sync.
//!
//! One `CalDavProvider` serves **every calendar** under the calendar home its discovery
//! found: [`sync_calendars`](Provider::sync_calendars) lists them under the per-account
//! [`DavCollectionList`](engine_core::sync::SyncScope::DavCollectionList) container scope,
//! each with the collection tag the same `PROPFIND` reads, and each calendar's events sync
//! under its own [`DavCollection`](engine_core::sync::SyncScope::DavCollection) scope
//! ([`event_scope`](Provider::event_scope)). The collection list is re-snapshotted each pass
//! (no list cursor), exactly as IMAP re-`LIST`s its folders. A create lands in the calendar
//! its draft names. The provider advertises
//! [`Capabilities::calendars`] **and** [`Capabilities::calendar_writes`] — it both
//! reads/syncs and writes (`PUT`/`DELETE`) over the same HTTP transport (`write`);
//! the mail methods keep their unsupported defaults.

use async_trait::async_trait;
use engine_core::{
    calendar::{Calendar, Event},
    ids::{AccountId, CalendarId, DavCollectionId, EventId, Uid},
    sync::{SyncScope, SyncState, SyncUpdate},
};
use engine_provider::{
    CalendarUserAddresses, CalendarWrites, Capabilities, ConnectObserver, ConnectStep,
    ConnectionInfo, EventDeletion, EventDraft, EventEdit, EventRsvp, EventWrite, EventWriteReceipt,
    IgnoreConnectSteps, OverrideSurvival, Provider, ProviderResult, RsvpControls, ScopeSync,
    WriteGuard,
};

use crate::{
    caldav_config::CalDavConfig,
    credential_hosts::refuse_unpermitted_base,
    discovery,
    error::CalDavError,
    href::{collection_href, encode_path_segment},
    transport::{DavClient, DavExecutor},
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

/// What a CalDAV series edit costs the user: nothing.
///
/// The client owns the bytes, and the structural patcher rewrites only the master
/// `VEVENT`'s own lines — so a `RECURRENCE-ID` component is untouched by construction
/// rather than by server policy. Measured, and re-measured by
/// `tests/live_calendar_survival.rs` against both harness servers.
const CALDAV_OVERRIDE_SURVIVAL: OverrideSurvival = OverrideSurvival::kept();

/// The opaque cursor the per-account calendar-list scope persists. Like IMAP's
/// folder-list sentinel, it is a fixed, non-empty token: the list is re-discovered
/// as a snapshot each pass (no real delta cursor), but an *empty* state must not be
/// used — elsewhere empty means "no cursor / full resync", a meaning this scope
/// must not overload.
const CALENDAR_LIST_CURSOR: &str = "caldav-calendar-list";

/// The CalDAV provider adapter (calendar read/sync and writes) for every calendar under one
/// calendar home.
///
/// A calendar's [`CalendarId`] is its collection href as the listing gave it, and its event
/// scope's [`DavCollectionId`] is the same key, so the two views of one href cannot drift.
pub struct CalDavProvider {
    executor: Box<dyn DavExecutor>,
    capabilities: Capabilities,
    home_href: String,
    /// The `current-user-principal` discovery found, asked for the address set on demand.
    principal: Option<String>,
    /// The address set, when discovery already read it off the principal.
    addresses: Option<CalendarUserAddresses>,
}

impl core::fmt::Debug for CalDavProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CalDavProvider")
            .field("home_href", &self.home_href)
            .finish_non_exhaustive()
    }
}

impl CalDavProvider {
    /// Connects to a CalDAV server, discovering the calendar home whose calendars it serves.
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
        refuse_unpermitted_base(config.credential_hosts.as_ref(), &config.base_url)?;
        let client = DavClient::new(
            &config.base_url,
            config.credentials,
            &config.tls,
            &config.retry,
            &config.user_agent,
        )?
        .with_credential_hosts(config.credential_hosts);
        Self::with_executor(Box::new(client), &config.discovery_path, observer).await
    }

    /// Builds a provider over an arbitrary executor (the live client, or a fake in
    /// tests), running discovery through it and reporting each step to `observer`.
    pub(crate) async fn with_executor(
        executor: Box<dyn DavExecutor>,
        discovery_path: &str,
        observer: &dyn ConnectObserver,
    ) -> Result<Self, CalDavError> {
        let home = discovery::discover_home(executor.as_ref(), discovery_path, observer).await?;
        observer.step(&ConnectStep::discovered(&home.href));
        // The calendar home is where every collection is resolved from: the endpoint this
        // connect settles on, on whichever origin serves it.
        let home_href = executor.settle_on(&home.href);
        // Whether the server schedules for itself is a property of *this* server, not of
        // CalDAV — RFC 4791 is calendar access and RFC 6638 a separate layer on top — so it
        // is asked rather than inferred from the fact that a PARTSTAT can be written
        // (`discovery.rs`).
        let scheduling = discovery::discover_scheduling(executor.as_ref(), &home_href).await?;
        // CalDAV *enforces* the lost-update guard: a stale `If-Match` is a `412` on
        // every server we have driven (proven live against both harness servers). It is
        // the transport that can actually promise it — contrast JMAP, which cannot.
        let capabilities = Capabilities::none()
            .with_calendars()
            .with_calendar_writes(WriteGuard::Enforced, CALDAV_OVERRIDE_SURVIVAL)
            .with_calendar_rsvp(CALDAV_RSVP);
        Ok(Self {
            executor,
            capabilities: if scheduling {
                capabilities.with_calendar_scheduling()
            } else {
                capabilities
            },
            home_href,
            principal: home.principal,
            addresses: home.addresses,
        })
    }

    /// The principal URL the server named during discovery, absolute, when it named one: what a
    /// host stores and starts the next connect from (RFC 6764 §6), rather than the calendar home,
    /// which the principal names afresh on every connect.
    #[must_use]
    pub fn principal_url(&self) -> Option<&str> {
        self.principal.as_deref()
    }

    /// Mints the resource href for a **new** event in `calendar`:
    /// `<collection>/<uid>.ics`, the universal CalDAV convention (RFC 4791 §5.3.2 lets the
    /// client choose the resource name). `calendar` is the href the listing gave it. The
    /// `uid` is percent-encoded as a single path segment, so an unusual `UID` still yields a
    /// valid href.
    ///
    /// [`create_event`](CalendarWrites::create_event) mints this itself, so a host does not need
    /// it — a create states an [`EventDraft`] and learns the resulting id from the receipt.
    /// It stays public for the operations that address a resource *before* it has been
    /// synced: pre-cleaning a throwaway event, or the iMIP RSVP path
    /// ([`imip`](crate::imip)). A patch or delete of a synced event reuses its stored
    /// [`Event::id`](engine_core::calendar::Event::id).
    ///
    /// # Errors
    ///
    /// Returns [`CalDavError`] if `calendar` is not a calendar's href.
    pub fn event_href(&self, calendar: &CalendarId, uid: &Uid) -> Result<EventId, CalDavError> {
        let href = format!(
            "{}{}.ics",
            collection_href(calendar.as_str())?,
            encode_path_segment(uid.as_str())
        );
        EventId::try_from(href.as_str())
            .map_err(|e| CalDavError::protocol(format!("bad event href {href:?}: {e}")))
    }
}

#[async_trait]
impl Provider for CalDavProvider {
    /// The fixed calendar read/write capabilities plus the versions the transport
    /// negotiated. The TLS version is `None` against a plaintext `http://` origin,
    /// which is what the harness serves (`docs/agent-guidance/tls.md`).
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            http_version: self.executor.http_version(),
            tls_version: self.executor.tls_version(),
            ..ConnectionInfo::new(self.capabilities)
        }
    }

    fn calendar_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::DavCollectionList {
            account: account.clone(),
        }
    }

    fn event_scope(&self, account: &AccountId, calendar: &CalendarId) -> SyncScope {
        SyncScope::DavCollection {
            account: account.clone(),
            collection: DavCollectionId::new(calendar.key().clone()),
        }
    }

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        // The collection list is re-discovered as a snapshot each pass (no list
        // cursor), so the store tombstones any calendar that has disappeared.
        let calendars = discovery::list_calendars(self.executor.as_ref(), &self.home_href).await?;
        let present = calendars.iter().map(|c| c.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(calendars, present),
            SyncState::new(CALENDAR_LIST_CURSOR),
        ))
    }

    async fn sync_events(
        &self,
        _account: &AccountId,
        calendar: &CalendarId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        Ok(crate::sync::sync_events(
            self.executor.as_ref(),
            &collection_href(calendar.as_str())?,
            calendar,
            cursor,
        )
        .await?)
    }
}
impl engine_provider::MailboxWrites for CalDavProvider {}

#[async_trait]
impl CalendarWrites for CalDavProvider {
    /// Mints the href from the draft's `UID` inside the calendar the draft names, then `PUT`s
    /// the built document there.
    async fn create_event(
        &self,
        _account: &AccountId,
        draft: &EventDraft,
    ) -> ProviderResult<EventWriteReceipt> {
        let href = self.event_href(&draft.calendar, &draft.uid)?;
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
        Ok(crate::write::rsvp_event(
            self.executor.as_ref(),
            base,
            rsvp,
            self.capabilities.calendar_scheduling(),
        )
        .await?)
    }

    async fn delete_event(
        &self,
        _account: &AccountId,
        base: Option<&Event>,
        deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        Ok(crate::write::delete_event(self.executor.as_ref(), base, deletion).await?)
    }

    /// The principal's `calendar-user-address-set`: kept from discovery when it read the
    /// principal, otherwise asked of the principal now. Unknown when the server named none.
    async fn calendar_user_addresses(
        &self,
        _account: &AccountId,
    ) -> ProviderResult<CalendarUserAddresses> {
        if let Some(addresses) = &self.addresses {
            return Ok(addresses.clone());
        }
        let Some(principal) = &self.principal else {
            return Ok(CalendarUserAddresses::Unknown);
        };
        Ok(discovery::principal_addresses(self.executor.as_ref(), principal).await?)
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "imip_flow_tests.rs"]
mod imip_flow_tests;
