//! Changing the account's folder tree: `CREATE`, `RENAME`, `DELETE` and the subscription
//! bookkeeping around them (RFC 9051 §6.3.4 to §6.3.8).
//!
//! On IMAP a folder's key **is** its full path, so every rename or move hands back a new
//! key, and the server renames every folder beneath it too (RFC 9051 §6.3.6). The path is
//! built from the parent's path, the hierarchy delimiter the server reported in `LIST`, and
//! the folder's own name; a name carrying that delimiter would make levels the user never
//! asked for, so it is refused.
//!
//! Subscriptions are not carried by a rename (RFC 9051 §6.3.9 keeps them apart from the
//! mailbox), and a client that lists only subscribed folders would lose one we moved. So a
//! write re-subscribes what it made and unsubscribes what it removed, and a server that
//! refuses either is not a failed write: the folder change itself went through.

use engine_core::ids::MailboxId;
use engine_provider::{MailboxEdit, MailboxEditReceipt, ProviderError, ProviderResult};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{error::ImapError, target::reject_control_chars, transport::Connection};

/// One folder as `LIST` reported it, with its name decoded to the form a
/// [`MailboxId`] carries.
struct Listed {
    path: String,
    delimiter: Option<String>,
    attributes: Vec<String>,
}

impl Listed {
    fn has_attribute(&self, attribute: &str) -> bool {
        self.attributes
            .iter()
            .any(|found| found.eq_ignore_ascii_case(attribute))
    }

    /// Whether it holds messages at all: `\Noselect` and `\NonExistent` rows are only
    /// levels of the hierarchy.
    fn selectable(&self) -> bool {
        !self.has_attribute("\\Noselect") && !self.has_attribute("\\NonExistent")
    }
}

/// The account's folders as the server has them now.
struct Tree {
    folders: Vec<Listed>,
}

impl Tree {
    async fn read<S>(connection: &mut Connection<S>) -> ProviderResult<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let modified_utf7 = connection.names_are_modified_utf7();
        let rows = connection.list().await?;
        let folders = rows
            .into_iter()
            .map(|row| Listed {
                path: if modified_utf7 {
                    crate::utf7::decode(&row.name)
                } else {
                    row.name
                },
                delimiter: row.delimiter.filter(|d| !d.is_empty()),
                attributes: row.attributes,
            })
            .collect();
        Ok(Self { folders })
    }

    fn get(&self, path: &str) -> Option<&Listed> {
        self.folders.iter().find(|f| f.path == path)
    }

    /// The hierarchy delimiter: the named folder's own, else the one the account uses.
    fn delimiter(&self, near: Option<&str>) -> Option<String> {
        near.and_then(|path| self.get(path))
            .and_then(|f| f.delimiter.clone())
            .or_else(|| self.folders.iter().find_map(|f| f.delimiter.clone()))
    }

    /// `path` and everything beneath it, deepest first, so each can be removed before
    /// the folder that holds it.
    fn subtree(&self, path: &str, delimiter: Option<&str>) -> Vec<&Listed> {
        let mut found: Vec<&Listed> = self
            .folders
            .iter()
            .filter(|f| {
                f.path == path
                    || delimiter.is_some_and(|d| f.path.starts_with(&format!("{path}{d}")))
            })
            .collect();
        found.sort_by_key(|f| core::cmp::Reverse(f.path.len()));
        found
    }
}

