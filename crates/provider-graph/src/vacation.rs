//! The out-of-office reply over Microsoft Graph, as the neutral [`Vacation`]
//! (`filters-and-out-of-office.md`, §3 "Out-of-office" and §5).
//!
//! - **Read** from `{root}/mailboxSettings/automaticRepliesSetting` and **written** with a `PATCH`
//!   of `{root}/mailboxSettings` carrying that one property.
//! - **Canonical on read**, so a reply that says the same reads the same: `status` `disabled` is
//!   off, `alwaysEnabled` on with no window, `scheduled` on within its two ends; the zoned schedule
//!   is read as instants (a cut-off, not a wall-clock event), its zone kept in the raw form. Each
//!   message is one HTML string, read as the HTML body. `externalAudience` `all` is everyone,
//!   `none` the organization, `contactsOnly` the organization and contacts; an external message
//!   equal to the internal one, or one nobody outside gets (`none`), is no outside reply.
//! - **Written as the model says**: a text body alone is sent as HTML, escaped; a reply off keeps
//!   its window where it has one, as the schedule Graph keeps beside `disabled`; with no outside
//!   reply the external message is the internal one.
//! - **No write guard on the server**: the setting carries no ETag. The version is a SHA-256 of the
//!   setting as read, and a guarded write reads it again and refuses as `Conflict` when it moved
//!   ([`WriteGuard::Absent`]). That narrows the race and no more: Graph applies a `mailboxSettings`
//!   write some time after answering `200`, a read straight after it can show the write while a
//!   later read shows the one before, and two writes in quick succession may land in either order
//!   (observed). So the check can pass on a read the server has not settled, and the receipt
//!   carries no version: a read-back proves nothing.
//! - **Apart from the rules**: Exchange keeps the reply beside the inbox rules, so a write of one
//!   never touches the other ([`RulesEffect::Untouched`]).

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    raw::RawVacation,
    time::{LocalDateTime, TimeZoneId, UtcDateTime, resolve_zone_name},
    vacation::{
        AutoReply, ReplyAudience, RulesEffect, VacationReceipt, VacationResponse,
        VacationVocabulary, VacationWindow, WindowSupport,
    },
    version::VacationVersion,
};
use engine_provider::{
    ProviderError, ProviderResult, Vacation, VacationPrecondition, WriteGuard,
    refuse_unheld_vacation,
};
use serde_json::{Map, Value, json};

use crate::{
    error::GraphError,
    json::{digest_hex, opt_str},
    provider::GraphProvider,
    transport::GraphClient,
};

const SETTING: &str = "/mailboxSettings/automaticRepliesSetting";

/// What Graph's reply holds: both ends of a window or neither, no subject, one (HTML) body per
/// message, three audiences, and a message of its own for senders outside the organization.
#[must_use]
pub(crate) fn vocabulary() -> VacationVocabulary {
    VacationVocabulary::everyone()
        .with_window(WindowSupport::BothEnds)
        .with_audience(ReplyAudience::Organization)
        .with_audience(ReplyAudience::OrganizationAndContacts)
        .with_outside_reply()
}

#[async_trait]
impl Vacation for GraphProvider {
    async fn vocabulary(&self, _account: &AccountId) -> ProviderResult<VacationVocabulary> {
        Ok(vocabulary())
    }

    /// [`WriteGuard::Absent`]: the setting has no ETag. A guarded write still refuses a reply
    /// that moved, as read again just before it writes (the module docs).
    fn vacation_write_guard(&self) -> WriteGuard {
        WriteGuard::Absent
    }

    async fn read_vacation(&self, _account: &AccountId) -> ProviderResult<VacationResponse> {
        Ok(read(self.client()).await?)
    }

