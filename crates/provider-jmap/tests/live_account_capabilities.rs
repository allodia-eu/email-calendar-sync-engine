//! Gated live integration: the domains a JMAP account offers are read from its primary accounts'
//! `accountCapabilities` (RFC 8620 §2), checked against the session Stalwart actually serves.
//!
//! The offline suite pins the rule over hand-written sessions. This pins that the rule reads the
//! shape a real server sends: every domain the adapter offers is listed by its primary account,
//! and every domain that account lists is offered, so a mismatch in either direction fails here.
//!
//! Skips with no `STALWART_HTTP_ADDR`, so the offline suite stays green.

use provider_jmap::{Credentials, JmapClient, JmapConfig};
use serde_json::Value;
use stalwart_harness::Harness;

const MAIL: &str = "urn:ietf:params:jmap:mail";
const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
const CALENDARS: &str = "urn:ietf:params:jmap:calendars";
const CONTACTS: &str = "urn:ietf:params:jmap:contacts";

/// Whether the raw session's primary account for `urn` lists it in its `accountCapabilities`.
fn primary_account_lists(session: &Value, urn: &str) -> bool {
    let Some(id) = session
        .pointer(&format!("/primaryAccounts/{}", urn.replace('/', "~1")))
        .and_then(Value::as_str)
    else {
        return false;
    };
    session
        .pointer(&format!("/accounts/{id}/accountCapabilities"))
        .and_then(Value::as_object)
        .is_some_and(|caps| caps.contains_key(urn))
}

#[tokio::test]
async fn the_offered_domains_are_the_ones_the_primary_accounts_list() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live account-capabilities test: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let raw = harness.jmap_session().expect("raw session");

    let client = JmapClient::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect");
    let caps = client.session().capabilities();

    for (urn, offered) in [
        (MAIL, caps.mail()),
        (SUBMISSION, caps.submission()),
        (CALENDARS, caps.calendars()),
        (CONTACTS, caps.contacts()),
    ] {
        assert_eq!(
            offered,
            primary_account_lists(&raw, urn),
            "{urn}: offered and listed by its primary account must agree"
        );
    }
    // The harness account holds all four; a regression that read nothing would agree above.
    assert!(caps.mail() && caps.submission() && caps.calendars() && caps.contacts());
}