/// Applies one [`MailboxEdit`] over `connection`.
///
/// # Errors
///
/// A folder that is gone, or a destination already taken, is a
/// [`ProviderError::conflict`]; a name carrying the hierarchy delimiter, a subfolder on a
/// server with no hierarchy, or a change to `INBOX` is a [`ProviderError::invalid_state`];
/// anything else is the IMAP failure, classified.
pub(crate) async fn edit_mailbox<S>(
    connection: &mut Connection<S>,
    edit: &MailboxEdit,
) -> ProviderResult<MailboxEditReceipt>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    if let Some(target) = edit.target()
        && target.as_str().eq_ignore_ascii_case("INBOX")
    {
        return Err(ProviderError::invalid_state("INBOX cannot be changed"));
    }
    let tree = Tree::read(connection).await?;
    match edit {
        MailboxEdit::Create { name, parent } => {
            let path = child_path(&tree, parent.as_ref(), name)?;
            if tree.get(&path).is_none() {
                match connection.create(&path).await {
                    Ok(()) => {}
                    // A retry meets the folder the first attempt made.
                    Err(err) if carries_code(&err, "ALREADYEXISTS") => {}
                    Err(err) => return Err(err.into()),
                }
            }
            subscribe(connection, &path).await;
            resolved(&path)
        }
        MailboxEdit::Update {
            target,
            name,
            parent,
        } => {
            let path = child_path(&tree, parent.as_ref(), name)?;
            rename(connection, &tree, target.as_str(), &path).await?;
            resolved(&path)
        }
        MailboxEdit::Trash {
            target,
            trash,
            name,
        } => {
            let Some(bin) = tree.get(trash.as_str()) else {
                return Err(ProviderError::conflict("the Trash folder is gone"));
            };
            if !bin.has_attribute("\\Noinferiors") && bin.delimiter.is_some() {
                let path = child_path(&tree, Some(trash), name)?;
                match rename(connection, &tree, target.as_str(), &path).await {
                    Ok(()) => return resolved(&path),
                    // A Trash that cannot hold folders after all: empty the folders
                    // into it instead.
                    Err(Refused::Cannot) => {}
                    Err(Refused::Other(err)) => return Err(err),
                }
            }
            empty_into_trash(connection, &tree, target.as_str(), trash.as_str()).await?;
            Ok(MailboxEditReceipt::removed())
        }
        MailboxEdit::Delete { target } => {
            if tree.get(target.as_str()).is_some() {
                delete_subtree(connection, &tree, target.as_str()).await?;
            }
            Ok(MailboxEditReceipt::removed())
        }
    }
}

/// The full path of a folder called `name` inside `parent` (the top level when `None`).
fn child_path(tree: &Tree, parent: Option<&MailboxId>, name: &str) -> ProviderResult<String> {
    reject_control_chars(name)?;
    let delimiter = tree.delimiter(parent.map(MailboxId::as_str));
    if let Some(d) = &delimiter
        && name.contains(d.as_str())
    {
        return Err(ProviderError::invalid_state(format!(
            "a folder name cannot contain the server's hierarchy delimiter '{d}'"
        )));
    }
    let Some(parent) = parent else {
        if name.eq_ignore_ascii_case("INBOX") {
            return Err(ProviderError::invalid_state("INBOX is reserved"));
        }
        return Ok(name.to_owned());
    };
    let Some(d) = delimiter else {
        return Err(ProviderError::invalid_state(
            "this server keeps no folders inside folders",
        ));
    };
    Ok(format!("{}{d}{name}", parent.as_str()))
}

/// Why a `RENAME` did not go through.
enum Refused {
    /// The server cannot put a folder there (`[CANNOT]`), which a trash can recover from.
    Cannot,
    /// Anything else, already classified.
    Other(ProviderError),
}

impl From<Refused> for ProviderError {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::Cannot => ProviderError::invalid_state("the server cannot put a folder there"),
            Refused::Other(err) => err,
        }
    }
}

