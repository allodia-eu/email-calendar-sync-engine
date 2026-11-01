//! The IMAP reads a Gmail pass makes ([`provider_imap::GmailMail`]), behind a trait so the pass
//! can be tested against an in-memory mailbox.

use async_trait::async_trait;
use engine_provider::ProviderResult;
use provider_imap::{
    GmailFolder, GmailFolderState, GmailMail, GmailMessage, GmailQuery, GmailSources, GmailState,
};
use tokio::io::{AsyncRead, AsyncWrite};

/// What [`crate::imap_pass`] and the body reads need from a Gmail account's IMAP side; each
/// method is [`GmailMail`]'s of the same name.
#[async_trait]
pub(crate) trait ImapMail: Send + Sync {
    /// [`GmailMail::open`].
    async fn open(&self, folder: GmailFolder) -> ProviderResult<Option<GmailFolderState>>;
    /// [`GmailMail::search`].
    async fn search(
        &self,
        folder: GmailFolder,
        validity: u32,
        query: &GmailQuery,
    ) -> ProviderResult<Vec<u32>>;
    /// [`GmailMail::messages`].
    async fn messages(
        &self,
        folder: GmailFolder,
        validity: u32,
        uids: &[u32],
    ) -> ProviderResult<Vec<GmailMessage>>;
    /// [`GmailMail::changed`].
    async fn changed(
        &self,
        folder: GmailFolder,
        validity: u32,
        below_uid: u32,
        modseq: u64,
    ) -> ProviderResult<Vec<GmailState>>;
    /// [`GmailMail::ids`].
    async fn ids(
        &self,
        folder: GmailFolder,
        validity: u32,
        uids: &[u32],
    ) -> ProviderResult<Vec<(u32, u64)>>;
    /// [`GmailMail::sources`].
    fn sources<'a>(&'a self, gmail_ids: &'a [u64]) -> GmailSources<'a>;
}

#[async_trait]
impl<S> ImapMail for GmailMail<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
    async fn open(&self, folder: GmailFolder) -> ProviderResult<Option<GmailFolderState>> {
        GmailMail::open(self, folder).await
    }

    async fn search(
        &self,
        folder: GmailFolder,
        validity: u32,
        query: &GmailQuery,
    ) -> ProviderResult<Vec<u32>> {
        GmailMail::search(self, folder, validity, query).await
    }

    async fn messages(
        &self,
        folder: GmailFolder,
        validity: u32,
        uids: &[u32],
    ) -> ProviderResult<Vec<GmailMessage>> {
        GmailMail::messages(self, folder, validity, uids).await
    }

    async fn changed(
        &self,
        folder: GmailFolder,
        validity: u32,
        below_uid: u32,
        modseq: u64,
    ) -> ProviderResult<Vec<GmailState>> {
        GmailMail::changed(self, folder, validity, below_uid, modseq).await
    }

    async fn ids(
        &self,
        folder: GmailFolder,
        validity: u32,
        uids: &[u32],
    ) -> ProviderResult<Vec<(u32, u64)>> {
        GmailMail::ids(self, folder, validity, uids).await
    }

    fn sources<'a>(&'a self, gmail_ids: &'a [u64]) -> GmailSources<'a> {
        GmailMail::sources(self, gmail_ids)
    }
}
