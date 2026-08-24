use engine_core::{
    error::FailureClass,
    ids::{MailboxId, ProviderKey},
};

use super::*;
use crate::{
    Capabilities, ConnectionInfo, Provider, ReportControls, ReportEvidence, ReportVerdict,
    ReportVerdicts,
};

fn account() -> AccountId {
    AccountId::try_from("account").unwrap()
}

fn report() -> MessageReport {
    MessageReport::new(
        ProviderKey::new("imap:v1:u42@INBOX").unwrap(),
        ReportVerdict::Junk,
        MailboxId::try_from("Junk").unwrap(),
    )
}

/// An adapter that overrides `report_message`, so a lost delegation shows: the trait
/// default rejects, this answers with a receipt.
struct Reports;

#[async_trait]
impl Provider for Reports {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail_report(ReportControls {
            verdicts: ReportVerdicts::all(),
            evidence: ReportEvidence::Convention,
        }))
    }
}

#[async_trait]
impl ReportingProvider for Reports {
    async fn report_message(
        &self,
        _account: &AccountId,
        report: &MessageReport,
    ) -> ProviderResult<ReportReceipt> {
        Ok(ReportReceipt::new(report.target.clone()))
    }
}

/// An adapter that reports nothing, taking the rejecting default.
struct Silent;

#[async_trait]
impl Provider for Silent {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none())
    }
}

impl ReportingProvider for Silent {}

/// Mirrors `engine-api`'s `Engine::report_message`, the only shape that needs the
/// blanket impl. A plain `boxed.report_message(..)` call would auto-deref to
/// `<dyn ReportingProvider>::report_message` and pass with no impl at all.
async fn as_engine_would<P: ReportingProvider>(
    provider: &P,
    report: &MessageReport,
) -> ProviderResult<ReportReceipt> {
    provider.report_message(&account(), report).await
}

#[tokio::test]
async fn a_boxed_adapter_reports_through_to_the_one_inside() {
    let boxed: Box<dyn ReportingProvider> = Box::new(Reports);

    let receipt = as_engine_would(&boxed, &report())
        .await
        .expect("the box must forward to the override, not answer the rejecting default");

    assert_eq!(receipt.message_key.as_str(), "imap:v1:u42@INBOX");
}

#[tokio::test]
async fn a_boxed_adapter_that_cannot_report_still_rejects() {
    let boxed: Box<dyn ReportingProvider> = Box::new(Silent);

    let err = as_engine_would(&boxed, &report())
        .await
        .expect_err("an adapter with no reporting must still refuse through the box");

    assert_eq!(err.class(), FailureClass::InvalidState);
}
