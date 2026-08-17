//! A Graph message's **mutable half**: the keywords its state booleans imply, and the
//! revision tokens a conditional write quotes.
//!
//! Split from [`crate::normalize`] because both the whole-object path and the delta's
//! state changes read it, and the two disagreeing about what "read" means is the bug
//! this file exists to prevent. Everything here is derived from the properties in
//! [`MESSAGE_STATE_SELECT`], so a state-only read answers it in full.

use std::collections::BTreeSet;

use engine_core::{
    mail::{Keyword, MailState, SystemKeyword},
    version::{ChangeKey, ETag, RevisionTokens},
};
use serde_json::Value;

use crate::{
    error::GraphError,
    json::{bool_field, datetime, opt_str},
};

/// The `$select` for a **state-only** read: the mutable half, plus the tokens and
/// timestamp that move with it. What an etag-less delta entry costs to resolve, instead
/// of [`MESSAGE_SELECT`](crate::normalize::MESSAGE_SELECT)'s whole message.
///
/// `@odata.etag` is deliberately **not** in this list and cannot be: it is an OData
/// annotation, not a property, and naming it in a `$select` is an error. The live service
/// returns it on a `$select`ed single-entity `GET` regardless — captured in
/// `tests/fixtures/mail/message_state.json` and asserted both offline and live, because
/// the etag is the token an `If-Match` quotes and nothing in the request asks for it.
/// Should that ever change, the store keeps the stored token rather than blanking it (see
/// [`RevisionTokens::or`](engine_core::version::RevisionTokens::or)), so the failure is a
/// stale guard rather than no guard.
pub(crate) const MESSAGE_STATE_SELECT: &[&str] = &[
    "id",
    "isRead",
    "isDraft",
    "flag",
    "lastModifiedDateTime",
    "changeKey",
];

/// The keywords a message's state properties imply. Graph models read/draft/flag as its
/// own booleans, not a keyword set.
pub(crate) fn keywords_from_json(value: &Value) -> BTreeSet<Keyword> {
    let mut keywords = BTreeSet::new();
    if bool_field(value, "isRead") {
        keywords.insert(Keyword::system(SystemKeyword::Seen));
    }
    if bool_field(value, "isDraft") {
        keywords.insert(Keyword::system(SystemKeyword::Draft));
    }
    if flag_is_flagged(value) {
        keywords.insert(Keyword::system(SystemKeyword::Flagged));
    }
    keywords
}

/// The complete mutable state of one message, read through [`MESSAGE_STATE_SELECT`].
///
/// It carries the revision tokens deliberately: a state change replaces the row's, and a
/// `MailState` that left them empty would blank the `changeKey` a later conditional write
/// quotes.
///
/// # Errors
///
/// Returns [`GraphError::Protocol`] if `lastModifiedDateTime` is malformed.
pub(crate) fn state_from_json(value: &Value) -> Result<MailState, GraphError> {
    Ok(MailState::with_keywords(keywords_from_json(value))
        .revised(revisions(value), datetime(value, "lastModifiedDateTime")?))
}

/// The revision tokens Graph supplies: the `@odata.etag` and the `changeKey` (both
/// requested in either `$select`). Absent on a delta *partial* entry that did not change
/// them. JMAP-style accounts carry none.
pub(crate) fn revisions(value: &Value) -> RevisionTokens {
    RevisionTokens {
        etag: opt_str(value, "@odata.etag").map(ETag::new),
        change_key: opt_str(value, "changeKey").map(ChangeKey::new),
        ..RevisionTokens::none()
    }
}

/// `true` when `flag.flagStatus == "flagged"`.
fn flag_is_flagged(value: &Value) -> bool {
    value
        .get("flag")
        .and_then(|flag| flag.get("flagStatus"))
        .and_then(Value::as_str)
        == Some("flagged")
}
