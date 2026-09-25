//! Deciding what a queued folder change still means, against the folder list as the server
//! has it now.
//!
//! A folder change can wait in the outbox for as long as the device is offline, and in that
//! time another client can rename, move or remove the same folder. Most transports would then
//! apply the queued change to whatever the folder has become, because they name a folder by
//! an id that survives a rename: the user's rename would silently overwrite someone else's.
//! So the change records **where the user saw the folder** ([`MailboxPlace`]), and the plan
//! refuses it as a conflict when the server's copy has moved on. The same reading makes a
//! retry safe: a create that finds its folder already made, or a delete that finds nothing,
//! is done rather than repeated.
//!
//! Pure: the caller reads the list, this decides, the caller sends.

use engine_core::{
    ids::MailboxId,
    mail::{Mailbox, MailboxRole},
};
use engine_provider::{MailboxEdit, ProviderError};
use serde::{Deserialize, Serialize};

/// Where a folder sits: its own name and the folder it is inside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxPlace {
    /// The folder's own name, one level.
    pub name: String,
    /// The folder it is inside, or `None` at the top of the account's tree.
    pub parent: Option<MailboxId>,
}

impl MailboxPlace {
    /// Where `mailbox` sits.
    #[must_use]
    pub fn of(mailbox: &Mailbox) -> Self {
        Self {
            name: mailbox.name.clone(),
            parent: mailbox.parent.clone(),
        }
    }
}

/// A change to an account's folder tree, as a host asks for it and the outbox keeps it.
///
/// Every variant that changes an existing folder carries `seen`, the place the user saw it
/// in, which is what lets a replay refuse a folder that has since changed elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MailboxChange {
    /// Make a folder called `name` inside `parent` (`None`: the top level).
    Create {
        /// The new folder's own name.
        name: String,
        /// The folder to make it in.
        parent: Option<MailboxId>,
    },
    /// Rename a folder, move it, or both.
    Update {
        /// The folder to change.
        target: MailboxId,
        /// Where the user saw it.
        seen: MailboxPlace,
        /// The name it is to have.
        name: String,
        /// The folder it is to be inside (`None`: the top level).
        parent: Option<MailboxId>,
    },
    /// Put a folder, its subfolders and their mail in Trash.
    Trash {
        /// The folder to trash.
        target: MailboxId,
        /// Where the user saw it.
        seen: MailboxPlace,
        /// The account's Trash folder.
        trash: MailboxId,
    },
    /// Remove a folder, its subfolders and their mail for good.
    Delete {
        /// The folder to remove.
        target: MailboxId,
    },
}

impl MailboxChange {
    /// Checks the names this change would give a folder, before it is queued.
    ///
    /// # Errors
    ///
    /// Returns the first [`MailboxNameError`] a name carries.
    pub fn validate(&self) -> Result<(), MailboxNameError> {
        match self {
            Self::Create { name, .. } | Self::Update { name, .. } => validate_mailbox_name(name),
            Self::Trash { .. } | Self::Delete { .. } => Ok(()),
        }
    }
}

/// Why a folder name cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MailboxNameError {
    /// The name is empty, or only whitespace.
    #[error("a folder name cannot be empty")]
    Empty,
    /// The name starts or ends with whitespace, which no two servers treat alike.
    #[error("a folder name cannot start or end with a space")]
    Surrounded,
    /// The name carries a control character.
    #[error("a folder name cannot contain a control character")]
    Control,
    /// The name carries `/`, which Gmail and most IMAP servers read as a level of the tree.
    #[error("a folder name cannot contain '/'")]
    Separator,
}

/// Checks a folder's own name against the rules every transport shares.
///
/// Not every rule a server has: an IMAP server whose hierarchy separator is `.` refuses a
/// name containing one, and says so when asked. What this catches is what no transport
/// accepts, so a host can say it while the user is still typing.
///
/// # Errors
///
/// Returns the [`MailboxNameError`] the name breaks.
pub fn validate_mailbox_name(name: &str) -> Result<(), MailboxNameError> {
    if name.trim().is_empty() {
        return Err(MailboxNameError::Empty);
    }
    if name.trim() != name {
        return Err(MailboxNameError::Surrounded);
    }
    if name.chars().any(char::is_control) {
        return Err(MailboxNameError::Control);
    }
    if name.contains('/') {
        return Err(MailboxNameError::Separator);
    }
    Ok(())
}

/// What a queued change comes to against the current folder list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Send this edit.
    Send(MailboxEdit),
    /// Nothing to send: the tree already is what the change asked for. Names the folder,
    /// where one remains.
    Done(Option<MailboxId>),
}

