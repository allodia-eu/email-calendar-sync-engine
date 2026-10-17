//! A config bound to a shared mailbox, connected through the real client against a mock
//! server that serves the session Stalwart hands `alice@`
//! (`tests/fixtures/session_shared_accounts.json`): the binding reaches the session, and a
//! handle the session does not list fails the connect instead of every call after it.

use engine_core::{error::FailureClass, ids::SharedMailboxId};

use super::{Credentials, JmapConfig};
use crate::{
    JmapClient,
    tests::{http_ok, mock_server},
};

const SESSION: &str = include_str!("../tests/fixtures/session_shared_accounts.json");

fn bound(base: String, handle: &str) -> JmapConfig {
    JmapConfig::new(base, Credentials::basic("alice", "pw"))
        .with_session_path("/jmap/session")
        .with_shared_mailbox(SharedMailboxId::try_from(handle).unwrap())
}

#[tokio::test]
async fn a_bound_config_addresses_every_call_at_the_shared_account() {
    let config = bound(mock_server(vec![http_ok(SESSION)]), "f");
    assert!(format!("{config:?}").contains("shared_mailbox"));
    let client = JmapClient::connect(config).await.unwrap();
    // The group's account `f`, where an unbound client would address alice's primary `c`.
    let session = client.session();
    assert_eq!(session.mail_account_id().unwrap(), "f");
    assert_eq!(session.submission_account_id().unwrap(), "f");
}

#[tokio::test]
async fn a_handle_the_session_does_not_list_fails_the_connect() {
    let config = bound(mock_server(vec![http_ok(SESSION)]), "zzz");
    let err = JmapClient::connect(config).await.unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Permanent);
    assert!(err.to_string().contains("lost access"), "{err}");
}
