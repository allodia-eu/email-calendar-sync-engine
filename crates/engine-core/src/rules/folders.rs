//! Where a source account's folders are on a target: what an import files by.

use core::fmt;
use std::collections::BTreeMap;

use crate::ids::MailboxId;

/// Each source folder id with the target folder id it was imported to: the map a transfer host's
/// mail import built, handed to a rules import so a rule filing into a source folder files into
/// its counterpart.
///
/// Folder ids are a server's own, and repeat across accounts (Stalwart numbers each account's
/// from `a`), so an id is never taken across as it is: one the map does not hold has no
/// counterpart, whatever the target happens to call its own folders. Its `Debug` shows only how
/// many it holds, since an id can be a folder's name.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct FolderMap(BTreeMap<MailboxId, MailboxId>);

impl FolderMap {
    /// A map with no folder in it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Maps `source` to `target` (builder-style).
    #[must_use]
    pub fn with(mut self, source: MailboxId, target: MailboxId) -> Self {
        self.insert(source, target);
        self
    }

    /// Maps `source` to `target`, answering the target it was mapped to before.
    pub fn insert(&mut self, source: MailboxId, target: MailboxId) -> Option<MailboxId> {
        self.0.insert(source, target)
    }

    /// The target folder `source` was imported to.
    #[must_use]
    pub fn target(&self, source: &MailboxId) -> Option<&MailboxId> {
        self.0.get(source)
    }

    /// Keeps only the pairs `keep` answers `true` for: what an importer does with a target folder
    /// it finds gone.
    pub fn retain(&mut self, mut keep: impl FnMut(&MailboxId, &MailboxId) -> bool) {
        self.0.retain(|source, target| keep(source, target));
    }

    /// Each source id with its target id, in source order.
    pub fn iter(&self) -> impl Iterator<Item = (&MailboxId, &MailboxId)> {
        self.0.iter()
    }

    /// How many folders it maps.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether it maps no folder.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<(MailboxId, MailboxId)> for FolderMap {
    fn from_iter<I: IntoIterator<Item = (MailboxId, MailboxId)>>(pairs: I) -> Self {
        Self(pairs.into_iter().collect())
    }
}

impl fmt::Debug for FolderMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FolderMap")
            .field("len", &self.0.len())
            .finish()
    }
}
