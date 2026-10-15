use engine_core::{affiliation::Affiliation, ids::OrganizationId};

use crate::{
    error::GoogleError,
    test_support::{fake_client_fallible, json, recording_client},
};

const WORKSPACE: &str = include_str!("../tests/fixtures/identity/userinfo_workspace.json");
const PERSONAL: &str = include_str!("../tests/fixtures/identity/userinfo_personal.json");
const WITHOUT_SCOPE: &str = include_str!("../tests/fixtures/error/userinfo_without_scope.json");

#[tokio::test]
async fn a_workspace_account_is_named_by_its_hosted_domain() {
    let (client, log) = recording_client(vec![("/oauth2/v3/userinfo", Ok(json(WORKSPACE)))]);
    assert_eq!(
        client.affiliation().await.unwrap(),
        Affiliation::Organization(OrganizationId::try_from("example.test").unwrap())
    );
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].method, "GET");
    assert_eq!(log[0].url, "https://google.test/oauth2/v3/userinfo");
}

#[tokio::test]
async fn an_account_without_a_hosted_domain_is_personal() {
    let client = fake_client_fallible(vec![("/oauth2/v3/userinfo", Ok(json(PERSONAL)))]);
    assert_eq!(client.affiliation().await.unwrap(), Affiliation::Personal);
}

#[tokio::test]
async fn a_token_without_the_email_scope_is_an_error_rather_than_personal() {
    let client = fake_client_fallible(vec![(
        "/oauth2/v3/userinfo",
        Err((401, json(WITHOUT_SCOPE))),
    )]);
    assert!(matches!(
        client.affiliation().await,
        Err(GoogleError::Status { status: 401, .. })
    ));
}
