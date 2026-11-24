use engine_core::{error::FailureClass, ids::AccountId};
use engine_provider::VacationPrecondition;
use serde_json::json;

use super::*;
use crate::test_support::{fake_client_fallible, recording_client};

fn account() -> AccountId {
    AccountId::try_from("a").unwrap()
}

fn at(text: &str) -> UtcDateTime {
    UtcDateTime::parse_rfc3339(text).unwrap()
}

const WORKSPACE: &str = include_str!("../tests/fixtures/identity/userinfo_workspace.json");
const PERSONAL: &str = include_str!("../tests/fixtures/identity/userinfo_personal.json");

/// A reply as Gmail keeps its `VacationSettings`, every part set: one body, since Gmail drops the
/// text where an HTML one is set (observed).
fn documented() -> Value {
    json!({
        "enableAutoReply": true,
        "responseSubject": "Away",
        "responseBodyHtml": "<p>Back soon.</p>",
        "restrictToContacts": true,
        "restrictToDomain": false,
        "startTime": "1767225600000",
        "endTime": "1767312000500",
    })
}

#[test]
fn a_reply_reads_as_the_model_says() {
    let reply = reply(&documented()).unwrap();
    assert!(reply.enabled);
    assert_eq!(reply.window.start, Some(at("2026-01-01T00:00:00Z")));
    assert_eq!(reply.window.end, Some(at("2026-01-02T00:00:00.5Z")));
    assert_eq!(reply.reply.subject.as_deref(), Some("Away"));
    assert_eq!(reply.reply.text, None);
    assert_eq!(reply.reply.html.as_deref(), Some("<p>Back soon.</p>"));
    assert_eq!(reply.audience, ReplyAudience::Contacts);
    assert!(reply.outside_reply.is_none());
    assert_eq!(
        reply.raw.as_ref().map(RawVacation::as_str),
        Some(documented().to_string().as_str())
    );
    assert_eq!(reply.version.as_ref().map(|v| v.as_str().len()), Some(64));
}

#[test]
fn restrictions_read_as_the_narrowest_audience_and_off_is_off() {
    let audience = |contacts: bool, domain: bool| {
        reply(&json!({ "restrictToContacts": contacts, "restrictToDomain": domain }))
            .unwrap()
            .audience
    };
    assert_eq!(audience(false, false), ReplyAudience::Everyone);
    assert_eq!(audience(true, false), ReplyAudience::Contacts);
    assert_eq!(audience(false, true), ReplyAudience::Organization);
    assert_eq!(audience(true, true), ReplyAudience::ContactsInOrganization);

    let off = reply(&json!({ "enableAutoReply": false, "responseBodyPlainText": "" })).unwrap();
    assert!(!off.enabled);
    assert!(off.window.is_open());
    assert!(off.reply.is_empty());
}

#[test]
fn a_setting_not_as_documented_is_a_protocol_error() {
    for bad in [
        json!([]),
        json!({"startTime": "soon"}),
        json!({"endTime": -1}),
        json!({"startTime": true}),
    ] {
        assert!(reply(&bad).is_err(), "{bad}");
    }
    assert!(reply(&json!({"startTime": 1_767_225_600_000_u64})).is_ok());
}

#[test]
fn a_reply_is_written_whole_and_reads_back_the_same() {
    let read = reply(&documented()).unwrap();
    let written = setting(&read).unwrap();
    assert_eq!(written, documented());
    // A part the model leaves out is left out of the `PUT`, which clears it.
    let bare = setting(&VacationResponse::default()).unwrap();
    assert_eq!(
        bare,
        json!({"enableAutoReply": false, "restrictToContacts": false, "restrictToDomain": false})
    );
    let org =
        setting(&VacationResponse::default().with_audience(ReplyAudience::ContactsInOrganization))
            .unwrap();
    assert_eq!(
        (
            org["restrictToContacts"].clone(),
            org["restrictToDomain"].clone()
        ),
        (json!(true), json!(true))
    );
    let domain =
        setting(&VacationResponse::default().with_audience(ReplyAudience::Organization)).unwrap();
    assert_eq!(domain["restrictToDomain"], json!(true));
}

