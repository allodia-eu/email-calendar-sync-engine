//! Gated live check: **why JMAP refuses to send an iMIP message** (issue #105).
//!
//! `provider-jmap` advertises `Capabilities::scheduling_submission` as `false` and refuses
//! a `Draft` carrying a `DraftCalendar`. A refusal is a strong claim, and this is its
//! evidence — driven against the server rather than argued from the RFCs, because the
//! argument from the RFCs points the *other* way.
//!
//! RFC 6047 §2.4 requires a `method=` parameter on the part's `Content-Type`, and a
//! `text/calendar` part without one is explicitly **not** an iMIP body part (§2.4 note 2):
//! it arrives as a calendar file the organizer's client never processes. An
//! `EmailBodyPart`'s `type` is a media type *without* parameters, so RFC 8621 §4.1.3's raw
//! `header:Content-Type` is the only candidate — and §4.6 does permit `Content-*` fields on
//! a body part. On paper it works.
//!
//! It does not. This test issues all three possible `Email/set` shapes over **raw JMAP**
//! (`Harness::jmap_post`, whose own docs reserve it for wire-level behaviour no adapter
//! surface expresses — the adapter deliberately has no code path that builds these), and
//! reads back the `Content-Type` fields the server stored. Every shape is **accepted**,
//! which is the whole problem: a naive adapter would report success and deliver something
//! unprocessable.
//!
//! It is a *server* limitation rather than a protocol one. Stalwart is the only JMAP mail
//! server this repo can drive (`jmap.md`), so the refusal follows from the only evidence
//! available — and if that ever changes, this test is what notices.
//!
//! Skips with no `STALWART_HTTP_ADDR`.

use std::time::Duration;

use engine_core::{
    ids::{AccountId, MessageIdHeader},
    mail::EmailAddress,
    scheduling::ScheduleMethod,
};
use engine_provider::{Draft, DraftCalendar, Provider, ProviderError};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use serde_json::{Value, json};
use stalwart_harness::Harness;

/// The iTIP `REPLY` a caller would hand the adapter.
const REPLY_ICAL: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Engine//Live//EN\r\n\
                          METHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:jmap-imip-reply@test.local\r\n\
                          DTSTAMP:20260501T080000Z\r\n\
                          ORGANIZER;CN=Bob:mailto:bob@test.local\r\n\
                          ATTENDEE;CN=Alice;PARTSTAT=ACCEPTED:mailto:alice@test.local\r\n\
                          SEQUENCE:0\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

/// The raw `Content-Type` header value this test hands the server on a body part.
const RAW_CONTENT_TYPE: &str = " text/calendar; charset=utf-8; method=REPLY";

fn account() -> AccountId {
    AccountId::try_from("live-imip").unwrap()
}

/// Posts one JMAP request and returns the parsed response.
fn jmap(harness: &Harness, calls: &Value) -> Value {
    let body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": calls.clone(),
    });
    let response = harness
        .jmap_post(body.to_string().as_bytes())
        .expect("JMAP probe");
    assert_eq!(response.status, 200, "{}", response.body_text());
    serde_json::from_slice(&response.body).expect("a JMAP response")
}

