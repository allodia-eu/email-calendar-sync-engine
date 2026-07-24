//! Unified-people filtering and generation-bound keyset cursors.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
};

use engine_core::{
    ids::PersonId,
    people::{Person, PersonSource, PersonSourceId},
};
use serde::{Deserialize, Serialize};

use super::contacts::PeopleQuery;
use crate::ApiError;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PeopleCursor {
    pub(super) generation: u64,
    pub(super) signature: u64,
    pub(super) display: String,
    pub(super) person: PersonId,
}

pub(super) fn person_matches(
    person: &Person,
    sources: &BTreeMap<PersonSourceId, PersonSource>,
    query: &PeopleQuery,
    needle: &str,
) -> bool {
    let group_members = query.group.as_ref().map(|group| {
        sources
            .values()
            .filter(|source| {
                source.card.id == *group
                    && query
                        .account
                        .as_ref()
                        .is_none_or(|account| source.id.account == *account)
            })
            .flat_map(|source| source.card.members.values())
            .map(|member| member.value.uid.clone())
            .collect::<BTreeSet<_>>()
    });
    let selected: Vec<&PersonSource> = person
        .sources
        .iter()
        .filter_map(|id| sources.get(id))
        .collect();
    let source_match = selected.iter().any(|source| {
        query
            .account
            .as_ref()
            .is_none_or(|account| source.id.account == *account)
            && query
                .address_book
                .as_ref()
                .is_none_or(|book| source.card.address_books.contains(book))
            && query
                .source_class
                .is_none_or(|class| source.source_class == class)
            && query
                .kind
                .as_ref()
                .is_none_or(|kind| source.card.kind == *kind)
            && query
                .writable
                .is_none_or(|writable| source.writable == writable)
            && group_members.as_ref().is_none_or(|members| {
                source
                    .card
                    .uid
                    .as_ref()
                    .is_some_and(|uid| members.contains(uid))
                    || members.contains(source.card.id.as_str())
            })
    });
    source_match
        && (needle.is_empty()
            || searchable_values(person).any(|value| value.to_lowercase().contains(needle)))
}

fn searchable_values(person: &Person) -> impl Iterator<Item = &str> {
    std::iter::once(person.display_name.as_str())
        .chain(person.names.iter().map(|value| value.value.as_str()))
        .chain(person.emails.iter().map(|value| value.value.as_str()))
        .chain(person.phones.iter().map(|value| value.value.as_str()))
        .chain(
            person
                .organizations
                .iter()
                .map(|value| value.value.as_str()),
        )
        .chain(person.titles.iter().map(|value| value.value.as_str()))
}

pub(super) fn person_key(person: &Person) -> (String, PersonId) {
    (person.display_name.to_lowercase(), person.id)
}

pub(super) fn query_signature(query: &PeopleQuery) -> u64 {
    let mut copy = query.clone();
    copy.cursor = None;
    let bytes = serde_json::to_vec(&copy).unwrap_or_default();
    bytes.into_iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        hash.wrapping_mul(0x0100_0000_01b3) ^ u64::from(byte)
    })
}

pub(super) fn encode_cursor(cursor: &PeopleCursor) -> Result<String, ApiError> {
    let bytes =
        serde_json::to_vec(cursor).map_err(|error| ApiError::InvalidInput(error.to_string()))?;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    Ok(output)
}

pub(super) fn decode_cursor(value: &str) -> Result<PeopleCursor, ApiError> {
    if !value.len().is_multiple_of(2) {
        return Err(ApiError::InvalidInput("malformed people cursor".into()));
    }
    // Chunk the *bytes*, never `&value[i..i + 2]`: this string comes from a host and a
    // multi-byte character would put those fixed offsets inside a UTF-8 sequence and
    // panic. `from_str_radix` then rejects any non-ASCII-hex pair as invalid input.
    let bytes = value
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or(())
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|()| ApiError::InvalidInput("malformed people cursor".into()))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::InvalidInput("malformed people cursor".into()))
}

#[cfg(test)]
mod tests {
    use super::{PeopleCursor, decode_cursor, encode_cursor};
    use crate::ApiError;

    /// The cursor is host-supplied, so every malformed shape must come back as
    /// `InvalidInput` — never a panic. A multi-byte character is the sharp case: byte
    /// slicing at fixed 2-byte offsets lands mid-sequence and aborts the process.
    #[test]
    fn a_malformed_cursor_is_rejected_rather_than_panicking() {
        for bogus in [
            "\u{20ac}a",  // even byte length, but offset 2 is not a char boundary
            "\u{20ac}",   // odd byte length
            "zz",         // ASCII, but not hex
            "ff\u{2603}", // hex prefix then a multi-byte tail
            "0",          // odd length
            "ffff",       // valid hex, but not valid JSON
        ] {
            assert!(
                matches!(decode_cursor(bogus), Err(ApiError::InvalidInput(_))),
                "expected InvalidInput for {bogus:?}"
            );
        }
    }

    #[test]
    fn a_well_formed_cursor_round_trips() {
        let cursor = PeopleCursor {
            generation: 7,
            signature: 0xdead_beef,
            display: "ada lovelace".to_owned(),
            person: engine_core::ids::PersonId::new(42).expect("person id"),
        };
        let encoded = encode_cursor(&cursor).expect("encode");
        let decoded = decode_cursor(&encoded).expect("decode");
        assert_eq!(encode_cursor(&decoded).expect("re-encode"), encoded);
        assert_eq!(decoded.generation, 7);
        assert_eq!(decoded.display, "ada lovelace");
    }
}
