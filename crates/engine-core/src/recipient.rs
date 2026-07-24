//! Recipient observations and deterministic global autosuggest ranking.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    ids::{AccountId, MailboxId, MessageId, PersonId},
    mail::{EmailAddress, Message},
    people::{CanonicalEmail, Person, PersonSourceId},
    sync::SyncWindow,
    time::UtcDateTime,
};

/// One idempotent sent-recipient observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipientObservation {
    /// Account whose Sent message supplied the observation.
    pub account: AccountId,
    /// Source message identity.
    pub source_message: MessageId,
    /// Conservative canonical email.
    pub email: CanonicalEmail,
    /// Display name from that recipient header.
    pub name: Option<String>,
    /// Source message's sent timestamp.
    pub sent_at: Option<UtcDateTime>,
}

/// An aggregate of eligible, non-suppressed observations for one email.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipientInteraction {
    /// Canonical email.
    pub email: CanonicalEmail,
    /// Most useful observed display name.
    pub name: Option<String>,
    /// Distinct source-message count.
    pub sent_count: u64,
    /// Most recent sent instant.
    pub last_sent: Option<UtcDateTime>,
}

impl RecipientInteraction {
    /// Creates an interaction aggregate.
    #[must_use]
    pub fn new(
        email: CanonicalEmail,
        name: Option<String>,
        sent_count: u64,
        last_sent: Option<UtcDateTime>,
    ) -> Self {
        Self {
            email,
            name,
            sent_count,
            last_sent,
        }
    }
}

/// One globally unique recipient suggestion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipientSuggestion {
    /// Canonical email.
    pub email: CanonicalEmail,
    /// Unified person, when backed by a contact.
    pub person_id: Option<PersonId>,
    /// Preferred display name.
    pub display_name: String,
    /// Contact source records that supplied this email.
    pub provenance: BTreeSet<PersonSourceId>,
    /// Number of observed distinct sent messages.
    pub sent_count: u64,
    /// Most recent observed send.
    pub last_sent: Option<UtcDateTime>,
    /// Whether a saved personal contact supplies the address.
    pub is_saved: bool,
}

/// Honest per-account coverage for recipient observations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipientCoverage {
    /// Account whose sent mail was observed.
    pub account: AccountId,
    /// Normal mail-sync window used for the observations.
    pub window: SyncWindow,
    /// Whether a normalized Sent collection could be identified.
    pub sent_collection_identified: bool,
}

/// Extracts recipients when `message` currently belongs to a normalized Sent
/// mailbox.
///
/// Invalid addresses are skipped. To/Cc/Bcc are deduplicated by conservative
/// canonical email for this source message. Self-addresses are intentionally
/// retained.
#[must_use]
pub fn observe_sent_recipients(
    account: &AccountId,
    message: &Message,
    sent_mailboxes: &BTreeSet<MailboxId>,
) -> Vec<RecipientObservation> {
    if !message
        .mailboxes
        .iter()
        .any(|mailbox| sent_mailboxes.contains(mailbox))
    {
        return Vec::new();
    }
    let mut recipients: BTreeMap<CanonicalEmail, &EmailAddress> = BTreeMap::new();
    for address in message
        .envelope
        .to
        .iter()
        .chain(&message.envelope.cc)
        .chain(&message.envelope.bcc)
    {
        if let Ok(email) = CanonicalEmail::parse(&address.email) {
            recipients.entry(email).or_insert(address);
        }
    }
    recipients
        .into_iter()
        .map(|(email, address)| RecipientObservation {
            account: account.clone(),
            source_message: message.id.clone(),
            email,
            name: address
                .name
                .as_ref()
                .filter(|name| !name.trim().is_empty())
                .cloned(),
            sent_at: message.sent_at,
        })
        .collect()
}

#[derive(Debug)]
struct Candidate {
    suggestion: RecipientSuggestion,
    writable: bool,
    interaction_name: Option<String>,
}

