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

use async_trait::async_trait;
use engine_core::calendar::{Calendar, Event};
use engine_core::ids::{AccountId, CalendarId, DavCollectionId, EventId, Uid};
use engine_core::sync::{SyncScope, SyncState, SyncUpdate};
use engine_provider::{
    Capabilities, EventDeletion, EventWrite, EventWriteReceipt, Provider, ProviderResult, ScopeSync,
};

use crate::discovery;
use crate::error::CalDavError;
use crate::transport::{Credentials, DavClient, DavExecutor};

/// Connection settings for a CalDAV account.
#[derive(Debug, Clone)]
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
    pub async fn connect(config: CalDavConfig) -> Result<Self, CalDavError> {
        let client = DavClient::new(&config.base_url, config.credentials)?;
        Self::with_executor(Box::new(client), &config.discovery_path, &config.calendar).await
    }

    /// Builds a provider over an arbitrary executor (the live client, or a fake in
    /// tests), running discovery through it.
    pub(crate) async fn with_executor(
        executor: Box<dyn DavExecutor>,
        discovery_path: &str,
        calendar: &str,
    ) -> Result<Self, CalDavError> {
        let home_href = discovery::discover_home(executor.as_ref(), discovery_path).await?;
        let collection = bind_collection(&home_href, calendar)?;
        Ok(Self {
            executor,
            capabilities: Capabilities::none().with_calendars().with_calendar_writes(),
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
    /// `<collection>/<uid>.ics`, the universal CalDAV convention (RFC 4791 §5.3.2
    /// lets the client choose the resource name). The `uid` is percent-encoded as a
    /// single path segment, so an unusual `UID` still yields a valid href. Use it as
    /// the [`EventWrite::create`](engine_provider::EventWrite::create) target; an
    /// update/delete reuses the stored
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

    /// The membership [`CalendarId`] for the bound collection (same href as
    /// [`collection_href`](Self::collection_href), a distinct id type).
    fn calendar_id(&self) -> CalendarId {
        // The collection href already validated as a provider key when bound.
        CalendarId::new(self.collection.key().clone())
    }
}

#[async_trait]
impl Provider for CalDavProvider {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
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

    async fn put_event(
        &self,
        _account: &AccountId,
        write: &EventWrite,
    ) -> ProviderResult<EventWriteReceipt> {
        Ok(crate::write::put_event(self.executor.as_ref(), write).await?)
    }

    async fn delete_event(
        &self,
        _account: &AccountId,
        deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        Ok(crate::write::delete_event(self.executor.as_ref(), deletion).await?)
    }
}

/// Binds a calendar argument to a collection id: an absolute path or full URL is
/// used as-is (a discovered calendar href), otherwise a bare name is joined onto
/// the calendar home. All end in a trailing slash (CalDAV collections are
/// directories).
///
/// # Errors
///
/// Returns [`CalDavError`] if the resolved href is not a valid provider key.
fn bind_collection(home_href: &str, calendar: &str) -> Result<DavCollectionId, CalDavError> {
    let href = resolve_collection(home_href, calendar);
    DavCollectionId::try_from(href.as_str())
        .map_err(|e| CalDavError::protocol(format!("bad collection href {href:?}: {e}")))
}

/// Adds a minimal [`Calendar`] for the bound collection when the home listing did
/// not include it, so the container snapshot always covers the events' membership.
fn ensure_bound_present(calendars: &mut Vec<Calendar>, bound: &CalendarId) {
    if calendars.iter().any(|c| &c.id == bound) {
        return;
    }
    let name = bound
        .as_str()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(bound.as_str())
        .to_owned();
    calendars.push(Calendar::new(bound.clone(), name));
}

/// Resolves the bound collection href (see [`bind_collection`]).
fn resolve_collection(home_href: &str, calendar: &str) -> String {
    if calendar.starts_with('/') || calendar.contains("://") {
        return with_trailing_slash(calendar);
    }
    format!(
        "{}{}/",
        with_trailing_slash(home_href),
        calendar.trim_matches('/')
    )
}

/// Ensures `href` ends with a single trailing slash.
fn with_trailing_slash(href: &str) -> String {
    if href.ends_with('/') {
        href.to_owned()
    } else {
        format!("{href}/")
    }
}

/// Percent-encodes one URL path segment to its **canonical** form: only RFC 3986
/// `unreserved` bytes (`ALPHA` / `DIGIT` / `-` / `.` / `_` / `~`) are kept verbatim;
/// every other byte — including `@`, sub-delims, and path-unsafe bytes — is
/// `%`-encoded. Encoding everything outside `unreserved` matches how CalDAV servers
/// store and report resource hrefs (Stalwart returns `@` as `%40`, verified live),
/// so a minted create href round-trips to the same href the server canonicalizes to
/// — otherwise a later `If-Match`/`DELETE` against the minted href would miss the
/// server's differently-encoded resource.
fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for &byte in segment.as_bytes() {
        if is_unreserved(byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(hex_upper(byte >> 4));
            out.push(hex_upper(byte & 0x0f));
        }
    }
    out
}

/// Whether `byte` is an RFC 3986 `unreserved` character (never percent-encoded;
/// `%XX` and the literal byte are equivalent only for this set, §2.3).
fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// The upper-case hex digit for a 0–15 nibble.
fn hex_upper(nibble: u8) -> char {
    char::from_digit(u32::from(nibble), 16).map_or('0', |c| c.to_ascii_uppercase())
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;
