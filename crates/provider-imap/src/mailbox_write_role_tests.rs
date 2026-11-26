//! A create that asks for a role: `CREATE … (USE (…))` where the server advertises
//! `CREATE-SPECIAL-USE` (RFC 6154 §3) and roles can be read back, nothing sent where not or
//! where the role has no attribute, the role read back after the create, and a refusal told
//! apart by the folder list rather than by the server's prose.

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId},
    mail::MailboxRole,
};
use engine_provider::{
    Capabilities, CreateRoleError, MailboxEdit, MailboxEditReceipt, MailboxWrites, Provider,
    ProviderError,
};

use super::{GREETING, LOGIN_OK, TREE, id, logged_in};
use crate::{
    ImapProvider,
    capability::Negotiated,
    mock::{MockStream, script, written},
    store::MailStore,
    transport::Connection,
};

/// A server that lets a client give a new folder a role, beside listing roles.
const CREATES_ROLES: &[&str] = &["IMAP4rev1", "SPECIAL-USE", "CREATE-SPECIAL-USE"];
/// A server that lists roles and lets no client set one: Dovecot's harness, by its capability.
const LISTS_ROLES: &[&str] = &["IMAP4rev1", "SPECIAL-USE"];
/// A rev1 server that would take `USE` but cannot list a role back.
const BLIND_TO_ROLES: &[&str] = &["IMAP4rev1", "CREATE-SPECIAL-USE"];

/// The `LIST` that reads back the folder a create made with a role, answered by `a4`.
const MADE_ARCHIVE: &str = "* LIST (\\HasNoChildren \\Archive) \"/\" \"Archive\"\r\na4 OK LIST\r\n";

async fn session(
    advertised: &[&str],
    replies: &[&str],
) -> (Connection<MockStream>, super::Recorded) {
    let mut parts = vec![GREETING, LOGIN_OK];
    parts.extend_from_slice(replies);
    let (mut connection, recorded) = logged_in(script(&parts)).await;
    let advertised: Vec<String> = advertised.iter().map(|a| (*a).to_owned()).collect();
    connection.negotiated = Negotiated::from_capabilities(&advertised);
    (connection, recorded)
}

/// What the provider over a server that advertised `advertised` reports it can do.
async fn capabilities(advertised: &[&str]) -> Capabilities {
    let (connection, _) = session(advertised, &[]).await;
    ImapProvider::with_connection(connection, MailboxId::try_from("INBOX").unwrap())
        .connection_info()
        .capabilities
}

/// Runs `edit` on a server that advertised `advertised`, answering `replies` after login, with
/// the capabilities the provider derives from it.
async fn run(
    advertised: &[&str],
    replies: &[&str],
    edit: &MailboxEdit,
) -> (Result<MailboxEditReceipt, ProviderError>, String) {
    run_redialled(advertised, advertised, replies, edit).await
}

/// [`run`], on a session that advertised `now`, under the capabilities the provider derived
/// at its first connect, when the server advertised `at_connect`: a redial that landed on
/// another backend.
async fn run_redialled(
    at_connect: &[&str],
    now: &[&str],
    replies: &[&str],
    edit: &MailboxEdit,
) -> (Result<MailboxEditReceipt, ProviderError>, String) {
    let capabilities = capabilities(at_connect).await;
    let (mut connection, recorded) = session(now, replies).await;
    let store = MailStore::Own { foreign: vec![] };
    let result = super::edit_mailbox(&mut connection, &store, edit, &capabilities).await;
    (result, written(&recorded))
}

fn archive() -> MailboxEdit {
    MailboxEdit::create_with_role("Archive", None, MailboxRole::Archive)
}

fn rule(error: &ProviderError) -> Option<CreateRoleError> {
    std::error::Error::source(error)
        .and_then(|source| source.downcast_ref::<CreateRoleError>())
        .cloned()
}

#[tokio::test]
async fn the_capability_follows_what_the_server_advertised_and_can_list_back() {
    for (advertised, expected) in [
        (CREATES_ROLES, true),
        (LISTS_ROLES, false),
        (BLIND_TO_ROLES, false),
    ] {
        let capabilities = capabilities(advertised).await;
        assert!(capabilities.mailbox_writes());
        assert_eq!(
            capabilities.mailbox_create_role(),
            expected,
            "{advertised:?}"
        );
    }
}