/// `RENAME from to`, then moves the subscriptions of the folder and everything beneath it.
async fn rename<S>(
    connection: &mut Connection<S>,
    tree: &Tree,
    from: &str,
    to: &str,
) -> Result<(), Refused>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    if from == to {
        return Ok(());
    }
    if tree.get(from).is_none() {
        return Err(Refused::Other(ProviderError::conflict(
            "the folder is gone",
        )));
    }
    if tree.get(to).is_some() {
        return Err(Refused::Other(ProviderError::conflict(
            "a folder of that name is already there",
        )));
    }
    reject_control_chars(from).map_err(Refused::Other)?;
    let command = format!(
        "RENAME {} {}",
        connection.quoted_name(from),
        connection.quoted_name(to)
    );
    if let Err(err) = connection.command(&command).await {
        return Err(if carries_code(&err, "CANNOT") {
            Refused::Cannot
        } else if carries_code(&err, "NONEXISTENT") || carries_code(&err, "ALREADYEXISTS") {
            Refused::Other(ProviderError::conflict(err.to_string()))
        } else {
            Refused::Other(err.into())
        });
    }
    let delimiter = tree.delimiter(Some(from));
    for moved in tree.subtree(from, delimiter.as_deref()) {
        let now = format!("{to}{}", &moved.path[from.len()..]);
        unsubscribe(connection, &moved.path).await;
        subscribe(connection, &now).await;
    }
    Ok(())
}

/// Moves every message of `target` and each folder beneath it into `trash`, then removes
/// the emptied folders: what trashing a folder comes to where Trash cannot hold one.
async fn empty_into_trash<S>(
    connection: &mut Connection<S>,
    tree: &Tree,
    target: &str,
    trash: &str,
) -> ProviderResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let Some(_) = tree.get(target) else {
        return Err(ProviderError::conflict("the folder is gone"));
    };
    let delimiter = tree.delimiter(Some(target));
    for folder in tree.subtree(target, delimiter.as_deref()) {
        if !folder.selectable() {
            continue;
        }
        let selected = connection.select(&folder.path).await?;
        if selected.exists > 0 {
            connection.uid_move("1:*", trash).await?;
        }
    }
    delete_subtree(connection, tree, target).await
}

/// `DELETE`s `target` and every folder beneath it, deepest first. A folder already gone
/// is not an error: a retry meets what the first attempt removed.
async fn delete_subtree<S>(
    connection: &mut Connection<S>,
    tree: &Tree,
    target: &str,
) -> ProviderResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    // Leave nothing selected that is about to go: not every server deletes the mailbox a
    // session has open.
    connection.examine("INBOX").await?;
    let delimiter = tree.delimiter(Some(target));
    for folder in tree.subtree(target, delimiter.as_deref()) {
        let command = format!("DELETE {}", connection.quoted_name(&folder.path));
        match connection.command(&command).await {
            Ok(_) => {}
            Err(err) if carries_code(&err, "NONEXISTENT") => {}
            Err(err) => return Err(err.into()),
        }
        unsubscribe(connection, &folder.path).await;
    }
    Ok(())
}

async fn subscribe<S>(connection: &mut Connection<S>, path: &str)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let command = format!("SUBSCRIBE {}", connection.quoted_name(path));
    let _ = connection.command(&command).await;
}

async fn unsubscribe<S>(connection: &mut Connection<S>, path: &str)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let command = format!("UNSUBSCRIBE {}", connection.quoted_name(path));
    let _ = connection.command(&command).await;
}

/// Whether a refusal opens with the response code `code` (RFC 5530), the only part of a
/// `NO` a client may act on: the text after it is the server's prose.
fn carries_code(err: &ImapError, code: &str) -> bool {
    let ImapError::No(detail) = err else {
        return false;
    };
    detail
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .is_some_and(|(found, _)| {
            found
                .split_whitespace()
                .next()
                .is_some_and(|atom| atom.eq_ignore_ascii_case(code))
        })
}

fn resolved(path: &str) -> ProviderResult<MailboxEditReceipt> {
    let id = MailboxId::try_from(path)
        .map_err(|_| ProviderError::invalid_state("a folder path cannot be empty"))?;
    Ok(MailboxEditReceipt::resolved(id))
}

#[cfg(test)]
#[path = "mailbox_write_tests.rs"]
mod tests;
