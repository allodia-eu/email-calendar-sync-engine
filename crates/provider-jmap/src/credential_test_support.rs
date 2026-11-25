//! A failure of the credential source, made the way the engine makes one, for tests that feed it
//! to the paths that answer a failed exchange (a batch's messages, a held-back `Blob/get`).

use core::time::Duration;

use async_trait::async_trait;
use engine_core::error::FailureClass;
use engine_provider::{
    Credential, CredentialProtocol, CredentialRequest, CredentialSource, CredentialSubject, Origin,
    ProviderError,
};

use crate::JmapError;

/// A source that fails every ask with its class.
#[derive(Debug)]
pub(crate) struct Failing(pub(crate) CredentialSubject, pub(crate) FailureClass);

#[async_trait]
impl CredentialSource for Failing {
    fn subject(&self) -> &CredentialSubject {
        &self.0
    }

    async fn credential(
        &self,
        _request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        Err(ProviderError::new(self.1, "the broker said no"))
    }
}

/// What an exchange fails with when the source has no credential to give, classed `class`.
pub(crate) async fn no_credential(class: FailureClass) -> JmapError {
    let source = Failing(CredentialSubject::new("acct-1"), class);
    let origin = Origin::new("https", "jmap.example.com", 443);
    let request = CredentialRequest::new(CredentialProtocol::Http, &origin);
    let attempt = |_| async { Ok::<(), JmapError>(()) };
    let refusal = |_: &JmapError| false;
    engine_provider::with_credential(&source, &request, Duration::from_secs(1), refusal, attempt)
        .await
        .expect_err("the source fails")
}
