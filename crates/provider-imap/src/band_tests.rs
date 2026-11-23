//! The band's request shape and its watermark, against a scripted server. The bytes it answers
//! are canned, so what these assert is what was **sent**: the search criteria, the UID bound, and
//! where a resumed band starts.

use engine_core::{
    ids::MailboxId,
    sync::{MailboxWindows, SyncState, SyncWindow, Widening},
    time::CalendarDate,
};
use engine_provider::{EmailChunk, PassMode};
use futures_util::StreamExt;

use super::{carried, widen_email};
use crate::{
    capability::Negotiated,
    cursor::{CURSOR_VERSION, MailboxCursor},
    mock::{MockStream, script, written},
    transport::Connection,
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";

fn select_resp(tag: &str, validity: u32, uid_next: u32) -> String {
    format!(
        "* 9 EXISTS\r\n* OK [UIDVALIDITY {validity}] v\r\n\
         * OK [UIDNEXT {uid_next}] n\r\n{tag} OK [READ-WRITE] done\r\n"
    )
}

fn fetch_resp(tag: &str, uids: &[u32]) -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    for (index, uid) in uids.iter().enumerate() {
        write!(
            out,
            "* {} FETCH (UID {uid} FLAGS (\\Seen) \
             INTERNALDATE \"18-Feb-2026 10:00:00 +0000\" RFC822.SIZE 10 \
             ENVELOPE (NIL \"s{uid}\" NIL NIL NIL NIL NIL NIL NIL \"<m{uid}@h>\") \
             BODY[HEADER.FIELDS (REFERENCES CONTENT-TYPE CONTENT-DISPOSITION)] \"\")\r\n",
            index + 1
        )
        .unwrap();
    }
    write!(out, "{tag} OK FETCH done\r\n").unwrap();
    out
}

fn search_resp(tag: &str, uids: &[u32]) -> String {
    let listed: Vec<String> = uids.iter().map(u32::to_string).collect();
    format!("* SEARCH {}\r\n{tag} OK SEARCH done\r\n", listed.join(" "))
}

fn day(month: u8) -> CalendarDate {
    CalendarDate::new(2026, month, 1).unwrap()
}

/// From April back to January.
fn widening() -> Widening {
    Widening::new(
        SyncWindow::since(day(4)).into(),
        SyncWindow::since(day(1)).into(),
    )
    .unwrap()
}

fn kept(band_low: Option<u32>) -> MailboxCursor {
    MailboxCursor {
        uid_validity: 1000,
        uid_next: 40,
        highest_modseq: Some(7),
        backfill_low: None,
        reconciled: true,
        version: CURSOR_VERSION,
        band_low,
    }
}

/// The band watermark at `low`.
fn mark(low: u32) -> u32 {
    low
}

async fn band(
    server: &[&str],
    cursor: &MailboxCursor,
    widening: &Widening,
    limit: Option<&str>,
) -> (Vec<EmailChunk>, String) {
    let mut parts = vec![GREETING, LOGIN_OK];
    parts.extend_from_slice(server);
    let (stream, recorded) = MockStream::new(script(&parts));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    if let Some(limit) = limit {
        conn.negotiated = Negotiated::from_capabilities(&[limit.to_owned()]);
    }
    let mailbox = MailboxId::try_from("INBOX").unwrap();
    let state = cursor.encode();
    let chunks: Vec<EmailChunk> = widen_email(&mut conn, None, &mailbox, &state, widening, 2, 0)
        .map(Result::unwrap)
        .collect()
        .await;
    (chunks, written(&recorded))
}

#[tokio::test]
async fn a_band_searches_below_the_kept_uidnext_between_its_two_days() {
    let select = select_resp("a2", 1000, 41);
    let (chunks, sent) = band(
        &[
            &select,
            &search_resp("a3", &[5, 9, 12]),
            &fetch_resp("a4", &[9, 12]),
            &fetch_resp("a5", &[5]),
        ],
        &kept(None),
        &widening(),
        None,
    )
    .await;

    assert!(
        sent.contains("a3 UID SEARCH UID 1:39 SINCE 31-Dec-2025 BEFORE 2-Apr-2026\r\n"),
        "{sent}"
    );
    assert!(
        sent.contains("a4 UID FETCH 9,12 "),
        "newest group first: {sent}"
    );
    assert_eq!(chunks.len(), 2);
    assert!(chunks.iter().all(|chunk| chunk.mode == PassMode::Additive));
    assert!(chunks.iter().all(|chunk| chunk.present.is_empty()));
    assert_eq!(chunks[0].total, Some(3));
    assert_eq!(
        MailboxCursor::decode(chunks[0].advance_to.as_ref().unwrap()),
        Some(kept(Some(mark(9)))),
        "the group's boundary checkpoints the band watermark beside the kept cursor"
    );
    assert_eq!(
        chunks[1].advance_to.as_ref(),
        Some(&kept(None).encode()),
        "the band ends on the cursor it kept"
    );
}

