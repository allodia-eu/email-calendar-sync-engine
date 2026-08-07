//! The commands that go through the **real** [`CalDavProvider`].
//!
//! That is the whole point of this tool, and the reason it lives in the workspace rather than
//! beside the detached OAuth helpers: what it prints is what the engine does. A debugging tool
//! with its own HTTP client and its own iCalendar parser answers questions about *itself* —
//! and will happily report a server behaviour the adapter never sees, or miss one it does.
//!
//! The one deliberate exception is [`crate::raw`], which is labelled as being outside the
//! adapter precisely because some questions (a scheduling-inbox `PROPFIND`, a `.well-known`
//! redirect) cannot be asked through a typed calendar API at all.

use engine_core::{
    calendar::Event,
    ids::AccountId,
    raw::{RawIcal, RawMime},
    sync::SyncUpdate,
};
use engine_ical::{Document, Edit, Edits, LineEdit};
use engine_provider::{EventRsvp, EventWrite, Provider, ReplyDelivery, RsvpResponse};
use provider_caldav::{CalDavConfig, CalDavProvider, Credentials, imip, schedule_status};

use crate::profile::Profile;

/// The account id every command runs under. Nothing here persists, so it only has to be
/// stable and recognisable in a log.
fn account() -> AccountId {
    AccountId::try_from("dav-cli").expect("a constant account id is valid")
}

/// Connects and discovers, printing what the adapter concluded about the server.
///
/// The capability line is the first thing worth knowing about an unfamiliar server, and it
/// decides whether the RSVP verdict is even askable: `calendar_scheduling` is discovered from
/// the `calendar-auto-schedule` token of RFC 6638 §2, and where it is false, nothing reports
/// delivery because nothing schedules.
pub(crate) async fn connect(profile: &Profile) -> Result<CalDavProvider, String> {
    let mut config = CalDavConfig::new(
        profile.url.clone(),
        Credentials::Basic {
            username: profile.user.clone(),
            password: profile.pass.clone(),
        },
    );
    if let Some(calendar) = &profile.calendar {
        config = config.with_calendar(calendar.clone());
    }
    CalDavProvider::connect(config)
        .await
        .map_err(|err| format!("connect failed: {err}"))
}

/// Prints what discovery learned: the capabilities that decide what the other commands can do.
pub(crate) fn describe(provider: &CalDavProvider) {
    let capabilities = provider.connection_info().capabilities;
    println!("  calendars           {}", capabilities.calendars());
    println!("  calendar writes     {}", capabilities.calendar_writes());
    println!(
        "  auto-scheduling     {}   (RFC 6638; false ⇒ nothing will report a reply verdict)",
        capabilities.calendar_scheduling()
    );
    match capabilities.calendar_rsvp() {
        Some(controls) => println!(
            "  rsvp controls       comment={} suppress={} guard={:?}",
            controls.comment, controls.suppress_notification, controls.guard
        ),
        None => println!("  rsvp controls       (cannot answer invitations)"),
    }
}

/// Every event on the bound collection, as the server currently stores it.
pub(crate) async fn events(provider: &CalDavProvider) -> Result<Vec<Event>, String> {
    let synced = provider
        .sync_events(&account(), None)
        .await
        .map_err(|err| format!("sync failed: {err}"))?;
    Ok(match synced.update {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { changed, .. } => changed,
    })
}

/// One line per event, plus the reply verdict its stored bytes currently carry.
///
/// The verdict is read with the adapter's own parser, so `list` and an actual RSVP can never
/// disagree about what a server said.
pub(crate) fn print_events(events: &[Event], me: &str) {
    println!("{} event(s):\n", events.len());
    for event in events {
        let verdict = event
            .raw_ical
            .as_ref()
            .map_or(ReplyDelivery::NotReported, |raw| {
                schedule_status::reply_delivery(raw.as_str())
            });
        let mine = event.participants.iter().find(|p| {
            p.email
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(me))
        });
        println!("  {}", event.title);
        println!("    uid        {}", event.uid.as_str());
        println!(
            "    my answer  {}",
            mine.map_or_else(
                || "(not an attendee)".to_owned(),
                |p| format!("{:?}", p.participation_status)
            )
        );
        println!("    reply      {}", describe_verdict(&verdict));
        println!();
    }
}

