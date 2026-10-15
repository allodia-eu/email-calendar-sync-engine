//! Keeping a keyword on the sender's filed copy as an Outlook **category**.
//!
//! Graph keeps no free-form keyword. What it has is `categories`, a list of names on each
//! message that Outlook shows the person, so a keyword is kept only where the host registered a
//! name for it ([`KeywordName`]); sync reads such a category back as the keyword
//! ([`crate::normalize_state::keywords_from_json`]).
//!
//! `sendMail` answers `202` with no body and files the copy itself, a moment later, so the copy
//! is looked up in Sent Items by the `Message-ID` the MIME carried (Graph keeps it verbatim,
//! `crate::submit`) and given the category with a `PATCH`. The lookup is repeated on a short
//! schedule ([`FIND_SCHEDULE`]): a live mailbox has shown the copy within about three seconds.
//! The message is delivered before any of it starts, and nothing here can fail the send: a copy
//! not found, or a `PATCH` refused, only leaves the keyword out of the receipt.
//!
//! **One category per keyword, whatever the device's language.** Before the first tag the Sent
//! Items folder is asked for a message already carrying any of the keyword's names, and the name
//! it carries is the one used from then on; only when none does is the category created under
//! [`KeywordName::create_as`]. The account's master category list would answer the same
//! question directly, but reading it takes the `MailboxSettings.Read` scope, which a mail client
//! otherwise has no use for.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    time::Duration,
};

use engine_core::mail::Keyword;
use engine_provider::{Draft, KeywordName};
use serde_json::{Value, json};

use crate::{error::GraphError, transport::GraphClient};

/// The waits before each lookup of the filed copy: five tries over about five seconds.
pub(crate) const FIND_SCHEDULE: &[Duration] = &[
    Duration::from_millis(500),
    Duration::from_millis(750),
    Duration::from_secs(1),
    Duration::from_millis(1250),
    Duration::from_millis(1500),
];

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

    /// Gives the copy of the just-sent `draft` the category of each keyword it asks for that
    /// has a registered name, returning the keywords the copy now carries. Waits `schedule`
    /// before each lookup of the copy.
    pub(crate) async fn tag_sent_copy(
        &self,
        client: &GraphClient,
        draft: &Draft,
        schedule: &[Duration],
    ) -> BTreeSet<Keyword> {
        let wanted: Vec<&KeywordName> = self
            .names
            .iter()
            .filter(|named| draft.sent_copy_keywords.contains(named.keyword()))
            .collect();
        if wanted.is_empty() {
            return BTreeSet::new();
        }
        let Some((id, mut categories)) =
            find_sent_copy(client, draft.message_id.as_str(), schedule).await
        else {
            return BTreeSet::new();
        };
        for named in &wanted {
            let name = self.category_for(client, named).await;
            if !categories.iter().any(|held| named.is_named(held)) {
                categories.push(name);
            }
        }
        let body = json!({ "categories": categories });
        let patched = client
            .patch(
                &client.url(&format!("/messages/{id}")),
                "application/json",
                None,
                body.to_string().into_bytes(),
            )
            .await;
        match patched {
            Ok(_) => wanted.iter().map(|named| named.keyword().clone()).collect(),
            Err(_) => BTreeSet::new(),
        }
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
/// looked for after each wait in `schedule`. `None` when it never turned up.
async fn find_sent_copy(
    client: &GraphClient,
    message_id: &str,
    schedule: &[Duration],
) -> Option<(String, Vec<String>)> {
    let filter = format!(
        "internetMessageId eq {}",
        literal(&format!("<{message_id}>"))
    );
    let url = client.url(&format!(
        "/mailFolders/sentitems/messages?$filter={}&$select=id,categories&$top=1",
        encode(&filter)
    ));
    for wait in schedule {
        tokio::time::sleep(*wait).await;
        let Ok(doc) = client.get(&url).await else {
            continue;
        };
        if let Some(found) = first(&doc) {
            let id = found.get("id").and_then(Value::as_str)?.to_owned();
            return Some((id, categories(found)));
        }
    }
    None
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
