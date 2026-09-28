//! Keeping a keyword on the sender's filed copy as an Outlook **category**.
//!
//! Graph keeps no free-form keyword. What it has is `categories`, a list of names on each
//! message that Outlook shows the person, so a keyword is kept only where the host registered a
//! name for it ([`KeywordName`]); sync reads such a category back as the keyword
//! ([`crate::normalize_state::keywords_from_json`]).
//!
//! `sendMail` answers `202` with no body and files the copy itself, a moment later, so the send
//! does not wait for it. The adapter keeps nothing within the send and says so
//! ([`Capabilities::sent_copy_keywords_deferred`](engine_provider::Capabilities::sent_copy_keywords_deferred));
//! the outbox then records a [`MailEdit::SetKeywords`](engine_provider::MailEdit::SetKeywords)
//! on the copy's `sent:<Message-ID>` placeholder key, and draining it lands here
//! ([`Categories::apply`]). A placeholder is resolved by looking the copy up in Sent Items by
//! the `Message-ID` the MIME carried (Graph keeps it verbatim, `crate::submit`); a copy not
//! there yet is a retryable failure, so the outbox tries again on its own schedule. A live
//! mailbox has shown the copy within about three seconds.
//!
//! **One category per keyword, whatever the device's language.** Before the first tag the Sent
//! Items folder is asked for a message already carrying any of the keyword's names, and the name
//! it carries is the one used from then on; only when none does is the category created under
//! [`KeywordName::create_as`]. The account's master category list would answer the same
//! question directly, but reading it takes the `MailboxSettings.Read` scope, which a mail client
//! otherwise has no use for.

use std::{collections::BTreeMap, sync::Mutex};

use engine_core::{ids::ProviderKey, mail::Keyword};
use engine_provider::{KeywordName, ProviderError, ProviderResult};
use serde_json::{Map, Value, json};

use crate::{error::GraphError, transport::GraphClient};

/// The prefix of the key a sent copy goes by until Sent Items syncs it back (`crate::submit`).
const SENT_PLACEHOLDER: &str = "sent:";

/// The names a host registered, and the category name settled on for each keyword.
#[derive(Debug, Default)]
pub(crate) struct Categories {
    names: Vec<KeywordName>,
    settled: Mutex<BTreeMap<Keyword, String>>,
}

impl Categories {
    pub(crate) fn new(names: Vec<KeywordName>) -> Self {
        Self {
            names,
            settled: Mutex::default(),
        }
    }

    /// The registered names, for reading categories back as keywords.
    pub(crate) fn names(&self) -> &[KeywordName] {
        &self.names
    }

    /// The registered name of `keyword`, if the host gave it one.
    pub(crate) fn named(&self, keyword: &Keyword) -> Option<&KeywordName> {
        self.names.iter().find(|named| named.keyword() == keyword)
    }

    /// Adds the category of each of `add` to `target` and takes every name of each of `remove`
    /// off it, together with the rest of `body` (the `isRead`/`flag` half of the same edit), in
    /// one `PATCH`. Returns the key of the message patched: the resolved id when `target` was a
    /// sent copy's placeholder.
    ///
    /// # Errors
    ///
    /// A retryable [`ProviderError`] while a placeholder's copy is not in Sent Items yet;
    /// otherwise the classification of the failed request.
    pub(crate) async fn apply(
        &self,
        client: &GraphClient,
        target: &ProviderKey,
        add: &[&KeywordName],
        remove: &[&KeywordName],
        mut body: Map<String, Value>,
    ) -> ProviderResult<ProviderKey> {
        let (id, mut categories) =
            if let Some(message_id) = target.as_str().strip_prefix(SENT_PLACEHOLDER) {
                find_sent_copy(client, message_id).await?.ok_or_else(|| {
                    ProviderError::retryable("the sent copy is not in Sent Items yet")
                })?
            } else {
                let doc = client
                    .get(&client.url(&format!(
                        "/messages/{}?$select=id,categories",
                        target.as_str()
                    )))
                    .await?;
                (target.as_str().to_owned(), categories(&doc))
            };
        categories.retain(|held| !remove.iter().any(|named| named.is_named(held)));
        for named in add {
            if !categories.iter().any(|held| named.is_named(held)) {
                categories.push(self.category_for(client, named).await);
            }
        }
        body.insert("categories".to_owned(), json!(categories));
        client
            .patch(
                &client.url(&format!("/messages/{id}")),
                "application/json",
                None,
                serde_json::to_vec(&Value::Object(body)).map_err(GraphError::from)?,
            )
            .await?;
        ProviderKey::new(id).map_err(|err| ProviderError::permanent(err.to_string()))
    }

    /// The category name `named` goes by in this mailbox: one a sent message already carries,
    /// else the name to create it under. Settled once found; a lookup that fails settles
    /// nothing, so the next tag asks again.
    async fn category_for(&self, client: &GraphClient, named: &KeywordName) -> String {
        if let Some(name) = self.settled().get(named.keyword()) {
            return name.clone();
        }
        let Ok(found) = category_in_use(client, named).await else {
            return named.create_as().to_owned();
        };
        let name = found.unwrap_or_else(|| named.create_as().to_owned());
        self.settled().insert(named.keyword().clone(), name.clone());
        name
    }

    fn settled(&self) -> std::sync::MutexGuard<'_, BTreeMap<Keyword, String>> {
        self.settled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The id and categories of the Sent Items message whose `internetMessageId` is `message_id`,
/// or `None` when it is not there (yet).
async fn find_sent_copy(
    client: &GraphClient,
    message_id: &str,
) -> Result<Option<(String, Vec<String>)>, GraphError> {
    let filter = format!(
        "internetMessageId eq {}",
        literal(&format!("<{message_id}>"))
    );
    let doc = client
        .get(&client.url(&format!(
            "/mailFolders/sentitems/messages?$filter={}&$select=id,categories&$top=1",
            encode(&filter)
        )))
        .await?;
    Ok(first(&doc).and_then(|found| {
        let id = found.get("id").and_then(Value::as_str)?.to_owned();
        Some((id, categories(found)))
    }))
}

/// Which of `named`'s names a Sent Items message already carries, if any.
async fn category_in_use(
    client: &GraphClient,
    named: &KeywordName,
) -> Result<Option<String>, GraphError> {
    let filter = named
        .names()
        .map(|name| format!("categories/any(c:c eq {})", literal(name)))
        .collect::<Vec<_>>()
        .join(" or ");
    let doc = client
        .get(&client.url(&format!(
            "/mailFolders/sentitems/messages?$filter={}&$select=categories&$top=1",
            encode(&filter)
        )))
        .await?;
    Ok(first(&doc).and_then(|found| {
        categories(found)
            .into_iter()
            .find(|held| named.is_named(held))
    }))
}

/// The first entry of a collection response.
fn first(doc: &Value) -> Option<&Value> {
    doc.get("value").and_then(Value::as_array)?.first()
}

/// A message's `categories`.
fn categories(message: &Value) -> Vec<String> {
    message
        .get("categories")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// `value` as an OData string literal: quoted, with a quote inside doubled.
fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Percent-encodes what a query value cannot carry literally, leaving the OData syntax
/// (`/`, `(`, `:`, `'`) that Graph reads as written.
fn encode(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'/'
            | b'('
            | b')'
            | b':'
            | b'\''
            | b'@' => out.push(char::from(byte)),
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "categories_tests.rs"]
mod tests;
