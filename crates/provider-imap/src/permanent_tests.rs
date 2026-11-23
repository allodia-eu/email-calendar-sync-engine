use std::collections::BTreeSet;

use engine_core::mail::{Keyword, SystemKeyword};

use super::PermanentFlags;

fn keyword(name: &str) -> Keyword {
    Keyword::new(name).unwrap()
}

fn seen() -> Keyword {
    Keyword::system(SystemKeyword::Seen)
}

fn flagged() -> Keyword {
    Keyword::system(SystemKeyword::Flagged)
}

fn select(lines: &[&str]) -> PermanentFlags {
    let lines: Vec<Vec<u8>> = lines.iter().map(|line| line.as_bytes().to_vec()).collect();
    PermanentFlags::from_select(&lines)
}

fn asked() -> BTreeSet<Keyword> {
    BTreeSet::from([
        seen(),
        flagged(),
        keyword("$forwarded"),
        keyword("project-x"),
    ])
}

#[test]
fn a_folder_that_allows_new_keywords_keeps_every_keyword_and_the_system_flags_it_lists() {
    let folder = select(&[
        "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)",
        "* OK [PERMANENTFLAGS (\\Deleted \\Seen \\*)] Limited",
    ]);

    assert_eq!(
        folder.keepable(&asked()),
        BTreeSet::from([seen(), keyword("$forwarded"), keyword("project-x")]),
        "`\\*` covers keywords, never a system flag the list leaves out (`\\Flagged`)"
    );
}

#[test]
fn a_folder_that_names_its_keywords_keeps_those_and_no_other() {
    let folder = select(&["* OK [PERMANENTFLAGS (\\Seen \\Flagged $Forwarded)] Limited"]);

    assert_eq!(
        folder.keepable(&asked()),
        BTreeSet::from([seen(), flagged(), keyword("$forwarded")])
    );
}

#[test]
fn an_empty_permanentflags_keeps_nothing() {
    let folder = select(&[
        "* FLAGS (\\Seen \\Flagged project-x)",
        "* OK [PERMANENTFLAGS ()] No permanent flags permitted",
    ]);

    assert_eq!(folder.keepable(&asked()), BTreeSet::new());
}

#[test]
fn without_permanentflags_the_flags_the_folder_names_are_kept_and_no_new_keyword() {
    // RFC 9051 §6.3.2: with no `PERMANENTFLAGS`, assume every flag can be changed permanently.
    let folder = select(&["* FLAGS (\\Seen \\Flagged project-x)", "* 3 EXISTS"]);

    assert_eq!(
        folder.keepable(&asked()),
        BTreeSet::from([seen(), flagged(), keyword("project-x")])
    );
}

#[test]
fn the_response_is_read_without_regard_to_case() {
    let folder = select(&["* ok [permanentflags (\\seen \\*)] fine"]);

    assert!(folder.keeps(&seen()));
    assert!(folder.keeps(&keyword("project-x")));
    assert!(!folder.keeps(&flagged()));
}

#[test]
fn a_folder_that_could_not_be_opened_keeps_nothing() {
    assert_eq!(PermanentFlags::none().keepable(&asked()), BTreeSet::new());
}

/// What each harness server really answers (`tests/fixtures/README.md`): every system flag
/// listed, and `\*`, so every keyword asked for is kept.
#[test]
fn every_harness_server_keeps_the_system_flags_and_new_keywords() {
    for (server, answer) in [
        (
            "stalwart",
            include_str!("../tests/fixtures/select_inbox_stalwart.txt"),
        ),
        (
            "dovecot rev1",
            include_str!("../tests/fixtures/select_inbox_dovecot_rev1.txt"),
        ),
        (
            "dovecot rev2",
            include_str!("../tests/fixtures/select_inbox_dovecot_rev2.txt"),
        ),
    ] {
        let lines: Vec<Vec<u8>> = answer
            .lines()
            .map(|line| line.as_bytes().to_vec())
            .collect();
        let folder = PermanentFlags::from_select(&lines);
        assert_eq!(folder.keepable(&asked()), asked(), "{server}");
        assert!(
            folder.keeps(&Keyword::system(SystemKeyword::Draft)),
            "{server}"
        );
    }
}
