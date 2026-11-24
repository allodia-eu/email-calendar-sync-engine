//! Gmail's vacation reply, as the neutral [`Vacation`] (`filters-and-out-of-office.md`, §3
//! "Out-of-office" and §5).
//!
//! - **Read** with `users.settings.getVacation` and **written** with `updateVacation`, a `PUT` of
//!   the whole setting: a part the model leaves out is sent as absent, which clears it.
//! - **As the model says**: `enableAutoReply` is whether it runs; `startTime` and `endTime`
//!   (milliseconds since the epoch) are the window, either end optional; `responseSubject` the
//!   subject, which Gmail puts before the subject of the message answered rather than in its place;
//!   `responseBodyPlainText` or `responseBodyHtml` the body. **One body**: where both are sent,
//!   Gmail keeps the HTML and drops the text (observed), so a reply with both is refused before
//!   sending ([`ReplyBodies::One`](engine_core::vacation::ReplyBodies::One)).
//! - **The audience** is `restrictToContacts` ([`Contacts`](ReplyAudience::Contacts)),
//!   `restrictToDomain` ([`Organization`](ReplyAudience::Organization)), both
//!   ([`ContactsInOrganization`](ReplyAudience::ContactsInOrganization)), or neither. The domain
//!   restriction is a Workspace account's alone, so the vocabulary asks the account's affiliation
//!   (`crate::affiliation`), and a write asks it only for a reply limited to the organization. A
//!   token that may not ask gets the narrower, personal vocabulary.
//! - **The window is never written wider than asked**: Gmail keeps milliseconds, so a start is
//!   rounded up and an end down, and a window that then holds no whole millisecond is refused.
//! - **No write guard on the server**: the setting carries no ETag. The version is a SHA-256 of the
//!   setting as read, and a guarded write reads it again and refuses as `Conflict` when it moved
//!   ([`WriteGuard::Absent`]): that narrows the race and no more. The receipt carries no version.
//! - **Apart from the filters**: a write of the reply never touches them
//!   ([`RulesEffect::Untouched`]).

use async_trait::async_trait;
use engine_core::{
    affiliation::Affiliation,
    ids::AccountId,
    raw::RawVacation,
    time::UtcDateTime,
    vacation::{
        AutoReply, ReplyAudience, RulesEffect, VacationReceipt, VacationResponse,
        VacationVocabulary, VacationWindow, WindowSupport,
    },
    version::VacationVersion,
};
use engine_provider::{
    ProviderError, ProviderResult, Vacation, VacationPrecondition, WriteGuard, digest_hex,
    refuse_unheld_vacation,
};
use serde_json::{Map, Value, json};

use crate::{error::GoogleError, json::opt_str, provider::GmailProvider, transport::GoogleClient};

const SETTING: &str = "/gmail/v1/users/me/settings/vacation";

/// What Gmail's reply holds: either end of a window, a subject, one body (text or HTML), and
/// replies limited to contacts; on a Workspace account also to the organization, and to contacts
/// in it.
#[must_use]
pub(crate) fn vocabulary(workspace: bool) -> VacationVocabulary {
    let personal = VacationVocabulary::everyone()
        .with_window(WindowSupport::EitherEnd)
        .with_subject()
        .with_audience(ReplyAudience::Contacts);
    if workspace {
        personal
            .with_audience(ReplyAudience::Organization)
            .with_audience(ReplyAudience::ContactsInOrganization)
    } else {
        personal
    }
}

