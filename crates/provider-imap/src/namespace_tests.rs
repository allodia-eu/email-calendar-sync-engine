//! `NAMESPACE` parsing and mailbox attribution.
//!
//! The two responses asserted here are the ones Stalwart really answered: alice, who has
//! been granted access to two other stores, and bob, who has not.

use super::*;

fn line(text: &str) -> Vec<Vec<u8>> {
    vec![text.as_bytes().to_vec()]
}

/// Alice's real answer. Note where Stalwart puts the shared stores: the **second**
/// position, which RFC 2342 §5 calls Other Users' — despite naming the prefix
/// "Shared Folders" — with the third (Shared) position `NIL`.
fn alice() -> Namespaces {
    parse_namespace(&line(
        r#"NAMESPACE (("" "/")) (("Shared Folders" "/")) NIL"#,
    ))
}

/// Bob's real answer: a personal namespace and nothing else.
fn bob() -> Namespaces {
    parse_namespace(&line(r#"NAMESPACE (("" "/")) NIL NIL"#))
}

#[test]
fn the_three_positions_are_read_in_order() {
    let ns = alice();
    assert_eq!(ns.personal.len(), 1);
    assert_eq!(ns.personal[0].prefix, "");
    assert_eq!(ns.personal[0].delimiter.as_deref(), Some("/"));
    // Stalwart uses Other Users', not Shared — which is exactly why the engine treats the
    // two alike instead of looking in one of them.
    assert_eq!(ns.other_users.len(), 1);
    assert_eq!(ns.other_users[0].prefix, "Shared Folders");
    assert!(ns.shared.is_empty());
    assert_eq!(ns.foreign().count(), 1);
}

#[test]
fn an_account_with_no_shares_has_no_foreign_namespace() {
    let ns = bob();
    assert_eq!(ns.personal.len(), 1);
    assert_eq!(ns.foreign().count(), 0);
    // So every mailbox is his own, and the store resolves to the personal one.
    assert!(ns.is_own("INBOX"));
    assert_eq!(
        MailStore::resolve(&ns, "INBOX").list_pattern(),
        "*",
        "with nothing foreign to exclude, the pattern is the whole tree"
    );
}

#[test]
fn a_server_without_the_extension_owns_everything() {
    // No `NAMESPACE` line at all, or an unparseable one: the engine must not conclude that
    // mailboxes are foreign, or it would refuse to sync the credential's own mail.
    for lines in [
        Vec::new(),
        line("OK NAMESPACE completed"),
        line("NAMESPACE"),
        line("NAMESPACE (unbalanced"),
    ] {
        let ns = parse_namespace(&lines);
        assert_eq!(ns.foreign().count(), 0);
        assert!(ns.is_own("Shared Folders/support@test.local/INBOX"));
    }
}

#[test]
fn a_prefix_match_must_end_at_the_delimiter() {
    let ns = alice();
    // The share, and the namespace container itself.
    assert!(!ns.is_own("Shared Folders/support@test.local/INBOX"));
    assert!(!ns.is_own("Shared Folders"));
    // A folder of the *user's own* that merely starts with the same characters is hers. A
    // bare `starts_with` would misattribute it and quietly stop syncing it.
    assert!(ns.is_own("Shared Foldersomething"));
    assert!(ns.is_own("Archive"));
    assert!(ns.is_own("INBOX"));
}

#[test]
fn a_trailing_delimiter_on_the_prefix_is_normalized_away() {
    // Servers differ on whether the advertised prefix carries its delimiter. Both spellings
    // must attribute the same mailbox to the same store.
    let with = parse_namespace(&line(r#"NAMESPACE (("" "/")) (("Other Users/" "/")) NIL"#));
    assert_eq!(with.other_users[0].prefix, "Other Users");
    assert!(!with.is_own("Other Users/bob/INBOX"));
    assert_eq!(
        with.other_users[0].join(&["bob", "INBOX"]),
        "Other Users/bob/INBOX"
    );
}

#[test]
fn a_bound_shared_mailbox_resolves_to_its_owners_store() {
    let ns = alice();
    let store = MailStore::resolve(&ns, "Shared Folders/support@test.local/INBOX");
    // The pattern narrows the LIST to that principal's tree — the container included.
    assert_eq!(store.list_pattern(), "Shared Folders/support@test.local*");
    assert!(store.contains(&ns, "Shared Folders/support@test.local"));
    assert!(store.contains(&ns, "Shared Folders/support@test.local/Sent Items"));
    // And nothing outside it: not the credential's own folders, and not a *different*
    // principal's share. Getting this wrong is what would put two principals' mail in one
    // engine account.
    assert!(!store.contains(&ns, "INBOX"));
    assert!(!store.contains(&ns, "Shared Folders/bob@test.local/INBOX"));
}

#[test]
fn binding_to_the_namespace_container_itself_lists_every_share_under_it() {
    // A host binding to `Shared Folders` — the `\NoSelect` container — names no owner.
    // Appending an empty component would leave a trailing delimiter on the root, after
    // which *nothing* matches and the folder list comes back silently empty, which is the
    // worst of the available answers.
    let ns = alice();
    let store = MailStore::resolve(&ns, "Shared Folders");
    assert_eq!(store.list_pattern(), "Shared Folders*");
    assert!(store.contains(&ns, "Shared Folders/support@test.local/INBOX"));
    assert!(store.contains(&ns, "Shared Folders/bob@test.local/INBOX"));
    // Still not the credential's own folders.
    assert!(!store.contains(&ns, "INBOX"));
}

#[test]
fn the_personal_store_excludes_every_foreign_namespace() {
    let ns = alice();
    let store = MailStore::resolve(&ns, "INBOX");
    // No pattern can express "everything except these prefixes", so the personal store
    // lists the whole tree and filters.
    assert_eq!(store.list_pattern(), "*");
    assert!(store.contains(&ns, "INBOX") && store.contains(&ns, "Archive"));
    assert!(!store.contains(&ns, "Shared Folders/support@test.local/INBOX"));
    assert!(!store.contains(&ns, "Shared Folders"));
}

#[test]
fn the_most_specific_foreign_prefix_wins() {
    // A server advertising nested namespaces must attribute a mailbox to the more specific
    // one, or the store root would be wrong (and the LIST pattern too wide).
    let ns = parse_namespace(&line(
        r#"NAMESPACE (("" "/")) (("Shared" "/")("Shared/Public" "/")) NIL"#,
    ));
    let store = MailStore::resolve(&ns, "Shared/Public/team/INBOX");
    assert_eq!(store.list_pattern(), "Shared/Public/team*");
}

#[test]
fn a_flat_namespace_has_no_delimiter() {
    // RFC 2342 allows `NIL` for the delimiter (a flat mailbox space), so nothing may assume
    // one exists.
    let ns = parse_namespace(&line(r#"NAMESPACE (("" NIL)) NIL NIL"#));
    assert_eq!(ns.personal.len(), 1);
    assert!(ns.personal[0].delimiter.is_none());
    assert_eq!(ns.personal[0].join(&["INBOX"]), "INBOX");
    assert!(ns.is_own("INBOX"));
}