#[tokio::test]
async fn a_band_resumes_below_its_watermark_only_when_the_widening_resumes() {
    let select = select_resp("a2", 1000, 41);
    let (_, sent) = band(
        &[&select, &search_resp("a3", &[5]), &fetch_resp("a4", &[5])],
        &kept(Some(mark(9))),
        &widening().resuming(),
        None,
    )
    .await;
    assert!(
        sent.contains("a3 UID SEARCH UID 1:8 SINCE 31-Dec-2025 BEFORE 2-Apr-2026\r\n"),
        "{sent}"
    );

    // Not told to resume: the watermark belongs to another widening, and the band starts afresh.
    let (_, sent) = band(
        &[&select, &search_resp("a3", &[])],
        &kept(Some(mark(9))),
        &widening(),
        None,
    )
    .await;
    assert!(
        sent.contains("a3 UID SEARCH UID 1:39 SINCE 31-Dec-2025 BEFORE 2-Apr-2026\r\n"),
        "a watermark the caller did not ask to continue was continued: {sent}"
    );

    let everything =
        Widening::new(SyncWindow::since(day(4)).into(), MailboxWindows::default()).unwrap();
    let (_, sent) = band(
        &[&select, &search_resp("a3", &[])],
        &kept(None),
        &everything,
        None,
    )
    .await;
    assert!(
        sent.contains("a3 UID SEARCH UID 1:39 BEFORE 2-Apr-2026\r\n"),
        "{sent}"
    );
}

#[tokio::test]
async fn an_empty_band_or_a_folder_that_gains_nothing_ends_on_the_kept_cursor() {
    let select = select_resp("a2", 1000, 41);
    let (chunks, _) = band(
        &[&select, &search_resp("a3", &[])],
        &kept(Some(mark(3))),
        &widening(),
        None,
    )
    .await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].advance_to.as_ref(), Some(&kept(None).encode()));
    assert_eq!(chunks[0].total, Some(0));

    // Only another mailbox is held further back: this folder has no band, and asks nothing.
    let sent_only = Widening::new(
        SyncWindow::since(day(4)).into(),
        MailboxWindows::new(SyncWindow::since(day(4)))
            .deepen(MailboxId::try_from("Sent").unwrap(), SyncWindow::full()),
    )
    .unwrap();
    let (chunks, sent) = band(&[&select], &kept(None), &sent_only, None).await;
    assert_eq!(chunks[0].advance_to.as_ref(), Some(&kept(None).encode()));
    assert!(!sent.contains("SEARCH"), "{sent}");
}

#[tokio::test]
async fn a_moved_uidvalidity_fetches_the_folder_afresh_under_the_wider_window() {
    let (chunks, sent) = band(
        &[
            &select_resp("a2", 2000, 4),
            &select_resp("a3", 2000, 4),
            &search_resp("a4", &[1, 3]),
            &fetch_resp("a5", &[1, 3]),
        ],
        &kept(None),
        &widening(),
        None,
    )
    .await;
    assert!(
        sent.contains("a4 UID SEARCH SINCE 31-Dec-2025\r\n"),
        "{sent}"
    );
    let last = chunks.last().unwrap();
    assert_eq!(last.mode, PassMode::Reconcile, "a fresh pass reconciles");
    let cursor = MailboxCursor::decode(last.advance_to.as_ref().unwrap()).unwrap();
    assert_eq!(cursor.uid_validity, 2000);
}

