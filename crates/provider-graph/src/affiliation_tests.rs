use engine_core::{affiliation::Affiliation, ids::OrganizationId};
use serde_json::json;

use crate::{
    error::GraphError,
    test_support::{fake_client_fallible, fake_client_logged, json},
};

const ORGANIZATION: &str = include_str!("../tests/fixtures/organization/organization.json");
const MSA_REFUSAL: &str = include_str!("../tests/fixtures/error/organization_msa_unsupported.json");

#[tokio::test]
async fn an_organizations_account_is_named_by_its_tenant() {
    let (client, _, requests) = fake_client_logged(vec![("/organization", Ok(json(ORGANIZATION)))]);
    assert_eq!(
        client.affiliation().await.unwrap(),
        Affiliation::Organization(
            OrganizationId::try_from("00000000-0000-4000-8000-00000000feed").unwrap()
        )
    );
    // At the root, not under `/me`: the tenant is the credential's, whichever mailbox it opens.
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1, "https://graph.test/organization?$select=id");
}

#[tokio::test]
async fn a_personal_accounts_refusal_is_the_answer_personal() {
    let client = fake_client_fallible(vec![("/organization", Err((400, json(MSA_REFUSAL))))]);
    assert_eq!(client.affiliation().await.unwrap(), Affiliation::Personal);
}

#[tokio::test]
async fn any_other_failure_is_an_error_rather_than_a_guess() {
    let failures = [
        (401, "InvalidAuthenticationToken"),
        (403, "Authorization_RequestDenied"),
        (429, "TooManyRequests"),
        (503, "ServiceUnavailable"),
        // A 400 that is not Graph's own refusal envelope says nothing about the account.
        (400, "Request_BadRequest"),
    ];
    for (status, code) in failures {
        let client = fake_client_fallible(vec![(
            "/organization",
            Err((status, json!({ "error": { "code": code, "message": "" } }))),
        )]);
        assert!(
            matches!(
                client.affiliation().await,
                Err(GraphError::Status { status: answered, .. }) if answered == status
            ),
            "{status} {code}"
        );
    }
}

#[tokio::test]
async fn an_organization_reply_without_a_tenant_is_a_protocol_error() {
    let client = fake_client_fallible(vec![("/organization", Ok(json!({ "value": [] })))]);
    assert!(client.affiliation().await.is_err());
}