/// Whether the account is a Workspace one, which alone can limit a reply to its domain. A token
/// that may not ask (`401` or `403` from `userinfo`, a grant without `userinfo.email`) answers
/// `false`: the narrower vocabulary, so a reply limited to the organization is refused as one the
/// account is not known to hold, rather than every vocabulary question failing.
async fn workspace(client: &GoogleClient) -> ProviderResult<bool> {
    match client.affiliation().await {
        Ok(affiliation) => Ok(matches!(affiliation, Affiliation::Organization(_))),
        Err(GoogleError::Status {
            status: 401 | 403, ..
        }) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

#[async_trait]
impl Vacation for GmailProvider {
    async fn vocabulary(&self, _account: &AccountId) -> ProviderResult<VacationVocabulary> {
        Ok(vocabulary(workspace(self.client()).await?))
    }

    /// [`WriteGuard::Absent`]: the setting has no ETag. A guarded write still refuses a reply
    /// that moved, as read again just before it writes (the module docs).
    fn vacation_write_guard(&self) -> WriteGuard {
        WriteGuard::Absent
    }

    async fn read_vacation(&self, _account: &AccountId) -> ProviderResult<VacationResponse> {
        read(self.client()).await
    }

    async fn set_vacation(
        &self,
        _account: &AccountId,
        precondition: VacationPrecondition<'_>,
        new: &VacationResponse,
    ) -> ProviderResult<VacationReceipt> {
        let client = self.client();
        let organization = matches!(
            new.audience,
            ReplyAudience::Organization | ReplyAudience::ContactsInOrganization
        );
        let workspace = organization && workspace(client).await?;
        refuse_unheld_vacation(&vocabulary(workspace), new)?;
        let body = setting(new)?;
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
        let put = client
            .put(
                &client.url(SETTING),
                "application/json",
                body.to_string().into_bytes(),
            )
            .await;
        GoogleError::optional_answer(put)?;
        Ok(VacationReceipt::new(None, RulesEffect::Untouched))
    }
}

async fn read(client: &GoogleClient) -> ProviderResult<VacationResponse> {
    reply(&client.get(&client.url(SETTING)).await?)
}

/// The reply a read `VacationSettings` says, with its version and raw form.
///
/// # Errors
///
/// A protocol error for a setting not as Gmail documents it.
pub(crate) fn reply(setting: &Value) -> ProviderResult<VacationResponse> {
    let fields = setting
        .as_object()
        .ok_or_else(|| GoogleError::protocol("vacation settings are not an object"))?;
    let raw = setting.to_string();
    let flag = |key: &str| fields.get(key).and_then(Value::as_bool) == Some(true);
    let text = |key: &str| opt_str(setting, key).filter(|text| !text.is_empty());
    let mut window = VacationWindow::default();
    if let Some(start) = instant(setting, "startTime")? {
        window = window.with_start(start);
    }
    if let Some(end) = instant(setting, "endTime")? {
        window = window.with_end(end);
    }
    let mut body = AutoReply::default();
    if let Some(subject) = text("responseSubject") {
        body = body.with_subject(subject);
    }
    if let Some(plain) = text("responseBodyPlainText") {
        body = body.with_text(plain);
    }
    if let Some(html) = text("responseBodyHtml") {
        body = body.with_html(html);
    }
    let audience = match (flag("restrictToContacts"), flag("restrictToDomain")) {
        (false, false) => ReplyAudience::Everyone,
        (true, false) => ReplyAudience::Contacts,
        (false, true) => ReplyAudience::Organization,
        (true, true) => ReplyAudience::ContactsInOrganization,
    };
    let mut out = VacationResponse::default()
        .with_window(window)
        .with_reply(body)
        .with_audience(audience)
        .with_version(VacationVersion::new(digest_hex(&raw)))
        .with_raw(RawVacation::new(raw));
    if flag("enableAutoReply") {
        out = out.turned_on();
    }
    Ok(out)
}

/// An instant Gmail states in milliseconds since the epoch, as a string or a number.
fn instant(setting: &Value, key: &str) -> ProviderResult<Option<UtcDateTime>> {
    let bad = || ProviderError::from(GoogleError::protocol(format!("bad {key}")));
    let millis = match setting.get(key) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(text)) => text.parse::<u64>().map_err(|_| bad())?,
        Some(value) => value.as_u64().ok_or_else(bad)?,
    };
    epoch()
        .checked_add(core::time::Duration::from_millis(millis))
        .map(Some)
        .ok_or_else(bad)
}

fn epoch() -> UtcDateTime {
    UtcDateTime::new(1970, 1, 1, 0, 0, 0).expect("the epoch is a valid instant")
}

/// Which way an instant is rounded to Gmail's milliseconds: so the window is never wider.
#[derive(Clone, Copy)]
enum Round {
    Up,
    Down,
}

/// `at` in milliseconds since the epoch, rounded as `round` says; `None` before the epoch.
fn millis(at: UtcDateTime, round: Round) -> Option<u64> {
    let month = time::Month::try_from(at.month()).ok()?;
    let nanos = time::Date::from_calendar_date(at.year(), month, at.day())
        .ok()?
        .with_hms_nano(at.hour(), at.minute(), at.second(), at.nanosecond())
        .ok()?
        .assume_utc()
        .unix_timestamp_nanos();
    let millis = match round {
        Round::Up => nanos.div_euclid(1_000_000) + i128::from(nanos.rem_euclid(1_000_000) != 0),
        Round::Down => nanos.div_euclid(1_000_000),
    };
    u64::try_from(millis).ok()
}

/// The `VacationSettings` that say `reply`; the caller has refused what Gmail cannot hold.
///
/// # Errors
///
/// `item_unavailable` for a window that holds no whole millisecond, or one before 1970.
pub(crate) fn setting(reply: &VacationResponse) -> ProviderResult<Value> {
    let mut out = Map::new();
    out.insert("enableAutoReply".to_owned(), json!(reply.enabled));
    let body = &reply.reply;
    for (key, value) in [
        ("responseSubject", &body.subject),
        ("responseBodyPlainText", &body.text),
        ("responseBodyHtml", &body.html),
    ] {
        if let Some(value) = value {
            out.insert(key.to_owned(), json!(value));
        }
    }
    let (contacts, domain) = match reply.audience {
        ReplyAudience::Contacts => (true, false),
        ReplyAudience::Organization => (false, true),
        ReplyAudience::ContactsInOrganization => (true, true),
        _ => (false, false),
    };
    out.insert("restrictToContacts".to_owned(), json!(contacts));
    out.insert("restrictToDomain".to_owned(), json!(domain));
    let unheld = || {
        ProviderError::item_unavailable(
            "this out-of-office reply's window cannot be held to the millisecond Gmail keeps",
        )
    };
    let start = match reply.window.start {
        Some(at) => Some(millis(at, Round::Up).ok_or_else(unheld)?),
        None => None,
    };
    let end = match reply.window.end {
        Some(at) => Some(millis(at, Round::Down).ok_or_else(unheld)?),
        None => None,
    };
    if let (Some(start), Some(end)) = (start, end)
        && end <= start
    {
        return Err(unheld());
    }
    for (key, value) in [("startTime", start), ("endTime", end)] {
        if let Some(value) = value {
            out.insert(key.to_owned(), json!(value.to_string()));
        }
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
#[path = "vacation_tests.rs"]
mod tests;
