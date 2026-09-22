//! Whose mail a folder holds: the `NAMESPACE` answer (RFC 2342), and the one **store** a
//! provider's folder list and placements are scoped to.
//!
//! IMAP has no per-store `LIST`. `LIST "" "*"` returns the credential's own folders and
//! every folder shared with it, flat and interleaved: Stalwart hands `alice@` her own
//! folders and, in the same answer, `Shared Folders/support@test.local/Sent Items`. Read
//! against the namespaces it is unmistakable — a path under a non-personal prefix is another
//! principal's mail — and without them it is indistinguishable from a folder she happens to
//! have named `Shared Folders`. Two things went wrong before this module: a folder list held
//! two principals' folders, and the `\Sent` a sent copy is filed into could be anyone's,
//! whichever the server listed first.
//!
//! RFC 2342 defines three positions — **Personal**, **Other Users'**, **Shared** — and which of
//! the last two a server uses for a given kind of share is not assumable: Stalwart puts a
//! group mailbox and a user's granted INBOX in *Other Users'* while naming the prefix
//! "Shared Folders". The only question the engine asks is mine-or-not, so both are
//! [`foreign`](Namespaces::foreign) alike.

use engine_core::{
    ids::{MailboxId, ProviderKey, SharedMailboxId},
    mail::{Mailbox, MailboxRole},
};
use engine_provider::{ProviderError, ProviderResult, SharedMailbox};

use crate::tokenize::{Item, items_of};

/// One namespace: the path prefix that introduces it and the hierarchy delimiter inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Namespace {
    /// The prefix, **decoded** the way a mailbox id is, and without a trailing delimiter
    /// however the server wrote it (`"Shared Folders"` and `"shared/"` both store bare).
    pub(crate) prefix: String,
    /// The delimiter inside this namespace, or `None` for a flat one (`NIL`).
    pub(crate) delimiter: Option<String>,
}

impl Namespace {
    /// The path of `name` below this namespace, `Some("")` for the container itself, or
    /// `None` when `name` is not inside it.
    ///
    /// A prefix match is not enough on its own: `Shared Foldersomething` starts with
    /// `Shared Folders` and is somebody's ordinary folder, so what follows the prefix must be
    /// the delimiter. A flat namespace (`NIL` delimiter, RFC 2342 §5) has none to require, and
    /// its prefix is the whole marker (`#news.`), so there everything after it is inside.
    pub(crate) fn relative<'a>(&self, name: &'a str) -> Option<&'a str> {
        if self.prefix.is_empty() {
            return Some(name);
        }
        let rest = name.strip_prefix(self.prefix.as_str())?;
        if rest.is_empty() {
            return Some(rest);
        }
        match self.delimiter.as_deref().filter(|d| !d.is_empty()) {
            Some(delimiter) => rest.strip_prefix(delimiter),
            None => Some(rest),
        }
    }

    /// The owner whose store `name` is the root of: the one path component directly below
    /// this namespace's prefix. `None` for anything else — the container itself, a folder
    /// deeper down, and every name in a flat namespace, which has no level for an owner.
    fn owner_of<'a>(&self, name: &'a str) -> Option<&'a str> {
        let delimiter = self.delimiter.as_deref().filter(|d| !d.is_empty())?;
        let owner = self.relative(name).filter(|rest| !rest.is_empty())?;
        (!owner.contains(delimiter)).then_some(owner)
    }

    /// `segment` below this namespace's prefix.
    pub(crate) fn join(&self, segment: &str) -> String {
        if self.prefix.is_empty() {
            return segment.to_owned();
        }
        format!(
            "{}{}{segment}",
            self.prefix,
            self.delimiter.as_deref().unwrap_or_default()
        )
    }
}

/// The three namespace lists a server advertises (RFC 2342 §5).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Namespaces {
    /// The credential's own mail — usually one entry with an empty prefix.
    pub(crate) personal: Vec<Namespace>,
    /// Other users' mail, reachable because they granted access.
    pub(crate) other_users: Vec<Namespace>,
    /// Mail shared with a group rather than owned by one user.
    pub(crate) shared: Vec<Namespace>,
}

