//! Shared mail-store identity.

use serde::{Deserialize, Serialize};

use super::{IdError, ProviderKey};

object_id! {
    /// An adapter-scoped handle that reopens a mail store the credential may access but
    /// does not necessarily own — a Microsoft 365 shared mailbox, a JMAP non-personal
    /// account, an IMAP shared-namespace prefix.
    ///
    /// Opaque like every other [`ProviderKey`], and deliberately **not** an address: the
    /// three protocols hand back three different things (a JMAP account id, an IMAP
    /// namespace path prefix, a Graph UPN), and an address is not even canonical — a
    /// Microsoft 365 alias resolves to its target mailbox, so two addresses can name one
    /// store. A host stores this handle and hands it back to the same adapter to bind a
    /// provider to that store.
    ///
    /// It sits beside [`super::DavCollectionId`] as the other adapter-scoped handle in
    /// this module: not a normalized engine identity, and never mixed with the
    /// host-assigned [`super::AccountId`] the store keys objects under — a discovered
    /// shared mailbox becomes an account only when a host decides to onboard it.
    SharedMailboxId
}
