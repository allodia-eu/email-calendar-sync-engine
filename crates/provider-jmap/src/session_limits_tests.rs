//! The limits an import respects, read from Stalwart's captured session and from the shapes
//! RFC 8620, RFC 8621 and RFC 9404 allow.

#[cfg(feature = "import")]
use std::num::NonZeroUsize;

use reqwest::Url;
use serde_json::{Value, json};

#[cfg(feature = "import")]
use crate::session_limits::BlobUpload;
use crate::{
    provider::provider_test_support::fixture,
    session::{Session, SessionUrlPolicy},
};

fn parse(doc: &Value) -> Session {
    let base = Url::parse("http://127.0.0.1:18080").unwrap();
    Session::parse(doc, &base, SessionUrlPolicy::RebaseToConnection, None).unwrap()
}

#[test]
fn the_core_upload_and_request_sizes_are_read_from_the_session() {
    let session = parse(&fixture("session_shared_accounts.json"));
    let limits = session.limits();
    assert_eq!(limits.max_size_upload, 50_000_000);
    assert_eq!(limits.max_concurrent_upload, 4);
    assert_eq!(limits.max_size_request, 10_000_000);
}

#[cfg(feature = "import")]
#[test]
fn the_mail_account_states_its_mailboxes_per_email_and_largest_inline_blob() {
    let session = parse(&fixture("session_shared_accounts.json"));
    let limits = session.mail_limits();
    assert_eq!(limits.max_mailboxes_per_email, NonZeroUsize::new(100));
    assert_eq!(
        limits.blob_upload,
        BlobUpload::Supported {
            max_size: Some(7_499_488)
        }
    );
}

/// RFC 8621 §1.3.1: `maxMailboxesPerEmail` is `null` for no limit; RFC 9404 §4.1:
/// `maxSizeBlobSet` is `null` for no limit. Neither absence is a limit of one or zero.
#[cfg(feature = "import")]
#[test]
fn a_null_limit_is_no_limit() {
    let mut doc = fixture("session_shared_accounts.json");
    let caps = &mut doc["accounts"]["c"]["accountCapabilities"];
    caps["urn:ietf:params:jmap:mail"]["maxMailboxesPerEmail"] = Value::Null;
    caps["urn:ietf:params:jmap:blob"]["maxSizeBlobSet"] = Value::Null;
    let limits = parse(&doc).mail_limits();
    assert_eq!(limits.max_mailboxes_per_email, None);
    assert_eq!(limits.blob_upload, BlobUpload::Supported { max_size: None });
}

/// The blob extension counts only where the mail account lists it: a server that supports it
/// for another account cannot take a `Blob/upload` into this one.
#[cfg(feature = "import")]
#[test]
fn an_account_without_the_blob_extension_uploads_by_url_only() {
    let mut doc = fixture("session_shared_accounts.json");
    doc["accounts"]["c"]["accountCapabilities"]
        .as_object_mut()
        .unwrap()
        .remove("urn:ietf:params:jmap:blob");
    assert_eq!(
        parse(&doc).mail_limits().blob_upload,
        BlobUpload::Unsupported
    );

    let no_server_support = json!({
        "capabilities": { "urn:ietf:params:jmap:mail": {} },
        "primaryAccounts": { "urn:ietf:params:jmap:mail": "c" },
        "accounts": { "c": { "accountCapabilities": {
            "urn:ietf:params:jmap:mail": {}, "urn:ietf:params:jmap:blob": {}
        } } },
        "apiUrl": "https://mail.test.local/jmap/"
    });
    assert_eq!(
        parse(&no_server_support).mail_limits().blob_upload,
        BlobUpload::Unsupported
    );
}

#[test]
fn a_session_stating_nothing_takes_the_suggested_minimums_and_no_mail_limit() {
    let doc = json!({
        "capabilities": { "urn:ietf:params:jmap:mail": {} },
        "primaryAccounts": { "urn:ietf:params:jmap:mail": "c" },
        "apiUrl": "https://mail.test.local/jmap/"
    });
    let session = parse(&doc);
    let limits = session.limits();
    // RFC 8620 §2's suggested minimums for sizes, and one at a time for concurrency, as for
    // `maxConcurrentRequests`: too wide is refused, too narrow is only slow.
    assert_eq!(limits.max_size_upload, 50_000_000);
    assert_eq!(limits.max_size_request, 10_000_000);
    assert_eq!(limits.max_concurrent_upload, 1);
    #[cfg(feature = "import")]
    {
        assert_eq!(session.mail_limits().max_mailboxes_per_email, None);
        assert_eq!(session.mail_limits().blob_upload, BlobUpload::Unsupported);
    }
}