#[tokio::test]
async fn a_band_cut_off_at_the_message_limit_is_decided_by_each_messages_date_and_uid() {
    // MESSAGELIMIT=2: the search returns as many as the limit, so it reads as cut off, and the
    // band is decided from each message's INTERNALDATE, both bounds and the UID bound applied.
    let select = select_resp("a2", 1000, 6);
    let dates = "* 1 FETCH (UID 1 INTERNALDATE \"31-Dec-2025 23:59:59 +0000\")\r\n\
                 * 2 FETCH (UID 2 INTERNALDATE \" 1-Jan-2026 00:00:00 +0000\")\r\n\
                 a4 OK done\r\n";
    let more = "* 3 FETCH (UID 3 INTERNALDATE \" 1-Apr-2026 23:59:59 +0000\")\r\n\
                * 4 FETCH (UID 4 INTERNALDATE \" 2-Apr-2026 00:00:00 +0000\")\r\n\
                a5 OK done\r\n";
    let cursor = MailboxCursor {
        uid_next: 5,
        ..kept(None)
    };
    let (chunks, sent) = band(
        &[
            &select,
            &search_resp("a3", &[2, 3]),
            dates,
            more,
            &fetch_resp("a6", &[2, 3]),
            &fetch_resp("a7", &[1]),
        ],
        &cursor,
        &widening(),
        Some("MESSAGELIMIT=2"),
    )
    .await;
    assert!(
        sent.contains("a3 UID SEARCH UID 1:4 SINCE 31-Dec-2025 BEFORE 2-Apr-2026\r\n"),
        "{sent}"
    );
    assert!(
        sent.contains("a4 UID FETCH 1:2 (UID INTERNALDATE)\r\n"),
        "{sent}"
    );
    assert!(
        sent.contains("a5 UID FETCH 3:4 (UID INTERNALDATE)\r\n"),
        "{sent}"
    );
    assert!(
        !sent.contains("a6 UID FETCH 5"),
        "read past the UID bound: {sent}"
    );
    assert!(sent.contains("a6 UID FETCH 2:3 "), "{sent}");
    assert_eq!(
        chunks[0].total,
        Some(3),
        "4 falls outside the days; 1 is the day before the band, a day searched past its floor"
    );
}

#[test]
fn a_delta_carries_the_watermark_only_into_a_cursor_of_its_own() {
    let next = kept(None).encode();
    let prior = kept(Some(mark(9)));
    assert_eq!(
        MailboxCursor::decode(&carried(next.clone(), Some(prior))),
        Some(prior)
    );
    assert_eq!(carried(next.clone(), Some(kept(None))), next);
    assert_eq!(carried(next.clone(), None), next);
    let foreign = SyncState::new("not-imap");
    assert_eq!(carried(foreign.clone(), Some(prior)), foreign);
}

/// Drains a delta from `cursor` over `server`, through the stream that runs it.
async fn delta(server: &[&str], cursor: &str, capabilities: &[&str]) -> Vec<EmailChunk> {
    let mut parts = vec![GREETING, LOGIN_OK];
    parts.extend_from_slice(server);
    let (stream, _) = MockStream::new(script(&parts));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let advertised: Vec<String> = capabilities.iter().map(|&c| c.to_owned()).collect();
    conn.negotiated = Negotiated::from_capabilities(&advertised);
    let mailbox = MailboxId::try_from("INBOX").unwrap();
    let state = SyncState::new(cursor);
    crate::stream::stream_email(
        &mut conn,
        None,
        &mailbox,
        Some(&state),
        SyncWindow::full(),
        50,
        0,
    )
    .map(Result::unwrap)
    .collect()
    .await
}

#[tokio::test]
async fn a_delta_that_tombstones_nothing_keeps_the_band_it_found() {
    let select = "* 10 EXISTS\r\n* OK [UIDVALIDITY 1000] v\r\n* OK [UIDNEXT 11] n\r\n\
                  * OK [HIGHESTMODSEQ 7]\r\na2 OK [READ-WRITE] done\r\n";
    let chunks = delta(&[select], "v1000;n11;g1;m7;r;w9", &["XYMHIGHESTMODSEQ"]).await;
    let last = chunks.last().unwrap();
    assert_eq!(last.mode, PassMode::Additive);
    assert_eq!(
        last.advance_to.as_ref().unwrap().as_str(),
        "v1000;n11;g1;m7;r;w9"
    );
}

