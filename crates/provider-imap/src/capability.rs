//! What a session may ask for: the extensions the server **advertised**, the ones it
//! **confirmed** in `ENABLE`, and the ones IMAP4rev2 supplies without either.
//!
//! IMAP4rev2 (RFC 9051) is largely IMAP4rev1 with a set of previously separate extensions
//! folded into the base protocol, so a rev2 session has them whether or not the server
//! names them in `CAPABILITY` — the client can turn one thing on and confidently rely on
//! all of them. [`Extension::folded_into_rev2`] is that list, taken from RFC 9051
//! Appendix E items 2–3, and nothing else in this crate second-guesses it.
//!
//! Two distinctions this module exists to keep straight, because collapsing either is a
//! bug that only shows up on some servers:
//!
//! - **Advertised is not enabled.** A server that supports both revisions announces `IMAP4rev2` in
//!   its greeting and then behaves as rev1 until the client sends `ENABLE IMAP4rev2` and it answers
//!   `* ENABLED IMAP4rev2` (RFC 9051 §6.3.1). Reading the capability as the session's dialect makes
//!   the client decode UTF-8 names that are still arriving as modified UTF-7.
//! - **Folded in is not the same as will arrive.** rev2 folds in SPECIAL-USE's *mailbox attributes*
//!   (Appendix E item 2) and makes them base attributes of every `LIST` response (§7.3.1), which
//!   reads like a rev2 session never has to ask for them. A rev2 server that also advertises RFC
//!   6154 may keep RFC 6154's rule anyway — Dovecot's rev2 does, and strips every role from an
//!   extended `LIST` that did not ask. So what a session may *use* and what it must still *request*
//!   are two questions, answered by [`Negotiated::has`] and
//!   [`Negotiated::must_request_special_use`] respectively. Neither is a question about the
//!   dialect.

use std::collections::BTreeSet;

/// The `ENABLE` argument that switches a dual-revision server into IMAP4rev2 behaviour,
/// and the capability that advertises it.
const IMAP4REV2: &str = "IMAP4rev2";

/// An optional IMAP extension this client changes its behaviour for.
///
/// Deliberately only the ones something actually consults: an entry here that no code
/// reads is a claim about a server nobody checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Extension {
    /// `IDLE` (RFC 2177) — a standing connection can be pushed change notifications.
    Idle,
    /// `LIST-STATUS` (RFC 5819) — a folder list can carry its unread counts.
    ListStatus,
    /// `SPECIAL-USE` (RFC 6154) — `LIST` can be asked for the role attributes.
    SpecialUse,
    /// `QRESYNC` (RFC 7162) — a mailbox delta can reconcile flags and expunges.
    Qresync,
}

impl Extension {
    /// The capability atom a server advertises this under.
    pub(crate) const fn atom(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::ListStatus => "LIST-STATUS",
            Self::SpecialUse => "SPECIAL-USE",
            Self::Qresync => "QRESYNC",
        }
    }

    /// Whether IMAP4rev2 folds this into the base protocol (RFC 9051 Appendix E items
    /// 2–3), so a rev2 session has it without the server advertising it separately.
    ///
    /// `QRESYNC` is **not** folded in — rev2 took only its `CLOSED` response code (item 9),
    /// leaving the extension itself to RFC 7162 and to its own `ENABLE`.
    pub(crate) const fn folded_into_rev2(self) -> bool {
        match self {
            Self::Idle | Self::ListStatus | Self::SpecialUse => true,
            Self::Qresync => false,
        }
    }

    /// Whether the client must `ENABLE` this before the server's behaviour changes
    /// (RFC 5161). Most extensions only add commands or response data and need no
    /// announcement; one changes what the server sends unbidden, and so does the dialect.
    pub(crate) const fn needs_enable(self) -> bool {
        match self {
            Self::Qresync => true,
            Self::Idle | Self::ListStatus | Self::SpecialUse => false,
        }
    }
}

/// The dialect and extension set of one authenticated session.
#[derive(Debug, Default, Clone)]
pub(crate) struct Negotiated {
    /// Capability atoms from the post-auth `CAPABILITY`, lowercased for comparison.
    advertised: BTreeSet<String>,
    /// Atoms the server confirmed in an `* ENABLED` response, lowercased. A bare
    /// `* ENABLED` enables nothing (RFC 5161 §3.1), which this represents as an empty set
    /// rather than as success.
    enabled: BTreeSet<String>,
}

impl Negotiated {
    /// Records what the server advertised. Any previously confirmed `ENABLE` is dropped:
    /// a fresh capability list belongs to a fresh session (a STARTTLS upgrade, a re-dial).
    pub(crate) fn from_capabilities(capabilities: &[String]) -> Self {
        Self {
            advertised: capabilities.iter().map(|c| c.to_lowercase()).collect(),
            enabled: BTreeSet::new(),
        }
    }

