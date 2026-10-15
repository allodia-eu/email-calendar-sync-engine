//! Keeping a keyword as a Gmail **label** under a name the host registered.
//!
//! Gmail stores no free-form keyword. A label is the nearest thing, and a label is also a place
//! (`normalize`), so a keyword is kept only where the host registered a name for it
//! ([`KeywordName`]), and a label carrying one of those names is read as that keyword rather
//! than as a folder: it is left out of the label list and out of every message's memberships,
//! and the message carries the keyword instead.
//!
//! **One label per keyword, whatever the device's language.** The label list is read before a
//! label is created, and a label under any of the keyword's names is used as it is; only when
//! there is none is one created under [`KeywordName::create_as`]. It is created hidden from
//! Gmail's label list (`labelListVisibility: labelHide`) and shown on the messages carrying it
//! (`messageListVisibility: show`).
//!
//! `messages.send` answers with the sent message's id, so the filed copy is labelled with one
//! `messages.modify` straight after the send. Nothing here can fail the send: a label that
//! cannot be found or made, or a refused modify, only leaves the keyword out of the receipt.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

use engine_core::{
    ids::{MailboxId, ProviderKey},
    mail::{Keyword, MailState, Message},
    membership::Memberships,
};
use engine_provider::{Draft, KeywordName, keyword_named};
use serde_json::{Value, json};

use crate::{error::GoogleError, normalize::ALL_MAIL_ID, transport::GoogleClient};

const LABELS: &str = "/gmail/v1/users/me/labels";

/// The account's labels that stand for a keyword, by label id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NamedLabels(BTreeMap<String, Keyword>);

impl NamedLabels {
    /// The labels in a raw `labels.list` answer whose name is one of `names`.
    pub(crate) fn from_list(labels: &[Value], names: &[KeywordName]) -> Self {
        let mut by_id = BTreeMap::new();
        for label in labels {
            let id = label.get("id").and_then(Value::as_str);
            let name = label.get("name").and_then(Value::as_str);
            if let (Some(id), Some(name)) = (id, name)
                && let Some(keyword) = keyword_named(names, name)
            {
                by_id.insert(id.to_owned(), keyword.clone());
            }
        }
        Self(by_id)
    }

    /// Whether the label `id` stands for a keyword.
    pub(crate) fn contains(&self, id: &str) -> bool {
        self.0.contains_key(id)
    }

    /// A label standing for `keyword`, if the account has one.
    fn id_for(&self, keyword: &Keyword) -> Option<&str> {
        self.0
            .iter()
            .find(|(_, named)| *named == keyword)
            .map(|(id, _)| id.as_str())
    }

    /// Moves a named label off `message`'s memberships onto its keywords.
    pub(crate) fn apply(&self, message: &mut Message) {
        if let Some((memberships, keywords)) = self.split(&message.mailboxes) {
            message.mailboxes = memberships;
            message.keywords.extend(keywords);
        }
    }

    /// The same for a state change that carries its filing.
    pub(crate) fn apply_state(&self, state: &mut MailState) {
        let Some(mailboxes) = &state.mailboxes else {
            return;
        };
        if let Some((memberships, keywords)) = self.split(mailboxes) {
            state.mailboxes = Some(memberships);
            state.keywords.extend(keywords);
        }
    }

    /// The memberships without the named labels, falling back to All Mail as the normalizer
    /// does, and the keywords those labels stand for. `None` when none is named.
    fn split(
        &self,
        memberships: &Memberships<MailboxId>,
    ) -> Option<(Memberships<MailboxId>, BTreeSet<Keyword>)> {
        let keywords: BTreeSet<Keyword> = memberships
            .iter()
            .filter_map(|id| self.0.get(id.as_str()).cloned())
            .collect();
        if keywords.is_empty() {
            return None;
        }
        let places = memberships
            .iter()
            .filter(|id| !self.contains(id.as_str()))
            .cloned();
        let memberships = Memberships::new(places).unwrap_or_else(|_| {
            Memberships::of_one(MailboxId::try_from(ALL_MAIL_ID).expect("reserved id is valid"))
        });
        Some((memberships, keywords))
    }
}

