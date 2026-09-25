//! One store's folder list — with its unread counts and the caller's rights — and the
//! other stores this credential can open.
//!
//! Both are `LIST` read against the session's namespaces (`crate::store`). Split from
//! `crate::provider`, which keeps the trait implementation thin and under the size limit.

use std::collections::HashMap;

use engine_core::{
    ids::MailboxId,
    mail::{Mailbox, MailboxAccess},
};
use engine_provider::{ProviderError, ProviderResult, SharedMailbox};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    capability::Extension,
    error::ImapError,
    mail::mailbox_from_list,
    parse::ListRow,
    provider::ImapProvider,
    store::{MailStore, Namespaces, shared_store},
    transport::Connection,
    unseen::{has_noselect, unseen_by_probing},
};

impl<S> ImapProvider<S> {
    /// The server's namespaces, asked for over `connection` the first time anything needs
    /// them and kept for the life of the provider.
    ///
    /// Asked for lazily because most providers never need it: a host runs one provider per
    /// folder, and only the one listing folders, placing a message, or discovering stores
    /// has to know whose mail is whose. A session without `NAMESPACE` answers "none", which
    /// reads as the credential owning every folder — what it always did.
    ///
    /// A `NO` or `BAD` reads as "none" too, but only for this call: it is not kept, so the
    /// next caller asks again. A refusal can be passing (`[UNAVAILABLE]`), and kept for the
    /// provider's life it would leave every shared folder in the credential's own list.
    ///
    /// # Errors
    ///
    /// The classified transport or protocol failure of the `NAMESPACE` command.
    pub(crate) async fn namespaces_on<T>(
        &self,
        connection: &mut Connection<T>,
    ) -> ProviderResult<Namespaces>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        if let Some(namespaces) = self.namespaces.get() {
            return Ok(namespaces.clone());
        }
        let namespaces = if connection.negotiated.has(Extension::Namespace) {
            match connection.namespace().await {
                Ok(namespaces) => namespaces,
                // Not a failure that takes the folder list and every placement down with
                // it, and not an answer to keep either.
                Err(ImapError::No(_) | ImapError::Bad(_)) => return Ok(Namespaces::default()),
                Err(err) => return Err(err.into()),
            }
        } else {
            Namespaces::default()
        };
        // Another caller may have got here first; both read the same server, so either
        // answer will do.
        let _ = self.namespaces.set(namespaces.clone());
        Ok(namespaces)
    }

    /// The store this provider covers — its bound shared mailbox, or the credential's own.
    ///
    /// # Errors
    ///
    /// As [`namespaces_on`](Self::namespaces_on), and `InvalidState` for a shared-mailbox
    /// handle this server's namespaces do not contain (`MailStore::shared`).
    pub(crate) async fn store_on<T>(
        &self,
        connection: &mut Connection<T>,
    ) -> ProviderResult<MailStore>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let namespaces = self.namespaces_on(connection).await?;
        match &self.shared_mailbox {
            None => Ok(MailStore::own(&namespaces)),
            Some(handle) => MailStore::shared(&namespaces, handle.as_str()),
        }
    }

    /// Refuses `folder` unless it belongs to the store this provider covers.
    ///
    /// Read against the namespaces, not as a string prefix: `…/support@test.local.invalid/INBOX`
    /// begins with the handle `…/support@test.local` and is another owner's store.
    ///
    /// # Errors
    ///
    /// As [`store_on`](Self::store_on), and `InvalidState` for a folder outside the store.
    pub(crate) async fn check_in_store(&self, folder: &MailboxId) -> ProviderResult<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut connection = self.connection.lock().await;
        if self
            .store_on(&mut connection)
            .await?
            .relative(folder.as_str())
            .is_some()
        {
            return Ok(());
        }
        Err(ProviderError::invalid_state(format!(
            "{:?} is not a folder of the shared mailbox this provider is bound to",
            folder.as_str()
        )))
    }
}

