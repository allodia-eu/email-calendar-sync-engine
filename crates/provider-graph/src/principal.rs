//! Which mailbox a Graph provider addresses.
//!
//! One signed-in user (one OAuth credential) can access several mailboxes: their
//! own (`/me`) and any shared/other mailbox they hold delegate access to
//! (`/users/{address}`, which needs the `*.Shared` delegated scopes). In the engine
//! each mailbox is a **separate account** — its own folders, `GraphFolder` scopes,
//! cursors, and search, scoped by `AccountId` like any other account. They differ
//! only by this principal, which selects the URL root; the credential is shared
//! (host-owned, outside the store — `north-star.md`), and a unified "all my
//! mailboxes" view is host-composed, not a storage-level join.
//!
//! So a shared mailbox is, for the engine, just **another account** pointed at a
//! [`MailboxPrincipal::User`]. Finding one is `crate::shared`; binding to what it found
//! is [`MailboxPrincipal::shared`].
//!
//! # An address is spliced into a URL path, so it is checked first
//!
//! A principal's address is ultimately **user input** — whatever someone typed into an
//! "add a shared mailbox" field — and it becomes a path segment of every request the
//! client makes. Percent-encoding alone does not contain it, because Graph decodes the
//! segment and **re-parses** the path: probed against a real tenant on 2026-09-22,
//! `…/users/a@b.example%2FmailFolders%2Fsentitems/mailFolders/inbox` answers `400 "Resource
//! not found for the segment 'mailFolders'"` — the encoded slash became a separator. The
//! same probe found every other character arriving as data once encoded (`%3F`, `%23`,
//! `%3C`, `;`, and `%252F`, which Graph decodes exactly once to a literal `%2F`), and
//! found `..` segments refused outright (`400`, "must not contain '..' path segments").
//! That last one is Graph's guard, not ours, and it has not always been there: the
//! predecessor of this module recorded `/users/..%2Fme/…` answering `200` with the
//! **signed-in user's own** Inbox. So the path separators Graph re-parses are refused
//! here, and nothing relies on the server continuing to refuse the rest.

use engine_core::ids::SharedMailboxId;

use crate::error::GraphError;

/// A mailbox address a Graph URL can carry: one `local@domain`, free of the path
/// separators Graph re-parses after decoding (see the module docs).
///
/// Construct it with [`MailboxAddress::new`]; there is no other way, so a
/// [`MailboxPrincipal::User`] cannot hold an address that restructures a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxAddress(String);

impl MailboxAddress {
    /// Checks `address` and wraps it.
    ///
    /// Refuses what is not a single `local@domain` with both halves non-empty, anything
    /// carrying whitespace or a control character, and the two path separators, `/` and
    /// `\`. Everything else is allowed — `+` tags, non-ASCII, and the `#` that Entra guest
    /// UPNs really contain (`user_partner.example#EXT#@tenant.example`) — because
    /// the request URL percent-encodes it and Graph was observed treating the
    /// encoded form as data.
    ///
    /// # Errors
    ///
    /// [`GraphError::InvalidAddress`], carrying a reason fit to show whoever typed it.
    pub fn new(address: impl Into<String>) -> Result<Self, GraphError> {
        let address = address.into();
        validate(&address)?;
        Ok(Self(address))
    }

    /// The address as given.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The mailbox a [`GraphClient`](crate::GraphClient)'s requests are rooted at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxPrincipal {
    /// The signed-in user's own mailbox — requests are rooted at `/me`.
    Me,
    /// A shared or other mailbox the signed-in user can access, addressed by its
    /// UPN/SMTP address — requests are rooted at `/users/{address}`.
    User(MailboxAddress),
}

impl MailboxPrincipal {
    /// A shared/other mailbox by its UPN or SMTP address (e.g. `info@example.org`).
    ///
    /// # Errors
    ///
    /// [`GraphError::InvalidAddress`] when `address` could not safely address a mailbox
    /// ([`MailboxAddress::new`]).
    pub fn user(address: impl Into<String>) -> Result<Self, GraphError> {
        MailboxAddress::new(address).map(Self::User)
    }

    /// The mailbox a [`SharedMailboxId`] from
    /// [`resolve_shared_mailbox`](engine_provider::Provider::resolve_shared_mailbox) names —
    /// the binding half of onboarding a shared mailbox, and the name every adapter gives it.
    ///
    /// The handle is re-checked rather than trusted: it is stored by the host between the
    /// two calls, and a handle that no longer passes is safer refused than spliced.
    ///
    /// # Errors
    ///
    /// [`GraphError::InvalidAddress`] for a handle this adapter would never have produced.
    pub fn shared(handle: &SharedMailboxId) -> Result<Self, GraphError> {
        Self::user(handle.as_str())
    }

    /// The address this principal names, or `None` for the signed-in user's own
    /// mailbox (whose address this adapter does not learn unless it asks).
    pub(crate) fn address(&self) -> Option<&str> {
        match self {
            Self::Me => None,
            Self::User(address) => Some(address.as_str()),
        }
    }

    /// The URL path root for this principal: `/me`, or `/users/{address}`.
    ///
    /// `@` and `+` pass through unencoded, matching Graph's own documented shared-mailbox
    /// URL shape `…/users/info@example.org/mailFolders('Inbox')/messages`; every byte
    /// outside RFC 3986's unreserved set is percent-encoded, so a conforming address goes on
    /// the wire verbatim and anything else arrives as data.
    pub(crate) fn root(&self) -> String {
        match self {
            Self::Me => "/me".to_owned(),
            Self::User(address) => format!("/users/{}", encode_segment(address.as_str())),
        }
    }
}

/// The checks behind [`MailboxAddress::new`].
fn validate(address: &str) -> Result<(), GraphError> {
    let invalid = |why: String| Err(GraphError::InvalidAddress(why));
    if let Some(bad) = address
        .chars()
        .find(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '\\'))
    {
        return invalid(format!(
            "a mailbox address cannot contain U+{:04X}",
            bad as u32
        ));
    }
    let Some((local, domain)) = address.split_once('@') else {
        return invalid("a mailbox address needs an `@`".to_owned());
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return invalid("a mailbox address is one `local@domain`".to_owned());
    }
    Ok(())
}

/// Percent-encodes `value` for use as one URL path segment, leaving RFC 3986's
/// unreserved characters plus `@` and `+` as they are.
fn encode_segment(value: &str) -> String {
    use core::fmt::Write as _;

    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'@' | b'+') {
            out.push(char::from(*byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
#[path = "principal_tests.rs"]
mod tests;