/// Gmail keeps milliseconds: a start is rounded up and an end down, so the window is never wider
/// than asked, and one that then holds no millisecond is refused.
#[test]
fn the_window_is_never_written_wider_than_asked() {
    let window = |start: &str, end: &str| {
        VacationResponse::default().with_window(
            VacationWindow::default()
                .with_start(at(start))
                .with_end(at(end)),
        )
    };
    let written = setting(&window(
        "2026-01-01T00:00:00.0004Z",
        "2026-01-01T00:00:01.0019Z",
    ))
    .unwrap();
    assert_eq!(written["startTime"], json!("1767225600001"));
    assert_eq!(written["endTime"], json!("1767225601001"));
    let none = setting(&window(
        "2026-01-01T00:00:00.0001Z",
        "2026-01-01T00:00:00.0009Z",
    ));
    assert_eq!(none.unwrap_err().class(), FailureClass::Permanent);
    let before = VacationResponse::default()
        .with_window(VacationWindow::default().with_start(at("1969-12-31T23:59:59Z")));
    assert!(setting(&before).is_err());
}

#[tokio::test]
async fn the_organization_is_offered_on_a_workspace_account_only() {
    let personal = GmailProvider::new(fake_client_fallible(vec![(
        "/oauth2/v3/userinfo",
        Ok(serde_json::from_str(PERSONAL).unwrap()),
    )]));
    let vocabulary = personal.vocabulary(&account()).await.unwrap();
    assert!(vocabulary.audiences().contains(&ReplyAudience::Contacts));
    assert!(
        !vocabulary
            .audiences()
            .contains(&ReplyAudience::Organization)
    );
    let workspace = GmailProvider::new(fake_client_fallible(vec![(
        "/oauth2/v3/userinfo",
        Ok(serde_json::from_str(WORKSPACE).unwrap()),
    )]));
    let vocabulary = workspace.vocabulary(&account()).await.unwrap();
    assert!(
        vocabulary
            .audiences()
            .contains(&ReplyAudience::ContactsInOrganization)
    );
    assert!(vocabulary.has_subject());
    assert_eq!(vocabulary.bodies(), engine_core::vacation::ReplyBodies::One);
    assert!(!vocabulary.has_outside_reply());
}

#[tokio::test]
async fn a_write_puts_the_setting_and_asks_the_affiliation_only_for_the_organization() {
    let (client, log) = recording_client(vec![("/settings/vacation", Ok(json!(null)))]);
    let provider = GmailProvider::new(client);
    let new = reply(&documented()).unwrap();
    let receipt = provider
        .set_vacation(&account(), VacationPrecondition::Unconditional, &new)
        .await
        .unwrap();
    assert_eq!(receipt.rules, RulesEffect::Untouched);
    assert!(receipt.version.is_none());
    {
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].method, "PUT");
        assert_eq!(log[0].body.as_ref(), Some(&documented()));
    }

    // A personal account cannot limit a reply to an organization: refused before sending.
    let (client, log) = recording_client(vec![(
        "/oauth2/v3/userinfo",
        Ok(serde_json::from_str(PERSONAL).unwrap()),
    )]);
    let provider = GmailProvider::new(client);
    let org = VacationResponse::default().with_audience(ReplyAudience::Organization);
    let err = provider
        .set_vacation(&account(), VacationPrecondition::Unconditional, &org)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .all(|request| request.method == "GET")
    );
}

