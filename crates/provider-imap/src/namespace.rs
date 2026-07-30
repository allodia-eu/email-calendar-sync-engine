//! The IMAP `NAMESPACE` response (RFC 2342): which parts of the mailbox tree are the
//! credential's own, and which belong to somebody else.
//!
//! Without this a flat `LIST "" "*"` is ambiguous. Stalwart answers alice
//! `* NAMESPACE (("" "/")) (("Shared Folders" "/")) NIL`, so
//! `Shared Folders/support@test.local/INBOX` arrives in the same list as her own
//! `Archive` — indistinguishable from a folder she happens to have named
//! `Shared Folders`. Read against the namespaces it is unmistakable: a path under a
//! non-personal prefix is another principal's mail.
//!
//! RFC 2342 defines three positions, in order: **Personal**, **Other Users'**, and
//! **Shared**. The engine cares about one distinction — mine versus not-mine — so the
//! latter two are treated alike ([`Namespaces::foreign`]). Which of the two a server
//! actually uses is not something to assume: Stalwart puts its stores in *Other Users'*
//! despite naming the prefix "Shared Folders", and another server may well use the third
//! position for the same thing.

use crate::tokenize::{Item, items_of};

/// One namespace: the path prefix that introduces it and the hierarchy delimiter within
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Namespace {
    /// The prefix every mailbox in this namespace starts with — `""` for the usual
    /// personal namespace, `"Shared Folders"` for Stalwart's foreign one. Stored without
    /// a trailing delimiter, however the server wrote it, so joining is uniform.
    pub(crate) prefix: String,
    /// The hierarchy delimiter inside this namespace, or `None` for a flat one (`NIL`).
    pub(crate) delimiter: Option<String>,
}

impl Namespace {
    /// The path of `mailbox` relative to this namespace's prefix, or `None` when the
    /// mailbox is not inside it.
    ///
    /// A prefix match alone is not enough: `Shared Foldersomething` starts with
    /// `Shared Folders` and is a *different* mailbox, so what follows the prefix must be
    /// the delimiter (or nothing, for the container itself).
    pub(crate) fn relative<'a>(&self, mailbox: &'a str) -> Option<&'a str> {
        if self.prefix.is_empty() {
            return Some(mailbox);
        }
        let rest = mailbox.strip_prefix(self.prefix.as_str())?;
        if rest.is_empty() {
            return Some(rest);
        }
        let delimiter = self.delimiter.as_deref()?;
        rest.strip_prefix(delimiter)
    }

    /// Joins `segments` onto this namespace's prefix with its delimiter.
    pub(crate) fn join(&self, segments: &[&str]) -> String {
        let delimiter = self.delimiter.as_deref().unwrap_or("");
        let mut path = self.prefix.clone();
        for segment in segments {
            if !path.is_empty() {
                path.push_str(delimiter);
            }
            path.push_str(segment);
        }
        path
    }
}

/// The three namespace lists a server advertises (RFC 2342 §5).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Namespaces {
    /// The credential's own mailboxes. Usually one entry with an empty prefix.
    pub(crate) personal: Vec<Namespace>,
    /// Other users' mailboxes, reachable because access was granted.
    pub(crate) other_users: Vec<Namespace>,
    /// Mailboxes shared with a group of users rather than owned by one.
    pub(crate) shared: Vec<Namespace>,
}

impl Namespaces {
    /// The namespaces holding mail the credential does **not** own — other users' and
    /// shared, treated alike because the engine's only question is whose mail it is.
    pub(crate) fn foreign(&self) -> impl Iterator<Item = &Namespace> {
        self.other_users.iter().chain(&self.shared)
    }

    /// The foreign namespace `mailbox` sits inside, with the mailbox's path relative to
    /// it — or `None` when the mailbox is the credential's own.
    ///
    /// The longest matching prefix wins, so a server advertising both `Shared` and
    /// `Shared/Public` attributes a mailbox to the more specific one.
    pub(crate) fn foreign_owner<'a, 'm>(
        &'a self,
        mailbox: &'m str,
    ) -> Option<(&'a Namespace, &'m str)> {
        self.foreign()
            .filter_map(|ns| ns.relative(mailbox).map(|rest| (ns, rest)))
            .max_by_key(|(ns, _)| ns.prefix.len())
    }

    /// Whether `mailbox` belongs to the credential itself.
    ///
    /// Defined as "in no foreign namespace" rather than "in a personal one": a server that
    /// advertises no namespaces at all (or only the personal one, as Stalwart does for a
    /// user with no shares) must not have its whole mailbox tree read as foreign.
    pub(crate) fn is_own(&self, mailbox: &str) -> bool {
        self.foreign_owner(mailbox).is_none()
    }
}

/// Which principal's mailboxes a provider's folder list covers.
///
/// This is what `NAMESPACE` buys. `LIST "" "*"` returns the credential's own folders *and*
/// every folder it has been granted access to, in one flat list — so without attribution a
/// provider bound to a shared mailbox would sync the sharer's folders alongside the shared
/// ones, and a provider bound to the credential's own mailbox would sync the *shared* ones
/// as if they were its own. Both are wrong for the same reason: one engine account must
/// hold one principal's mail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MailStore {
    /// The path prefix every mailbox in this store shares: empty for the credential's own
    /// store, `Shared Folders/support@test.local` for a store reached through a foreign
    /// namespace.
    root: String,
    /// The delimiter between the root and a mailbox inside it.
    delimiter: Option<String>,
}

