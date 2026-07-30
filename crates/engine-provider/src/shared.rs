//! Discovering mail stores a credential may open besides its own.
//!
//! A Microsoft 365 shared mailbox has no credentials of its own: a user is *granted
//! access* to it and their client opens it by address. That is not a Microsoft
//! concept — three of the four mail protocols this engine speaks have a first-class
//! mechanism for it, and they differ in exactly one way that matters to a caller:
//! whether the server will **list** the stores or only **answer for a named one**.
//! [`SharedMailboxes`] is that difference, and nothing more.
//!
//! What this seam deliberately does *not* do is create a scope, a cursor, or a
//! second kind of account. A shared mailbox **is just another account** (`providers.md`):
//! the host onboards it under its own [`AccountId`](engine_core::ids::AccountId) with the
//! same credential, and every existing sync/store/search path applies unchanged. What was
//! missing was only the onboarding step — find or verify a store you can open — which is
//! what [`Provider::list_shared_mailboxes`](crate::Provider::list_shared_mailboxes) and
//! [`Provider::resolve_shared_mailbox`](crate::Provider::resolve_shared_mailbox) answer.
//!
//! Note what [`SharedMailbox`] does **not** carry: rights, and mailbox kind.
//!
//! - **Rights.** Live against Stalwart, an account shared read-only reports
//!   `accounts.<id>.isReadOnly: false` in the JMAP session while the single mailbox it exposes
//!   grants only `lr` (lookup + read). Account-level read-only is therefore not a usable signal, so
//!   rights are carried per mailbox on [`Mailbox::access`](engine_core::mail::Mailbox::access)
//!   instead.
//! - **Kind** (shared vs. user vs. room). Graph's `userPurpose` is the only mailbox-kind vocabulary
//!   any provider here publishes, and a delegated credential can read `mailboxSettings` **only for
//!   its own mailbox** — every `/users/{other}/mailboxSettings` route answers `403
//!   ErrorAccessDenied` even with Full Access to that mailbox (`graph.md`). For the credential's
//!   own store [`SharedMailbox::personal`] already says it. A field that is always either redundant
//!   or unknown would be speculative surface.

use engine_core::ids::SharedMailboxId;

/// How an adapter can find mail stores its credential may open besides its own.
///
/// Read this off [`Capabilities::shared_mailboxes`](crate::Capabilities::shared_mailboxes)
/// **before** offering an onboarding flow: it decides whether a host can present a
/// pick-from-a-list UI or must ask the user to type an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SharedMailboxes {
    /// No such mechanism for this credential.
    ///
    /// Gmail: delegation exists but is UI-only. The API route that would serve it
    /// (`users/{userId}` for a `userId` other than `me`) needs a service account with
    /// domain-wide delegation, not a user bearer token, so a user-credential adapter
    /// cannot reach another mailbox at all (`google.md`).
    #[default]
    Unsupported,
    /// The server enumerates them, so **both** verbs work:
    /// [`list_shared_mailboxes`](crate::Provider::list_shared_mailboxes) and
    /// [`resolve_shared_mailbox`](crate::Provider::resolve_shared_mailbox).
    ///
    /// JMAP: the session's `accounts` map, whose non-`isPersonal` entries are exactly
    /// these (RFC 8620 §1.6.2) — already in the document every connect fetches. IMAP:
    /// `NAMESPACE` (RFC 2342) names the other-users'/shared prefixes, and `LIST` under
    /// them names the stores.
    Enumerable,
    /// The server will not list them, but answers for a **named address**, so only
    /// [`resolve_shared_mailbox`](crate::Provider::resolve_shared_mailbox) works.
    ///
    /// Microsoft Graph: there is no API that lists the mailboxes a delegated credential
    /// has been granted; `GET /users/{address}/…` either answers or fails, and the
    /// failure shape is what distinguishes "no such mailbox" from "not shared with you".
    ByAddress,
}

/// One mail store a credential can open.
///
/// `#[non_exhaustive]`, because a future protocol may report a fact about a shared store
/// that none of today's three do; construct it with [`SharedMailbox::new`] and the `with_*`
/// chain.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SharedMailbox {
    /// The opaque, adapter-scoped handle that reopens this store. JMAP: the account id.
    /// IMAP: the mailbox-path prefix inside the shared namespace. Graph: the UPN.
    pub handle: SharedMailboxId,
    /// The address the server reports for this store, when it reports one.
    ///
    /// **Not canonical, and not identity.** A Microsoft 365 alias resolves to its target
    /// mailbox, so two addresses can name one store; the handle is what identifies it.
    /// `None` where the protocol names the store without an address.
    pub address: Option<String>,
    /// A display name for the store — the server's own label where it has one, otherwise
    /// the best the adapter can offer (an IMAP shared-namespace entry is named by its
    /// path component, which is usually the owner's address).
    pub name: String,
    /// Whether this is the credential's **own** store rather than one shared with it
    /// (JMAP `isPersonal`).
    ///
    /// An [`Enumerable`](SharedMailboxes::Enumerable) adapter reports the personal store
    /// alongside the shared ones, so a host can render "every store this credential can
    /// open" from one list without special-casing the account it signed in as.
    pub personal: bool,
}

impl SharedMailbox {
    /// A store shared *with* the credential: `handle` reopens it, `name` labels it.
    #[must_use]
    pub fn new(handle: SharedMailboxId, name: impl Into<String>) -> Self {
        Self {
            handle,
            address: None,
            name: name.into(),
            personal: false,
        }
    }

    /// Records the address the server reports for this store (see
    /// [`address`](Self::address) on why it is not identity).
    #[must_use]
    pub fn with_address(mut self, address: impl Into<String>) -> Self {
        self.address = Some(address.into());
        self
    }

    /// Marks this as the credential's **own** store rather than a shared one.
    #[must_use]
    pub fn as_personal(mut self) -> Self {
        self.personal = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(value: &str) -> SharedMailboxId {
        SharedMailboxId::try_from(value).unwrap()
    }

    #[test]
    fn a_shared_mailbox_defaults_to_shared_and_addressless() {
        let mailbox = SharedMailbox::new(handle("f"), "support@test.local");
        assert_eq!(mailbox.handle.as_str(), "f");
        assert_eq!(mailbox.name, "support@test.local");
        // A protocol that names a store without an address leaves this unset rather than
        // inventing one from the name — the name is a label, the address is a fact.
        assert!(mailbox.address.is_none());
        assert!(!mailbox.personal);
    }

    #[test]
    fn the_builders_set_what_they_name() {
        let own = SharedMailbox::new(handle("c"), "Alice")
            .with_address("alice@test.local")
            .as_personal();
        assert_eq!(own.address.as_deref(), Some("alice@test.local"));
        assert!(own.personal);
    }

    #[test]
    fn unsupported_is_the_default_mechanism() {
        // So an adapter that says nothing about shared mailboxes is read as "no such
        // mechanism" rather than as one that silently rejects both verbs.
        assert_eq!(SharedMailboxes::default(), SharedMailboxes::Unsupported);
    }
}
