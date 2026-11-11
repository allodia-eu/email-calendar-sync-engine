//! A JMAP calendar's `myRights` (JMAP Calendars `CalendarRights`), read as the caller's
//! [`CalendarAccess`] on it.

use engine_core::calendar::CalendarAccess;
use serde_json::Value;

/// Reads a JMAP Calendars `CalendarRights` object as [`CalendarAccess`], the way
/// `crate::mail` reads `MailboxRights`: a right missing from the object is withheld, the safe
/// direction for a permission, and no object at all is [`CalendarAccess::owner`], what every
/// caller assumed before rights were read.
///
/// `may_write` is `mayWriteAll` alone. `mayWriteOwn` covers only events the user owns, and a
/// host told it may write would offer to edit everyone's, which the server refuses.
/// `mayUpdatePrivate` and `maySetMetadata` have no neutral counterpart.
pub(crate) fn calendar_rights(rights: Option<&Value>) -> CalendarAccess {
    let Some(rights) = rights.filter(|rights| rights.is_object()) else {
        return CalendarAccess::owner();
    };
    let may = |name: &str| rights.get(name).and_then(Value::as_bool).unwrap_or(false);
    CalendarAccess {
        may_read: may("mayReadItems"),
        may_write: may("mayWriteAll"),
        may_share: may("mayShare"),
        may_delete: may("mayDelete"),
        may_rsvp: may("mayRSVP"),
        may_read_free_busy: may("mayReadFreeBusy"),
    }
}