impl MailStore {
    /// The store the mailbox at `bound` belongs to, according to `namespaces`.
    ///
    /// A path inside a foreign namespace resolves to that principal's store — its root
    /// being the prefix plus the first path component after it, which is how both
    /// other-users' and shared namespaces name the owner. Anything else is the
    /// credential's own store, which is also what a server advertising no namespaces
    /// yields: the pre-shared-mailbox behaviour, unchanged.
    pub(crate) fn resolve(namespaces: &Namespaces, bound: &str) -> Self {
        let Some((namespace, relative)) = namespaces.foreign_owner(bound) else {
            return Self {
                root: String::new(),
                delimiter: namespaces
                    .personal
                    .first()
                    .and_then(|ns| ns.delimiter.clone()),
            };
        };
        let owner = match namespace.delimiter.as_deref() {
            Some(delim) if !delim.is_empty() => relative.split(delim).next().unwrap_or(relative),
            _ => relative,
        };
        // A bound mailbox that *is* the namespace container (`Shared Folders`) names no
        // owner, so there is nothing to append — and appending an empty component would
        // leave a trailing delimiter on the root, after which nothing matches and the
        // folder list comes back silently empty. The whole namespace is the honest reading:
        // every share under it.
        let root = if owner.is_empty() {
            namespace.prefix.clone()
        } else {
            namespace.join(&[owner])
        };
        Self {
            root,
            delimiter: namespace.delimiter.clone(),
        }
    }

    /// The `LIST` pattern that fetches this store's mailboxes.
    ///
    /// `*` for the credential's own store (which then needs the foreign rows filtered out —
    /// no pattern can express "everything except these prefixes"), and `<root><delim>*`
    /// plus the root itself for a foreign one.
    pub(crate) fn list_pattern(&self) -> String {
        if self.root.is_empty() {
            return "*".to_owned();
        }
        // A trailing `*` after the root matches the root itself and everything under it in
        // one command, so the `\NoSelect` container the namespace introduces still appears.
        format!("{}*", self.root)
    }

    /// Whether the mailbox named `name` belongs to this store.
    ///
    /// For a foreign store that is "inside my root". For the credential's own it is "in no
    /// foreign namespace" — the filter [`list_pattern`](Self::list_pattern) cannot express.
    pub(crate) fn contains(&self, namespaces: &Namespaces, name: &str) -> bool {
        if self.root.is_empty() {
            return namespaces.is_own(name);
        }
        self.as_namespace().relative(name).is_some()
    }

    /// The full path a mailbox called `name` has **inside this store**.
    ///
    /// The credential's own store is rooted at the empty prefix, so the name is already the
    /// path; a foreign store prepends its root. That matters for a folder the client has to
    /// *create*: filing a sent copy falls back to the conventional `Sent` when the store
    /// advertises no `\Sent`, and an unqualified name would create it — and file another
    /// principal's mail into it — in the credential's own namespace (`crate::filing`).
    pub(crate) fn qualify(&self, name: &str) -> String {
        if self.root.is_empty() {
            return name.to_owned();
        }
        self.as_namespace().join(&[name])
    }

    /// This store's root as a [`Namespace`], so prefix matching and joining reuse the one
    /// implementation that knows a prefix must be followed by the delimiter.
    fn as_namespace(&self) -> Namespace {
        Namespace {
            prefix: self.root.clone(),
            delimiter: self.delimiter.clone(),
        }
    }
}

/// Parses the untagged `* NAMESPACE` response.
///
/// The three positions are each `NIL` or a list of `(prefix delimiter [params…])`
/// (RFC 2342 §5). **Infallible on purpose**: absent or malformed input yields empty lists
/// rather than an error, because a server without the extension simply has no namespaces
/// to report — and the engine then treats every mailbox as the credential's own, which is
/// exactly the behaviour that predates this file. Failing the connect instead would break
/// every server that does not speak RFC 2342.
pub(crate) fn parse_namespace(lines: &[Vec<u8>]) -> Namespaces {
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
        return Namespaces {
            personal: namespace_list(personal),
            other_users: namespace_list(other_users),
            shared: namespace_list(shared),
        };
    }
    Namespaces::default()
}

/// Reads one of the three positions: `NIL` → no namespaces, otherwise a list of
/// `(prefix delimiter …)` pairs. An entry without a readable prefix is skipped.
fn namespace_list(item: &Item) -> Vec<Namespace> {
    item.as_list()
        .unwrap_or(&[])
        .iter()
        .filter_map(|entry| {
            let fields = entry.as_list()?;
            let prefix = fields.first()?.as_nstring()?;
            let delimiter = fields.get(1).and_then(Item::as_nstring);
            // Servers differ on whether the prefix carries its trailing delimiter
            // (`"Shared Folders"` vs `"Shared Folders/"`); normalize it off so joining and
            // matching need not care.
            let prefix = match delimiter.as_deref() {
                Some(delim) if !delim.is_empty() => {
                    prefix.strip_suffix(delim).unwrap_or(&prefix).to_owned()
                }
                _ => prefix,
            };
            Some(Namespace { prefix, delimiter })
        })
        .collect()
}

#[cfg(test)]
#[path = "namespace_tests.rs"]
mod tests;