    async fn set_vacation(
        &self,
        _account: &AccountId,
        precondition: VacationPrecondition<'_>,
        new: &VacationResponse,
    ) -> ProviderResult<VacationReceipt> {
        refuse_unheld_vacation(&vocabulary(), new)?;
        let client = self.client();
        if let VacationPrecondition::IfUnchanged(read_as) = precondition {
            let Some(version) = &read_as.version else {
                return Err(ProviderError::permanent(
                    "a guarded write of the out-of-office reply needs the version it was read at; \
                     read it first, or write unconditionally",
                ));
            };
            if read(client).await?.version.as_ref() != Some(version) {
                return Err(ProviderError::conflict(
                    "the out-of-office reply changed since it was read; read it again",
                ));
            }
        }
        let body = json!({ "automaticRepliesSetting": setting(new) });
        let body = serde_json::to_vec(&body).map_err(GraphError::from)?;
        client
            .patch(
                &client.url("/mailboxSettings"),
                "application/json",
                None,
                body,
            )
            .await
            .map_err(GraphError::into_write_error)?;
        // Graph applies a `mailboxSettings` write after answering it, and two quick writes may
        // land out of order (observed), so a read now proves nothing: the receipt names no
        // version, and the next guarded write reads first.
        Ok(VacationReceipt::new(None, RulesEffect::Untouched))
    }
}

async fn read(client: &GraphClient) -> ProviderResult<VacationResponse> {
    reply(&client.get(&client.url(SETTING)).await?)
}

/// The reply a read `automaticRepliesSetting` says, with its version and raw form.
///
/// # Errors
///
/// A protocol error for a setting not as Graph documents it, and
/// [`item_unavailable`](ProviderError::item_unavailable) for a schedule in a zone the engine
/// cannot resolve: when that reply runs cannot be said, and a guess could answer senders the user
/// did not mean to.
pub(crate) fn reply(setting: &Value) -> ProviderResult<VacationResponse> {
    let mut kept = setting
        .as_object()
        .cloned()
        .ok_or_else(|| GraphError::protocol("automaticRepliesSetting is not an object"))?;
    // The context names the mailbox; it is not part of the setting.
    kept.remove("@odata.context");
    let raw = Value::Object(kept).to_string();
    let status = opt_str(setting, "status").unwrap_or("disabled");
    let (enabled, scheduled) = match status {
        "disabled" => (false, false),
        "alwaysEnabled" => (true, false),
        "scheduled" => (true, true),
        _ => return Err(GraphError::protocol("unknown automatic replies status").into()),
    };
    let mut reply = VacationResponse::default();
    if enabled {
        reply = reply.turned_on();
    }
    if scheduled {
        let window = VacationWindow::default()
            .with_start(instant(setting, "scheduledStartDateTime", Edge::Start)?)
            .with_end(instant(setting, "scheduledEndDateTime", Edge::End)?);
        reply = reply.with_window(window);
    }
    let internal = message(setting, "internalReplyMessage");
    let external = message(setting, "externalReplyMessage");
    let audience = match opt_str(setting, "externalAudience") {
        Some("all") => ReplyAudience::Everyone,
        Some("contactsOnly") => ReplyAudience::OrganizationAndContacts,
        // `none`, and anything unknown: the narrowest audience, never a wider one.
        _ => ReplyAudience::Organization,
    };
    reply = reply
        .with_reply(body(internal))
        .with_audience(audience)
        .with_version(VacationVersion::new(digest_hex(&raw)))
        .with_raw(RawVacation::new(raw));
    if audience != ReplyAudience::Organization && external != internal {
        reply = reply.with_outside_reply(body(external));
    }
    Ok(reply)
}

fn message<'a>(setting: &'a Value, key: &str) -> Option<&'a str> {
    opt_str(setting, key).filter(|text| !text.is_empty())
}

fn body(html: Option<&str>) -> AutoReply {
    html.map_or_else(AutoReply::default, |html| {
        AutoReply::default().with_html(html)
    })
}

/// Which end of the window a wall time is.
#[derive(Clone, Copy)]
enum Edge {
    Start,
    End,
}

