//! The end of a scope lease: releasing it, and forgetting the scope it held.
//!
//! Both take the lease by value at the trait (a released lease must not be reused); these
//! bodies read it by reference.

use super::{MemStore, ScopeCell};
use crate::{
    error::{Result, StoreError},
    lease::{Clock, SyncLease},
};

impl<C: Clock> MemStore<C> {
    /// Frees the scope if `lease` is still its current one; a stale lease frees nothing.
    pub(super) fn release_scope(&self, lease: &SyncLease) {
        let mut inner = self.lock();
        if let Some(cell) = inner.scopes.get_mut(lease.scope())
            && cell.token == lease.token()
        {
            cell.lease_expiry = None;
        }
    }

    /// Empties the scope `lease` holds, cursor included, and frees it.
    pub(super) fn forget_scope_under(&self, lease: &SyncLease) -> Result<()> {
        let mut inner = self.lock();
        let cell = inner
            .scopes
            .get_mut(lease.scope())
            .ok_or(StoreError::StaleLease)?;
        if lease.token() != cell.token {
            return Err(StoreError::StaleLease);
        }
        // The generation survives, so a lease older than this one stays fenced out.
        let token = cell.token;
        *cell = ScopeCell::new();
        cell.token = token;
        Ok(())
    }
}
