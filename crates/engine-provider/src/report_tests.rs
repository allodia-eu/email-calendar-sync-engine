use engine_core::error::FailureClass;

use super::*;
use crate::Capabilities;

fn key() -> ProviderKey {
    ProviderKey::new("imap:v1:u42@INBOX").unwrap()
}

fn junk_mailbox() -> MailboxId {
    MailboxId::try_from("Junk").unwrap()
}

fn report(verdict: ReportVerdict) -> MessageReport {
    MessageReport::new(key(), verdict, junk_mailbox())
}

#[test]
fn junk_and_phishing_file_as_junk_but_not_junk_does_not() {
    assert!(ReportVerdict::Junk.files_as_junk());
    assert!(ReportVerdict::Phishing.files_as_junk());
    assert!(!ReportVerdict::NotJunk.files_as_junk());
}

#[test]
fn a_transport_without_phishing_allows_only_the_other_two() {
    let verdicts = ReportVerdicts::without_phishing();
    assert!(verdicts.allows(ReportVerdict::Junk));
    assert!(verdicts.allows(ReportVerdict::NotJunk));
    assert!(!verdicts.allows(ReportVerdict::Phishing));

    let all = ReportVerdicts::all();
    for verdict in [
        ReportVerdict::Junk,
        ReportVerdict::NotJunk,
        ReportVerdict::Phishing,
    ] {
        assert!(all.allows(verdict), "{verdict:?} should be allowed");
    }
}

#[test]
fn accept_refuses_a_verdict_the_transport_cannot_express() {
    let controls = ReportControls {
        verdicts: ReportVerdicts::without_phishing(),
        evidence: ReportEvidence::Convention,
    };

    // The two it has are accepted.
    controls.accept(&report(ReportVerdict::Junk)).unwrap();
    controls.accept(&report(ReportVerdict::NotJunk)).unwrap();

    // The one it lacks is refused, not silently downgraded to junk.
    let err = controls
        .accept(&report(ReportVerdict::Phishing))
        .expect_err("phishing must be refused where the transport has no such verdict");
    assert_eq!(err.class(), FailureClass::InvalidState);
    assert!(
        err.detail().contains("Phishing"),
        "the error should name the verdict, got: {}",
        err.detail()
    );
}

#[test]
fn a_report_round_trips_through_the_outbox_payload_encoding() {
    // The outbox stores the request as serde JSON before the side effect, so the
    // shape has to survive that trip unchanged.
    let original = report(ReportVerdict::Phishing);
    let encoded = serde_json::to_value(&original).unwrap();
    let decoded: MessageReport = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn capabilities_report_none_until_advertised() {
    assert!(Capabilities::none().mail_report().is_none());

    let controls = ReportControls {
        verdicts: ReportVerdicts::all(),
        evidence: ReportEvidence::Acknowledged,
    };
    let caps = Capabilities::none()
        .with_mail_writes()
        .with_mail_report(controls);
    assert_eq!(caps.mail_report(), Some(controls));
    // Reporting is its own capability: advertising writes never implies it.
    assert!(
        Capabilities::none()
            .with_mail_writes()
            .mail_report()
            .is_none()
    );
}

#[tokio::test]
async fn the_default_provider_impl_rejects_rather_than_pretending() {
    struct Unsupported;

    #[async_trait]
    impl Provider for Unsupported {
        fn connection_info(&self) -> crate::ConnectionInfo {
            crate::ConnectionInfo::new(Capabilities::none())
        }
    }
    impl ReportingProvider for Unsupported {}

    let account = AccountId::try_from("account").unwrap();
    let err = Unsupported
        .report_message(&account, &report(ReportVerdict::Junk))
        .await
        .expect_err("the default must reject");
    assert_eq!(err.class(), FailureClass::InvalidState);
}
