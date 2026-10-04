//! The conformance tables stay honest: every row that applies is covered by tests
//! that exist, or is openly planned.

use std::{collections::HashSet, fs, path::Path};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    id: String,
    section: String,
    keyword: String,
    excerpt: String,
    scope: Scope,
    area: Option<String>,
    note: Option<String>,
    status: Option<Status>,
    tests: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Scope {
    Library,
    Policy,
    NotApplicable,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Status {
    Covered,
    Planned,
}

const AREAS: &[&str] = &[
    "parsing",
    "armor",
    "recognise",
    "certificates",
    "verify",
    "decrypt",
    "compose",
    "generate",
    "secret-keys",
    "cleartext",
];

fn table(json: &str) -> Vec<Row> {
    serde_json::from_str(json).expect("a conformance table parses")
}

fn tables() -> Vec<(&'static str, Vec<Row>)> {
    vec![
        ("rfc9580", table(include_str!("conformance/rfc9580.json"))),
        ("rfc3156", table(include_str!("conformance/rfc3156.json"))),
    ]
}

/// Every `fn name(` under `crates/`.
fn test_functions() -> HashSet<String> {
    fn visit(dir: &Path, found: &mut HashSet<String>) {
        for entry in fs::read_dir(dir).expect("readable source tree") {
            let path = entry.expect("readable entry").path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "target") {
                    visit(&path, found);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source = fs::read_to_string(&path).expect("utf-8 source");
                for piece in source.split("fn ").skip(1) {
                    let name: String = piece
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if piece[name.len()..].starts_with('(') {
                        found.insert(name);
                    }
                }
            }
        }
    }
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crates directory");
    let mut found = HashSet::new();
    visit(crates, &mut found);
    found
}

#[test]
fn rfc_9580_keeps_every_extracted_statement() {
    let (_, rows) = &tables()[0];
    assert_eq!(rows.len(), 221);
}

#[test]
fn every_row_is_well_formed() {
    let mut ids = HashSet::new();
    for (name, rows) in tables() {
        for row in rows {
            assert!(ids.insert(row.id.clone()), "{} repeats", row.id);
            assert!(row.id.starts_with(name), "{} is not in {name}", row.id);
            assert!(!row.section.is_empty(), "{}", row.id);
            assert!(row.excerpt.contains(&row.keyword), "{}", row.id);
            if let Some(area) = &row.area {
                assert!(AREAS.contains(&area.as_str()), "{}: area {area}", row.id);
            }
            match row.scope {
                Scope::NotApplicable => {
                    assert!(
                        row.note.as_deref().is_some_and(|note| !note.is_empty()),
                        "{} is not applicable without saying why",
                        row.id,
                    );
                    assert!(row.status.is_none() && row.tests.is_none(), "{}", row.id);
                }
                Scope::Library | Scope::Policy => {
                    assert!(row.area.is_some(), "{} has no area", row.id);
                    let tests = row.tests.as_deref().unwrap_or_default();
                    match row.status {
                        Some(Status::Covered) => {
                            assert!(!tests.is_empty(), "{} is covered by nothing", row.id);
                        }
                        Some(Status::Planned) => {
                            assert!(tests.is_empty(), "{} is planned but names tests", row.id);
                        }
                        None => panic!("{} applies and has no status", row.id),
                    }
                }
            }
        }
    }
}

#[test]
fn every_named_test_exists() {
    let functions = test_functions();
    let mut missing = Vec::new();
    for (_, rows) in tables() {
        for row in rows {
            for test in row.tests.unwrap_or_default() {
                if !functions.contains(&test) {
                    missing.push(format!("{}: {test}", row.id));
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "named tests that do not exist: {missing:#?}"
    );
}
