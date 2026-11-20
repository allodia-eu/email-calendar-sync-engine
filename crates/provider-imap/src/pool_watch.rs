//! Reserving a connection for a watch to own.

use std::sync::Arc;

use engine_provider::{ProviderError, ProviderResult};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{ImapPool, MIN_WORKER_CONNECTIONS};
use crate::{
    pool_lease::{ReleaseWatch, WatchLease},
    transport::Connection,
};

impl<S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> ImapPool<S> {
    /// Takes a connection out of the pool for a watch to own, spending one permit for its whole
    /// lifetime.
    ///
    /// A parked connection is reused when there is one: the watcher's `EXAMINE` reads whatever
    /// untagged backlog it carries before the first `IDLE`, so it starts as clean as a fresh
    /// dial, and dialling beside a parked connection would put one socket over the budget.
    ///
    /// # Errors
    ///
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState) when the
    /// reservation would leave the workers below their floor — the host must watch fewer
    /// folders, or poll. The classified dial failure otherwise.
    pub(crate) async fn reserve_watch(
        self: &Arc<Self>,
    ) -> ProviderResult<(Connection<S>, WatchLease)> {
        {
            let mut watches = self.watches.lock().expect("pool watch counter poisoned");
            let ceiling = self.ceiling();
            // Refuse the reservation that would take the last worker: with a budget of 5 this
            // allows 4 watches and always keeps 1 free to sync. The host's own limit on watched
            // folders is normally stricter — this is the floor that makes over-reserving
            // impossible, not the policy that makes it sensible.
            if *watches + MIN_WORKER_CONNECTIONS >= ceiling {
                let limited = if ceiling < self.max_connections {
                    " while the server is limiting it"
                } else {
                    ""
                };
                return Err(ProviderError::invalid_state(format!(
                    "cannot watch another folder: this account may hold {ceiling} \
                     connection(s){limited}, {} already watching a folder, and \
                     {MIN_WORKER_CONNECTIONS} must stay free to sync",
                    *watches,
                )));
            }
            *watches += 1;
        }
        // From here the slot is ours, so every exit must return it. The lease exists before the
        // dial for exactly that reason: if the dial fails, dropping it releases the slot.
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("pool semaphore is never closed");
        let lease = WatchLease::new(Arc::clone(self) as Arc<dyn ReleaseWatch>, permit);
        let (connection, _generation) = self.take_or_dial(None, false).await?;
        Ok((connection, lease))
    }
}