/// A `dateTimeTimeZone` as the instant it names. A wall time the zone skips or repeats names two
/// instants; the start takes the later and the end the earlier, so the window is never wider than
/// the schedule says (and one wholly inside a skipped hour runs at no instant).
fn instant(setting: &Value, key: &str, edge: Edge) -> ProviderResult<UtcDateTime> {
    let bad = || ProviderError::from(GraphError::protocol(format!("bad {key}")));
    let value = setting.get(key).ok_or_else(bad)?;
    let local: LocalDateTime = opt_str(value, "dateTime")
        .ok_or_else(bad)?
        .parse()
        .map_err(|_| bad())?;
    let unknown = || {
        ProviderError::item_unavailable(
            "this out-of-office reply's schedule names a time zone the engine cannot resolve",
        )
    };
    let zone = resolve_zone_name(opt_str(value, "timeZone").unwrap_or("UTC"))
        .ok()
        .filter(TimeZoneId::is_iana)
        .ok_or_else(unknown)?;
    let tz = jiff::tz::TimeZone::get(zone.as_str()).map_err(|_| unknown())?;
    let civil = to_civil(local).ok_or_else(bad)?;
    let ambiguous = tz.to_ambiguous_timestamp(civil);
    let (Ok(a), Ok(b)) = (ambiguous.earlier(), ambiguous.later()) else {
        return Err(bad());
    };
    let at = match edge {
        Edge::Start => a.max(b),
        Edge::End => a.min(b),
    };
    UtcDateTime::parse_rfc3339(&at.to_string()).map_err(|_| bad())
}

fn to_civil(local: LocalDateTime) -> Option<jiff::civil::DateTime> {
    jiff::civil::DateTime::new(
        i16::try_from(local.year()).ok()?,
        i8::try_from(local.month()).ok()?,
        i8::try_from(local.day()).ok()?,
        i8::try_from(local.hour()).ok()?,
        i8::try_from(local.minute()).ok()?,
        i8::try_from(local.second()).ok()?,
        i32::try_from(local.nanosecond()).ok()?,
    )
    .ok()
}

/// The `automaticRepliesSetting` that says `reply`; the caller has refused what Graph cannot
/// hold ([`refuse_unheld_vacation`]).
pub(crate) fn setting(reply: &VacationResponse) -> Value {
    let mut out = Map::new();
    let window = reply.window;
    let status = match (reply.enabled, window.is_open()) {
        (false, _) => "disabled",
        (true, true) => "alwaysEnabled",
        (true, false) => "scheduled",
    };
    out.insert("status".to_owned(), json!(status));
    if let (Some(start), Some(end)) = (window.start, window.end) {
        out.insert("scheduledStartDateTime".to_owned(), zoned(start));
        out.insert("scheduledEndDateTime".to_owned(), zoned(end));
    }
    let internal = html(&reply.reply);
    // With no outside reply, outside senders get the same one; where none of them gets a reply
    // at all, the model holds no external message, and the one stored is left as it is.
    let external = match (&reply.outside_reply, reply.audience) {
        (Some(outside), _) => Some(html(outside)),
        (None, ReplyAudience::Organization) => None,
        (None, _) => Some(internal.clone()),
    };
    let audience = match reply.audience {
        ReplyAudience::Organization => "none",
        ReplyAudience::OrganizationAndContacts => "contactsOnly",
        _ => "all",
    };
    out.insert("externalAudience".to_owned(), json!(audience));
    out.insert("internalReplyMessage".to_owned(), json!(internal));
    if let Some(external) = external {
        out.insert("externalReplyMessage".to_owned(), json!(external));
    }
    Value::Object(out)
}

/// One message as the HTML string Graph keeps: the HTML body, else the text escaped.
fn html(reply: &AutoReply) -> String {
    if let Some(html) = &reply.html {
        return html.clone();
    }
    let Some(text) = &reply.text else {
        return String::new();
    };
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("<br>"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// An instant as a Graph `dateTimeTimeZone` in UTC, to the 100 ns Graph keeps.
fn zoned(at: UtcDateTime) -> Value {
    let mut text = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        at.year(),
        at.month(),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    );
    if at.nanosecond() != 0 {
        use core::fmt::Write as _;
        let _ = write!(text, ".{:07}", at.nanosecond() / 100);
    }
    json!({ "dateTime": text, "timeZone": "UTC" })
}

#[cfg(test)]
#[path = "vacation_tests.rs"]
mod tests;