/// The account's mail account id and Drafts mailbox id.
fn mail_context(harness: &Harness) -> (String, String) {
    let session = harness.jmap_session().expect("session");
    let account_id = session["primaryAccounts"]["urn:ietf:params:jmap:mail"]
        .as_str()
        .expect("a mail account")
        .to_owned();
    let response = jmap(
        harness,
        &json!([[
            "Mailbox/get",
            { "accountId": account_id, "ids": null, "properties": ["role"] },
            "m"
        ]]),
    );
    let drafts = response["methodResponses"][0][1]["list"]
        .as_array()
        .expect("a mailbox list")
        .iter()
        .find(|mailbox| mailbox["role"] == "drafts")
        .expect("a Drafts mailbox")["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    (account_id, drafts)
}

/// Creates a draft whose second body part is `part`, then reads back **every**
/// `Content-Type` field the server stored on that part, and destroys the draft.
///
/// `header:Content-Type:all` is the load-bearing property: it returns the field as an
/// array of instances, so a server that emitted two is distinguishable from one that
/// emitted the one we asked for. Asking for the singular form would return only the last
/// and hide the defect.
fn stored_content_types(
    harness: &Harness,
    account_id: &str,
    drafts: &str,
    part: &Value,
) -> Vec<String> {
    let response = jmap(
        harness,
        &json!([[
            "Email/set",
            {
                "accountId": account_id,
                "create": { "d": {
                    "mailboxIds": { drafts: true },
                    "from": [{ "email": "alice@test.local" }],
                    "to": [{ "email": "bob@test.local" }],
                    "subject": "iMIP shape probe",
                    "bodyStructure": {
                        "type": "multipart/alternative",
                        "subParts": [{ "partId": "text", "type": "text/plain" }, part.clone()],
                    },
                    "bodyValues": {
                        "text": { "value": "Alice has accepted this invitation." },
                        "cal": { "value": REPLY_ICAL },
                    },
                }},
            },
            "s"
        ]]),
    );
    let created = &response["methodResponses"][0][1]["created"]["d"];
    assert!(
        !created.is_null(),
        "the server ACCEPTED nothing — if it now rejects these shapes with \
         invalidProperties, the refusal has a better justification and this test should say \
         so: {}",
        response["methodResponses"][0][1]
    );
    let id = created["id"].as_str().expect("a created id").to_owned();

    let response = jmap(
        harness,
        &json!([[
            "Email/get",
            {
                "accountId": account_id,
                "ids": [id],
                "properties": ["bodyStructure"],
                "bodyProperties": ["type", "header:Content-Type:all"],
            },
            "g"
        ]]),
    );
    let stored = response["methodResponses"][0][1]["list"][0]["bodyStructure"]["subParts"][1]
        ["header:Content-Type:all"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect();

    jmap(
        harness,
        &json!([[
            "Email/set",
            { "accountId": account_id, "destroy": [id] },
            "z"
        ]]),
    );
    stored
}

#[tokio::test]
async fn live_jmap_refuses_an_itip_draft_it_cannot_encode() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_jmap_refuses_an_itip_draft_it_cannot_encode: unset");
        return;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("ready");
    let provider = JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect");

    // The capability a host reads, and the refusal it saves them from reaching.
    let caps = provider.connection_info().capabilities;
    assert!(caps.submission(), "ordinary mail still sends");
    assert!(
        !caps.scheduling_submission(),
        "JMAP must not claim it can send a scheduling message"
    );

    let scheduling = Draft::new(
        MessageIdHeader::new("jmap-imip-refused@test.local").unwrap(),
        EmailAddress::named("Alice", &harness.account),
        vec![EmailAddress::new("bob@test.local")],
        "Accepted: Sprint planning",
        "Alice has accepted this invitation.",
    )
    .with_calendar(DraftCalendar::new(ScheduleMethod::Reply, REPLY_ICAL));

    let refusal = provider
        .submit_email(&account(), &scheduling)
        .await
        .expect_err("a draft this transport cannot faithfully encode must be refused");
    assert_eq!(
        ProviderError::class(&refusal),
        engine_core::error::FailureClass::InvalidState,
        "the refusal is a caller error, not a retryable failure"
    );
}

#[tokio::test]
async fn live_jmap_cannot_put_the_method_parameter_on_a_body_part() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_jmap_cannot_put_the_method_parameter_on_a_body_part: unset");
        return;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("ready");
    let (account_id, drafts) = mail_context(&harness);

    // ---- The raw header alone: the server writes ours AND generates its own. ----
    let header_only = stored_content_types(
        &harness,
        &account_id,
        &drafts,
        &json!({ "partId": "cal", "header:Content-Type": RAW_CONTENT_TYPE }),
    );
    assert_eq!(
        header_only.len(),
        2,
        "expected the duplicate this refusal is based on, got {header_only:?}"
    );
    assert!(
        header_only[1].contains("text/plain"),
        "the generated field defaults the part to text/plain, and a parser taking the last \
         instance sees plain text: {header_only:?}"
    );

    // ---- `type` as well: still two fields, and the generated one has no method=. ----
    let typed_and_header = stored_content_types(
        &harness,
        &account_id,
        &drafts,
        &json!({
            "partId": "cal",
            "type": "text/calendar",
            "header:Content-Type": RAW_CONTENT_TYPE,
        }),
    );
    assert_eq!(
        typed_and_header.len(),
        2,
        "expected two fields again, got {typed_and_header:?}"
    );
    assert!(
        !typed_and_header[1].to_ascii_lowercase().contains("method="),
        "the server's own field carries no method=: {typed_and_header:?}"
    );

    // ---- `type` alone: one clean field, and no method= anywhere. ----
    let typed_only = stored_content_types(
        &harness,
        &account_id,
        &drafts,
        &json!({ "partId": "cal", "type": "text/calendar" }),
    );
    assert_eq!(typed_only.len(), 1, "got {typed_only:?}");
    assert!(
        !typed_only[0].to_ascii_lowercase().contains("method="),
        "`type` is a media type without parameters, so it cannot carry method= — if this \
         ever fails, JMAP can express an iMIP part and the refusal should be lifted: \
         {typed_only:?}"
    );
}
