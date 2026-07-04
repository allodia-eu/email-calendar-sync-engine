//! Unit tests for the JMAP session resource ([`super::Session`]) — capability
//! derivation, URL rebasing, and the read-only / EventSource gates. Split out to
//! keep `session.rs` under the 500-line limit (AGENTS.md).

use super::*;
use serde_json::json;

/// A representative session subset, mirroring the live Stalwart shape (account
/// id `"c"`, an advertised foreign `apiUrl`, the core limits).
fn session_doc() -> Value {
    json!({
        "capabilities": {
            "urn:ietf:params:jmap:core": {
                "maxCallsInRequest": 16,
                "maxObjectsInGet": 500,
                "maxObjectsInSet": 500
            },
            "urn:ietf:params:jmap:mail": {},
            "urn:ietf:params:jmap:submission": {},
            "urn:ietf:params:jmap:calendars": {}
        },
        "primaryAccounts": {
            "urn:ietf:params:jmap:mail": "c",
            "urn:ietf:params:jmap:submission": "c",
            "urn:ietf:params:jmap:calendars": "c"
        },
        "accounts": {
            "c": { "name": "alice@test.local", "isReadOnly": false }
        },
        "apiUrl": "https://mail.test.local/jmap/",
        "downloadUrl": "https://mail.test.local/download/{accountId}/{blobId}/{name}?accept={type}",
        "uploadUrl": "https://mail.test.local/upload/{accountId}/",
        "eventSourceUrl": "https://mail.test.local/eventsource/?types={types}&closeafter={closeafter}&ping={ping}",
        "state": "2f72d7c8"
    })
}

#[test]
fn rebases_api_url_onto_connection_base_by_default() {
    let base = Url::parse("http://127.0.0.1:18080").unwrap();
    let session =
        Session::parse(&session_doc(), &base, SessionUrlPolicy::RebaseToConnection).unwrap();
    // The advertised foreign HTTPS origin is replaced by the connection origin.
    assert_eq!(session.api_url(), "http://127.0.0.1:18080/jmap/");
    assert_eq!(session.mail_account_id().unwrap(), "c");
    assert_eq!(session.submission_account_id().unwrap(), "c");
    assert_eq!(session.calendar_account_id().unwrap(), "c");
    assert_eq!(session.state(), Some("2f72d7c8"));
}

#[test]
fn trust_advertised_keeps_the_server_origin() {
    let base = Url::parse("http://127.0.0.1:18080").unwrap();
    let session = Session::parse(&session_doc(), &base, SessionUrlPolicy::TrustAdvertised).unwrap();
    assert_eq!(session.api_url(), "https://mail.test.local/jmap/");
}

#[test]
fn rebases_download_template_onto_connection_keeping_placeholders() {
    let base = Url::parse("http://127.0.0.1:18080").unwrap();
    let session =
        Session::parse(&session_doc(), &base, SessionUrlPolicy::RebaseToConnection).unwrap();
    // Origin rebased to the connection; the `{…}` placeholders survive intact
    // (they would be percent-encoded if run through URL parsing).
    assert_eq!(
        session.download_url(),
        Some("http://127.0.0.1:18080/download/{accountId}/{blobId}/{name}?accept={type}")
    );
    // TrustAdvertised keeps the server origin, still un-mangled.
    let trusted = Session::parse(&session_doc(), &base, SessionUrlPolicy::TrustAdvertised).unwrap();
    assert_eq!(
        trusted.download_url(),
        Some("https://mail.test.local/download/{accountId}/{blobId}/{name}?accept={type}")
    );
}

#[test]
fn reads_capabilities_and_limits() {
    let base = Url::parse("http://127.0.0.1:18080").unwrap();
    let session =
        Session::parse(&session_doc(), &base, SessionUrlPolicy::RebaseToConnection).unwrap();
    let caps = session.capabilities();
    assert!(caps.mail() && caps.submission() && caps.calendars());
    // Mail + a download template ⇒ on-demand message-source fetch is advertised.
    assert!(caps.message_source());
    // Mail + a writable account ⇒ mail writes (`Email/set`) are advertised.
    assert!(caps.mail_writes());
    // An EventSource endpoint ⇒ push / change notification is advertised.
    assert!(caps.idle());
    assert_eq!(session.limits().max_objects_in_get, 500);
    assert_eq!(session.limits().max_calls_in_request, 16);
}

