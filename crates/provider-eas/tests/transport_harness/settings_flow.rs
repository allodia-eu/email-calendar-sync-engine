// SPDX-License-Identifier: MPL-2.0
//! Settings + ValidateCert + ResolveRecipients scenarios over the real
//! transport: UserInformation Get, Oof Get/Set, DevicePassword Set,
//! ValidateCert verdicts, and the non-1 status gates of each family.

use std::sync::Arc;

use provider_eas::{
    types::{OofSettings, ResolveRecipientsRequest, ValidateCertRequest},
    wbxml::{
        WbxmlElement,
        tags::{pages, recipients, validatecert},
    },
};

use super::{
    fixtures::{
        device_password_element, oof_get_element, oof_set_element, settings_response,
        user_information_element,
    },
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// Settings UserInformation (Get): the SMTP address(es) surface on the
/// result.
#[tokio::test]
async fn user_information_returns_the_smtp_addresses() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&settings_response(user_information_element(
            "user@example.test",
        )))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client
        .settings_user_information()
        .await
        .expect("UserInformation parses");
    assert_eq!(result.status, 1);
    assert!(
        result
            .email_addresses
            .contains(&"user@example.test".to_owned()),
        "the SMTP address surfaces: {:?}",
        result.email_addresses
    );
}

/// Settings Oof Get: state + the internal reply message.
#[tokio::test]
async fn oof_get_returns_state_and_reply_messages() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&settings_response(oof_get_element(
            "2",
            "Away until Monday",
        )))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let settings = client
        .settings_oof_get("Text")
        .await
        .expect("Oof Get parses");
    assert_eq!(settings.state, Some(2), "OofState 2 = time-based/scheduled");
    assert!(
        settings.messages.iter().any(|m| m
            .reply_message
            .as_deref()
            .is_some_and(|t| t.contains("Away until Monday"))),
        "the internal reply message surfaces: {:?}",
        settings.messages
    );
}

/// Settings Oof Set: ack status 1.
#[tokio::test]
async fn oof_set_round_trips_the_state() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&settings_response(oof_set_element("1")))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client
        .settings_oof_set(&OofSettings {
            state: Some(1),
            start_time: None,
            end_time: None,
            messages: Vec::new(),
        })
        .await
        .expect("Oof Set parses");
    assert_eq!(result.status, 1);
    // The request DID go out as a Settings command with an Oof container.
    assert_eq!(server.request(1).cmd().as_deref(), Some("Settings"));
    assert!(
        server.request(1).wbxml_tree().is_some(),
        "the Set request carries a WBXML body"
    );
}

/// Settings DevicePassword Set: ack status 1.
#[tokio::test]
async fn device_password_set_reports_status() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&settings_response(device_password_element("1")))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let result = client
        .settings_device_password("recovery-secret")
        .await
        .expect("DevicePassword parses");
    assert_eq!(result.status, 1);
}

/// A Settings family non-1 status (Oof Set answering 2 = protocol error)
/// surfaces as `CommandStatus` with the common-status message.
#[tokio::test]
async fn settings_non_success_status_surfaces_command_status() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&settings_response(oof_set_element("2")))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .settings_oof_set(&OofSettings::default())
        .await
        .expect_err("status 2 must surface");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::CommandStatus { status: 2, .. }
        ),
        "expected CommandStatus 2, got {err:?}"
    );
}

/// ValidateCert: command status 1 + one per-certificate verdict (root-level
/// `Certificate` elements per the parser).
#[tokio::test]
async fn validate_cert_returns_per_certificate_verdicts() {
    super::harness::init_logger();
    let response = WbxmlElement::container(
        pages::VALIDATE,
        validatecert::VALIDATE_CERT,
        vec![
            WbxmlElement::text(pages::VALIDATE, validatecert::STATUS, "1"),
            WbxmlElement::container(
                pages::VALIDATE,
                validatecert::CERTIFICATE,
                vec![
                    WbxmlElement::text(pages::VALIDATE, validatecert::CERTIFICATE_CHAIN, "MA=="),
                    WbxmlElement::text(pages::VALIDATE, validatecert::STATUS, "1"),
                ],
            ),
        ],
    );
    let server =
        MockServer::http(
            Arc::new(move |_: &CapturedRequest, _| MockResponse::wbxml(&response)) as Handler,
        );
    let mut client = client_at(&server.eas_url());
    let result = client
        .validate_cert(&ValidateCertRequest {
            certificate_chain: vec!["MA==".into()],
            certificates: vec!["MA==".into()],
            check_crl: false,
        })
        .await
        .expect("ValidateCert parses");
    assert_eq!(result.status, 1);
    assert_eq!(result.certificate_statuses, vec![1]);
}

