//! Scripts Roundcube's managesieve plugin wrote (`fixtures/roundcube/README.md`) split into one
//! segment per rule, each carrying its `# rule:[name]` line, and joined back byte for byte.

use engine_sieve::{Segment, SegmentKind, require, segment};

const FIXTURES: [(&str, &str, usize); 5] = [
    (
        "one_rule",
        include_str!("fixtures/roundcube/one_rule.sieve"),
        1,
    ),
    (
        "disabled_rule",
        include_str!("fixtures/roundcube/disabled_rule.sieve"),
        2,
    ),
    (
        "many_rules",
        include_str!("fixtures/roundcube/many_rules.sieve"),
        6,
    ),
    (
        "quoting",
        include_str!("fixtures/roundcube/quoting.sieve"),
        3,
    ),
    (
        "vacation_rule",
        include_str!("fixtures/roundcube/vacation_rule.sieve"),
        2,
    ),
];

#[test]
fn each_rule_is_one_segment_and_the_segments_are_the_script() {
    for (name, script, rules) in FIXTURES {
        assert!(script.contains("\r\n"), "{name}: the fixture kept its CRLF");
        let segments = segment(script).unwrap_or_else(|err| panic!("{name}: {err}"));
        let texts: String = segments.iter().map(Segment::text).collect();
        assert_eq!(texts, script, "{name}: joined back byte for byte");
        assert_eq!(segments[0].kind(), SegmentKind::Require, "{name}");
        assert_eq!(
            segments.len(),
            rules + 1,
            "{name}: a require and one segment per rule"
        );
        for rule in &segments[1..] {
            assert_eq!(rule.kind(), SegmentKind::Command, "{name}");
            assert_eq!(rule.identifier(), Some("if"), "{name}");
            assert!(rule.command().ends_with('}'), "{name}");
            assert_eq!(rule.trailer(), "\r\n", "{name}");
            assert!(
                rule.leading().is_empty() || rule.leading().starts_with("# rule:["),
                "{name}: a rule's name travels with it"
            );
        }
    }
}

#[test]
fn the_require_names_what_the_plugin_required() {
    let segments = segment(FIXTURES[2].1).unwrap();
    let names: Vec<String> = require::required(&segments).unwrap().into_iter().collect();
    assert_eq!(names, ["body", "copy", "fileinto", "imap4flags", "regex"]);
    // The plugin's layout is the one `require_command` writes.
    assert_eq!(
        require::require_command(&names.into_iter().collect()).as_deref(),
        Some(segments[0].text())
    );
}

#[test]
fn an_unnamed_rule_has_no_leading_comment() {
    let segments = segment(FIXTURES[2].1).unwrap();
    assert_eq!(segments[4].leading(), "");
    assert!(segments[4].command().starts_with("if body :text"));
}
