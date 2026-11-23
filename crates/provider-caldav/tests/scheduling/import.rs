//! Importing a meeting tells nobody: proven against Stalwart's auto-scheduler, beside a control
//! that does tell.
//!
//! An absence proves nothing unless the same server, sent the same meeting without the
//! suppression, is seen to deliver it (`AGENTS.md`, "Test against the real server"). So each
//! direction runs twice on the two scratch accounts:
//!
//! - **The organizer's copy.** Bob stores a meeting inviting Carol. Stored as a plain guarded
//!   create, the server puts it on Carol's calendar, in her scheduling inbox and in her mailbox.
//!   Imported, the same meeting under another `UID` reaches none of the three in five times the
//!   time the control's mail took (its last delivery), and never under three seconds.
//!
//! The attendee's direction has no such proof here, and that is recorded rather than asserted:
//! Carol storing her accepted copy of Bob's meeting plainly, as the control, made Stalwart send
//! Bob no scheduling-inbox `REPLY` within 30 s (S3 saw the same), so an import arm there could
//! only show an absence the server would have produced anyway. The stored copy still carries
//! `SCHEDULE-AGENT=CLIENT` on both parties (`tests/common/import.rs`), which is what RFC 6638 makes
//! a server honour for a reply.
//! The scratch accounts take the mail, as the parent module explains; each step removes what it
//! made, the control's deliveries included.

use core::time::Duration;
use std::time::Instant;

use engine_core::{ids::Uid, raw::RawIcal};
use engine_provider::{CalendarWrites, EventWrite, Provider, import::EventImport};

use super::{Parties, clean_up, invitation, poll_until, scheduling_inbox_hrefs};
use crate::common;

/// The least time the import arm watches for a delivery that must not come.
const LEAST_WATCH: Duration = Duration::from_secs(3);

/// A summary no earlier run used, so the mailbox check finds this run's mail alone.
fn summary(what: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("Import silence {what} {nanos}")
}

/// Whether `who`'s mailbox holds mail whose subject contains `text`.
fn mailed(parties: &Parties, who: &stalwart_harness::ScratchAccount, text: &str) -> bool {
    !parties
        .harness
        .emails_with_subject_as(who.auth(), text)
        .expect("Email/query")
        .is_empty()
}

/// Removes the mail a control delivered.
fn unmail(parties: &Parties, who: &stalwart_harness::ScratchAccount, text: &str) {
    let ids = parties
        .harness
        .emails_with_subject_as(who.auth(), text)
        .expect("Email/query");
    parties
        .harness
        .destroy_emails_as(who.auth(), &ids)
        .expect("Email/set destroy");
}

/// Waits until `ready` holds, failing after the scheduling suite's delivery timeout.
async fn until(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Bob invites Carol: the control delivers, the import does not.
async fn an_imported_invitation_reaches_nobody(parties: &Parties) {
    // The control: the meeting stored as a create, with nothing suppressing scheduling.
    let control = Uid::new("caldav-import-control@test.local").unwrap();
    let control_summary = summary("control");
    clean_up(parties, &control).await;
    let started = Instant::now();
    let href = parties.organizer.event_href(&control).unwrap();
    parties
        .organizer
        .put_event(
            &parties.organizer_account,
            &EventWrite::creating(
                href,
                control.clone(),
                RawIcal::new(invitation(
                    control.as_str(),
                    &control_summary,
                    "Europe/Amsterdam",
                    parties,
                )),
            ),
        )
        .await
        .expect("the organizer stores the control");
    poll_until(
        &parties.attendee,
        &parties.attendee_account,
        &control,
        "the control reaches the attendee's calendar",
        |_| true,
    )
    .await;
    assert!(
        !scheduling_inbox_hrefs(parties, control.as_str()).is_empty(),
        "the control reaches the attendee's scheduling inbox"
    );
    until("the control's invitation mail", || {
        mailed(parties, &parties.attendee_auth, &control_summary)
    })
    .await;
    // The mail is the last of the three deliveries to land, so the quiet window is measured
    // from it: a window shorter than the mail takes could not see the mail not come.
    let delivered = started.elapsed();
    clean_up(parties, &control).await;
    unmail(parties, &parties.attendee_auth, &control_summary);

    // The import: the same meeting, under another UID, through the importer.
    let imported = Uid::new("caldav-import-silent@test.local").unwrap();
    let imported_summary = summary("import");
    clean_up(parties, &imported).await;
    let answers = parties
        .organizer
        .calendar_importer()
        .expect("CalDAV imports")
        .import_events(
            &parties.organizer_account,
            &[EventImport::new(
                RawIcal::new(invitation(
                    imported.as_str(),
                    &imported_summary,
                    "Europe/Amsterdam",
                    parties,
                )),
                parties.organizer.calendar_id(),
            )],
        )
        .await;
    assert!(
        !answers[0].as_ref().expect("imported").already_there,
        "the import stored the meeting"
    );
    let watch = (delivered * 5).max(LEAST_WATCH);
    let until_quiet = Instant::now() + watch;
    while Instant::now() < until_quiet {
        assert!(
            common::fetch(
                &parties.attendee,
                &parties.attendee_account,
                imported.as_str()
            )
            .await
            .is_none(),
            "an imported meeting must not reach the attendee's calendar"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        scheduling_inbox_hrefs(parties, imported.as_str()).is_empty(),
        "nor the attendee's scheduling inbox"
    );
    assert!(
        !mailed(parties, &parties.attendee_auth, &imported_summary),
        "nor the attendee's mailbox"
    );
    assert!(
        common::fetch(
            &parties.organizer,
            &parties.organizer_account,
            imported.as_str()
        )
        .await
        .is_some(),
        "while the organizer's calendar holds it"
    );
    clean_up(parties, &imported).await;
}

/// Calendar import against Stalwart (`common::import`), in the scratch account Bob's home, so
/// the seeded account's calendar list is never touched. Here rather than in `live_caldav.rs`,
/// which is at the file-length limit, because this module already holds the scratch accounts.
#[tokio::test]
async fn caldav_import_round_trip() {
    let Some(parties) = super::parties("caldav_import_round_trip").await else {
        return;
    };
    let _serial = common::serial_guard().await;
    let server = common::import::DavServer {
        base: format!("http://{}", parties.harness.http_addr),
        user: parties.organizer_auth.address.clone(),
        pass: parties.organizer_auth.password.clone(),
    };
    common::import::import_round_trip(&parties.organizer, &parties.organizer_account, &server)
        .await;
}

/// An imported meeting tells nobody, beside a control that does.
#[tokio::test]
async fn caldav_an_imported_meeting_tells_nobody() {
    let Some(parties) = super::parties("caldav_an_imported_meeting_tells_nobody").await else {
        return;
    };
    let _serial = common::serial_guard().await;
    an_imported_invitation_reaches_nobody(&parties).await;
}
