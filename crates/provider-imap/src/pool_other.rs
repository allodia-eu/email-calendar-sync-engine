//! A slot in the account's budget for a connection to another of its services: ManageSieve.

use tokio::sync::OwnedSemaphorePermit;

use super::ImapPool;

impl<S> ImapPool<S> {
    /// A slot for a connection the account opens to another of its services (ManageSieve), held
    /// for as long as the permit is.
    ///
    /// Waits when every permit is spent, as a worker does. A parked IMAP connection holds no
    /// permit, so where the shelf and the slots in use would together go over the ceiling, the
    /// longest-parked connection is closed: the budget counts sockets, and the new one is a socket.
    pub(crate) async fn reserve_other(&self) -> OwnedSemaphorePermit {
        let permit = std::sync::Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("pool semaphore is never closed");
        let in_use = self
            .ceiling()
            .saturating_sub(self.permits.available_permits());
        let mut parked = self
            .parked
            .lock()
            .expect("pool parked-connection mutex poisoned");
        while !parked.is_empty() && parked.len() + in_use > self.ceiling() {
            parked.pop_front();
        }
        permit
    }
}

/// The account's budget, as a connection to another of its services spends it: object-safe, so
/// that service's handle is not generic over the IMAP stream.
pub(crate) trait Budget: Send + Sync {
    /// A slot, held for as long as the permit is ([`ImapPool::reserve_other`]).
    fn reserve(&self) -> futures_util::future::BoxFuture<'_, OwnedSemaphorePermit>;
}

impl<S: Send + Sync + 'static> Budget for ImapPool<S> {
    fn reserve(&self) -> futures_util::future::BoxFuture<'_, OwnedSemaphorePermit> {
        Box::pin(self.reserve_other())
    }
}

#[cfg(test)]
#[path = "pool_other_tests.rs"]
mod tests;
