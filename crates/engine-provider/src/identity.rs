//! The addresses an account may send as, and the name that goes out with each.
//!
//! Every transport puts a display name on outgoing mail — the `From` header the
//! assembler writes (`engine-rfc5322`), or the `from` object JMAP's `Email/set`
//! takes. What differs is **who owns that name**, and the difference is not
//! cosmetic: it decides whether a host may offer to change it.
//!
//! - **Nobody but the host.** IMAP/SMTP has no identity object at all. The name is the host's to
//!   keep and to put on the wire, and [`Capabilities::sender_identities`] is `None`.
//! - **The server, and the account holder may change it.** JMAP keeps `Identity` objects (RFC 8621
//!   §6) and Gmail keeps send-as aliases, both writable by the signed-in user:
//!   [`IdentityControls::Writable`].
//! - **The server, and the account holder may not.** A Graph mailbox's display name comes from the
//!   directory, which a tenant administrator owns: [`IdentityControls::ReadOnly`]. A host that
//!   offers an editor here is offering an edit that cannot land.
//!
//! Reading identities is what lets a host fill its "your name" field in without
//! asking someone to type what the server already knows. The engine stores none of
//! it: a sender name is a host preference, not synced state, and the account's own
//! copy is what reaches the wire.

use core::fmt;

use engine_core::mail::EmailAddress;

/// A provider's own handle for one sender identity.
///
/// Opaque, and provider-shaped: a JMAP `Identity` id, a Gmail send-as address. A host
/// reads it from [`SenderIdentity::id`] and passes it back to
/// [`Provider::set_sender_name`](crate::Provider::set_sender_name); nothing else may be
/// inferred from its contents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SenderIdentityId(Box<str>);

impl SenderIdentityId {
    /// Wraps a provider-assigned identity handle.
    #[must_use]
    pub fn new(value: impl Into<Box<str>>) -> Self {
        Self(value.into())
    }

    /// The handle as the provider spelled it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SenderIdentityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One address the account may send as, with the name the server holds for it.
///
/// The address carries both halves, so a caller that has matched
/// [`address`](Self::address)`.email` against the account's own address already holds
/// the name it was looking for. `name` is `None` where the server has one but has
/// stored nothing in it, which is the ordinary state of a freshly created mailbox and
/// is exactly the case a host fills in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderIdentity {
    /// The provider's handle, for [`Provider::set_sender_name`](crate::Provider::set_sender_name).
    pub id: SenderIdentityId,
    /// The address, and the display name the server currently holds for it.
    pub address: EmailAddress,
}

impl SenderIdentity {
    /// Builds an identity from a provider handle and the address it stands for.
    #[must_use]
    pub fn new(id: SenderIdentityId, address: EmailAddress) -> Self {
        Self { id, address }
    }
}

/// What a provider lets a host do with the account's sender identities.
///
/// One field on [`Capabilities`](crate::Capabilities) rather than a "can read" flag
/// beside a "can write" flag, so "writable but unreadable" is unrepresentable — the
/// same shape as [`WriteGuard`](crate::WriteGuard) on calendar writes.
///
/// Neither variant promises the *server* will accept a given change: JMAP advertises
/// no per-account flag for whether `Identity/set` is permitted, and a Gmail token may
/// simply lack the settings scope. A capability cannot say "…and it works"
/// (`crate::report` makes the same point about [`ReportEvidence`](crate::ReportEvidence));
/// what it can say is which door exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityControls {
    /// The names can be read and not changed: they belong to a directory the account
    /// holder does not administer (Graph).
    ReadOnly,
    /// The names can be read and changed (JMAP `Identity/set`, Gmail send-as patch).
    Writable,
}

impl IdentityControls {
    /// Whether a host may offer to change the name.
    #[must_use]
    pub const fn writable(self) -> bool {
        matches!(self, Self::Writable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_writable_controls_permit_an_edit() {
        assert!(IdentityControls::Writable.writable());
        assert!(!IdentityControls::ReadOnly.writable());
    }

    #[test]
    fn an_identity_carries_the_name_beside_the_address() {
        let identity = SenderIdentity::new(
            SenderIdentityId::new("id-1"),
            EmailAddress::named("Alice Smith", "alice@example.com"),
        );
        assert_eq!(identity.id.as_str(), "id-1");
        assert_eq!(identity.address.name.as_deref(), Some("Alice Smith"));
        assert_eq!(identity.address.email, "alice@example.com");
    }

    #[test]
    fn a_server_holding_no_name_yields_none_rather_than_an_empty_string() {
        // The ordinary state of a fresh mailbox, and the case a host exists to fill in:
        // it must be distinguishable from a name that is deliberately blank.
        let identity = SenderIdentity::new(
            SenderIdentityId::new("id-2"),
            EmailAddress::new("bob@example.com"),
        );
        assert!(identity.address.name.is_none());
    }

    #[test]
    fn the_handle_displays_as_the_provider_spelled_it() {
        assert_eq!(
            SenderIdentityId::new("alice@example.com").to_string(),
            "alice@example.com"
        );
    }
}
