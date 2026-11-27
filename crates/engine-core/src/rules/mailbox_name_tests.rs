//! How a rule names an account's folder, and which folder a read rule names, on the shapes of
//! vocabulary each transport builds (each adapter tests its own round trip through its writer and
//! reader).

use super::*;
use crate::{ids::MailboxId, rules::RuleFeature};

fn folder(id: &str, role: Option<MailboxRole>) -> Mailbox {
    let mut folder = Mailbox::new(MailboxId::try_from(id).unwrap(), id);
    folder.role = role;
    folder
}

/// A Sieve account: `fileinto` and `copy`, with `special-use` and `mailboxid` where listed, and
/// folder ids that are paths where `paths` (IMAP).
fn sieve(special_use: bool, mailboxid: bool, paths: bool) -> RuleVocabulary {
    let mut vocabulary =
        RuleVocabulary::new().with_exact([RuleFeature::FileInto, RuleFeature::FileIntoCopy]);
    if special_use {
        vocabulary = vocabulary.with_exact([RuleFeature::FileIntoRole]);
    }
    if mailboxid {
        vocabulary = vocabulary.with_exact([RuleFeature::FileIntoId]);
    }
    if paths {
        vocabulary = vocabulary.with_folder_ids_as_paths();
    }
    vocabulary
}

/// Graph's shape: by id and by role, into real folders only.
fn graph() -> RuleVocabulary {
    RuleVocabulary::new()
        .with_exact([
            RuleFeature::FileIntoId,
            RuleFeature::FileIntoCopy,
            RuleFeature::FileIntoRole,
        ])
        .with_limit(RuleLimit::RealFoldersOnly)
}

/// Gmail's shape: Graph's, with roles only its own Trash, Spam and archive.
fn gmail() -> RuleVocabulary {
    graph().with_limit(RuleLimit::TrashJunkArchiveOnly)
}

/// How a rule names `folder` on an account that lists it alone.
fn named(folder: &Mailbox, vocabulary: &RuleVocabulary) -> Option<RuleMailbox> {
    RuleMailbox::for_folder(folder, core::slice::from_ref(folder), vocabulary)
}

fn path(path: &str) -> RuleMailbox {
    RuleMailbox::Path(MailboxPath::new(path).unwrap())
}

#[test]
fn a_folder_with_a_role_is_named_by_it_where_the_account_files_by_role() {
    let trash = folder("b", Some(MailboxRole::Trash));
    let inbox = folder("a", Some(MailboxRole::Inbox));
    let jmap = sieve(true, true, false);
    assert_eq!(
        named(&trash, &jmap),
        Some(RuleMailbox::Role(MailboxRole::Trash))
    );
    assert_eq!(
        named(&inbox, &jmap),
        Some(RuleMailbox::Role(MailboxRole::Inbox))
    );
    // The inbox needs no role extension: every Sieve server has it by name.
    assert_eq!(
        named(&inbox, &sieve(false, false, false)),
        Some(RuleMailbox::Role(MailboxRole::Inbox))
    );
    // A folder without a role, by id.
    let projects = folder("i", None);
    assert_eq!(
        named(&projects, &jmap),
        Some(RuleMailbox::Id(projects.id.clone()))
    );
}

#[test]
fn an_imap_folder_is_named_by_its_path_where_no_role_or_id_names_it() {
    let imap = sieve(false, false, true);
    let trash = folder("Trash", Some(MailboxRole::Trash));
    assert_eq!(
        named(&trash, &imap),
        Some(path("Trash")),
        "no special-use: by path"
    );
    assert_eq!(
        named(&folder("Archive/2024", None), &imap),
        Some(path("Archive/2024"))
    );
    assert_eq!(named(&folder("Reçus", None), &imap), Some(path("Reçus")));
    assert_eq!(
        named(&trash, &sieve(true, false, true)),
        Some(RuleMailbox::Role(MailboxRole::Trash)),
        "special-use: by role"
    );
    let level = Mailbox {
        selectable: false,
        ..folder("Archive", None)
    };
    assert_eq!(named(&level, &imap), None, "a level holding no mail");
}

#[test]
fn no_path_is_made_up_where_the_account_s_ids_are_not_paths() {
    // A JMAP server listing neither `mailboxid` nor `special-use`: only the inbox has a name.
    let jmap = sieve(false, false, false);
    assert_eq!(named(&folder("i", None), &jmap), None);
    assert_eq!(named(&folder("b", Some(MailboxRole::Trash)), &jmap), None);
}

#[test]
fn gmail_files_into_its_own_trash_and_spam_by_role_and_a_user_label_by_id() {
    let gmail = gmail();
    assert_eq!(
        named(&folder("TRASH", Some(MailboxRole::Trash)), &gmail),
        Some(RuleMailbox::Role(MailboxRole::Trash))
    );
    assert_eq!(
        named(&folder("SPAM", Some(MailboxRole::Junk)), &gmail),
        Some(RuleMailbox::Role(MailboxRole::Junk))
    );
    assert_eq!(
        named(&folder("Label_1", None), &gmail),
        Some(RuleMailbox::Id(MailboxId::try_from("Label_1").unwrap()))
    );
    // Its other system labels are roles it adds no rule to, never ids to file into.
    for (id, role) in [
        ("INBOX", MailboxRole::Inbox),
        ("SENT", MailboxRole::Sent),
        ("DRAFT", MailboxRole::Drafts),
        ("IMPORTANT", MailboxRole::Important),
        ("ALL_MAIL", MailboxRole::All),
    ] {
        assert_eq!(named(&folder(id, Some(role)), &gmail), None, "{id}");
    }
}