#[tokio::test]
async fn a_band_resumes_after_a_reconciling_delta_without_qresync() {
    // A server without QRESYNC reconciles on every delta. Asked under the depth the band widens
    // to, that reconcile lists every message the band stored, so it tombstones none of them, and
    // the watermark rides through it: the band then continues below it.
    let select = select_resp("a2", 1000, 11);
    let flags = "* 1 FETCH (FLAGS (\\Seen) UID 1)\r\na3 OK FETCH completed\r\n";
    let chunks = delta(&[&select, flags], "v1000;n11;g1;w9", &[]).await;
    let last = chunks.last().unwrap();
    assert!(last.is_reconcile_final());
    let after = MailboxCursor::decode(last.advance_to.as_ref().unwrap()).unwrap();
    assert_eq!(
        after.band_low,
        Some(9),
        "the reconcile dropped the watermark"
    );

    let select = select_resp("a2", 1000, 11);
    let (_, sent) = band(
        &[&select, &search_resp("a3", &[])],
        &after,
        &widening().resuming(),
        None,
    )
    .await;
    assert!(
        sent.contains("a3 UID SEARCH UID 1:8 SINCE 31-Dec-2025 BEFORE 2-Apr-2026\r\n"),
        "{sent}"
    );

    // A UID space that moved takes the watermark with it.
    let reset = select_resp("a2", 2000, 3);
    let chunks = delta(
        &[&reset, &fetch_resp("a3", &[1, 2])],
        "v1000;n11;g1;w9",
        &[],
    )
    .await;
    let last = MailboxCursor::decode(chunks.last().unwrap().advance_to.as_ref().unwrap()).unwrap();
    assert_eq!(last.band_low, None);
}

#[tokio::test]
async fn the_provider_widens_over_a_session_of_its_own() {
    use engine_core::ids::AccountId;
    use engine_provider::Provider;

    use crate::ImapProvider;

    let select = select_resp("a2", 1000, 41);
    let search = search_resp("a3", &[]);
    let (stream, recorded) = MockStream::new(script(&[GREETING, LOGIN_OK, &select, &search]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let provider = ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap());
    let account = AccountId::try_from("acct-1").unwrap();
    let state = kept(None).encode();
    let widening = widening();
    let chunks: Vec<EmailChunk> = provider
        .widen_email(&account, &state, &widening, 2, 0)
        .map(Result::unwrap)
        .collect()
        .await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].advance_to.as_ref(), Some(&state));
    assert!(
        written(&recorded)
            .contains("a3 UID SEARCH UID 1:39 SINCE 31-Dec-2025 BEFORE 2-Apr-2026\r\n")
    );

    // A failed band hands its session back as unusable.
    let (stream, _) = MockStream::new(script(&[
        GREETING,
        LOGIN_OK,
        "a2 NO [NONEXISTENT] gone\r\n",
    ]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let provider = ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap());
    let failed: Vec<_> = provider
        .widen_email(&account, &state, &widening, 2, 0)
        .collect()
        .await;
    assert!(failed.last().unwrap().is_err());
}

#[tokio::test]
async fn a_fresh_pass_searches_a_day_early_and_keeps_only_what_the_window_admits() {
    // The window starts 19 February; the server's day for the message is the 18th, and so is the
    // UTC day the engine judges by, so it is enumerated (its key is present) and not stored.
    let select = select_resp("a2", 1000, 6);
    let (stream, recorded) = MockStream::new(script(&[
        GREETING,
        LOGIN_OK,
        &select,
        &search_resp("a3", &[5]),
        &fetch_resp("a4", &[5]),
    ]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let mailbox = MailboxId::try_from("INBOX").unwrap();
    let window = SyncWindow::since(CalendarDate::new(2026, 2, 19).unwrap());
    let chunks: Vec<EmailChunk> =
        crate::stream::stream_email(&mut conn, None, &mailbox, None, window, 50, 0)
            .map(Result::unwrap)
            .collect()
            .await;
    assert!(written(&recorded).contains("a3 UID SEARCH SINCE 18-Feb-2026\r\n"));
    let last = chunks.last().unwrap();
    assert!(last.is_reconcile_final());
    assert_eq!(last.changed, []);
    assert_eq!(
        last.present.len(),
        1,
        "enumerated, so nothing held is tombstoned for it"
    );
}
