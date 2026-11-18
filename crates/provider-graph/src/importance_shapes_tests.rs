//! The engine's importance rule against Exchange's own answers, shape by shape
//! (`tests/fixtures/mail/importance_header_shapes.json`): a message reads the same importance on a
//! Graph account, where Exchange decides, as on every other transport, where the engine does.

use engine_core::mail::{Importance, ImportanceHeaders};
use serde_json::Value;

const SHAPES: &str = include_str!("../tests/fixtures/mail/importance_header_shapes.json");

/// The shapes where the engine reads RFC 5322 and Exchange does not (`docs/spec-deviations.md`,
/// MICROSOFT-1): a comment beside the token, and whitespace after it.
const DEVIATIONS: [&str; 3] = [
    "importance_comment",
    "importance_spaces",
    "importance_trailing_spaces",
];

/// The header lines of one shape as (name, unfolded value) pairs, in order.
fn fields(lines: &[Value]) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in lines.iter().filter_map(Value::as_str) {
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = fields.last_mut() {
                value.push_str(line);
            }
            continue;
        }
        let (name, value) = line.split_once(':').expect("a header line");
        fields.push((name.to_owned(), value.to_owned()));
    }
    fields
}

#[test]
fn the_rule_reads_every_shape_as_exchange_does_but_where_exchange_departs_from_the_rfc() {
    let doc: Value = serde_json::from_str(SHAPES).unwrap();
    let shapes = doc["shapes"].as_array().unwrap();
    assert!(shapes.len() >= 30, "the fixture holds every observed shape");
    for shape in shapes {
        let name = shape["name"].as_str().unwrap();
        let fields = fields(shape["headers"].as_array().unwrap());
        let ours = Importance::from_headers(ImportanceHeaders::from_lookup(|header| {
            fields
                .iter()
                .rev()
                .find(|(field, _)| field.eq_ignore_ascii_case(header))
                .map(|(_, value)| value.as_str())
        }));
        let exchange = Importance::parse(shape["importance"].as_str().unwrap()).unwrap();
        if DEVIATIONS.contains(&name) {
            assert_ne!(
                ours, exchange,
                "{name}: no longer a deviation, update MICROSOFT-1"
            );
        } else {
            assert_eq!(ours, exchange, "{name}");
        }
    }
}
