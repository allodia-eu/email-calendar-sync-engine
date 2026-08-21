//! Reporting a message as junk, not junk, or phishing.
//!
//! A report is **not** a keyword change wearing a different name, which is why it is
//! its own verb rather than a [`MailEdit`](crate::MailEdit) variant. Setting `$seen`
//! changes what one mailbox row says; reporting tells the *provider* something about
//! the message, and on one transport it leaves the account entirely. The same split
//! the calendar side draws between
//! [`patch_event`](crate::Provider::patch_event) and
//! [`rsvp_event`](crate::Provider::rsvp_event): an edit changes an object, an answer
//! makes the server tell someone.
//!
//! The four transports disagree about almost everything around it, so the capability
//! ([`ReportControls`]) carries the differences a host must know **before** it offers
//! the action:
//!
//! - **Which verdicts exist.** Gmail has no phishing concept at all — asking for one is a hard
//!   error, not a no-op — so "Report phishing" must be absent there rather than quietly filed as
//!   junk.
//! - **Whether the provider acknowledges the report** ([`ReportEvidence`]). Graph answers with a
//!   status; JMAP, IMAP and Gmail take a flag or a label and say nothing about what they do with
//!   it. A host writing "this is reported to your provider" over the second kind is claiming
//!   something the engine cannot back.
//!
//! Every transport **files the message** as part of a report — to the account's Junk
//! for [`ReportVerdict::Junk`] and [`ReportVerdict::Phishing`], back to the Inbox for
//! [`ReportVerdict::NotJunk`]. They differ only in *who* moves it, which is an
//! adapter concern rather than a caller's: Graph and Gmail file it server-side and
//! cannot be told not to, so [`MessageReport::destination`] is the mailbox the caller
//! *would* have moved it to, and the adapters that need it use it.
//!
//! No transport we speak adds the sender to a block list. Outlook's own "Report Junk"
//! dialog says it blocks the sender, and its **deprecated** `markAsJunk` did; the
//! `reportMessage` action that replaced it was observed not to (`graph.md`). If a
//! provider that blocks is ever added, that becomes a third field here, because it is
//! a promise the user has to be shown before they press the button — not a detail.

use async_trait::async_trait;
use engine_core::ids::{AccountId, MailboxId, ProviderKey};
use serde::{Deserialize, Serialize};

use crate::{Provider, ProviderError, ProviderResult};

/// What the user is saying about the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportVerdict {
    /// Unsolicited bulk mail. The message belongs in Junk, and the provider should
    /// treat it as a spam sample.
    Junk,
    /// Legitimate mail that was filed as junk. The message belongs back in the Inbox,
    /// and the provider should treat it as a ham sample.
    NotJunk,
    /// A deliberate attempt to steal from or impersonate someone — a stronger claim
    /// than [`Junk`](Self::Junk), and a separate verdict wherever the transport has
    /// one, because providers route it differently.
    Phishing,
}

impl ReportVerdict {
    /// Whether the message belongs in the account's Junk mailbox afterwards. `false`
    /// only for [`NotJunk`](Self::NotJunk), which sends it back to the Inbox.
    #[must_use]
    pub const fn files_as_junk(self) -> bool {
        matches!(self, Self::Junk | Self::Phishing)
    }
}

/// Which verdicts a transport can express.
///
/// Not every transport has all three, and the gap is not cosmetic: a host builds its
/// "Report" menu from this, and an adapter asked for a verdict it lacks
/// **refuses** rather than substituting a near-enough one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportVerdicts {
    /// The transport can report junk.
    pub junk: bool,
    /// The transport can report not-junk.
    pub not_junk: bool,
    /// The transport can report phishing **as distinct from junk**. `false` on Gmail,
    /// whose label set has no phishing member (`google.md`).
    pub phishing: bool,
}

impl ReportVerdicts {
    /// All three verdicts — JMAP, IMAP and Graph.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            junk: true,
            not_junk: true,
            phishing: true,
        }
    }

    /// Junk and not-junk, but no distinct phishing verdict — Gmail.
    #[must_use]
    pub const fn without_phishing() -> Self {
        Self {
            junk: true,
            not_junk: true,
            phishing: false,
        }
    }

    /// Whether `verdict` is one this transport can express.
    #[must_use]
    pub const fn allows(self, verdict: ReportVerdict) -> bool {
        match verdict {
            ReportVerdict::Junk => self.junk,
            ReportVerdict::NotJunk => self.not_junk,
            ReportVerdict::Phishing => self.phishing,
        }
    }
}