/// ValidateCert answering a non-1 command status surfaces as CommandStatus.
#[tokio::test]
async fn validate_cert_non_success_surfaces() {
    super::harness::init_logger();
    let response = WbxmlElement::container(
        pages::VALIDATE,
        validatecert::VALIDATE_CERT,
        vec![WbxmlElement::text(
            pages::VALIDATE,
            validatecert::STATUS,
            "17",
        )],
    );
    let server =
        MockServer::http(
            Arc::new(move |_: &CapturedRequest, _| MockResponse::wbxml(&response)) as Handler,
        );
    let mut client = client_at(&server.eas_url());
    let err = client
        .validate_cert(&ValidateCertRequest {
            certificate_chain: vec![],
            certificates: vec!["MA==".into()],
            check_crl: false,
        })
        .await
        .expect_err("status 17 must surface");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::CommandStatus { status: 17, .. }
        ),
        "expected CommandStatus 17, got {err:?}"
    );
}

/// ResolveRecipients: command status 1 + one resolution per To entry.
#[tokio::test]
async fn resolve_recipients_returns_resolutions() {
    super::harness::init_logger();
    let response = WbxmlElement::container(
        pages::RECIPIENTS,
        recipients::RESOLVE_RECIPIENTS,
        vec![
            WbxmlElement::text(pages::RECIPIENTS, recipients::STATUS, "1"),
            WbxmlElement::container(
                pages::RECIPIENTS,
                recipients::RESPONSE,
                vec![
                    WbxmlElement::text(pages::RECIPIENTS, recipients::TO, "alice@example.test"),
                    WbxmlElement::text(pages::RECIPIENTS, recipients::STATUS, "1"),
                    WbxmlElement::text(pages::RECIPIENTS, recipients::RECIPIENT_COUNT, "1"),
                    WbxmlElement::container(
                        pages::RECIPIENTS,
                        recipients::RECIPIENT,
                        vec![
                            WbxmlElement::text(
                                pages::RECIPIENTS,
                                recipients::DISPLAY_NAME,
                                "Alice Example",
                            ),
                            WbxmlElement::text(
                                pages::RECIPIENTS,
                                recipients::EMAIL_ADDRESS,
                                "alice@example.test",
                            ),
                        ],
                    ),
                ],
            ),
        ],
    );
    let server =
        MockServer::http(
            Arc::new(move |_: &CapturedRequest, _| MockResponse::wbxml(&response)) as Handler,
        );
    let mut client = client_at(&server.eas_url());
    let result = client
        .resolve_recipients(&ResolveRecipientsRequest {
            to: vec!["alice@example.test".into()],
            max_ambiguous_recipients: Some(5),
            availability: None,
        })
        .await
        .expect("ResolveRecipients parses");
    assert_eq!(result.status, 1);
    assert_eq!(result.responses.len(), 1);
    assert_eq!(result.responses[0].status, 1);
}

/// ResolveRecipients with a non-1 command status (6 = server error)
/// surfaces as CommandStatus.
#[tokio::test]
async fn resolve_recipients_non_success_surfaces() {
    super::harness::init_logger();
    let response = WbxmlElement::container(
        pages::RECIPIENTS,
        recipients::RESOLVE_RECIPIENTS,
        vec![WbxmlElement::text(
            pages::RECIPIENTS,
            recipients::STATUS,
            "6",
        )],
    );
    let server =
        MockServer::http(
            Arc::new(move |_: &CapturedRequest, _| MockResponse::wbxml(&response)) as Handler,
        );
    let mut client = client_at(&server.eas_url());
    let err = client
        .resolve_recipients(&ResolveRecipientsRequest {
            to: vec!["alice@example.test".into()],
            max_ambiguous_recipients: None,
            availability: None,
        })
        .await
        .expect_err("status 6 must surface");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::CommandStatus { status: 6, .. }
        ),
        "expected CommandStatus 6, got {err:?}"
    );
}

// ---- non-1 status gates of the remaining Settings family members ----

