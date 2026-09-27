//! One mailbox's messages over a span of time, and how far back the store holds that mailbox.

use std::collections::HashMap;

use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey},
    mail::Message,
    time::UtcDateTime,
};
use engine_store::{MailSelector, StoreRead};

use crate::{ApiError, Engine};

/// What [`Engine::mail_in_mailbox_between`] returns: a span of one mailbox's messages, and how far
/// back the store holds that mailbox.
#[derive(Debug, Clone, PartialEq)]
pub struct MailboxSpan {
    /// The mailbox's messages within the span, newest first.
    pub messages: Vec<Message>,
    /// The date of the oldest message the store holds in the mailbox, whatever span was asked
    /// for: how far back the mailbox reaches on this device. `None` when it holds no dated mail.
    ///
    /// A span that starts before this is not missing mail the device knows about; reaching
    /// further back needs a deeper sync of the mailbox
    /// ([`MailboxWindows`](crate::MailboxWindows)).
    pub oldest: Option<UtcDateTime>,
}

impl Engine {
    /// The messages `account` holds in `mailbox` dated within `[since, until)`, newest first and
    /// capped at `limit`, together with the oldest date the store holds for that mailbox.
    ///
    /// A message filed in several mailboxes (JMAP, Gmail) is selected by any of them. Undated
    /// mail has no place in a span of time and is not returned; a span with `since` not before
    /// `until` returns no messages. To page further back, ask again with `until` set to the
    /// oldest date returned.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn mail_in_mailbox_between(
        &self,
        account: &AccountId,
        mailbox: &MailboxId,
        since: UtcDateTime,
        until: UtcDateTime,
        limit: usize,
    ) -> Result<MailboxSpan, ApiError> {
        let rows = self
            .store
            .list_mail(
                core::slice::from_ref(account),
                MailSelector::Mailbox {
                    mailbox,
                    since,
                    until,
                },
                limit,
            )
            .await?;
        let order: HashMap<ProviderKey, usize> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| (row.mail.key.clone(), index))
            .collect();
        let keys: Vec<ProviderKey> = rows.into_iter().map(|row| row.mail.key).collect();
        let mut messages = self.messages_by_keys(account, &keys).await?;
        // `messages_by_keys` leaves the order unspecified; the rows came newest first.
        messages.sort_by_key(|message| order.get(message.id.key()).copied());
        let oldest = self.store.oldest_in_mailbox(account, mailbox).await?;
        Ok(MailboxSpan { messages, oldest })
    }
}
