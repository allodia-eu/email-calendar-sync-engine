//! The `Box<dyn ReportingProvider>` blanket implementation.
//!
//! Its own file rather than a third impl beside the other two: `boxed.rs` is already
//! near the size limit.

use async_trait::async_trait;
use engine_core::ids::AccountId;

use crate::{MessageReport, ProviderResult, ReportReceipt, ReportingProvider};

/// A boxed reporting adapter is itself a [`ReportingProvider`], for the same reason its
/// [`Provider`](crate::Provider) and [`ContactsProvider`](crate::ContactsProvider)
/// counterparts exist: `engine-api`'s `Engine::report_message` is generic over
/// `P: ReportingProvider`, so a host that picks its adapter at runtime — a language
/// binding choosing IMAP vs JMAP from account config — cannot reach it through a trait
/// object without this.
///
/// [`ReportingProvider::report_message`] has a default body that **rejects**, so an impl
/// that forgot to forward would not fail to compile: it would answer "provider does not
/// support reporting a message" for an adapter that reports perfectly well. The test
/// beside this is what holds the forwarding.
#[async_trait]
impl<P: ReportingProvider + ?Sized> ReportingProvider for Box<P> {
    async fn report_message(
        &self,
        account: &AccountId,
        report: &MessageReport,
    ) -> ProviderResult<ReportReceipt> {
        (**self).report_message(account, report).await
    }
}

#[cfg(test)]
#[path = "boxed_report_tests.rs"]
mod tests;
