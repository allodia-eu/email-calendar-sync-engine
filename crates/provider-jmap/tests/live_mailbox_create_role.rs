//! Gated live integration: a create that gives the new folder a role (`Mailbox/set` `role`,
//! RFC 8621 §2), against the Stalwart harness.
//!
//! The offline suite asserts that `role` is sent and how a refusal is classified from canned
//! answers; only a real server says whether it takes the role, lists the folder with it, and
//! how it refuses a role another folder holds. Each assertion reads the tree back through the
//! adapter's own folder sync, so a create that dropped the role fails here.
//!
//! A raw `Mailbox/set` beside the adapter's asks for a role the account already has and prints
//! the server's answer, the bytes a fixture of that refusal is made from.
//!
//! Runs as the scratch account `carol@` under names unique to this run, and deletes what it
//! made. Skips with no `STALWART_HTTP_ADDR`.

use core::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId},
    mail::{Mailbox, MailboxRole},
    sync::SyncUpdate,
};
use engine_provider::{MailboxEdit, MailboxWrites, Provider};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use serde_json::{Value, json};
use stalwart_harness::Harness;

fn account() -> AccountId {
    AccountId::try_from("live-mailbox-create-role").unwrap()
}

fn unique(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("live-jmap-{label}-{nanos}")
}

async fn connect(harness: &Harness) -> JmapProvider {
    let carol = &harness.scratch[1];
    JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&carol.address, &carol.password),
        engine_http::test_server::user_agent(),
    ))
    .await
    .expect("connect")
}

async fn folders(provider: &JmapProvider) -> Vec<Mailbox> {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_mailboxes(&account(), None)
        .await
        .unwrap()
        .update
    else {
        panic!("a cursorless folder sync is a snapshot");
    };
    objects
}

async fn named(provider: &JmapProvider, name: &str) -> Option<Mailbox> {
    folders(provider).await.into_iter().find(|m| m.name == name)
}

async fn create(provider: &JmapProvider, edit: &MailboxEdit) -> Result<MailboxId, FailureClass> {
    provider
        .edit_mailbox(&account(), edit)
        .await
        .map(|receipt| receipt.mailbox.expect("a create names its folder"))
        .map_err(|error| error.class())
}

async fn delete(provider: &JmapProvider, target: &MailboxId) {
    provider
        .edit_mailbox(
            &account(),
            &MailboxEdit::Delete {
                target: target.clone(),
            },
        )
        .await
        .expect("delete what the test made");
}

/// Sends one raw `Mailbox/set` as carol and returns its arguments.
fn raw_set(harness: &Harness, arguments: &Value) -> Value {
    let body = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [["Mailbox/set", arguments, "0"]],
    });
    let response = harness
        .jmap_post_as(harness.scratch[1].auth(), body.to_string().as_bytes())
        .expect("POST /jmap/");
    let response: Value = serde_json::from_slice(&response.body).expect("JSON");
    eprintln!("Mailbox/set answered: {response}");
    response["methodResponses"][0][1].clone()
}

fn carols_mail_account(harness: &Harness) -> String {
    harness
        .jmap_session_as(harness.scratch[1].auth())
        .expect("carol's session")["primaryAccounts"]["urn:ietf:params:jmap:mail"]
        .as_str()
        .expect("a mail account")
        .to_owned()
}

#[tokio::test]
async fn a_folder_is_made_with_a_role_and_a_held_or_formless_role_is_refused() {
    let Some(harness) = Harness::from_env() else {
        return;
    };
    harness.wait_until_ready(Duration::from_secs(30)).unwrap();
    let provider = connect(&harness).await;
    assert!(
        provider
            .connection_info()
            .capabilities
            .mailbox_create_role()
    );

    // A run that failed halfway left its role folder behind, holding a role this run needs.
    for left in folders(&provider).await {
        if left.name.starts_with("live-jmap-role-") || left.name.starts_with("live-jmap-plain-") {
            delete(&provider, &left.id).await;
        }
    }

    // A registered role carol's account does not hold yet. A role the server will not give
    // at all is `InvalidState`: noted, and the next one tried.
    let before = folders(&provider).await;
    let held: Vec<&MailboxRole> = before.iter().filter_map(|m| m.role.as_ref()).collect();
    eprintln!("carol's roles before: {held:?}");
    let name = unique("role");
    let mut made = None;
    for role in [
        MailboxRole::Archive,
        MailboxRole::Important,
        MailboxRole::Flagged,
        MailboxRole::All,
    ] {
        if held.contains(&&role) {
            continue;
        }
        let edit = MailboxEdit::create_with_role(name.as_str(), None, role.clone());
        match create(&provider, &edit).await {
            Ok(id) => {
                made = Some((id, role));
                break;
            }
            Err(FailureClass::InvalidState) => eprintln!("Stalwart will not give the role {role}"),
            Err(class) => panic!("a create with the free role {role}: {class:?}"),
        }
    }
    let (id, role) = made.expect("Stalwart gives a new folder at least one registered role");

    let stored = named(&provider, &name).await.expect("the folder is listed");
    assert_eq!(stored.id, id);
    assert_eq!(stored.role.as_ref(), Some(&role), "listed with the role");
    assert!(stored.subscribed);

    // A retry meets the folder its first attempt made, role and all.
    let again = MailboxEdit::create_with_role(name.as_str(), None, role.clone());
    assert_eq!(create(&provider, &again).await, Ok(id.clone()));

    // RFC 8621 §2: one folder per role. The role this test gave, and one the account had.
    let second = unique("second");
    let taken = MailboxEdit::create_with_role(second.as_str(), None, role.clone());
    assert_eq!(create(&provider, &taken).await, Err(FailureClass::Conflict));
    let sent = MailboxEdit::create_with_role(second.as_str(), None, MailboxRole::Sent);
    assert_eq!(create(&provider, &sent).await, Err(FailureClass::Conflict));
    assert!(
        named(&provider, &second).await.is_none(),
        "nothing was made"
    );

    // A folder of the name without the role is not the one asked for.
    let plain_name = unique("plain");
    let plain = create(&provider, &MailboxEdit::create(plain_name.as_str(), None))
        .await
        .expect("an ordinary folder");
    let with_role = MailboxEdit::create_with_role(plain_name.as_str(), None, role.clone());
    assert_eq!(
        create(&provider, &with_role).await,
        Err(FailureClass::Conflict)
    );

    // A role no registry names is refused before sending.
    let formless = unique("records");
    let records =
        MailboxEdit::create_with_role(formless.as_str(), None, MailboxRole::ServerRecords);
    assert_eq!(
        create(&provider, &records).await,
        Err(FailureClass::InvalidState)
    );
    assert!(named(&provider, &formless).await.is_none());

    // The server's own answer to a role already held, for the fixture.
    let raw_name = unique("raw");
    let answer = raw_set(
        &harness,
        &json!({
            "accountId": carols_mail_account(&harness),
            "create": { "new": { "name": raw_name, "parentId": null, "role": "sent" } }
        }),
    );
    assert!(answer["created"]["new"].is_null(), "{answer}");
    assert!(answer["notCreated"]["new"].is_object(), "{answer}");
    assert!(named(&provider, &raw_name).await.is_none());

    delete(&provider, &plain).await;
    delete(&provider, &id).await;
    assert!(
        named(&provider, &name).await.is_none(),
        "the role folder is gone"
    );
}
