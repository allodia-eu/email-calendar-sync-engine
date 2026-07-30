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
//! So adding a shared mailbox is, for the engine, just **another account** pointed
//! at a [`MailboxPrincipal::User`]; nothing in `engine-core` changes. The onboarding
//! flow that discovers and registers a shared mailbox is the host's job (deferred).

/// The mailbox a [`GraphClient`](crate::GraphClient)'s requests are rooted at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailboxPrincipal {
    /// The signed-in user's own mailbox — requests are rooted at `/me`.
    Me,
    /// A shared or other mailbox the signed-in user can access, addressed by its
    /// UPN/SMTP address — requests are rooted at `/users/{address}`.
    User(String),
}

impl MailboxPrincipal {
    /// A shared/other mailbox by its UPN or SMTP address (e.g. `info@company.org`).
    #[must_use]
    pub fn user(address: impl Into<String>) -> Self {
        Self::User(address.into())
    }

    /// The address this principal names, or `None` for the signed-in user's own mailbox
    /// (whose address the credential does not reveal without a directory read).
    pub(crate) fn address(&self) -> Option<&str> {
        match self {
            Self::Me => None,
            Self::User(address) => Some(address),
        }
    }

    /// The URL path root for this principal: `/me`, or `/users/{address}`.
    ///
    /// Graph accepts an unencoded `@` in the path segment (it is a valid `pchar`),
    /// matching the documented shared-mailbox URL shape
    /// `…/users/info@company.org/mailFolders('Inbox')/messages` — so `@`, `+` and the
    /// unreserved set pass through and every other byte is percent-encoded.
    ///
    /// **Encoding alone is not a safety boundary here**, which is worth stating because it
    /// is the natural assumption and it is wrong: Graph *decodes* the segment and then
    /// re-resolves the path. `GET /v1.0/users/..%2Fme/mailFolders/inbox` was observed
    /// answering `200` with the **signed-in user's own** Inbox — the encoded traversal
    /// walked back a segment server-side. So an address is validated before it ever
    /// reaches a URL ([`validate_address`]); the encoding here is the second layer, not the
    /// first.
    pub(crate) fn root(&self) -> String {
        match self {
            Self::Me => "/me".to_owned(),
            Self::User(address) => format!("/users/{}", encode_segment(address)),
        }
    }
}

/// Rejects an address that could restructure a URL it is spliced into.
///
/// The address is **user input** — whatever someone typed into an "add a shared mailbox"
/// field — and percent-encoding does not contain it, because Graph decodes the segment and
/// re-resolves the path (see [`MailboxPrincipal::root`]). So the structural characters are
/// refused outright rather than escaped:
///
/// - `/` and `\` — the demonstrated traversal: `..%2Fme` resolved to the signed-in user's own
///   mailbox, so an accepted `../me` would have a host onboard its own inbox believing it had
///   onboarded somebody else's.
/// - `%` — a second encoding layer would let any of the others back in.
/// - `?` — would re-parameterize the request.
/// - ASCII controls, whitespace, `"`, `<`, `>` — never in an address someone means to type;
///   overwhelmingly a pasted display form (`"Name" <a@b.test>`) that should be corrected rather
///   than probed.
///
/// Everything else is allowed, deliberately including `#` — Entra guest UPNs really are
/// shaped `user_domain#EXT#@tenant.onmicrosoft.com`, and Graph treats the decoded `#` as
/// data (it echoes the address back in `ErrorInvalidUser`) rather than as a fragment. Also
/// including non-ASCII, which encodes to its UTF-8 bytes and carries no structure.
///
/// # Errors
///
/// The reason, ready to be shown to whoever typed the address.
pub(crate) fn validate_address(address: &str) -> Result<(), String> {
    let Some((local, domain)) = address.split_once('@') else {
        return Err(format!("{address:?} is not an email address (no `@`)"));
    };
    if local.is_empty() || domain.is_empty() {
        return Err(format!("{address:?} is not an email address (empty half)"));
    }
    if domain.contains('@') {
        return Err(format!("{address:?} is not an email address (several `@`)"));
    }
    if let Some(bad) = address
        .chars()
        .find(|c| c.is_ascii_control() || c.is_whitespace() || "/\\%?\"<>".contains(*c))
    {
        return Err(format!(
            "{address:?} contains {bad:?}, which cannot appear in a mailbox address"
        ));
    }
    Ok(())
}

/// Percent-encodes `value` for use as one URL path segment.
///
/// Everything outside RFC 3986's *unreserved* set is escaped, except `@` and `+`, which
/// are legal `pchar`s that appear in real addresses and that Graph's own documented URLs
/// carry unencoded. A conforming address is therefore untouched.
///
/// The second layer, not the first — see [`validate_address`] for why.
fn encode_segment(value: &str) -> String {
    use core::fmt::Write as _;

    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'@' | b'+') {
            out.push(*byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_roots_match_the_graph_url_shape() {
        assert_eq!(MailboxPrincipal::Me.root(), "/me");
        assert_eq!(
            MailboxPrincipal::user("info@company.org").root(),
            "/users/info@company.org"
        );
        // The constructor is `Into<String>`-flexible; equality is by address.
        assert_eq!(
            MailboxPrincipal::user("info@company.org"),
            MailboxPrincipal::User("info@company.org".to_owned())
        );
        assert_eq!(MailboxPrincipal::Me.address(), None);
        assert_eq!(
            MailboxPrincipal::user("info@company.org").address(),
            Some("info@company.org")
        );
    }

    #[test]
    fn a_hostile_address_cannot_escape_its_path_segment() {
        // The address is whatever a user typed. Unescaped, each of these would re-point or
        // re-parameterize every URL the client builds from this root.
        assert_eq!(
            MailboxPrincipal::user("../me/messages").root(),
            "/users/..%2Fme%2Fmessages"
        );
        assert_eq!(
            MailboxPrincipal::user("a@b.test?$select=id").root(),
            "/users/a@b.test%3F%24select%3Did"
        );
        assert_eq!(
            MailboxPrincipal::user("a@b.test#frag").root(),
            "/users/a@b.test%23frag"
        );
        // A conforming address — including the `+` of a tagged local part — is untouched,
        // so the documented Graph URL shape still goes on the wire verbatim.
        assert_eq!(
            MailboxPrincipal::user("first.last+tag@company.org").root(),
            "/users/first.last+tag@company.org"
        );
        // Non-ASCII is encoded as its UTF-8 bytes rather than passed through.
        assert_eq!(
            MailboxPrincipal::user("ö@b.test").root(),
            "/users/%C3%B6@b.test"
        );
    }
}
