//! The shared-store fixture's coordinates, and the two probes that read another store.
//!
//! The seeded account can reach three stores besides its own, and between them they cover
//! every shape a shared store comes in (`docs/agent-guidance/stalwart-harness.md`):
//!
//! | Store | Mail | Calendars |
//! | --- | --- | --- |
//! | [`SHARED_GROUP_ACCOUNT`](crate::SHARED_GROUP_ACCOUNT) | every right | every right |
//! | [`Harness::read_only_share_owner`] (bob) | `lr` on his INBOX | none: his home answers `403` |
//! | [`CALENDAR_SHARE_OWNER`] (dave) | none | one calendar, read-only |
//!
//! Carol shares nothing, so she is the store the seeded account cannot reach at all.

use crate::{Harness, HarnessError, HttpResponse, http};

/// Address of the account that shares **one calendar and nothing else** with the seeded
/// account, read-only — the calendar-only half of the fixture, as bob's INBOX is the
/// mail-only half.
///
/// Not env-overridable, like [`SHARED_GROUP_ACCOUNT`](crate::SHARED_GROUP_ACCOUNT): it exists
/// only because the bundled entrypoint provisions it.
pub const CALENDAR_SHARE_OWNER: &str = "dave@test.local";
/// Display name of the calendar [`CALENDAR_SHARE_OWNER`] shares. It is the only one of his
/// the seeded account can see; his default calendar stays private.
pub const SHARED_CALENDAR_NAME: &str = "Dave Shared";
/// iCalendar UID of the one event in [`SHARED_CALENDAR_NAME`] (`shared/dave-shared.ics`).
pub const SHARED_CALENDAR_EVENT_UID: &str = "dave-shared-2201@test.local";
/// iCalendar UID of the one event seeded into the group's own calendar
/// (`shared/support-rota.ics`), found in none of the seeded account's calendars.
pub const GROUP_CALENDAR_EVENT_UID: &str = "support-2301@test.local";

impl Harness {
    /// `POST` a raw JMAP request body to `/jmap/` as `auth`.
    ///
    /// The same escape hatch as [`jmap_post`](Self::jmap_post), for a probe that has to be
    /// made as an account other than the seeded one.
    ///
    /// # Errors
    /// Propagates transport/parse failures from the HTTP probe.
    pub fn jmap_post_as(
        &self,
        auth: (&str, &str),
        body: &[u8],
    ) -> Result<HttpResponse, HarnessError> {
        http::request(
            &self.http_addr,
            "POST",
            "/jmap/",
            Some(auth),
            &[("Content-Type", "application/json")],
            body,
        )
    }

    /// `PROPFIND` `path` at `depth` with a caller-chosen body, as the seeded account.
    ///
    /// [`dav_propfind_as`](Self::dav_propfind_as) asks a fixed set of properties; discovery
    /// probes need others (`calendar-home-set`, `DAV:owner`, the proxy sets).
    ///
    /// # Errors
    /// Propagates transport/parse failures from the HTTP probe.
    pub fn dav_propfind_body(
        &self,
        path: &str,
        depth: &str,
        body: &str,
    ) -> Result<HttpResponse, HarnessError> {
        dav_propfind(
            &self.http_addr,
            (self.account.as_str(), self.password.as_str()),
            path,
            depth,
            body,
        )
    }
}

/// `PROPFIND` `path` at `depth` on **any** DAV server at `addr`, as `auth`.
///
/// For the other DAV fixtures (`docker/sabredav`, `docker/nextcloud`), whose own live tests
/// pin server facts the provider cannot express — a principal's proxy sets, a home it may
/// not read — with the same dependency-free probe the smoke suite uses.
///
/// # Errors
/// Propagates transport/parse failures from the HTTP probe.
pub fn dav_propfind(
    addr: &str,
    auth: (&str, &str),
    path: &str,
    depth: &str,
    body: &str,
) -> Result<HttpResponse, HarnessError> {
    http::request(
        addr,
        "PROPFIND",
        path,
        Some(auth),
        &[("Depth", depth), ("Content-Type", "application/xml")],
        body.as_bytes(),
    )
}
