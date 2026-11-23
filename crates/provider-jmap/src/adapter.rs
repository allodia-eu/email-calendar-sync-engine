//! The JMAP adapter handle, [`JmapProvider`]: how one is made, and what every capability
//! module asks of it (the executor, the session's account ids for each data type, and the
//! contact calls).
//!
//! Apart from `provider.rs`, which holds the [`Provider`](engine_provider::Provider)
//! implementation, because the capability modules (`crate::vacation`, `crate::contact`,
//! `crate::provider_calendar`, …) build on these helpers and not on that trait.

use engine_core::ids::AddressBookId;
use engine_provider::Capabilities;

use crate::{
    JmapClient, JmapConfig,
    error::JmapError,
    executor::Executor,
    request::{Request, capability},
};

/// The JMAP provider adapter.
///
/// Construct one with [`JmapProvider::connect`]. It implements
/// [`engine_provider::Provider`] for the step-4 mail spine (mailboxes + email);
/// submission and calendar land in later slices.
pub struct JmapProvider {
    pub(crate) executor: Box<dyn Executor>,
    pub(crate) capabilities: Capabilities,
    /// The address book host-facing contact writes target, once
    /// [`JmapProvider::with_contact_address_book`] has bound one. `None` until then:
    /// JMAP has no well-known default book, so a fabricated id would advertise a
    /// destination the server will reject.
    pub(crate) contact_address_book: Option<AddressBookId>,
}

impl core::fmt::Debug for JmapProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("JmapProvider")
            .field("capabilities", &self.capabilities)
            .field("contact_address_book", &self.contact_address_book)
            .finish_non_exhaustive()
    }
}

impl JmapProvider {
    /// Connects to a JMAP server and discovers its session.
    ///
    /// # Errors
    ///
    /// Returns [`JmapError`] on a connect/HTTP failure or a malformed session.
    pub async fn connect(config: JmapConfig) -> Result<Self, JmapError> {
        let client = JmapClient::connect(config).await?;
        Ok(Self::with_executor(Box::new(client)))
    }

    /// Wraps an executor, snapshotting its advertised capabilities.
    pub(crate) fn with_executor(executor: Box<dyn Executor>) -> Self {
        let capabilities = executor.session().capabilities();
        // Where it syncs mail, this adapter widens by band (`crate::email_band`): a fact of the
        // adapter rather than of the server, so it is added here and not read off the session.
        let capabilities = if capabilities.mail() {
            capabilities.with_band_widening()
        } else {
            capabilities
        };
        Self {
            executor,
            capabilities,
            contact_address_book: None,
        }
    }

    /// Binds host-facing contact writes to one **discovered** address book.
    ///
    /// Until this is called the provider advertises no contact destination, so a host
    /// that forgot to bind one fails at its own validation rather than on the wire.
    #[must_use]
    pub fn with_contact_address_book(mut self, address_book: AddressBookId) -> Self {
        self.contact_address_book = Some(address_book);
        self
    }

    pub(crate) fn executor(&self) -> &dyn Executor {
        self.executor.as_ref()
    }

    pub(crate) fn contact_account(&self) -> Result<String, JmapError> {
        Ok(self.executor.session().contact_account_id()?.to_string())
    }

    pub(crate) async fn contact_call(
        &self,
        method: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, JmapError> {
        let mut request = Request::new([capability::CORE, capability::CONTACTS]);
        let call = request.invoke(method, arguments);
        let response = self.executor.execute(&request).await?;
        Ok(response.result(&call)?.clone())
    }

    pub(crate) async fn download_contact_blob(
        &self,
        blob: &str,
        media_type: Option<&str>,
    ) -> Result<Vec<u8>, JmapError> {
        let account = self.contact_account()?;
        let template = self
            .executor
            .session()
            .download_url()
            .ok_or_else(|| JmapError::session("server advertised no downloadUrl"))?;
        // `media_type` comes from the server's JSContact payload, not from a fixed
        // literal, so the substitution must be encoded: an unencoded `?`/`#`/`&`/`..`
        // in it would re-point or re-parameterize the download URL.
        let url = crate::blob::fill_download(
            template,
            &account,
            blob,
            media_type.unwrap_or("application/octet-stream"),
            "contact-photo",
        );
        self.executor.download(&url).await
    }

    /// The JMAP (server-side) mail account id for mail method arguments.
    pub(crate) fn mail_account(&self) -> Result<String, JmapError> {
        Ok(self.executor.session().mail_account_id()?.to_string())
    }

    /// The JMAP (server-side) calendar account id for calendar method arguments.
    pub(crate) fn calendar_account(&self) -> Result<String, JmapError> {
        Ok(self.executor.session().calendar_account_id()?.to_string())
    }
}