impl Namespaces {
    /// Every namespace holding mail the credential does **not** own.
    pub(crate) fn foreign(&self) -> impl Iterator<Item = &Namespace> {
        self.other_users.iter().chain(&self.shared)
    }

    /// The foreign namespace `name` sits in, with its path below it — the most specific
    /// prefix wins — or `None` when `name` is the credential's own.
    fn foreign_owner<'a, 'm>(&'a self, name: &'m str) -> Option<(&'a Namespace, &'m str)> {
        self.foreign()
            .filter(|ns| !ns.prefix.is_empty())
            .filter_map(|ns| ns.relative(name).map(|rest| (ns, rest)))
            .max_by_key(|(ns, _)| ns.prefix.len())
    }
}

/// Parses the untagged `NAMESPACE` response.
///
/// Infallible on purpose: a missing or unreadable answer yields no namespaces, which reads
/// as "every folder is the credential's own" — exactly what a server without the extension
/// always meant, so nothing that worked before this parser stops working.
///
/// On a session whose names travel as modified UTF-7 (IMAP4rev1) the prefixes do too, so
/// they are decoded here: a prefix must compare against the same decoded form a mailbox id
/// carries.
pub(crate) fn parse_namespace(lines: &[Vec<u8>], modified_utf7: bool) -> Namespaces {
    for line in lines {
        let Ok(items) = items_of(line) else { continue };
        let [keyword, personal, other_users, shared, ..] = items.as_slice() else {
            continue;
        };
        if !keyword
            .as_atom()
            .is_some_and(|atom| atom.eq_ignore_ascii_case("NAMESPACE"))
        {
            continue;
        }
        let list = |item: &Item| namespace_list(item, modified_utf7);
        return Namespaces {
            personal: list(personal),
            other_users: list(other_users),
            shared: list(shared),
        };
    }
    Namespaces::default()
}

/// One of the three positions: `NIL`, or a list of `(prefix delimiter [extensions…])`.
fn namespace_list(item: &Item, modified_utf7: bool) -> Vec<Namespace> {
    item.as_list()
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| {
            let fields = entry.as_list()?;
            let raw = fields.first()?.as_nstring()?;
            let delimiter = fields.get(1).and_then(Item::as_nstring);
            let decoded = if modified_utf7 {
                crate::utf7::decode(&raw)
            } else {
                raw
            };
            let prefix = match delimiter.as_deref().filter(|d| !d.is_empty()) {
                Some(delim) => decoded.strip_suffix(delim).unwrap_or(&decoded).to_owned(),
                None => decoded,
            };
            Some(Namespace { prefix, delimiter })
        })
        .collect()
}

/// The one principal's mail a provider covers.
///
/// `LIST` answers for every principal at once; this is what narrows it. It holds only what
/// it needs to decide, never the connection, so the same value scopes the provider's
/// standing session and a freshly dialed one alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MailStore {
    /// The credential's own mail: everything **outside** the foreign namespaces. With none
    /// advertised that is the whole tree — the behaviour that predates this type.
    Own {
        /// The namespaces whose folders are someone else's.
        foreign: Vec<Namespace>,
    },
    /// One store under a foreign namespace, bound with `ImapConfig::with_shared_mailbox`.
    Shared {
        /// The store's own path as a namespace: the handle, and the delimiter below it.
        root: Namespace,
    },
}

impl MailStore {
    /// The credential's own store.
    pub(crate) fn own(namespaces: &Namespaces) -> Self {
        Self::Own {
            foreign: namespaces.foreign().cloned().collect(),
        }
    }