#[test]
fn graph_files_by_role_where_it_can_and_by_id_where_it_cannot() {
    let graph = graph();
    assert_eq!(
        named(&folder("junk-id", Some(MailboxRole::Junk)), &graph),
        Some(RuleMailbox::Role(MailboxRole::Junk))
    );
    // The inbox is named by role only where a rule files by name; Graph files it by its id.
    assert_eq!(
        named(&folder("inbox-id", Some(MailboxRole::Inbox)), &graph),
        Some(RuleMailbox::Id(MailboxId::try_from("inbox-id").unwrap()))
    );
    assert_eq!(
        named(
            &folder("records-id", Some(MailboxRole::ServerRecords)),
            &graph
        ),
        Some(RuleMailbox::Id(MailboxId::try_from("records-id").unwrap())),
        "a role no rule names: by id"
    );
    assert_eq!(
        named(&folder("search-id", Some(MailboxRole::Flagged)), &graph),
        None,
        "a virtual collection is no folder"
    );
}

#[test]
fn a_virtual_collection_is_named_by_nothing_whatever_the_server_lets_a_rule_name() {
    for role in [
        MailboxRole::All,
        MailboxRole::Flagged,
        MailboxRole::Important,
    ] {
        let view = folder("Virtual", Some(role));
        for vocabulary in [sieve(true, true, false), sieve(false, false, true), graph()] {
            assert_eq!(
                named(&view, &vocabulary),
                None,
                "{view:?} in {vocabulary:?}"
            );
        }
    }
}

#[test]
fn a_role_several_folders_share_names_none_of_them() {
    let folders = [
        folder("Trash", Some(MailboxRole::Trash)),
        folder("Deleted", Some(MailboxRole::Trash)),
    ];
    let name = |at: usize, vocabulary: &RuleVocabulary| {
        RuleMailbox::for_folder(&folders[at], &folders, vocabulary)
    };
    // By id, or by path, where the account has either: the server would choose by role.
    assert_eq!(
        name(1, &sieve(true, true, false)),
        Some(RuleMailbox::Id(MailboxId::try_from("Deleted").unwrap()))
    );
    assert_eq!(name(1, &sieve(true, false, true)), Some(path("Deleted")));
    assert_eq!(
        name(0, &sieve(true, false, false)),
        None,
        "nothing else names it"
    );
    assert_eq!(name(0, &gmail()), None);
}

#[test]
fn a_read_name_resolves_to_the_one_folder_it_means() {
    let folders = [
        folder("INBOX", Some(MailboxRole::Inbox)),
        folder("Trash", Some(MailboxRole::Trash)),
        folder("Archive", None),
        folder("Old", Some(MailboxRole::Archive)),
        folder("Older", Some(MailboxRole::Archive)),
    ];
    let imap = sieve(false, false, true);
    let at = |mailbox: &RuleMailbox, vocabulary: &RuleVocabulary| -> Option<String> {
        mailbox
            .folder_in(&folders, vocabulary)
            .map(|folder| folder.id.as_str().to_owned())
    };
    assert_eq!(
        at(&RuleMailbox::Role(MailboxRole::Trash), &imap).as_deref(),
        Some("Trash")
    );
    assert_eq!(
        at(&RuleMailbox::Role(MailboxRole::Archive), &imap),
        None,
        "two have the role: the server chooses"
    );
    assert_eq!(at(&RuleMailbox::Role(MailboxRole::Junk), &imap), None);
    let archive = RuleMailbox::Path(MailboxPath::new("Archive").unwrap());
    assert_eq!(at(&archive, &imap).as_deref(), Some("Archive"));
    assert_eq!(
        at(&archive, &sieve(true, true, false)),
        None,
        "a path, where ids are not paths"
    );
    let id = RuleMailbox::Id(MailboxId::try_from("Archive").unwrap());
    assert_eq!(at(&id, &graph()).as_deref(), Some("Archive"));
}

#[test]
fn every_folder_a_rule_can_name_resolves_back_to_itself() {
    let folders = [
        folder("INBOX", Some(MailboxRole::Inbox)),
        folder("Trash", Some(MailboxRole::Trash)),
        folder("Junk", Some(MailboxRole::Junk)),
        folder("Sent", Some(MailboxRole::Sent)),
        folder("All", Some(MailboxRole::All)),
        folder("Archive", None),
        folder("Archive/2024", None),
        folder("Old", Some(MailboxRole::Archive)),
        folder("Older", Some(MailboxRole::Archive)),
    ];
    let vocabularies = [
        sieve(true, true, false),
        sieve(false, true, false),
        sieve(false, false, false),
        sieve(true, false, true),
        sieve(false, false, true),
        graph(),
        gmail(),
    ];
    for vocabulary in &vocabularies {
        for folder in &folders {
            if let Some(mailbox) = RuleMailbox::for_folder(folder, &folders, vocabulary) {
                assert_eq!(
                    mailbox.folder_in(&folders, vocabulary).map(|f| &f.id),
                    Some(&folder.id),
                    "{mailbox:?} in {vocabulary:?}"
                );
            }
        }
    }
}
