//! The one judgement of a refused DAV write: the object's refusal, the target's, or neither.
//!
//! A `PUT` can be refused for the object it carries (an invalid object, one too large) or for
//! where it goes (a full store, a missing collection, no permission), and a host has to tell
//! them apart: the first will never be taken, the second will once the target changes. An
//! import and a sync create read the same answer here, so the two cannot drift. Its own file for
//! the 500-line limit, beside `error.rs`, whose status classes it agrees with.

use engine_provider::{ProviderError, TargetRefusal};

use crate::error::CalDavError;

/// The CalDAV and CardDAV preconditions a server refuses one object by (RFC 4791 §5.3.2.1,
/// RFC 6352 §6.3.2.1): the object, never the collection or the request, is what fails them.
const OBJECT_PRECONDITIONS: [&str; 11] = [
    "supported-calendar-data",
    "valid-calendar-data",
    "valid-calendar-object-resource",
    "supported-calendar-component",
    "max-resource-size",
    "min-date-time",
    "max-date-time",
    "max-instances",
    "max-attendees-per-instance",
    "supported-address-data",
    "valid-address-data",
];

/// Whether an error body names one of the [`OBJECT_PRECONDITIONS`].
pub(crate) fn names_object_precondition(body: &str) -> bool {
    OBJECT_PRECONDITIONS
        .iter()
        .any(|name| crate::dav::has_precondition(body, name))
}

/// Why a server refused a write, as far as its answer says. Read by
/// [`CalDavError::write_refusal`], the one place a write's refusal is judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteRefusal {
    /// This object, and only it, will never be taken as it is: a CalDAV or CardDAV object
    /// precondition, or a `400`, `413` or `415` that is not a throttle.
    Object,
    /// The target's own state refuses every object until it changes ([`TargetRefusal`]):
    ///
    /// - [`StorageFull`](TargetRefusal::StorageFull): `507` (RFC 4918 §11.5) or
    ///   `DAV:quota-not-exceeded` (RFC 4331 §6);
    /// - [`NoSuchContainer`](TargetRefusal::NoSuchContainer): `404`, or a `409` naming no
    ///   precondition, what a `PUT` under a missing collection answers (RFC 4918 §9.7.1). SabreDAV
    ///   and Nextcloud answer a missing calendar `404`, Stalwart `409` (observed);
    /// - [`ReadOnlyContainer`](TargetRefusal::ReadOnlyContainer): a `403` naming
    ///   `DAV:need-privileges` (RFC 3744 §7.1.1, SabreDAV), or a plain `403`, which is how Stalwart
    ///   answers a calendar shared read-only and Nextcloud its generated birthday calendar
    ///   (observed). A `403` naming an object precondition or `no-uid-conflict` is neither.
    Target(TargetRefusal),
}

impl CalDavError {
    /// What this error says about a refused write: the object's refusal, the target's, or `None`
    /// where it is no refusal (a throttle, a conflict, a server error, no answer).
    pub(crate) fn write_refusal(&self) -> Option<WriteRefusal> {
        let Self::Status { status, body, .. } = self else {
            return None;
        };
        let names = |name| crate::dav::has_precondition(body, name);
        let target = WriteRefusal::Target;
        Some(match status {
            507 => target(TargetRefusal::StorageFull),
            _ if names("quota-not-exceeded") => target(TargetRefusal::StorageFull),
            400 if crate::throttle::is_concurrency_limit(body) => return None,
            400 | 413 | 415 => WriteRefusal::Object,
            403 | 412 if names_object_precondition(body) => WriteRefusal::Object,
            403 if names("no-uid-conflict") => return None,
            403 => target(TargetRefusal::ReadOnlyContainer),
            404 => target(TargetRefusal::NoSuchContainer),
            409 if !names("no-uid-conflict") && !names("valid-sync-token") => {
                target(TargetRefusal::NoSuchContainer)
            }
            _ => return None,
        })
    }

    /// This failure, the answer to a sync create, as a [`ProviderError`]: a refusal judged by
    /// [`write_refusal`](Self::write_refusal), the rule an import reads too, as
    /// [`refused_write`](Self::refused_write) answers it, and anything else as before.
    pub(crate) fn into_write_error(self) -> ProviderError {
        if self.write_refusal().is_some() {
            self.refused_write("writing an event")
        } else {
            self.into()
        }
    }

    /// This failure, the answer to a write, as a [`ProviderError`] that says nothing the server
    /// said: the object's refusal is [`ProviderError::item_unavailable`], the target's is
    /// [`ProviderError::refused_by_target`], and anything else is
    /// [`content_free`](Self::content_free).
    pub(crate) fn refused_write(self, what: &str) -> ProviderError {
        match self.write_refusal() {
            Some(WriteRefusal::Object) => {
                let status = match &self {
                    Self::Status { status, .. } => *status,
                    _ => 0,
                };
                ProviderError::item_unavailable(format!(
                    "{what}: the server refused it (HTTP {status})"
                ))
            }
            Some(WriteRefusal::Target(reason)) => ProviderError::refused_by_target(reason),
            None => self.content_free(what),
        }
    }
}

#[cfg(test)]
#[path = "write_refusal_tests.rs"]
mod tests;