/// UserInformation answering a non-1 effective status surfaces as
/// `CommandStatus` (the UserInformation-level Status overrides the top one).
#[tokio::test]
async fn user_information_non_success_surfaces() {
    super::harness::init_logger();
    let mut ui = user_information_element("user@example.test");
    // Flip the UserInformation-level Status to 2 (the more-specific-wins rule).
    if let Some(status) = ui.children.first_mut() {
        status.value = provider_eas::wbxml::WbxmlValue::Text("2".into());
    }
    let server = MockServer::http(Arc::new(move |_: &CapturedRequest, _| {
        MockResponse::wbxml(&settings_response(ui.clone()))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .settings_user_information()
        .await
        .expect_err("status 2 must surface");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::CommandStatus { status: 2, .. }
        ),
        "expected CommandStatus 2, got {err:?}"
    );
}

/// Oof Get answering a non-1 effective status surfaces as `CommandStatus`.
#[tokio::test]
async fn oof_get_non_success_surfaces() {
    super::harness::init_logger();
    let mut oof = oof_get_element("0", "reply");
    if let Some(status) = oof.children.first_mut() {
        status.value = provider_eas::wbxml::WbxmlValue::Text("3".into());
    }
    let server = MockServer::http(Arc::new(move |_: &CapturedRequest, _| {
        MockResponse::wbxml(&settings_response(oof.clone()))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .settings_oof_get("Text")
        .await
        .expect_err("status 3 surfaces");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::CommandStatus { status: 3, .. }
        ),
        "expected CommandStatus 3, got {err:?}"
    );
}

/// DevicePassword Set answering a non-1 status surfaces as `CommandStatus`.
#[tokio::test]
async fn device_password_non_success_surfaces() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&settings_response(device_password_element("165")))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .settings_device_password("recovery-secret")
        .await
        .expect_err("status 165 surfaces");
    assert!(
        matches!(
            &err,
            provider_eas::client::EasError::CommandStatus { status: 165, .. }
        ),
        "expected CommandStatus 165, got {err:?}"
    );
}

/// A scheduled OOF window with all three audiences parses fully: the
/// Start/End times land and each audience's Enabled flag survives.
#[tokio::test]
async fn oof_get_scheduled_window_and_all_audiences_parse() {
    use provider_eas::wbxml::{WbxmlElement, tags::settings};
    super::harness::init_logger();

    let message = |applies: u8, enabled: &str, reply: &str| {
        WbxmlElement::container(
            pages::SETTINGS,
            settings::OOF_MESSAGE,
            vec![
                WbxmlElement::empty(pages::SETTINGS, applies),
                WbxmlElement::text(pages::SETTINGS, settings::ENABLED, enabled),
                WbxmlElement::text(pages::SETTINGS, settings::REPLY_MESSAGE, reply),
                WbxmlElement::text(pages::SETTINGS, settings::BODY_TYPE, "Text"),
            ],
        )
    };
    let get = WbxmlElement::container(
        pages::SETTINGS,
        settings::GET,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::OOF_STATE, "2"),
            WbxmlElement::text(
                pages::SETTINGS,
                settings::START_TIME,
                "2026-09-01T08:00:00.000Z",
            ),
            WbxmlElement::text(
                pages::SETTINGS,
                settings::END_TIME,
                "2026-09-08T08:00:00.000Z",
            ),
            message(settings::APPLIES_TO_INTERNAL, "1", "internal away"),
            message(settings::APPLIES_TO_EXTERNAL_KNOWN, "1", "known away"),
            message(settings::APPLIES_TO_EXTERNAL_UNKNOWN, "0", "unknown away"),
        ],
    );
    let oof = WbxmlElement::container(
        pages::SETTINGS,
        settings::OOF,
        vec![
            WbxmlElement::text(pages::SETTINGS, settings::STATUS, "1"),
            get,
        ],
    );
    let server = MockServer::http(Arc::new(move |_: &CapturedRequest, _| {
        MockResponse::wbxml(&settings_response(oof.clone()))
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let parsed = client
        .settings_oof_get("Text")
        .await
        .expect("scheduled parse");
    assert_eq!(parsed.state, Some(2));
    assert_eq!(
        parsed.start_time.as_deref(),
        Some("2026-09-01T08:00:00.000Z"),
        "the scheduled window start lands: {:?}",
        parsed.start_time
    );
    assert_eq!(parsed.end_time.as_deref(), Some("2026-09-08T08:00:00.000Z"));
    assert_eq!(parsed.messages.len(), 3, "all three audiences parse");
    assert!(parsed.messages.iter().any(|m| m.enabled == Some(true)));
    assert!(parsed.messages.iter().any(|m| m.enabled == Some(false)));
}