/// Decides what `change` still means against `current`, the server's folder list.
///
/// # Errors
///
/// A folder that is gone or has moved since the user saw it, a destination that is gone, a
/// move into the folder's own subtree, or a name already taken beside it is
/// [`FailureClass::Conflict`](engine_core::error::FailureClass::Conflict). Changing the
/// Inbox, which IMAP cannot rename without emptying, or trashing Trash itself is
/// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
pub(crate) fn plan(change: &MailboxChange, current: &[Mailbox]) -> Result<Plan, ProviderError> {
    let find = |id: &MailboxId| current.iter().find(|m| &m.id == id);
    match change {
        MailboxChange::Create { name, parent } => {
            if let Some(parent) = parent
                && find(parent).is_none()
            {
                return Err(ProviderError::conflict(
                    "the folder to create it in is gone",
                ));
            }
            Ok(match sibling_named(current, parent.as_ref(), name, None) {
                Some(existing) => Plan::Done(Some(existing.id.clone())),
                None => Plan::Send(MailboxEdit::Create {
                    name: name.clone(),
                    parent: parent.clone(),
                }),
            })
        }
        MailboxChange::Update {
            target,
            seen,
            name,
            parent,
        } => {
            let folder = unchanged(find(target), seen)?;
            if &folder.name == name && &folder.parent == parent {
                return Ok(Plan::Done(Some(target.clone())));
            }
            if let Some(parent) = parent {
                if find(parent).is_none() {
                    return Err(ProviderError::conflict("the destination folder is gone"));
                }
                if within(current, parent, target) {
                    return Err(ProviderError::conflict(
                        "a folder cannot move inside itself",
                    ));
                }
            }
            if sibling_named(current, parent.as_ref(), name, Some(target)).is_some() {
                return Err(ProviderError::conflict(
                    "the destination already holds a folder of that name",
                ));
            }
            Ok(Plan::Send(MailboxEdit::Update {
                target: target.clone(),
                name: name.clone(),
                parent: parent.clone(),
            }))
        }
        MailboxChange::Trash {
            target,
            seen,
            trash,
        } => {
            let folder = unchanged(find(target), seen)?;
            if find(trash).is_none() {
                return Err(ProviderError::conflict("the Trash folder is gone"));
            }
            if target == trash || within(current, trash, target) {
                return Err(ProviderError::invalid_state("Trash cannot go in Trash"));
            }
            Ok(Plan::Send(MailboxEdit::Trash {
                target: target.clone(),
                trash: trash.clone(),
                name: free_name(current, trash, &folder.name),
            }))
        }
        MailboxChange::Delete { target } => Ok(match find(target) {
            None => Plan::Done(None),
            Some(folder) if folder.role == Some(MailboxRole::Inbox) => {
                return Err(ProviderError::invalid_state("the Inbox cannot be removed"));
            }
            Some(_) => Plan::Send(MailboxEdit::Delete {
                target: target.clone(),
            }),
        }),
    }
}

/// The folder, if it is still where the user saw it and is not the Inbox.
fn unchanged<'a>(
    folder: Option<&'a Mailbox>,
    seen: &MailboxPlace,
) -> Result<&'a Mailbox, ProviderError> {
    let Some(folder) = folder else {
        return Err(ProviderError::conflict("the folder is gone"));
    };
    if folder.role == Some(MailboxRole::Inbox) {
        return Err(ProviderError::invalid_state("the Inbox cannot be changed"));
    }
    if &MailboxPlace::of(folder) != seen {
        return Err(ProviderError::conflict(
            "the folder has changed since it was read",
        ));
    }
    Ok(folder)
}

/// Whether `folder` is `ancestor` or lies anywhere beneath it.
///
/// Bounded by the list's length, so a server that reports a cycle cannot hang the walk.
fn within(current: &[Mailbox], folder: &MailboxId, ancestor: &MailboxId) -> bool {
    let mut at = Some(folder.clone());
    for _ in 0..=current.len() {
        match at {
            Some(ref id) if id == ancestor => return true,
            Some(ref id) => {
                at = current
                    .iter()
                    .find(|m| &m.id == id)
                    .and_then(|m| m.parent.clone());
            }
            None => return false,
        }
    }
    false
}

/// The folder called `name` directly inside `parent`, other than `except`.
fn sibling_named<'a>(
    current: &'a [Mailbox],
    parent: Option<&MailboxId>,
    name: &str,
    except: Option<&MailboxId>,
) -> Option<&'a Mailbox> {
    current
        .iter()
        .find(|m| m.parent.as_ref() == parent && m.name == name && Some(&m.id) != except)
}

/// `name`, or `name 2`, `name 3` …: the first that nothing directly inside `parent` is
/// called. Compared without case, because a server that folds case refuses the collision
/// a case-sensitive comparison would let through.
fn free_name(current: &[Mailbox], parent: &MailboxId, name: &str) -> String {
    let taken = |candidate: &str| {
        current.iter().any(|m| {
            m.parent.as_ref() == Some(parent) && m.name.to_lowercase() == candidate.to_lowercase()
        })
    };
    if !taken(name) {
        return name.to_owned();
    }
    // One more candidate than there are folders: at least one of them is free.
    (2..=current.len() + 2)
        .map(|n| format!("{name} {n}"))
        .find(|candidate| !taken(candidate))
        .expect("more candidates than folders leaves one free")
}

#[cfg(test)]
#[path = "mailbox_plan_tests.rs"]
mod tests;
