//! The `UID` a new card gets when its draft carries none.

use engine_core::{contact::ContactDraft, ids::Uid};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The `UID` minted for a draft without one, and the SHA-256 it was minted from.
///
/// It is a **fallback that keeps a retried create stable, not an identity**: the same draft
/// mints the same `UID`, so a create sent again after its answer was lost sends the same one,
/// but an edited draft mints another. Only within one engine version: the hash covers the
/// engine's serde form of the whole draft, so a field added to or renamed in `ContactCard`, or a
/// draft rebuilt after a sync changed its revisions, raw JSON or timestamps, mints another, and a
/// create persisted before an upgrade does not meet its earlier write after it. A host that has an
/// identity for the card (a migration host has its pair and the source card's id) sets
/// [`ContactCard::uid`] itself.
///
/// [`ContactCard::uid`]: engine_core::contact::ContactCard::uid
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentUid {
    uid: Uid,
    digest: [u8; 32],
}

impl ContentUid {
    /// The minted `UID`: `urn:uuid:` and an RFC 9562 §5.8 UUIDv8 of [`digest`](Self::digest).
    #[must_use]
    pub fn uid(&self) -> &Uid {
        &self.uid
    }

    /// The SHA-256 of the draft's canonical form, which an adapter may also name the new object
    /// by, so its name and its `UID` come from one hash.
    #[must_use]
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

/// Mints the `UID` for `draft`, a card to create that carries none ([`ContentUid`]).
///
/// A contact needs one on two targets: RFC 6352 §5.1 says "vCard components in an address book
/// collection MUST have a UID property value", and RFC 9553 §2.1.9 makes a Card's `uid`
/// mandatory. A card from a source without a real UID has none, so an adapter writing it there
/// mints one here: one rule for every adapter.
///
/// It hashes the **whole draft**: its address book and every field of the card except `uid` and
/// `id`, so two cards that differ anywhere (a birthday, a photo) mint two `UID`s, even where the
/// adapter writes only part of the card, and the same draft mints the same `UID` only within one
/// engine version ([`ContentUid`]). The form hashed is JSON with every object's keys
/// sorted, so it does not depend on how a map was built or on serde_json's `preserve_order`
/// feature, which a host's dependency graph may turn on.
#[must_use]
pub fn content_uid_of(draft: &ContactDraft) -> ContentUid {
    let mut value = serde_json::to_value(draft).expect("a contact draft serializes");
    if let Some(card) = value.get_mut("card").and_then(Value::as_object_mut) {
        card.remove("uid");
        card.remove("id");
    }
    let mut canonical = String::new();
    write_canonical(&value, &mut canonical);
    let digest: [u8; 32] = Sha256::digest(canonical.as_bytes()).into();
    ContentUid {
        uid: uuid_v8(&digest),
        digest,
    }
}

/// `value` as JSON with every object's keys in sorted order, whatever order they were built in.
fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in sorted(map.iter().collect()).into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(item, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// An object's entries in key order, whatever order its map iterates in: a serde_json `Map` is
/// sorted without the `preserve_order` feature and in insertion order with it.
fn sorted<'a>(mut entries: Vec<(&'a String, &'a Value)>) -> Vec<(&'a String, &'a Value)> {
    entries.sort_by(|left, right| left.0.cmp(right.0));
    entries
}

/// `urn:uuid:` and the RFC 9562 §5.8 UUIDv8 of `digest`'s first 128 bits, with the version (8)
/// and the variant (`10`) set.
fn uuid_v8(digest: &[u8; 32]) -> Uid {
    let mut octets = [0_u8; 16];
    octets.copy_from_slice(&digest[..16]);
    octets[6] = (octets[6] & 0x0f) | 0x80;
    octets[8] = (octets[8] & 0x3f) | 0x80;
    let hex: Vec<String> = octets.iter().map(|byte| format!("{byte:02x}")).collect();
    let minted = format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        hex[..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..].concat()
    );
    Uid::new(minted).expect("a 45-octet urn:uuid is a Uid")
}

#[cfg(test)]
#[path = "content_uid_tests.rs"]
mod tests;