    /// The store a shared-mailbox `handle` names.
    ///
    /// # Errors
    ///
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState) when
    /// `handle` is not exactly one level below a namespace this server says is foreign —
    /// not a handle discovery could have produced for this credential.
    pub(crate) fn shared(namespaces: &Namespaces, handle: &str) -> ProviderResult<Self> {
        let not_a_store = || {
            ProviderError::invalid_state(format!(
                "{handle:?} is not a store under any namespace this server shares with the \
                 credential"
            ))
        };
        let (namespace, _) = namespaces.foreign_owner(handle).ok_or_else(not_a_store)?;
        namespace.owner_of(handle).ok_or_else(not_a_store)?;
        Ok(Self::Shared {
            root: Namespace {
                prefix: handle.to_owned(),
                delimiter: namespace.delimiter.clone(),
            },
        })
    }

    /// The `LIST` pattern that fetches this store: `*` for the credential's own (no pattern
    /// can say "everything except these prefixes", so [`relative`](Self::relative) filters),
    /// and the root **with** everything below it for a shared one — the root may itself be a
    /// folder (see [`adopt`](Self::adopt)), so a pattern starting below it would miss it.
    pub(crate) fn list_pattern(&self) -> String {
        match self {
            Self::Own { .. } => "*".to_owned(),
            Self::Shared { root } => format!("{}*", root.prefix),
        }
    }

    /// The path of `name` inside this store — `""` for a shared store's root — or `None`
    /// when it belongs to another.
    pub(crate) fn relative<'a>(&self, name: &'a str) -> Option<&'a str> {
        match self {
            Self::Own { foreign } => {
                let theirs = foreign
                    .iter()
                    .filter(|ns| !ns.prefix.is_empty())
                    .any(|ns| ns.relative(name).is_some());
                (!theirs).then_some(name)
            }
            Self::Shared { root } => root.relative(name),
        }
    }

    /// The full path a folder called `name` has **inside this store** — what a folder the
    /// client must create is called, so a shared store with no `\Sent` gets one of its own
    /// instead of having its mail filed into the credential's.
    pub(crate) fn qualify(&self, name: &str) -> String {
        match self {
            Self::Own { .. } => name.to_owned(),
            Self::Shared { root } => root.join(name),
        }
    }

    /// Takes a folder into this store's tree, or `None` when it is not this store's.
    /// `selectable` is whether its `LIST` row lacked `\Noselect`.
    ///
    /// A shared store is re-rooted so it reads like any account: its top-level folders have
    /// no parent, and its inbox is the Inbox. Servers disagree about where that inbox is, and
    /// both shapes are real:
    ///
    /// - **Stalwart** makes the root a `\Noselect` container and lists the inbox as a child,
    ///   `Shared Folders/support@test.local/INBOX` — the reserved name at the top of the store.
    /// - **Dovecot** makes the root the inbox itself: `shared/support@test.local` is selectable and
    ///   holds the mail, and `…/INBOX` works as an alias it never lists.
    ///
    /// So a selectable root is the Inbox, named `INBOX` like any other; a `\Noselect` one is a
    /// path component and not a folder of the store at all.
    pub(crate) fn adopt(&self, mut mailbox: Mailbox, selectable: bool) -> Option<Mailbox> {
        let relative = self.relative(mailbox.id.as_str())?;
        if let Self::Shared { root } = self {
            if relative.is_empty() {
                if !selectable {
                    return None;
                }
                "INBOX".clone_into(&mut mailbox.name);
                mailbox.role = Some(MailboxRole::Inbox);
            } else if mailbox.role.is_none() && relative.eq_ignore_ascii_case("INBOX") {
                mailbox.role = Some(MailboxRole::Inbox);
            }
            if mailbox.parent.as_ref().map(MailboxId::as_str) == Some(root.prefix.as_str()) {
                mailbox.parent = None;
            }
            mailbox.parent = mailbox.parent.filter(|_| !relative.is_empty());
        }
        Some(mailbox)
    }
}

/// One store in a foreign namespace, from the `LIST` row that names its root — or `None`
/// when the row is the namespace container itself or something deeper than a store.
///
/// The component below the prefix is the store's owner. It is an address on every server
/// observed (Stalwart, and Dovecot keyed by address), and is reported as one only when it
/// has that shape; a server keyed by bare user names gives a label and no address.
pub(crate) fn shared_store(namespace: &Namespace, name: &str) -> Option<SharedMailbox> {
    let owner = namespace.owner_of(name)?;
    let handle = SharedMailboxId::new(ProviderKey::new(name.to_owned()).ok()?);
    let store = SharedMailbox::new(handle, owner);
    Some(if looks_like_address(owner) {
        store.with_address(owner)
    } else {
        store
    })
}

/// Whether `text` is shaped like one `local@domain`.
fn looks_like_address(text: &str) -> bool {
    !text.chars().any(char::is_whitespace)
        && text.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        })
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
