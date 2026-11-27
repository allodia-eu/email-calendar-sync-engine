//! How a rule on an account names one of the account's folders, and which folder a read rule's
//! name means: the one answer for every transport, so a host building "move to this folder" from a
//! folder list, or showing where a read rule files, never learns that IMAP names a folder by path,
//! JMAP and Graph by id, Gmail by label id, and each of them a role by a name of its own.
//!
//! The account's [`RuleVocabulary`] carries what decides it: whether a rule files by role
//! ([`FileIntoRole`](super::RuleFeature::FileIntoRole)) and by id
//! ([`FileIntoId`](super::RuleFeature::FileIntoId))
//! there, whether a folder's id is the path a rule names it by
//! ([`folder_ids_are_paths`](RuleVocabulary::folder_ids_are_paths)), and the limits a target puts
//! on roles. A rule is judged as [`plan_rules`](super::plan_rules) judges it, so a folder this
//! names is one the plan keeps a move into.

use super::{Action, MailboxPath, RuleLimit, RuleMailbox, RuleVocabulary};
use crate::mail::{Mailbox, MailboxRole};

impl RuleMailbox {
    /// How a rule written to an account whose language is `vocabulary` files into `folder`, one of
    /// `folders`, the folders the account's provider listed, or `None` where no rule there can.
    ///
    /// A folder that is the only one with its role is named by its role where a rule there files
    /// by role, so the rule follows the role and reads back as one: the inbox where a rule files by
    /// name (Sieve), the rest where the account files by role (Sieve's `special-use`, Graph's
    /// well-known folders, Gmail's Trash and Spam). Where several share the role, the server
    /// chooses among them, so none is named by it. Else a folder is named by its id where a rule
    /// files by id (JMAP's `mailboxid`, a Graph folder, a Gmail label), and else by its path where
    /// the account's folder ids are paths (IMAP).
    ///
    /// `None`, never an id or a path:
    ///
    /// - a virtual collection (all mail, flagged, important): a view over other folders, which
    ///   holds no message of its own, whatever the server lets a rule name;
    /// - on a target that keeps roles as labels of its own and adds no other
    ///   ([`TrashJunkArchiveOnly`](RuleLimit::TrashJunkArchiveOnly)), a folder with any other role;
    /// - a level of the hierarchy that holds no mail ([`Mailbox::selectable`]);
    /// - a folder no way above names: no path is made up where the account's ids are not paths, so
    ///   a JMAP account whose server lists neither `mailboxid` nor `special-use` names only its
    ///   inbox.
    #[must_use]
    pub fn for_folder(
        folder: &Mailbox,
        folders: &[Mailbox],
        vocabulary: &RuleVocabulary,
    ) -> Option<Self> {
        if !folder.selectable {
            return None;
        }
        if let Some(role) = &folder.role {
            if matches!(
                role,
                MailboxRole::All | MailboxRole::Flagged | MailboxRole::Important
            ) {
                return None;
            }
            let shared = folders
                .iter()
                .any(|other| other.id != folder.id && other.role.as_ref() == Some(role));
            let by_role = Self::Role(role.clone());
            if !shared && files_into(&by_role, vocabulary) {
                return Some(by_role);
            }
            if vocabulary.has_limit(RuleLimit::TrashJunkArchiveOnly) {
                return None;
            }
        }
        let by_id = Self::Id(folder.id.clone());
        if files_into(&by_id, vocabulary) {
            return Some(by_id);
        }
        if vocabulary.folder_ids_are_paths() {
            let by_path = Self::Path(MailboxPath::new(folder.id.as_str()).ok()?);
            if files_into(&by_path, vocabulary) {
                return Some(by_path);
            }
        }
        None
    }

    /// Which of `folders`, the folders the account's provider listed, a rule read from that
    /// account, whose language is `vocabulary`, files into by this name, or `None` where none.
    ///
    /// A role names the one folder with that role, and none where several have it, since the
    /// server then chooses; an id names the folder with that id; a path names the folder whose id
    /// it is, only where the account's folder ids are paths
    /// ([`folder_ids_are_paths`](RuleVocabulary::folder_ids_are_paths)), since anywhere else
    /// what a path means has not been proven. A role no folder has is `None`: a host shows the
    /// role itself (Gmail lists no archive folder, and archiving is a role all the same).
    #[must_use]
    pub fn folder_in<'a>(
        &self,
        folders: &'a [Mailbox],
        vocabulary: &RuleVocabulary,
    ) -> Option<&'a Mailbox> {
        match self {
            Self::Role(role) => {
                let mut with_role = folders
                    .iter()
                    .filter(|folder| folder.role.as_ref() == Some(role));
                let only = with_role.next()?;
                with_role.next().is_none().then_some(only)
            }
            Self::Id(id) => folders.iter().find(|folder| &folder.id == id),
            Self::Path(path) if vocabulary.folder_ids_are_paths() => folders
                .iter()
                .find(|folder| folder.id.as_str() == path.as_str()),
            Self::Path(_) => None,
        }
    }
}

/// Whether a rule there can move a message into `mailbox`, as the plan judges it.
fn files_into(mailbox: &RuleMailbox, vocabulary: &RuleVocabulary) -> bool {
    let action = Action::FileInto {
        mailbox: mailbox.clone(),
        copy: false,
    };
    super::plan_support::action_support(&action, vocabulary).is_some()
}

#[cfg(test)]
#[path = "mailbox_name_tests.rs"]
mod tests;