/// The names a host registered, and the labels the account was last seen to have for them.
#[derive(Debug, Default)]
pub(crate) struct Labels {
    names: Vec<KeywordName>,
    known: Mutex<Option<NamedLabels>>,
}

impl Labels {
    pub(crate) fn new(names: Vec<KeywordName>) -> Self {
        Self {
            names,
            known: Mutex::default(),
        }
    }

    pub(crate) fn names(&self) -> &[KeywordName] {
        &self.names
    }

    /// Notes what a label-list read found.
    pub(crate) fn remember(&self, named: NamedLabels) {
        *self.known() = Some(named);
    }

    /// The named labels, from the last label-list read or, before one, a read of its own.
    /// Costs no request when no name is registered.
    pub(crate) async fn resolved(&self, client: &GoogleClient) -> Result<NamedLabels, GoogleError> {
        if self.names.is_empty() {
            return Ok(NamedLabels::default());
        }
        if let Some(known) = self.known().clone() {
            return Ok(known);
        }
        self.reread(client).await
    }

    async fn reread(&self, client: &GoogleClient) -> Result<NamedLabels, GoogleError> {
        let doc = client.get(&client.url(LABELS)).await?;
        let list = doc
            .get("labels")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let named = NamedLabels::from_list(list, &self.names);
        self.remember(named.clone());
        Ok(named)
    }

    /// Labels the just-sent message `sent` with each keyword `draft` asks for that has a
    /// registered name, returning the keywords the copy now carries.
    pub(crate) async fn tag_sent_copy(
        &self,
        client: &GoogleClient,
        sent: &ProviderKey,
        draft: &Draft,
    ) -> BTreeSet<Keyword> {
        let mut ids = Vec::new();
        let mut kept = BTreeSet::new();
        for named in &self.names {
            if !draft.sent_copy_keywords.contains(named.keyword()) {
                continue;
            }
            if let Ok(id) = self.label_for(client, named).await {
                ids.push(id);
                kept.insert(named.keyword().clone());
            }
        }
        if ids.is_empty() {
            return BTreeSet::new();
        }
        let body = json!({ "addLabelIds": ids, "removeLabelIds": [] });
        let modified = client
            .post(
                &client.url(&format!(
                    "/gmail/v1/users/me/messages/{}/modify",
                    sent.as_str()
                )),
                "application/json",
                body.to_string().into_bytes(),
            )
            .await;
        if modified.is_ok() {
            kept
        } else {
            BTreeSet::new()
        }
    }

    /// The id of the label standing for `named`: one the account has under any of its names,
    /// else one created under [`KeywordName::create_as`]. A create another device won by a
    /// moment (`409`) reads the list again.
    async fn label_for(
        &self,
        client: &GoogleClient,
        named: &KeywordName,
    ) -> Result<String, GoogleError> {
        if let Some(id) = self.resolved(client).await?.id_for(named.keyword()) {
            return Ok(id.to_owned());
        }
        if let Some(id) = self.reread(client).await?.id_for(named.keyword()) {
            return Ok(id.to_owned());
        }
        let body = json!({
            "name": named.create_as(),
            "labelListVisibility": "labelHide",
            "messageListVisibility": "show",
        });
        match client
            .post(
                &client.url(LABELS),
                "application/json",
                body.to_string().into_bytes(),
            )
            .await
        {
            Ok(Some(created)) => {
                let id = created
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| GoogleError::protocol("labels.create answered no id"))?
                    .to_owned();
                self.known()
                    .get_or_insert_with(NamedLabels::default)
                    .0
                    .insert(id.clone(), named.keyword().clone());
                Ok(id)
            }
            Ok(None) => Err(GoogleError::protocol("labels.create answered no body")),
            Err(GoogleError::Status { status: 409, .. }) => self
                .reread(client)
                .await?
                .id_for(named.keyword())
                .map(str::to_owned)
                .ok_or_else(|| GoogleError::protocol("a label conflicted and is not listed")),
            Err(other) => Err(other),
        }
    }

    fn known(&self) -> std::sync::MutexGuard<'_, Option<NamedLabels>> {
        self.known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[path = "named_labels_tests.rs"]
mod tests;