#[tokio::test]
async fn a_create_with_a_role_names_its_attribute_in_a_use_parameter_and_reads_it_back() {
    let replies = [
        TREE,
        "a3 OK CREATE\r\n",
        MADE_ARCHIVE,
        "a5 OK SUBSCRIBE\r\n",
    ];
    let (result, sent) = run(CREATES_ROLES, &replies, &archive()).await;

    assert_eq!(result.unwrap(), MailboxEditReceipt::resolved(id("Archive")));
    assert!(
        sent.contains("a3 CREATE \"Archive\" (USE (\\Archive))\r\n"),
        "{sent}"
    );
    // Success is read, not assumed: one `LIST` of the new folder, its roles asked for.
    assert!(
        sent.contains("a4 LIST \"\" \"Archive\" RETURN (SPECIAL-USE)\r\n"),
        "{sent}"
    );
    assert!(sent.contains("a5 SUBSCRIBE \"Archive\""), "{sent}");
}

#[tokio::test]
async fn a_create_the_server_took_without_the_role_is_not_a_success() {
    // What Dovecot does with an unadvertised `USE` (DOVECOT-1), were a server to do it here.
    let dropped = "* LIST (\\HasNoChildren) \"/\" \"Archive\"\r\na4 OK LIST\r\n";
    let replies = [TREE, "a3 OK CREATE\r\n", dropped, "a5 OK SUBSCRIBE\r\n"];
    let (result, sent) = run(CREATES_ROLES, &replies, &archive()).await;
    let error = result.unwrap_err();
    assert_eq!(error.class(), FailureClass::InvalidState);
    // The one refusal with a side effect: the folder is named, finished as a plain create
    // finishes one (subscribed), and not deleted.
    assert_eq!(
        rule(&error),
        Some(CreateRoleError::NotKept {
            mailbox: id("Archive")
        })
    );
    assert!(sent.contains("a5 SUBSCRIBE \"Archive\""), "{sent}");
    assert!(!sent.contains("DELETE"), "{sent}");
}

#[tokio::test]
async fn a_create_without_a_role_sends_no_use_parameter() {
    let replies = [TREE, "a3 OK CREATE\r\n", "a4 OK SUBSCRIBE\r\n"];
    let create = MailboxEdit::create("Receipts", None);
    let (_, sent) = run(CREATES_ROLES, &replies, &create).await;
    assert!(sent.contains("a3 CREATE \"Receipts\"\r\n"), "{sent}");
}

#[tokio::test]
async fn a_server_that_does_not_advertise_it_is_sent_nothing() {
    // RFC 6154 §3: a client MUST NOT send `USE` unless the server advertises
    // `CREATE-SPECIAL-USE`. Through the provider, so the gate is the capability it reports.
    for advertised in [LISTS_ROLES, BLIND_TO_ROLES] {
        let (connection, recorded) = session(advertised, &[]).await;
        let provider =
            ImapProvider::with_connection(connection, MailboxId::try_from("INBOX").unwrap());
        let account = AccountId::try_from("alice").unwrap();
        let error = provider
            .edit_mailbox(&account, &archive())
            .await
            .unwrap_err();
        assert_eq!(error.class(), FailureClass::InvalidState);
        assert_eq!(rule(&error), Some(CreateRoleError::NotSupported));
        let sent = written(&recorded);
        assert!(!sent.contains("LIST") && !sent.contains("CREATE"), "{sent}");
    }
}

#[tokio::test]
async fn a_role_with_no_attribute_is_refused_before_anything_is_sent() {
    // The inbox is `INBOX` by name on IMAP; the others no registry names.
    for role in [
        MailboxRole::Inbox,
        MailboxRole::ServerRecords,
        MailboxRole::ChatHistory,
        MailboxRole::Other("snoozed".into()),
    ] {
        let create = MailboxEdit::create_with_role("Records", None, role);
        let (result, sent) = run(CREATES_ROLES, &[], &create).await;
        let error = result.unwrap_err();
        assert_eq!(error.class(), FailureClass::InvalidState);
        assert_eq!(rule(&error), Some(CreateRoleError::NoTransportForm));
        assert!(!sent.contains("CREATE"), "{sent}");
    }
}

