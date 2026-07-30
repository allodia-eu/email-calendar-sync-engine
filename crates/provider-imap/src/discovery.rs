//! Listing one principal's mailboxes, with rights — and finding the other principals whose
//! mail this credential may open.
//!
//! Both are `LIST` plus attribution against the session's [`Namespaces`]. What makes them
//! non-trivial is that IMAP has no per-store `LIST`: `LIST "" "*"` returns the credential's
//! own folders and every folder shared with it, flat and interleaved (Stalwart hands alice
//! her nine folders and eight more belonging to `support@` and `bob@`). So the rows are
//! filtered to the [`MailStore`] the provider is bound to — otherwise one engine account
//! would hold two principals' mail, which no amount of downstream care can untangle.

use engine_core::mail::{Mailbox, MailboxAccess};
use engine_provider::{ProviderError, ProviderResult, SharedMailbox};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::ImapResult,
    mail::mailbox_from_list,
    namespace::{MailStore, Namespaces},
    parse::ListRow,
    transport::Connection,
};

/// Lists the mailboxes of the store the provider is bound to, each carrying the caller's
/// rights.
///
/// One `LIST` (its pattern narrowed to the store where the protocol allows) plus one
/// `MYRIGHTS` per **selectable** mailbox. That per-mailbox round trip is the cost of an
/// honest answer: RFC 4314 offers no bulk form, and rights are exactly what a shared
/// mailbox differs in. It is skipped entirely when the server does not advertise `ACL`, and
/// for the `\NoSelect` containers a shared namespace introduces — Stalwart answers those
/// `NO Mailbox does not exist.`, since they are path components rather than mailboxes.
pub(crate) async fn list_store<S>(
    connection: &mut Connection<S>,
    namespaces: &Namespaces,
    store: &MailStore,
) -> ImapResult<Vec<Mailbox>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let rows = connection.list_pattern(&store.list_pattern()).await?;
    let mine: Vec<&ListRow> = rows
        .iter()
        .filter(|row| store.contains(namespaces, &row.name))
        .collect();
    let ask_rights = connection.acl_advertised();

    let mut mailboxes = Vec::with_capacity(mine.len());
    for row in mine {
        let Some(mut mailbox) = mailbox_from_list(row) else {
            continue;
        };
        mailbox.access = if ask_rights && is_selectable(row) {
            rights_of(connection, &row.name).await?
        } else {
            // No way to ask (no `ACL`), or nothing to ask about (a namespace container).
            // `owner` is what a caller assumed before rights existed, and for the
            // credential's own mailboxes on a server without ACLs it is also correct.
            MailboxAccess::owner()
        };
        mailboxes.push(mailbox);
    }
    Ok(mailboxes)
}

/// The stores this credential may open besides its own: one entry per principal appearing
/// under a foreign namespace.
///
/// One `LIST` per foreign namespace, listing **one level** below its prefix (`%`), because
/// that level is where the owner sits: `Shared Folders/support@test.local` is the store,
/// and everything below it is that store's folders.
///
/// The credential's **own** store is deliberately not reported. Its handle would have to be
/// the personal namespace's prefix, which is the empty string — not a handle at all — and a
/// host holding an IMAP credential already knows how to open the mailbox it dialed. This is
/// the one place the IMAP and JMAP answers differ in shape: a JMAP session names the
/// personal account explicitly, so `provider-jmap` does include it.
///
/// # Errors
///
/// Rejects with
/// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState) when the
/// server advertised **no** foreign namespace — matching the
/// [`SharedMailboxes::Unsupported`](engine_provider::SharedMailboxes::Unsupported) the
/// provider then reports. An empty `Ok` would be the worse answer: a host cannot tell it
/// apart from "no shares yet", and would offer a pick-a-mailbox list that can never fill.
pub(crate) async fn list_shared<S>(
    connection: &mut Connection<S>,
    namespaces: &Namespaces,
) -> ProviderResult<Vec<SharedMailbox>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    if namespaces.foreign().next().is_none() {
        return Err(ProviderError::invalid_state(
            "the server advertised no shared namespace for this credential",
        ));
    }
    let mut stores = Vec::new();
    for namespace in namespaces.foreign() {
        let rows = connection.list_pattern(&namespace.join(&["%"])).await?;
        for row in rows {
            // The prefix itself comes back for a `%` pattern on some servers; it names the
            // namespace, not a store, so only a path *inside* it counts.
            let Some(owner) = namespace
                .relative(&row.name)
                .filter(|rest| !rest.is_empty())
            else {
                continue;
            };
            // Only the first level: a `%` pattern should not descend, but a server that
            // returns more must not turn each folder into a separate "store".
            if namespace
                .delimiter
                .as_deref()
                .is_some_and(|delim| !delim.is_empty() && owner.contains(delim))
            {
                continue;
            }
            let Ok(handle) = engine_core::ids::ProviderKey::new(row.name.clone()) else {
                continue;
            };
            // The path component is the owner's address on every server observed, but it is
            // the server's label rather than a parsed address — reported as the name, and
            // as the address only because that is what the component is. The handle is what
            // identifies the store either way.
            stores.push(
                SharedMailbox::new(
                    engine_core::ids::SharedMailboxId::new(handle),
                    owner.to_owned(),
                )
                .with_address(owner.to_owned()),
            );
        }
    }
    Ok(stores)
}

/// Finds the store whose owner component is `address`.
///
/// # Errors
///
/// [`FailureClass::Permanent`](engine_core::error::FailureClass::Permanent) when no foreign
/// namespace holds one. There is no forbidden case to distinguish: a server lists in these
/// namespaces exactly what the credential has been granted, so anything absent is absent.
pub(crate) async fn resolve_shared<S>(
    connection: &mut Connection<S>,
    namespaces: &Namespaces,
    address: &str,
) -> ProviderResult<SharedMailbox>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let stores = list_shared(connection, namespaces).await?;
    stores
        .into_iter()
        .find(|store| {
            store
                .address
                .as_deref()
                .is_some_and(|owner| owner.eq_ignore_ascii_case(address))
        })
        .ok_or_else(|| {
            ProviderError::permanent(format!(
                "no shared namespace holds a mailbox for {address:?}"
            ))
        })
}

/// `MYRIGHTS` on one mailbox, mapped to [`MailboxAccess`].
///
/// An answer the server declines to give (`None`) falls back to `owner`, matching the
/// no-`ACL` case: "unknown" is not "no rights", and reporting a mailbox as unreadable
/// because the rights lookup was refused would hide mail the caller can see.
async fn rights_of<S>(connection: &mut Connection<S>, mailbox: &str) -> ImapResult<MailboxAccess>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    Ok(connection
        .myrights(mailbox)
        .await?
        .map_or_else(MailboxAccess::owner, |rights| rights.access()))
}

/// Whether a `LIST` row names a mailbox that can be opened, as opposed to a path component.
///
/// RFC 9051 spells the attribute `\NoSelect` and RFC 3501 spelled it `\Noselect`; both are
/// in the wild, and attributes are case-insensitive, so the comparison is too.
fn is_selectable(row: &ListRow) -> bool {
    !row.attributes.iter().any(|attr| {
        attr.trim_start_matches('\\')
            .eq_ignore_ascii_case("noselect")
    })
}

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod tests;
