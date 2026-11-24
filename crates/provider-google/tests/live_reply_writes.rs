//! Gated live **writes** of the vacation reply. Not run by default, nor by a token alone: it
//! changes the account, so it needs `GMAIL_REPLY_WRITES=1`, its own gate, beside a
//! `GOOGLE_ACCESS_TOKEN` holding `gmail.settings.basic`. It never writes a filter.
//!
//! How it leaves the account: the reply is read raw first, and whatever happens it is put back
//! byte for byte with a raw `PUT` of what was read, then read until it matches; the comparison is
//! all it prints. The reply it writes runs only in 2099, so no mail is answered meanwhile.
//!
//! It proves a guarded write (and whether Gmail shows it at once, measured by reads), the window
//! rounded to the millisecond and never wider, a reply with both bodies refused before sending
//! (Gmail drops the text where HTML is set: shown by an earlier run that sent both), a stale read
//! refused, a part left out cleared, and a write of the reply as read giving back what was read.

mod gmail_settings_support;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine_core::{
    error::FailureClass,
    time::UtcDateTime,
    vacation::{AutoReply, ReplyAudience, VacationResponse, VacationWindow},
};
use engine_provider::{Provider, VacationPrecondition};
use gmail_settings_support::{account, failed, gated, provider, send, settled};
use reqwest::Method;
use serde_json::Value;

const GATE: &str = "GMAIL_REPLY_WRITES";

async fn raw(token: &str) -> Value {
    let (status, setting) = send(token, Method::GET, "/settings/vacation", None).await;
    assert_eq!(status, 200, "read");
    setting
}

fn at(text: &str) -> UtcDateTime {
    UtcDateTime::parse_rfc3339(text).unwrap()
}

#[tokio::test]
async fn live_reply_is_written_and_put_back_byte_for_byte() {
    let Some(token) = gated(GATE) else {
        eprintln!("skipping: {GATE}=1 and GOOGLE_ACCESS_TOKEN not both set");
        return;
    };
    let before = raw(&token).await;
    let run = tokio::spawn(cycle(token.clone())).await;
    let (status, _) = send(&token, Method::PUT, "/settings/vacation", Some(&before)).await;
    assert_eq!(status, 200, "restore");
    let restored = settled(
        "the restore",
        3,
        Duration::from_secs(2),
        || raw(&token),
        |now: &Value| *now == before,
    )
    .await;
    eprintln!(
        "restored byte for byte; reads before shown: {}",
        restored.reads_before
    );
    if let Err(panic) = run {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn cycle(token: String) {
    let gmail = provider(&token);
    let vacation = gmail.vacation().unwrap();
    let read = vacation
        .read_vacation(&account())
        .await
        .unwrap_or_else(|err| failed("read", &err));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let marker = format!("engine-live-reply-{nanos}");
    let reply = VacationResponse::default()
        .turned_on()
        .with_window(
            VacationWindow::default()
                .with_start(at("2099-01-01T00:00:00.0004Z"))
                .with_end(at("2099-01-02T00:00:00.0019Z")),
        )
        .with_reply(
            AutoReply::default()
                .with_subject(format!("Away {marker}"))
                .with_html(format!("<p>Back soon. {marker}</p>")),
        )
        .with_audience(ReplyAudience::Contacts);
    // Text and HTML side by side: Gmail keeps the HTML and drops the text (observed in an earlier
    // run of this suite), so the engine refuses it before sending.
    let mut both = reply.clone();
    both.reply = both.reply.clone().with_text(format!("Back soon. {marker}"));
    let err = vacation
        .set_vacation(&account(), VacationPrecondition::IfUnchanged(&read), &both)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent, "both bodies refused");
    let receipt = vacation
        .set_vacation(&account(), VacationPrecondition::IfUnchanged(&read), &reply)
        .await
        .unwrap_or_else(|err| failed("write", &err));
    assert!(receipt.version.is_none());
    let shown = settled(
        "the write",
        3,
        Duration::from_secs(2),
        || raw(&token),
        |now: &Value| {
            now["responseSubject"]
                .as_str()
                .is_some_and(|s| s.contains(&marker))
        },
    )
    .await;
    eprintln!(
        "write: reads before shown: {}; flaps: {}",
        shown.reads_before, shown.flaps
    );
    let setting = shown.state;
    // Rounded so the window is never wider: the start up, the end down, to the millisecond.
    assert_eq!(
        setting["startTime"].as_str(),
        Some("4070908800001"),
        "start"
    );
    assert_eq!(setting["endTime"].as_str(), Some("4070995200001"), "end");
    assert_eq!(setting["restrictToContacts"], true);
    assert_eq!(setting["enableAutoReply"], true);
    let html_kept = setting["responseBodyHtml"].as_str()
        == Some(format!("<p>Back soon. {marker}</p>").as_str());
    eprintln!("html kept as sent: {html_kept}");
    let model = vacation
        .read_vacation(&account())
        .await
        .unwrap_or_else(|err| failed("read back", &err));
    assert_eq!(model.window.start, Some(at("2099-01-01T00:00:00.001Z")));
    assert_eq!(model.window.end, Some(at("2099-01-02T00:00:00.001Z")));
    assert_eq!(model.audience, ReplyAudience::Contacts);

    // A guarded write on the read taken before the write is stale.
    let err = vacation
        .set_vacation(&account(), VacationPrecondition::IfUnchanged(&read), &reply)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict, "stale read");

    // A part left out is cleared: no subject, no HTML.
    let mut plain = reply.clone();
    plain.reply = AutoReply::default().with_text(format!("Plain {marker}"));
    let _ = vacation
        .set_vacation(
            &account(),
            VacationPrecondition::IfUnchanged(&model),
            &plain,
        )
        .await
        .unwrap_or_else(|err| failed("write plain", &err));
    let cleared = settled(
        "the plain write",
        3,
        Duration::from_secs(2),
        || raw(&token),
        |now: &Value| {
            now["responseBodyPlainText"]
                .as_str()
                .is_some_and(|s| s.starts_with("Plain"))
        },
    )
    .await
    .state;
    let left = |key: &str| cleared[key].as_str().is_some_and(|s| !s.is_empty());
    eprintln!(
        "after a write without them, subject left: {}; html left: {}",
        left("responseSubject"),
        left("responseBodyHtml")
    );
    assert!(
        !left("responseSubject") && !left("responseBodyHtml"),
        "a part left out is cleared"
    );

    // The reply as first read, written through the model, gives back what was read.
    let _ = vacation
        .set_vacation(&account(), VacationPrecondition::Unconditional, &read)
        .await
        .unwrap_or_else(|err| failed("write as read", &err));
    let first: Value = serde_json::from_str(read.raw.as_ref().unwrap().as_str()).unwrap();
    let back = settled(
        "the write as read",
        3,
        Duration::from_secs(2),
        || raw(&token),
        |now: &Value| {
            now["responseSubject"]
                .as_str()
                .is_none_or(|s| !s.contains(&marker))
                && now["responseBodyPlainText"]
                    .as_str()
                    .is_none_or(|s| !s.contains(&marker))
        },
    )
    .await
    .state;
    eprintln!(
        "the reply as read, written through the model, reads back byte for byte: {}",
        back == first
    );
    assert_eq!(back, first, "a write of the reply as read");
}
