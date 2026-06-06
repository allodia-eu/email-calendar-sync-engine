//! WebDAV collection identity.

use serde::{Deserialize, Serialize};

use super::{IdError, ProviderKey};

object_id! {
    /// Identifies a WebDAV collection used as a sync scope (a CalDAV calendar or,
    /// later, a CardDAV address book), keyed by its collection URL/href.
    ///
    /// This is the per-collection unit CalDAV/CardDAV sync state attaches to
    /// (RFC 6578 sync-token, or CTag + per-resource ETags). It is distinct from
    /// the normalized [`super::CalendarId`]: the adapter maps a collection's href
    /// to the calendar it represents.
    DavCollectionId
}