    /// Whether the server named `atom` in `CAPABILITY`.
    fn advertises(&self, atom: &str) -> bool {
        self.advertised.contains(&atom.to_lowercase())
    }

    /// The arguments for a single `ENABLE`, in the order they are sent: the dialect first,
    /// then every extension that needs enabling and was advertised. Empty when there is
    /// nothing to enable, in which case no `ENABLE` is issued at all.
    pub(crate) fn enable_arguments(&self) -> Vec<&'static str> {
        let mut arguments = Vec::new();
        if self.advertises(IMAP4REV2) {
            arguments.push(IMAP4REV2);
        }
        arguments.extend(
            [
                Extension::Idle,
                Extension::ListStatus,
                Extension::SpecialUse,
                Extension::Qresync,
            ]
            .into_iter()
            .filter(|ext| ext.needs_enable() && self.advertises(ext.atom()))
            .map(Extension::atom),
        );
        arguments
    }

    /// Records what an `* ENABLED` response confirmed. Only these take effect: a server may
    /// answer `OK` while enabling a subset, or nothing at all.
    pub(crate) fn confirm_enabled(&mut self, enabled: &[String]) {
        self.enabled
            .extend(enabled.iter().map(|atom| atom.to_lowercase()));
    }

    /// Whether this session is speaking IMAP4rev2 — **confirmed**, never merely offered.
    pub(crate) fn rev2(&self) -> bool {
        self.enabled.contains(&IMAP4REV2.to_lowercase())
    }

    /// Whether this session may use `ext`: the server advertised it, or rev2 folded it in.
    ///
    /// An extension that [`needs_enable`](Extension::needs_enable) must also have been
    /// confirmed, since advertising it changes nothing on its own.
    pub(crate) fn has(&self, ext: Extension) -> bool {
        if ext.needs_enable() {
            return self.enabled.contains(&ext.atom().to_lowercase());
        }
        self.advertises(ext.atom()) || (self.rev2() && ext.folded_into_rev2())
    }

    /// Whether a `LIST` has to ask for the SPECIAL-USE attributes with a return option.
    ///
    /// Gated on what the server **advertised**, not on the dialect and not on
    /// [`has`](Self::has):
    ///
    /// - **Advertised, so ask — on either dialect.** rev2 makes those attributes base `LIST` data
    ///   (RFC 9051 §7.3.1) and defines no return option for them, which reads like a rev2 session
    ///   need never ask. A rev2 server that *also* advertises RFC 6154 may keep RFC 6154's rule
    ///   instead, and Dovecot's rev2 does: an extended `LIST` that does not ask comes back with
    ///   every role attribute stripped, so the sent copy has no folder to go to (`place.rs`).
    ///   Asking costs nothing where the attributes were coming anyway.
    /// - **Not advertised, so never ask.** On rev2 `has` is true for a server that never named RFC
    ///   6154, because rev2 folded the attributes in — but a client MUST NOT send a return option
    ///   the server has not advertised, and the server MUST answer `BAD` (RFC 9051 §6.3.9). Those
    ///   attributes arrive as base data or not at all.
    pub(crate) fn must_request_special_use(&self) -> bool {
        self.advertises(Extension::SpecialUse.atom())
    }

    /// The dialect this session settled on, named as the protocol names it — for the
    /// connect trace a support session reads, never for a behavioural decision (those go
    /// through [`has`](Self::has), so a new dialect cannot silently change one).
    pub(crate) fn dialect(&self) -> &'static str {
        if self.rev2() {
            "IMAP4rev2"
        } else {
            "IMAP4rev1"
        }
    }

    /// Every extension this session may use, in declaration order so two logs compare.
    ///
    /// Reports what is **usable**, not what was advertised: on rev2 that includes the
    /// extensions folded into the base protocol, which is exactly the difference a
    /// support session is trying to see.
    pub(crate) fn available_extensions(&self) -> Vec<&'static str> {
        [
            Extension::Idle,
            Extension::ListStatus,
            Extension::SpecialUse,
            Extension::Qresync,
        ]
        .into_iter()
        .filter(|ext| self.has(*ext))
        .map(Extension::atom)
        .collect()
    }

    /// Whether mailbox names on this session's wire are modified UTF-7 (RFC 3501 §5.1.3)
    /// rather than UTF-8 (RFC 9051 §5.1) — the one place the dialect reaches the data.
    pub(crate) fn names_are_modified_utf7(&self) -> bool {
        !self.rev2()
    }
}

#[cfg(test)]
#[path = "capability_tests.rs"]
mod tests;