/// How much the provider actually tells us about a report.
///
/// This exists because a capability cannot say "…and it works", and the honest answer
/// differs by transport. A host uses it to decide what it is entitled to *say* — the
/// difference between "reported to your provider" and "your mail server has been
/// told", which is not a wording preference but the difference between a claim we can
/// back and one we cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportEvidence {
    /// The provider takes the report as an explicit action and answers whether it was
    /// accepted. **Graph** only: `POST /messages/{id}/reportMessage` returns a status.
    Acknowledged,
    /// The report is a **convention**: we set a flag or a label the provider is
    /// expected to notice, and the protocol offers no way to tell whether it did.
    ///
    /// RFC 8621 §4.1.1 says clients SHOULD set `$junk` "to help train automated
    /// spam-detection systems" — a client-side SHOULD with no server obligation, no
    /// capability to probe, and no error if the server ignores it. The same is true of
    /// the IMAP keyword and of Gmail's `SPAM` label, whose filter is documented to
    /// learn from the label but reports nothing back.
    ///
    /// Note this is **not** "it probably did not work". Stalwart, Gmail and most IMAP
    /// servers do train on these. It says only that the engine has no evidence, and so
    /// neither does anything built on it.
    Convention,
}

/// What a transport lets a caller do when the user reports a message.
///
/// Read **before** offering the action. An adapter refuses a verdict it cannot express
/// rather than substituting one, and [`accept`](Self::accept) is the single shared
/// implementation of that rule, so an adapter cannot advertise a verdict it then drops
/// or drop one it advertises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportControls {
    /// Which of the three verdicts this transport can express.
    pub verdicts: ReportVerdicts,
    /// Whether the provider acknowledges the report or merely receives a convention.
    pub evidence: ReportEvidence,
}

impl ReportControls {
    /// Refuses a report asking for a verdict this transport does not have.
    ///
    /// # Errors
    ///
    /// Returns an [`InvalidState`](engine_core::error::FailureClass::InvalidState)
    /// [`ProviderError`] naming the verdict. A host that read
    /// [`Capabilities::mail_report`](crate::Capabilities::mail_report) never reaches
    /// it.
    pub fn accept(self, report: &MessageReport) -> Result<(), ProviderError> {
        if !self.verdicts.allows(report.verdict) {
            return Err(ProviderError::invalid_state(format!(
                "this transport cannot report {:?}; read Capabilities::mail_report \
                 before offering the verdict",
                report.verdict
            )));
        }
        Ok(())
    }
}

/// A request to report one already-synced message.
///
/// Serializable so the outbox can store it as a durable payload before the side
/// effect, exactly like [`MailEdit`](crate::MailEdit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageReport {
    /// The message being reported.
    pub target: ProviderKey,
    /// What the user is saying about it.
    pub verdict: ReportVerdict,
    /// Where the message belongs afterwards — the account's Junk mailbox for a junk or
    /// phishing verdict, its Inbox for not-junk. The caller resolves it, exactly as it
    /// resolves Trash for a [`MailEdit::MoveTo`](crate::MailEdit::MoveTo) delete.
    ///
    /// Adapters whose server files the message itself (Graph, Gmail) do not send it.
    /// That is not a dropped control: the message lands in the same place either way,
    /// and the caller has no way to ask for a *different* one on any transport.
    pub destination: MailboxId,
}

impl MessageReport {
    /// Reports `target` with `verdict`, filing it into `destination`.
    #[must_use]
    pub fn new(target: ProviderKey, verdict: ReportVerdict, destination: MailboxId) -> Self {
        Self {
            target,
            verdict,
            destination,
        }
    }
}

/// The result of a successful [`MessageReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportReceipt {
    /// The provider key the outbox records as resolved — the reported message's key.
    ///
    /// For a transport whose move mints a new key (IMAP synthesizes a new
    /// `(mailbox, UIDVALIDITY, UID)`) this is the **source** key, and the destination
    /// copy reconciles on that mailbox's next sync — the same contract as
    /// [`MailEditReceipt`](crate::MailEditReceipt).
    pub message_key: ProviderKey,
}

impl ReportReceipt {
    /// Records a successful report.
    #[must_use]
    pub fn new(message_key: ProviderKey) -> Self {
        Self { message_key }
    }
}

/// Reporting a message to its provider — an optional capability over [`Provider`].
///
/// A sub-trait rather than a method on [`Provider`] for the same reason
/// [`ContactsProvider`](crate::ContactsProvider) is one: it is optional, gated by its
/// own capability, and an adapter that cannot do it should not have to say so.
#[async_trait]
pub trait ReportingProvider: Provider {
    /// Reports `report.target` to the provider.
    ///
    /// Providers advertising
    /// [`Capabilities::mail_report`](crate::Capabilities::mail_report) override this;
    /// the default rejects, so a capability-checking caller never relies on it.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`].
    /// [`InvalidState`](engine_core::error::FailureClass::InvalidState) for a verdict
    /// the transport cannot express (via [`ReportControls::accept`]); a stale target —
    /// an IMAP UID under a changed `UIDVALIDITY` — is
    /// [`Conflict`](engine_core::error::FailureClass::Conflict), so the caller
    /// re-syncs and retries.
    async fn report_message(
        &self,
        account: &AccountId,
        report: &MessageReport,
    ) -> ProviderResult<ReportReceipt> {
        let _ = (account, report);
        Err(ProviderError::invalid_state(
            "provider does not support reporting a message",
        ))
    }
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
