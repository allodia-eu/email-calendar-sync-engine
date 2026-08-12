//! Gated live integration: the folder-list contract, asserted against **every** configured
//! IMAP server — Stalwart (IMAP4rev2) and Dovecot (IMAP4rev1).
//!
//! Dialect-independent on purpose. These are things a client must get right on any correct
//! server, so the file is organized by the contract rather than by vendor, and each test
//! loops over whichever harnesses are up (`docs/agent-guidance/imap-smtp.md` → "Which
//! server proves what"). What makes the loop worth its round trips is that servers differ
//! in **strictness**, not only in dialect: a server that volunteers data cannot show you
//! that you forgot to ask for it, so the strictest configured server is the one that fails.
//! Which server that is changes with a version bump, so no test hard-codes it.
//!
//! Assertions are seed-relative (`> 0`, `== 0`, "no such name") rather than absolute,
//! because the two harnesses share `docker/stalwart/seed/mail` but not the whole dataset.
//! Skips per server when its address variable is unset, so the offline
//! `cargo test --workspace` stays green.

#[path = "common/imap_live.rs"]
mod imap_live;

use engine_core::mail::MailboxRole;
use imap_live::{SERVERS, connect, find, folders};

#[tokio::test]
async fn the_account_has_exactly_one_folder_for_each_special_role() {
    for server in &SERVERS {
        let Some(provider) = connect(
            server,
            "the_account_has_exactly_one_folder_for_each_special_role",
        )
        .await
        else {
            continue;
        };
        let all = folders(&provider).await;
        let with_role =
            |role: &MailboxRole| all.iter().filter(|m| m.role.as_ref() == Some(role)).count();

        // Asserted by **role**, never by name: the two harnesses call these folders
        // different things ("Sent" vs "Sent Items"), which is exactly why a client sorts
        // and badges by role and `place.rs` resolves the sent-copy destination by role.
        // On a strict server the roles arrive only because the folder list asked for them;
        // on a lenient one they would arrive regardless — same assertion, and only one of
        // the two can catch the omission.
        for role in [
            MailboxRole::Inbox,
            MailboxRole::Sent,
            MailboxRole::Drafts,
            MailboxRole::Trash,
            MailboxRole::Junk,
        ] {
            let names: Vec<&str> = all.iter().map(|m| m.name.as_str()).collect();
            assert_eq!(
                with_role(&role),
                1,
                "{}: expected one {role:?} folder among {names:?}",
                server.label
            );
        }
    }
}

#[tokio::test]
async fn the_folder_list_carries_its_unread_counts() {
    for server in &SERVERS {
        let Some(provider) = connect(server, "the_folder_list_carries_its_unread_counts").await
        else {
            continue;
        };
        let all = folders(&provider).await;

        // Every listed mailbox is counted — absent would render as no badge, which is
        // indistinguishable from "nothing unread" and so must not happen silently.
        for mailbox in &all {
            assert!(
                mailbox.unread_count.is_some(),
                "{} left {} uncounted",
                server.label,
                mailbox.name
            );
        }
    }
}

#[tokio::test]
async fn no_mailbox_is_invented_from_a_completion_line() {
    for server in &SERVERS {
        let Some(provider) = connect(server, "no_mailbox_is_invented_from_a_completion_line").await
        else {
            continue;
        };
        let all = folders(&provider).await;

        // A tagged completion's detail begins with the command name, and on some servers
        // ends in a period — four items whose last is a bare `.` sitting where the mailbox
        // name goes. Read as data it becomes a folder the user can see, which then takes a
        // sync scope of its own.
        let names: Vec<&str> = all.iter().map(|m| m.name.as_str()).collect();
        assert!(
            !names.contains(&"."),
            "{} invented a mailbox: {names:?}",
            server.label
        );
        assert!(
            !names.iter().any(|name| name.is_empty()),
            "{} listed an empty name: {names:?}",
            server.label
        );
    }
}

#[tokio::test]
async fn a_mailbox_id_addresses_the_mailbox_it_names() {
    for server in &SERVERS {
        let Some(provider) = connect(server, "a_mailbox_id_addresses_the_mailbox_it_names").await
        else {
            continue;
        };
        let all = folders(&provider).await;

        // The id is the decoded name on either dialect, and the transport puts the wire
        // form back — so selecting by id must reach the mailbox the list named. A rev1
        // server whose id was left in modified UTF-7 would still pass a `SELECT`; one whose
        // decoded id was sent unencoded would not.
        for mailbox in &all {
            assert_eq!(
                mailbox.id.as_str(),
                mailbox.name,
                "{}: id and name are one identity",
                server.label
            );
        }
    }
}

#[tokio::test]
async fn one_mailbox_has_one_identity_on_either_dialect() {
    for server in &SERVERS {
        let Some(provider) =
            connect(server, "one_mailbox_has_one_identity_on_either_dialect").await
        else {
            continue;
        };
        let all = folders(&provider).await;

        // Both harnesses carry this folder, and put entirely different bytes on the wire
        // for it: `&ANw-berweisungen` on rev1, the UTF-8 name itself on rev2. It reaching
        // the model identically on both is the whole claim of the identity model — and it
        // is what keeps a message key stable the day a server starts offering rev2.
        let mailbox = find(&all, "Überweisungen");
        assert_eq!(mailbox.id.as_str(), "Überweisungen", "{}", server.label);
        assert!(!mailbox.id.as_str().contains('&'), "{}", server.label);
    }
}
