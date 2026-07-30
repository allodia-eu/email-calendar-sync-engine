//! `MYRIGHTS` parsing and the RFC 4314 letter → [`MailboxAccess`] mapping.
//!
//! The two rights strings asserted here are the ones Stalwart really answered for a mailbox
//! shared read-only (`rl`) and one the caller owns (`rliteswkxpa`), captured while building
//! the harness fixture.

use super::*;

fn line(text: &str) -> Vec<Vec<u8>> {
    vec![text.as_bytes().to_vec()]
}

fn rights(text: &str) -> MailboxRights {
    parse_myrights(&line(text)).expect("a MYRIGHTS line")
}

#[test]
fn a_read_only_grant_reads_and_nothing_else() {
    // `lr` is what Stalwart reports for a mailbox shared with `SETACL … lr`.
    let access = rights(r#"MYRIGHTS "Shared Folders/bob@test.local/INBOX" rl"#).access();
    assert_eq!(access, MailboxAccess::reader());
}

#[test]
fn a_full_grant_is_the_owners() {
    // `rliteswkxpa` is what Stalwart reports for a mailbox the caller owns, and for the
    // group mailbox a member holds full rights on.
    let parsed = rights(r#"MYRIGHTS "INBOX" rliteswkxpa"#);
    assert_eq!(parsed.mailbox, "INBOX");
    assert_eq!(parsed.access(), MailboxAccess::owner());
}

#[test]
fn each_letter_maps_to_the_right_it_names() {
    // One letter at a time, so a mapping that is silently wired to the wrong field cannot
    // hide behind a full grant.
    let only = |letters: &str| rights(&format!(r#"MYRIGHTS "m" {letters}"#)).access();

    // Reading needs `l` (visible) *and* `r` (readable); neither alone is a readable mailbox.
    assert!(!only("l").may_read_items);
    assert!(!only("r").may_read_items);
    assert!(only("lr").may_read_items);

    // Removing a message needs `t` (mark `\Deleted`) *and* `e` (expunge) — RFC 6851 `MOVE`
    // requires the same pair — so either alone leaves the message in place.
    assert!(!only("t").may_remove_items);
    assert!(!only("e").may_remove_items);
    assert!(only("te").may_remove_items);

    assert!(only("i").may_add_items);
    assert!(only("s").may_set_seen);
    assert!(only("w").may_set_keywords);
    assert!(only("k").may_create_child);
    assert!(only("p").may_submit);
    assert!(only("a").may_share);
    // `x` grants deleting the mailbox and renaming it away (RFC 4314 §4). The `k` a rename
    // also needs belongs to the *new parent*, so it is not a right of this mailbox.
    let x = only("x");
    assert!(x.may_delete && x.may_rename);

    // And the fields are genuinely independent: `s` (per-user seen state) without `w` is
    // the shape a shared mailbox is commonly granted in.
    let seen_only = only("lrs");
    assert!(seen_only.may_read_items && seen_only.may_set_seen && !seen_only.may_set_keywords);
}

#[test]
fn rights_letters_are_case_sensitive() {
    // RFC 4314 §2.1.1 reserves uppercase letters for server-specific extensions, so an `R`
    // must not be read as the standard `r`.
    let access = rights(r#"MYRIGHTS "m" LR"#).access();
    assert!(!access.may_read_items);
}

#[test]
fn a_response_without_myrights_is_unknown_rather_than_empty() {
    // A server without the ACL extension, or one that answered something else entirely.
    // The caller reads `None` as "cannot ask" and keeps `owner`, rather than as "no rights"
    // — which would hide mail it can plainly see.
    assert!(parse_myrights(&line("OK MYRIGHTS completed")).is_none());
    assert!(parse_myrights(&[]).is_none());
    // Truncated and malformed lines are `None`, never a panic (mail servers are hostile
    // input like mail itself).
    assert!(parse_myrights(&line("MYRIGHTS")).is_none());
    assert!(parse_myrights(&line(r#"MYRIGHTS "m""#)).is_none());
    assert!(parse_myrights(&line("MYRIGHTS (unbalanced")).is_none());
}

#[test]
fn the_keyword_match_is_case_insensitive() {
    // IMAP response names are case-insensitive (RFC 9051 §1.2), so a server shouting is
    // still understood.
    assert_eq!(
        rights(r#"myrights "INBOX" lr"#).access(),
        MailboxAccess::reader()
    );
}