#[tokio::test]
async fn a_role_another_folder_holds_that_the_server_refuses_is_a_conflict() {
    // RFC 6154 §3 lets a server refuse a role another folder has as a conflict; `[USEATTR]`
    // says the refusal is about the role, and the list says who holds it.
    let bin = MailboxEdit::create_with_role("Bin", None, MailboxRole::Trash);
    let replies = [TREE, "a3 NO [USEATTR] \\Trash already in use\r\n"];
    let (result, sent) = run(CREATES_ROLES, &replies, &bin).await;
    assert_eq!(result.unwrap_err().class(), FailureClass::Conflict);
    assert!(sent.contains("a3 CREATE \"Bin\" (USE (\\Trash))"), "{sent}");
    assert!(!sent.contains("SUBSCRIBE"), "{sent}");
}

#[tokio::test]
async fn a_role_nobody_holds_that_the_server_refuses_is_invalid_state() {
    let everything = MailboxEdit::create_with_role("Everything", None, MailboxRole::All);
    let replies = [TREE, "a3 NO [USEATTR] \\All not supported\r\n"];
    let (result, _) = run(CREATES_ROLES, &replies, &everything).await;
    assert_eq!(result.unwrap_err().class(), FailureClass::InvalidState);
}

#[tokio::test]
async fn a_folder_already_there_with_the_role_is_the_retry_and_one_without_it_a_conflict() {
    let bin = "* LIST () \"/\" \"INBOX\"\r\n* LIST (\\Trash) \"/\" \"Bin\"\r\na2 OK LIST\r\n";
    let trash = MailboxEdit::create_with_role("Bin", None, MailboxRole::Trash);
    let (result, sent) = run(CREATES_ROLES, &[bin, "a3 OK SUBSCRIBE\r\n"], &trash).await;
    assert_eq!(result.unwrap(), MailboxEditReceipt::resolved(id("Bin")));
    assert!(!sent.contains("CREATE"), "{sent}");

    let plain = "* LIST () \"/\" \"INBOX\"\r\n* LIST () \"/\" \"Archive\"\r\na2 OK LIST\r\n";
    let (result, sent) = run(CREATES_ROLES, &[plain], &archive()).await;
    assert_eq!(result.unwrap_err().class(), FailureClass::Conflict);
    assert!(
        !sent.contains("CREATE") && !sent.contains("SUBSCRIBE"),
        "{sent}"
    );
}

#[tokio::test]
async fn a_folder_made_between_the_list_and_the_create_is_read_again_for_its_role() {
    let made =
        "* LIST () \"/\" \"INBOX\"\r\n* LIST (\\Archive) \"/\" \"Archive\"\r\na4 OK LIST\r\n";
    let replies = [
        TREE,
        "a3 NO [ALREADYEXISTS] Mailbox exists\r\n",
        made,
        "a5 OK SUBSCRIBE\r\n",
    ];
    let (result, sent) = run(CREATES_ROLES, &replies, &archive()).await;
    assert_eq!(result.unwrap(), MailboxEditReceipt::resolved(id("Archive")));
    assert!(sent.contains("a4 LIST"), "{sent}");

    // A server that says it exists and does not list it gives nothing to resolve to.
    let unlisted = "* LIST () \"/\" \"INBOX\"\r\na4 OK LIST\r\n";
    let replies = [TREE, "a3 NO [ALREADYEXISTS] Mailbox exists\r\n", unlisted];
    let (result, _) = run(CREATES_ROLES, &replies, &archive()).await;
    assert_eq!(result.unwrap_err().class(), FailureClass::Conflict);

    let theirs = "* LIST () \"/\" \"INBOX\"\r\n* LIST () \"/\" \"Archive\"\r\na4 OK LIST\r\n";
    let replies = [TREE, "a3 NO [ALREADYEXISTS] Mailbox exists\r\n", theirs];
    let (result, _) = run(CREATES_ROLES, &replies, &archive()).await;
    assert_eq!(result.unwrap_err().class(), FailureClass::Conflict);
}

#[tokio::test]
async fn a_redial_onto_a_backend_that_does_not_advertise_it_is_sent_nothing() {
    // The capability said yes at the first connect; this session's server says no.
    let (result, sent) = run_redialled(CREATES_ROLES, LISTS_ROLES, &[], &archive()).await;
    let error = result.unwrap_err();
    assert_eq!(rule(&error), Some(CreateRoleError::NotSupported));
    assert!(!sent.contains("LIST") && !sent.contains("CREATE"), "{sent}");
}
