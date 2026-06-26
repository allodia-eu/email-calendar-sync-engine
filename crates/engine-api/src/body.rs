//! The host-facing message-body read (`Engine::message_body`).
//!
//! Kept in its own module so the already-large `engine.rs` does not grow; it is a
//! second `impl Engine` block over the same store.

use engine_core::ids::AccountId;
use engine_core::mail::{Message, MessageBody};
use engine_provider::Provider;
use engine_sync::fetch_message_body;

use crate::engine::map_sync_error;
use crate::{ApiError, Engine};

impl Engine {
    /// Returns the displayable body of `message`, fetching its raw RFC 5322 source
    /// from `provider` on the first call and serving it from the store's
    /// content-addressed blob cache thereafter (`north-star.md` Tier-3 bodies).
    ///
    /// [`MessageBody::plain`] is the plain-text reading view; [`MessageBody::html`]
    /// is the message's **unsanitized** HTML, present only when the message carries
    /// a real HTML part — a host must sanitize before rendering. `message` is one of
    /// the objects [`Engine::messages`] returned; it carries the id (and JMAP/Graph
    /// blob handle) the adapter needs to address the fetch. This read takes **no**
    /// lease, so it never contends with an in-flight sync of the message's scope.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the provider fetch fails (a stale IMAP target is
    /// a `Conflict` — re-sync via [`Engine::clear_mail_cursors`] then retry) or the
    /// store cache read/write fails.
    pub async fn message_body<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        message: &Message,
    ) -> Result<MessageBody, ApiError> {
        fetch_message_body(provider, &self.store, account, message)
            .await
            .map_err(map_sync_error)
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use engine_core::ids::{AccountId, MailboxId, MessageId};
    use engine_core::membership::Memberships;
    use engine_core::raw::RawMime;
    use engine_provider::{Capabilities, Provider, ProviderResult};

    use crate::Engine;

    struct BodyProvider {
        caps: Capabilities,
    }

    #[async_trait]
    impl Provider for BodyProvider {
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }

        async fn fetch_message_source(
            &self,
            _account: &AccountId,
            _message: &engine_core::mail::Message,
        ) -> ProviderResult<RawMime> {
            Ok(RawMime::new(
                b"Content-Type: text/plain\r\n\r\nthe reading view".to_vec(),
            ))
        }
    }

    #[tokio::test]
    async fn message_body_fetches_and_extracts_plain_text() {
        let engine = Engine::open_in_memory().expect("engine");
        let provider = BodyProvider {
            caps: Capabilities::none().with_mail().with_message_source(),
        };
        assert!(provider.capabilities().message_source());
        let account = AccountId::try_from("acct").expect("account");
        let message = engine_core::mail::Message::new(
            MessageId::try_from("imap:v1:u1@INBOX").expect("id"),
            Memberships::of_one(MailboxId::try_from("INBOX").expect("mailbox")),
        );

        let body = engine
            .message_body(&provider, &account, &message)
            .await
            .expect("body");
        assert!(body.plain().unwrap().contains("the reading view"));
    }
}
