//! Shared mail-store identity.

use serde::{Deserialize, Serialize};

use super::{IdError, ProviderKey};

object_id! {
    /// An adapter-scoped handle that reopens a mail store the credential may open but does
    /// not own — a Microsoft 365 shared mailbox, a JMAP account shared with the user, an
    /// IMAP store under another user's or a shared namespace.
    ///
    /// Opaque like every other [`ProviderKey`], and deliberately **not** an address. The
    /// three protocols hand back three different things (a JMAP account id, an IMAP
    /// namespace path, a Graph address), and an address is not even canonical: a Microsoft
    /// 365 alias resolves to its target mailbox, so two addresses can name one store. A host
    /// stores this handle and gives it back to the **same** adapter to bind a provider to
    /// that store; a handle from one adapter means nothing to another.
    ///
    /// It sits beside [`super::DavCollectionId`] as the other adapter-scoped handle in this
    /// module: not a normalized engine identity, and never interchangeable with the
    /// host-assigned [`super::AccountId`] the store keys objects under. A discovered store
    /// becomes an account only when a host decides to onboard it, under an id of its choosing.
    SharedMailboxId
}
