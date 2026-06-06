//! Account identity.

use serde::{Deserialize, Serialize};

use super::{IdError, ProviderKey};

object_id! {
    /// Identifies one account hosted by the engine.
    ///
    /// `AccountId` scopes every object, sync scope, cursor, and write: a
    /// provider key is only unique within an `(account, type)` pair. A change of
    /// `AccountId` for "the same" mailbox is a hard cache-invalidation boundary
    /// — JMAP models a catastrophic server reset as "account deleted then
    /// recreated under a new id" (RFC 8620 §1.6.2), at which point the engine
    /// drops all data under the old id and full-resyncs under the new one.
    AccountId
}
