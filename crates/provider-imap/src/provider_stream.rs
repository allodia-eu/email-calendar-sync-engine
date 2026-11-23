//! A pass over the bound folder on a pooled session: the plumbing `Provider::stream_email` and
//! `Provider::widen_email` share, kept beside the trait implementation rather than in it.

use engine_core::sync::{SyncState, SyncWindow};
use engine_provider::{EmailStream, ProviderResult};
use futures_util::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{pool::PooledConnection, provider::ImapProvider};

impl<S> ImapProvider<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
    /// A pooled session for a pass over the bound folder.
    ///
    /// A provider scoped to a shared store syncs that store's folders only. Binding any other
    /// folder to it would put that folder's mail in the shared mailbox's account — refused, not
    /// synced.
    pub(crate) async fn folder_session(&self) -> ProviderResult<PooledConnection<S>> {
        let mut connection = self.session().await?;
        if self.shared_account.is_some()
            && let Err(refusal) = self.check_in_store(&mut connection, &self.mailbox).await
        {
            connection.discard();
            return Err(refusal);
        }
        Ok(connection)
    }

    /// [`crate::stream::stream_email`] over a pooled session on the bound folder, under
    /// `window`, the folder's own.
    pub(crate) fn folder_stream<'a>(
        &'a self,
        cursor: Option<&'a SyncState>,
        window: SyncWindow,
        fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        Box::pin(async_stream::stream! {
            let mut connection = match self.folder_session().await {
                Ok(connection) => connection,
                Err(err) => {
                    yield Err(err);
                    return;
                }
            };
            // The stream holds its connection for as long as it lives, and hands it back
            // healthy only if every item was: a pass that failed mid-way has left the session
            // in a state no one should inherit. One abandoned mid-fetch parks normally — the
            // connection drains the leftover response before its next command.
            let mut failed = false;
            {
                let pass = crate::stream::stream_email(
                    &mut connection,
                    Some(&self.pool),
                    &self.mailbox,
                    cursor,
                    window,
                    fetch_batch,
                    chunk_size,
                );
                futures_util::pin_mut!(pass);
                while let Some(item) = pass.next().await {
                    failed = item.is_err();
                    yield item;
                }
            }
            if failed {
                connection.discard();
            }
        })
    }
}