/// A verdict in words, with the distinction that matters spelled out rather than implied.
pub(crate) fn describe_verdict(verdict: &ReplyDelivery) -> String {
    match verdict {
        ReplyDelivery::Delivered { status } => {
            format!("DELIVERED — the server says the organizer was told (status {status})")
        }
        ReplyDelivery::Failed { status } => {
            format!("FAILED — the server says it could NOT tell the organizer (status {status})")
        }
        ReplyDelivery::Unrecognized { status } => {
            format!("UNRECOGNIZED — the server reported {status:?}, which we do not classify")
        }
        ReplyDelivery::NotReported => {
            "not reported — no information. NOT a success, and not a failure.".to_owned()
        }
    }
}

/// Answers an invitation and prints what the server reported about delivering the reply.
///
/// This is a real RSVP against a real server: on an auto-scheduling one it emails the
/// organizer. The caller is expected to have said so out loud first.
pub(crate) async fn rsvp(
    provider: &CalDavProvider,
    events: &[Event],
    uid: &str,
    response: RsvpResponse,
    me: &str,
) -> Result<(), String> {
    let event = events
        .iter()
        .find(|event| event.uid.as_str() == uid)
        .ok_or_else(|| format!("no event with UID {uid} on this calendar"))?;

    let receipt = provider
        .rsvp_event(&account(), event, &EventRsvp::to(event, me, response))
        .await
        .map_err(|err| format!("the RSVP was refused: {err}"))?;

    println!("answered  {}", event.title);
    println!("revision  {:?}", receipt.revisions.etag);
    println!("reply     {}", describe_verdict(&receipt.reply_delivery));
    if receipt.reply_delivery.failed() {
        println!(
            "\n  The answer IS stored — this is about delivery, not the write. The organizer\n  \
             does not know, and will not find out by waiting."
        );
    }
    Ok(())
}

/// Puts an invitation onto the calendar as a **guarded create** — the attendee flow of RFC
/// 6638 §3.2.2, and what the product core does when a server files no iMIP mail itself.
///
/// Accepts an `.eml` (the invitation as it arrived, whose `text/calendar` part is found with
/// the engine's own `extract_calendar_part`) or a bare `.ics`. The transit-only `METHOD` is
/// stripped, because RFC 4791 §4.1 forbids it on a stored resource — that is what makes the
/// bytes an event rather than an iTIP message, and a conforming server refuses the `PUT`
/// without the strip.
///
/// `If-None-Match: *` rather than an unconditional write: the interesting failure is
/// something already being there, and an unconditional `PUT` would overwrite it silently.
pub(crate) async fn store(provider: &CalDavProvider, path: &str) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|err| format!("cannot read {path}: {err}"))?;
    let text = if starts_with_calendar(&bytes) {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        engine_mime::extract_calendar_part(&RawMime::new(bytes))
            .ok_or_else(|| format!("no text/calendar part in {path}"))?
            .text()
            .to_owned()
    };

    let message = imip::parse(&text).map_err(|err| format!("not a scheduling object: {err}"))?;
    let uid = message.event.uid.clone();
    let stored = strip_method(&text);

    let href = provider
        .event_href(&uid)
        .map_err(|err| format!("cannot mint an href for this UID: {err}"))?;
    println!("uid       {}", uid.as_str());
    println!("href      {}", href.as_str());
    println!("method    {:?} (stripped before storing)", message.method);

    provider
        .put_event(
            &account(),
            &EventWrite::creating(href, uid, RawIcal::new(stored)),
        )
        .await
        .map_err(|err| format!("the guarded create was refused: {err}"))?;
    println!("\nstored. `dav list` will now show it; `dav rsvp <uid> accept` answers it.");
    Ok(())
}

/// Whether the bytes are already an iCalendar document rather than a mail message.
fn starts_with_calendar(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(64)]);
    head.trim_start()
        .to_ascii_uppercase()
        .starts_with("BEGIN:VCALENDAR")
}

/// The document with its `METHOD` property removed (RFC 4791 §4.1).
fn strip_method(ical: &str) -> String {
    let doc = Document::parse(ical);
    let mut edits = Edits::new();
    for index in 0..doc.len() {
        let logical = doc.logical(index);
        let name_end = logical.find([';', ':']).unwrap_or(logical.len());
        if logical[..name_end].trim().eq_ignore_ascii_case("METHOD") {
            edits.insert(
                index,
                Edit {
                    before: String::new(),
                    line: LineEdit::Remove,
                },
            );
        }
    }
    doc.render(&edits)
}
