use std::collections::BTreeSet;

use super::*;
use crate::segment;

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn a_require_names_a_string_or_a_list() {
    let segments =
        segment("require \"fileinto\";\r\nREQUIRE [ \"copy\" , \"body\" ] ;\r\nkeep;\r\n").unwrap();
    assert_eq!(capabilities(&segments[0]).unwrap(), vec!["fileinto"]);
    assert_eq!(capabilities(&segments[1]).unwrap(), vec!["copy", "body"]);
    assert_eq!(
        required(&segments).unwrap(),
        names(&["body", "copy", "fileinto"])
    );
}

#[test]
fn a_malformed_require_is_refused_at_its_segment() {
    for script in [
        "require;",
        "require [];",
        "require [\"a\",];",
        "require [\"a\" \"b\"];",
        "require keep;",
        "require \"a\" \"b\";",
        "require [\"a\"",
        "keep;",
    ] {
        let Ok(segments) = segment(script) else {
            continue;
        };
        let err = capabilities(&segments[0]).unwrap_err();
        assert_eq!(err.kind(), SieveErrorKind::MalformedRequire, "{script}");
    }
    let trailing = segment("# no command").unwrap();
    assert_eq!(
        capabilities(&trailing[0]).unwrap_err().kind(),
        SieveErrorKind::MalformedRequire
    );
    let segments = segment("keep;\r\nrequire 1;").unwrap();
    assert_eq!(required(&segments).unwrap_err().offset(), 7);
}

#[test]
fn one_require_is_written_as_roundcube_writes_it() {
    assert_eq!(
        require_command(&names(&["imap4flags", "body", "copy"]))
            .unwrap()
            .as_deref(),
        Some("require [\"body\",\"copy\",\"imap4flags\"];\r\n")
    );
    assert_eq!(require_command(&BTreeSet::new()), Ok(None));
}

#[test]
fn a_merge_keeps_the_script_s_requires_when_they_name_enough() {
    let segments = segment("require [\"fileinto\",\"regex\"];\r\nrequire \"copy\";\r\n").unwrap();
    assert_eq!(
        merged(&segments, &names(&["copy", "fileinto"])).unwrap(),
        None
    );
    assert_eq!(
        merged(&segments, &names(&["body"])).unwrap().as_deref(),
        Some("require [\"body\",\"copy\",\"fileinto\",\"regex\"];\r\n"),
        "never drops a name an unread segment may use"
    );
    assert_eq!(
        merged(&[], &names(&["fileinto"])).unwrap().as_deref(),
        Some("require [\"fileinto\"];\r\n")
    );
}

#[test]
fn a_require_that_cannot_be_written_is_an_error_never_nothing_to_add() {
    let unwritable = SieveErrorKind::UnwritableValue;
    assert_eq!(
        require_command(&names(&["a\nb"])).unwrap_err().kind(),
        unwritable
    );
    let segments = segment("require \"fileinto\";\r\nkeep;\r\n").unwrap();
    assert_eq!(
        merged(&segments, &names(&["a\nb"])).unwrap_err().kind(),
        unwritable
    );
    // A name read from a `text:` block keeps its CRLF, which a quoted string can hold.
    let segments = segment("require text:\r\nfoo\r\nbar\r\n.\r\n;\r\nkeep;\r\n").unwrap();
    assert_eq!(
        merged(&segments, &names(&["fileinto"])).unwrap().as_deref(),
        Some("require [\"fileinto\",\"foo\r\nbar\r\n\"];\r\n")
    );
}