/// Combines people and interaction aggregates into stable recipient suggestions.
#[must_use]
pub fn rank_recipient_suggestions(
    query: &str,
    people: &[Person],
    interactions: &[RecipientInteraction],
    limit: usize,
) -> Vec<RecipientSuggestion> {
    let mut candidates = BTreeMap::<CanonicalEmail, Candidate>::new();
    for person in people {
        for email in &person.emails {
            let candidate = candidates
                .entry(email.value.clone())
                .or_insert_with(|| Candidate {
                    suggestion: RecipientSuggestion {
                        email: email.value.clone(),
                        person_id: Some(person.id),
                        display_name: person.display_name.clone(),
                        provenance: BTreeSet::new(),
                        sent_count: 0,
                        last_sent: None,
                        is_saved: person.is_saved,
                    },
                    writable: person.is_writable,
                    interaction_name: None,
                });
            candidate
                .suggestion
                .provenance
                .extend(email.sources.iter().cloned());
            candidate.suggestion.is_saved |= person.is_saved;
            candidate.writable |= person.is_writable;
        }
    }
    for interaction in interactions {
        let candidate = candidates
            .entry(interaction.email.clone())
            .or_insert_with(|| Candidate {
                suggestion: RecipientSuggestion {
                    email: interaction.email.clone(),
                    person_id: None,
                    display_name: interaction
                        .name
                        .clone()
                        .unwrap_or_else(|| interaction.email.to_string()),
                    provenance: BTreeSet::new(),
                    sent_count: 0,
                    last_sent: None,
                    is_saved: false,
                },
                writable: false,
                interaction_name: interaction.name.clone(),
            });
        candidate.suggestion.sent_count = candidate
            .suggestion
            .sent_count
            .saturating_add(interaction.sent_count);
        candidate.suggestion.last_sent = candidate.suggestion.last_sent.max(interaction.last_sent);
        if candidate.suggestion.person_id.is_none() {
            candidate.interaction_name.clone_from(&interaction.name);
            if let Some(name) = &candidate.interaction_name {
                candidate.suggestion.display_name.clone_from(name);
            }
        }
    }

    let query = query.trim();
    let exact = CanonicalEmail::parse(query).ok();
    let query_folded = query.to_lowercase();
    let mut ranked: Vec<(u8, Candidate)> = candidates
        .into_values()
        .filter_map(|candidate| {
            let quality = match_quality(&candidate, exact.as_ref(), &query_folded)?;
            Some((quality, candidate))
        })
        .collect();
    ranked.sort_by(|(left_quality, left), (right_quality, right)| {
        if query.is_empty() {
            right
                .suggestion
                .sent_count
                .gt(&0)
                .cmp(&left.suggestion.sent_count.gt(&0))
                .then(right.suggestion.last_sent.cmp(&left.suggestion.last_sent))
                .then(right.suggestion.sent_count.cmp(&left.suggestion.sent_count))
                .then(right.suggestion.is_saved.cmp(&left.suggestion.is_saved))
                .then(right.writable.cmp(&left.writable))
                .then(left.suggestion.email.cmp(&right.suggestion.email))
        } else {
            left_quality
                .cmp(right_quality)
                .then(right.suggestion.is_saved.cmp(&left.suggestion.is_saved))
                .then(right.writable.cmp(&left.writable))
                .then(right.suggestion.last_sent.cmp(&left.suggestion.last_sent))
                .then(right.suggestion.sent_count.cmp(&left.suggestion.sent_count))
                .then(left.suggestion.email.cmp(&right.suggestion.email))
        }
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, candidate)| candidate.suggestion)
        .collect()
}

fn match_quality(candidate: &Candidate, exact: Option<&CanonicalEmail>, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    if exact == Some(&candidate.suggestion.email) {
        return Some(0);
    }
    let email = candidate.suggestion.email.as_str().to_lowercase();
    let name = candidate.suggestion.display_name.to_lowercase();
    if token_prefix(&email, query) || token_prefix(&name, query) {
        Some(1)
    } else if email.contains(query) {
        Some(2)
    } else {
        None
    }
}

fn token_prefix(value: &str, query: &str) -> bool {
    value
        .split(|character: char| !character.is_alphanumeric())
        .any(|token| token.starts_with(query))
}
