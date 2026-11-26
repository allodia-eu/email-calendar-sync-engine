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
//!
//! A create may give the folder a role: `CREATE … (USE (\Archive))` (RFC 6154 §3), only where
//! the server advertises `CREATE-SPECIAL-USE`, and only for a role with an attribute. Anything
//! else is refused before a command is sent. A refusal is told apart by the folder list, never
//! by the server's prose: a folder of that name already there with the role is the retry of a
//! create that landed; one without the role, or a `[USEATTR]` refusal while another folder of
//! the store holds the role, is a conflict; a `[USEATTR]` with no holder is a role this server
//! will not give. A server may also take the role from another folder, or let two folders
//! share it (RFC 6154 §3): that is its choice, and the create succeeds.
//!
//! Every edit is made in one store (`crate::store`): the credential's own, or the shared
//! mailbox the provider is bound to. The folders an edit names come from that store's list,
//! so they are its already; what the store decides is where the **top level** is — a folder
//! made or moved there with no parent — and which folder is the Inbox no edit may touch.

use engine_core::{ids::MailboxId, mail::MailboxRole};
use engine_provider::{
    Capabilities, CreateRoleError, MailboxEdit, MailboxEditReceipt, ProviderError, ProviderResult,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::ImapError, store::MailStore, target::reject_control_chars, transport::Connection,
};

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
        Self::read_matching(connection, "*").await
    }

    /// The folders `pattern` matches (a `LIST` pattern, so `*` and `%` in it are wildcards).
    async fn read_matching<S>(connection: &mut Connection<S>, pattern: &str) -> ProviderResult<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let modified_utf7 = connection.names_are_modified_utf7();
        let rows = connection.list(pattern).await?;
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

    /// The folder at `path`. `INBOX` finds the inbox however the server spells it, since that
    /// is the id the folder list gives it.
    fn get(&self, path: &str) -> Option<&Listed> {
        let inbox = path == "INBOX";
        self.folders
            .iter()
            .find(|f| f.path == path || (inbox && f.path.eq_ignore_ascii_case("INBOX")))
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

/// Applies one [`MailboxEdit`] to `store`'s tree over `connection`.
///
/// # Errors
///
/// A folder that is gone, or a destination already taken, is a
/// [`ProviderError::conflict`]; a name carrying the hierarchy delimiter, a subfolder on a
/// server with no hierarchy, or a change to the store's Inbox is a
/// [`ProviderError::invalid_state`]; anything else is the IMAP failure, classified.
pub(crate) async fn edit_mailbox<S>(
    connection: &mut Connection<S>,
    store: &MailStore,
    edit: &MailboxEdit,
    capabilities: &Capabilities,
) -> ProviderResult<MailboxEditReceipt>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    if let Some(target) = edit.target()
        && store.is_inbox(target.as_str())
    {
        return Err(ProviderError::invalid_state("INBOX cannot be changed"));
    }
    // The provider's own capability: `USE` only where `CREATE-SPECIAL-USE` is advertised and
    // roles can be read back (`Negotiated::may_create_with_role`; DOVECOT-1).
    let role = edit.role_as(capabilities, MailboxRole::as_imap_special_use)?;
    // The capability was read at the first connect; a redial may land on a backend that does
    // not advertise it. The same rule, on this session.
    if role.is_some() && !connection.negotiated.may_create_with_role() {
        return Err(CreateRoleError::NotSupported.into());
    }
    let tree = Tree::read(connection).await?;
    match edit {
        MailboxEdit::Create { name, parent, .. } => {
            let path = child_path(&tree, store, parent.as_ref(), name)?;
            create(connection, &tree, store, &path, role).await?;
            subscribe(connection, &path).await;
            resolved(&path)
        }
        MailboxEdit::Update {
            target,
            name,
            parent,
        } => {
            let path = child_path(&tree, store, parent.as_ref(), name)?;
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
                let path = child_path(&tree, store, Some(trash), name)?;
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

/// The full path of a folder called `name` inside `parent`, or at the top of `store` when
/// `None` — which for a shared store is below its root, not beside the credential's `INBOX`.
///
/// Refuses, before any change is sent, exactly the names IMAP cannot carry as one level: an
/// empty one, one with a byte no command line can hold (CR, LF, NUL), one carrying the server's
/// delimiter (known only from the `LIST`, which is why this runs after it), a subfolder on a
/// server with no hierarchy, and a top-level `INBOX`. Nothing more: a tab, a `/` on a server
/// whose delimiter is `.`, a surrounding space are the server's to judge. Every edit that names
/// a folder builds its path here, a trash's included.
fn child_path(
    tree: &Tree,
    store: &MailStore,
    parent: Option<&MailboxId>,
    name: &str,
) -> ProviderResult<String> {
    if name.is_empty() {
        return Err(ProviderError::invalid_state(
            "a folder name cannot be empty",
        ));
    }
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
        return Ok(store.qualify(name));
    };
    let Some(d) = delimiter else {
        return Err(ProviderError::invalid_state(
            "this server keeps no folders inside folders",
        ));
    };
    let parent = tree
        .get(parent.as_str())
        .map_or(parent.as_str(), |f| f.path.as_str());
    Ok(format!("{parent}{d}{name}"))
}

/// `CREATE`s the folder at `path`, with the special-use `attribute` where a role was asked for.
///
/// A folder already at `path` is the retry of a create that landed, so it is not created again,
/// unless a role was asked for and it lacks it. A folder made with a role is listed once more
/// to see that it has it; where it does not, it is subscribed as any create's folder is and
/// named in [`CreateRoleError::NotKept`]. The same holds for one
/// another client made between the `LIST` and the `CREATE` (`[ALREADYEXISTS]`), which is listed
/// again to see.
async fn create<S>(
    connection: &mut Connection<S>,
    tree: &Tree,
    store: &MailStore,
    path: &str,
    attribute: Option<&str>,
) -> ProviderResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    if let Some(existing) = tree.get(path) {
        return holds(existing, attribute);
    }
    match connection.create(path, attribute).await {
        Ok(()) => match attribute {
            None => Ok(()),
            // Success is read, not assumed: one `LIST` of the new folder, its roles asked for.
            Some(attribute) => match Tree::read_matching(connection, path).await?.get(path) {
                Some(made) if made.has_attribute(attribute) => Ok(()),
                // The one refusal with a side effect: the folder is there without the role, so
                // it is finished as any create finishes it, and named, never deleted.
                _ => {
                    subscribe(connection, path).await;
                    let mailbox = MailboxId::try_from(path).map_err(|_| {
                        ProviderError::invalid_state("a folder path cannot be empty")
                    })?;
                    Err(CreateRoleError::NotKept { mailbox }.into())
                }
            },
        },
        Err(err) if carries_code(&err, "ALREADYEXISTS") => match attribute {
            None => Ok(()),
            Some(_) => match Tree::read(connection).await?.get(path) {
                Some(existing) => holds(existing, attribute),
                None => Err(ProviderError::conflict(
                    "the server says the folder exists, and does not list it",
                )),
            },
        },
        Err(err) => match attribute {
            Some(attribute) if carries_code(&err, "USEATTR") => {
                let held = tree.folders.iter().any(|folder| {
                    store.relative(&folder.path).is_some() && folder.has_attribute(attribute)
                });
                Err(if held {
                    ProviderError::conflict("another folder already has that role")
                } else {
                    ProviderError::invalid_state("the server will not give a folder that role")
                })
            }
            _ => Err(err.into()),
        },
    }
}

/// Whether a folder already there is the one a create asked for: any folder of the name, or
/// with a role asked for, only one carrying its `attribute`.
fn holds(existing: &Listed, attribute: Option<&str>) -> ProviderResult<()> {
    match attribute {
        Some(attribute) if !existing.has_attribute(attribute) => Err(ProviderError::conflict(
            "a folder of that name is already there without that role",
        )),
        _ => Ok(()),
    }
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
pub(crate) fn carries_code(err: &ImapError, code: &str) -> bool {
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