#[test]
fn rebases_upload_and_event_source_templates_keeping_placeholders() {
    let base = Url::parse("http://127.0.0.1:18080").unwrap();
    let session =
        Session::parse(&session_doc(), &base, SessionUrlPolicy::RebaseToConnection).unwrap();
    // Origin rebased onto the connection; the `{…}` placeholders survive intact.
    assert_eq!(
        session.upload_url(),
        Some("http://127.0.0.1:18080/upload/{accountId}/")
    );
    assert_eq!(
        session.event_source_url(),
        Some(
            "http://127.0.0.1:18080/eventsource/?types={types}&closeafter={closeafter}&ping={ping}"
        )
    );
}

#[test]
fn read_only_account_does_not_advertise_mail_writes() {
    let base = Url::parse("http://127.0.0.1:18080").unwrap();
    let doc = json!({
        "capabilities": { "urn:ietf:params:jmap:mail": {} },
        "primaryAccounts": { "urn:ietf:params:jmap:mail": "c" },
        "accounts": { "c": { "isReadOnly": true } },
        "apiUrl": "https://mail.test.local/jmap/"
    });
    let session = Session::parse(&doc, &base, SessionUrlPolicy::RebaseToConnection).unwrap();
    // Mail is readable, but the read-only account cannot write.
    assert!(session.capabilities().mail());
    assert!(!session.capabilities().mail_writes());
}

#[test]
fn no_event_source_means_no_idle_capability() {
    let base = Url::parse("http://127.0.0.1:18080").unwrap();
    let doc = json!({
        "capabilities": { "urn:ietf:params:jmap:mail": {} },
        "primaryAccounts": { "urn:ietf:params:jmap:mail": "c" },
        "apiUrl": "https://mail.test.local/jmap/"
    });
    let session = Session::parse(&doc, &base, SessionUrlPolicy::RebaseToConnection).unwrap();
    assert!(session.capabilities().mail());
    assert!(!session.capabilities().idle());
    assert_eq!(session.event_source_url(), None);
    assert_eq!(session.upload_url(), None);
}

#[test]
fn event_source_without_a_syncable_domain_does_not_advertise_idle() {
    // A push endpoint but no mail/calendar domain the engine can sync → no `idle`,
    // so a host never opens a watcher that could not map a change to a synced scope.
    let base = Url::parse("http://127.0.0.1:18080").unwrap();
    let doc = json!({
        "capabilities": { "urn:ietf:params:jmap:contacts": {} },
        "primaryAccounts": { "urn:ietf:params:jmap:contacts": "c" },
        "apiUrl": "https://mail.test.local/jmap/",
        "eventSourceUrl": "https://mail.test.local/eventsource/?types={types}&closeafter={closeafter}&ping={ping}"
    });
    let session = Session::parse(&doc, &base, SessionUrlPolicy::RebaseToConnection).unwrap();
    assert!(!session.capabilities().mail() && !session.capabilities().calendars());
    assert!(session.event_source_url().is_some());
    assert!(!session.capabilities().idle());
}

#[test]
fn no_download_template_means_no_message_source_capability() {
    let base = Url::parse("http://127.0.0.1:18080").unwrap();
    let doc = json!({
        "capabilities": { "urn:ietf:params:jmap:mail": {} },
        "primaryAccounts": { "urn:ietf:params:jmap:mail": "c" },
        "apiUrl": "https://mail.test.local/jmap/"
    });
    let session = Session::parse(&doc, &base, SessionUrlPolicy::RebaseToConnection).unwrap();
    assert!(session.capabilities().mail());
    assert!(!session.capabilities().message_source());
    assert_eq!(session.download_url(), None);
}

#[test]
fn missing_api_url_is_a_session_error() {
    let base = Url::parse("http://127.0.0.1:18080").unwrap();
    let doc = json!({ "capabilities": {}, "primaryAccounts": {} });
    assert!(matches!(
        Session::parse(&doc, &base, SessionUrlPolicy::RebaseToConnection),
        Err(JmapError::Session(_))
    ));
}

#[test]
fn absent_core_capability_falls_back_to_default_limits() {
    let base = Url::parse("http://127.0.0.1:18080").unwrap();
    let doc = json!({
        "capabilities": { "urn:ietf:params:jmap:mail": {} },
        "primaryAccounts": { "urn:ietf:params:jmap:mail": "c" },
        "apiUrl": "https://mail.test.local/jmap/"
    });
    let session = Session::parse(&doc, &base, SessionUrlPolicy::RebaseToConnection).unwrap();
    assert_eq!(session.limits(), CoreLimits::default());
    assert!(session.mail_account_id().is_ok());
}