/// `store`'s folders, each with its unread count and the caller's rights.
///
/// One `LIST` narrowed to the store where a pattern can say so (a shared store's subtree),
/// filtered where it cannot (the credential's own tree, minus every foreign namespace).
/// Counts come from the same `LIST` where the server has `LIST-STATUS`, else one `STATUS`
/// per **kept** folder — never for another principal's. Rights likewise come from the same
/// `LIST` where the server has `LIST-MYRIGHTS`, else from one pipelined `MYRIGHTS` per kept
/// selectable folder where it has `ACL`, and read `owner()` where it has neither: a server
/// with no ACLs has no way to share, so nothing it lists is anyone else's to restrict.
///
/// # Errors
///
/// A classified failure of `LIST`/`STATUS`/`MYRIGHTS`, and `Permanent` for a shared store
/// the server lists nothing of — its grant was withdrawn, or the handle is for another
/// server — which a host must be told rather than shown as an empty mailbox.
pub(crate) async fn list_store<S>(
    connection: &mut Connection<S>,
    store: &MailStore,
) -> ProviderResult<Vec<Mailbox>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let modified_utf7 = connection.names_are_modified_utf7();
    let listing = connection.list_folders(&store.list_pattern()).await?;
    let kept: Vec<(ListRow, Mailbox)> = listing
        .rows
        .into_iter()
        .filter_map(|row| {
            let selectable = !has_noselect(&row.attributes);
            let mailbox = store.adopt(mailbox_from_list(&row, modified_utf7)?, selectable)?;
            Some((row, mailbox))
        })
        .collect();
    if kept.is_empty() && matches!(store, MailStore::Shared { .. }) {
        return Err(ProviderError::permanent(
            "the server lists no folder of the shared mailbox this provider is bound to; \
             the credential may have lost access to it",
        ));
    }
    let unseen = if let Some(counted) = listing.unseen {
        counted
    } else {
        let rows: Vec<ListRow> = kept.iter().map(|(row, _)| row.clone()).collect();
        unseen_by_probing(connection, &rows).await?
    };
    let rights = match listing.rights {
        Some(listed) => kept
            .iter()
            .filter_map(|(row, mailbox)| {
                let letters = listed.get(&row.name)?;
                Some((mailbox.id.as_str().to_owned(), crate::acl::access(letters)))
            })
            .collect(),
        None => rights_of(connection, &kept).await?,
    };
    Ok(kept
        .into_iter()
        .map(|(row, mut mailbox)| {
            // Absent stays absent: a mailbox the server did not count must not read as one
            // with nothing unread.
            mailbox.unread_count = unseen.get(&row.name).copied();
            if let Some(access) = rights.get(mailbox.id.as_str()) {
                mailbox.access = *access;
            }
            mailbox
        })
        .collect())
}

/// The caller's rights on each selectable folder in `kept`, keyed by folder id. A folder
/// missing from the answer keeps the `owner()` it was built with: the server has no ACLs, it
/// is a `\Noselect` container (Stalwart answers `NO` there), or the server declined to say —
/// "unknown", which must not read as "no rights" and hide mail the caller can see.
async fn rights_of<S>(
    connection: &mut Connection<S>,
    kept: &[(ListRow, Mailbox)],
) -> ProviderResult<HashMap<String, MailboxAccess>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    if !connection.negotiated.has(Extension::Acl) {
        return Ok(HashMap::new());
    }
    let names: Vec<String> = kept
        .iter()
        .filter(|(row, _)| !has_noselect(&row.attributes))
        .map(|(_, mailbox)| mailbox.id.as_str().to_owned())
        .collect();
    let letters = connection.myrights(&names).await?;
    Ok(names
        .into_iter()
        .zip(letters)
        .filter_map(|(name, letters)| Some((name, crate::acl::access(&letters?))))
        .collect())
}

/// The stores this credential can open besides its own: one per entry directly below each
/// foreign namespace, found with a `%` pattern there, since that level is the owner and
/// everything below it is that store's folders.
///
/// # Errors
///
/// A classified failure of a `LIST`.
pub(crate) async fn discover<S>(
    connection: &mut Connection<S>,
    namespaces: &Namespaces,
) -> ProviderResult<Vec<SharedMailbox>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let modified_utf7 = connection.names_are_modified_utf7();
    let mut stores = Vec::new();
    for namespace in namespaces.foreign().filter(|ns| !ns.prefix.is_empty()) {
        for row in connection.list(&namespace.join("%")).await? {
            let name = if modified_utf7 {
                crate::utf7::decode(&row.name)
            } else {
                row.name
            };
            stores.extend(shared_store(namespace, &name));
        }
    }
    Ok(stores)
}

#[cfg(test)]
#[path = "folders_tests.rs"]
mod tests;
