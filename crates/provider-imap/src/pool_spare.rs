//! Borrowing a worker only when one is spare.

use std::sync::{Arc, atomic::Ordering};

use tokio::io::{AsyncRead, AsyncWrite};

use super::{ImapPool, refuses_sessions};
use crate::pool_lease::PooledConnection;

impl<S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> ImapPool<S> {
    /// A worker if the budget has one free right now, preferring a parked connection with
    /// `mailbox` open; `None` rather than a wait. For work a pass would spread over spare
    /// connections and does not need: it never holds up another folder's pass. A dial the
    /// server refuses lowers the ceiling, as [`acquire`](Self::acquire)'s does.
    pub(crate) async fn try_acquire_for(
        self: &Arc<Self>,
        mailbox: &str,
    ) -> Option<PooledConnection<S>> {
        let permit = Arc::clone(&self.permits).try_acquire_owned().ok()?;
        match self.take_or_dial(Some(mailbox), false).await {
            Ok((connection, generation)) => {
                self.workers.fetch_add(1, Ordering::SeqCst);
                Some(PooledConnection::new(
                    connection,
                    generation,
                    Arc::clone(self),
                    permit,
                ))
            }
            Err(err) => {
                if refuses_sessions(&err) {
                    self.lower_ceiling(permit);
                }
                None
            }
        }
    }
}

#[cfg(test)]
#[path = "pool_spare_tests.rs"]
mod tests;
