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

    /// The URL path root for this principal: `/me`, or `/users/{address}`.
    ///
    /// Graph accepts an unencoded address in the path segment (`@` is a valid
    /// `pchar`), matching the documented shared-mailbox URL shape
    /// `…/users/info@company.org/mailFolders('Inbox')/messages`.
    pub(crate) fn root(&self) -> String {
        match self {
            Self::Me => "/me".to_owned(),
            Self::User(address) => format!("/users/{address}"),
        }
    }
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
    }
}