#[tokio::test]
async fn a_guarded_write_refuses_a_reply_that_moved_or_was_never_read() {
    let stored = documented();
    let (client, log) = recording_client(vec![
        ("GET /settings/vacation", Ok(stored.clone())),
        ("PUT /settings/vacation", Ok(json!(null))),
    ]);
    let provider = GmailProvider::new(client);
    let read = provider.read_vacation(&account()).await.unwrap();
    let new = read.clone().turned_off();
    let receipt = provider
        .set_vacation(&account(), VacationPrecondition::IfUnchanged(&read), &new)
        .await
        .unwrap();
    assert_eq!(receipt.rules, RulesEffect::Untouched);
    assert_eq!(log.lock().unwrap().last().unwrap().method, "PUT");

    let mut stale = read.clone();
    stale.version = Some(VacationVersion::new("0".repeat(64)));
    let err = provider
        .set_vacation(&account(), VacationPrecondition::IfUnchanged(&stale), &new)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
    let built = VacationResponse::default();
    let err = provider
        .set_vacation(&account(), VacationPrecondition::IfUnchanged(&built), &new)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    let puts = log
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.method == "PUT")
        .count();
    assert_eq!(puts, 1);
    assert_eq!(provider.vacation_write_guard(), WriteGuard::Absent);
}

/// A reply never set, as Gmail answers it (observed): only some fields, an empty subject.
#[test]
fn a_reply_never_set_reads_as_off_and_empty() {
    let captured: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/mail/settings_vacation_off.json"
    ))
    .unwrap();
    let off = reply(&captured).unwrap();
    assert!(!off.enabled);
    assert!(off.reply.is_empty());
    assert_eq!(off.audience, ReplyAudience::Everyone);
    assert!(off.window.is_open());
}

/// A token that may not ask who the account belongs to (`401` or `403` from `userinfo`) gets the
/// narrower, personal vocabulary rather than an error; a reply limited to the organization is then
/// refused before sending, as one the account is not known to hold.
#[tokio::test]
async fn a_token_that_may_not_ask_the_affiliation_gets_the_personal_vocabulary() {
    for status in [401, 403] {
        let (client, log) = recording_client(vec![
            (
                "/oauth2/v3/userinfo",
                Err((status, json!({"error": {"code": status}}))),
            ),
            ("/settings/vacation", Ok(json!(null))),
        ]);
        let provider = GmailProvider::new(client);
        let vocabulary = provider.vocabulary(&account()).await.unwrap();
        assert!(
            !vocabulary
                .audiences()
                .contains(&ReplyAudience::Organization)
        );
        assert!(vocabulary.audiences().contains(&ReplyAudience::Contacts));
        let org = VacationResponse::default().with_audience(ReplyAudience::Organization);
        let err = provider
            .set_vacation(&account(), VacationPrecondition::Unconditional, &org)
            .await
            .unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent);
        assert!(log.lock().unwrap().iter().all(|r| r.method == "GET"));
    }
    // Any other failure is still one: a throttled or failing ask says nothing of the account.
    let rate = json!({"error": {"code": 403, "errors": [{"reason": "rateLimitExceeded"}]}});
    for (status, body, class) in [
        (
            500,
            json!({"error": {"code": 500}}),
            FailureClass::Retryable,
        ),
        (
            429,
            json!({"error": {"code": 429}}),
            FailureClass::RateLimited,
        ),
        (403, rate, FailureClass::RateLimited),
    ] {
        let client = fake_client_fallible(vec![("/oauth2/v3/userinfo", Err((status, body)))]);
        let err = GmailProvider::new(client)
            .vocabulary(&account())
            .await
            .unwrap_err();
        assert_eq!(err.class(), class, "{status}");
    }
}

/// A write answered with a success whose body does not read was done: the status says so.
#[tokio::test]
async fn a_write_whose_answer_does_not_read_was_done() {
    let (base, _seen) = crate::test_support::capturing_server("200 OK", "not json");
    let client = GoogleClient::with_base(
        "t",
        base,
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap();
    let receipt = GmailProvider::new(client)
        .set_vacation(
            &account(),
            VacationPrecondition::Unconditional,
            &VacationResponse::default(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.rules, RulesEffect::Untouched);
}
