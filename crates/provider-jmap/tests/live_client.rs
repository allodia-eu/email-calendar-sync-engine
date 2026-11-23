//! Gated live integration: the public JMAP client against the Stalwart harness, used the way a
//! host that needs JMAP the provider does not wrap would use it, through the public API only.
//!
//! The offline suites assert what is sent; only a server says it resolves a result reference
//! built by [`ResultRef`], answers a method error under the call's own [`CallId`] while the next
//! call still answers, and hands back through `downloadUrl` the bytes `uploadUrl` took, whole and
//! streamed.
//!
//! Each claim has its control: the reference resolves to the ids the plain query listed, the
//! method error sits beside a call that succeeded in the same request, and the uploaded bytes
//! are unique to the run, so a download can only match them.
//!
//! It uploads one small blob as Alice, which changes no mailbox any suite counts.
//!
//! ```sh
//! STALWART_HTTP_ADDR=127.0.0.1:18080 \
//!   cargo test -p provider-jmap --test live_client -- --nocapture
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use engine_core::error::FailureClass;
use provider_jmap::{
    Credentials, JmapAccountId, JmapClient, JmapConfig, JmapError, Request, ResultRef, blob,
    capability,
};
use serde_json::{Map, Value, json};
use stalwart_harness::Harness;

async fn connect() -> Option<JmapClient> {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_client: STALWART_HTTP_ADDR unset");
        return None;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("harness ready");
    let client = JmapClient::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
        engine_http::test_server::user_agent(),
    ))
    .await
    .expect("connect");
    Some(client)
}

fn ids(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("an id list")
        .iter()
        .map(|id| id.as_str().expect("an id").to_owned())
        .collect()
}

#[tokio::test]
async fn live_a_result_reference_resolves_and_a_method_error_answers_under_its_own_call() {
    let Some(client) = connect().await else {
        return;
    };
    let account = client.session().mail_account_id().expect("mail account");

    let mut request = Request::new([capability::CORE, capability::MAIL]);
    let query = request.invoke("Mailbox/query", json!({ "accountId": account }));
    let mut get = Map::new();
    get.insert("accountId".to_owned(), json!(account));
    get.insert("properties".to_owned(), json!(["id", "role"]));
    ResultRef::new(&query, "Mailbox/query", "/ids").set_on(&mut get, "ids");
    let got = request.invoke("Mailbox/get", get.into());
    // An account no session lists: the server refuses this one call and runs the rest.
    let refused = request.invoke(
        "Mailbox/get",
        json!({ "accountId": JmapAccountId::new("no-such-account") }),
    );
    let after = request.invoke("Mailbox/query", json!({ "accountId": account }));

    let response = client.execute(&request).await.expect("execute");

    let listed = ids(&response.result(&query).expect("query")["ids"]);
    let fetched: Vec<String> = response.result(&got).expect("get")["list"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|mailbox| mailbox["id"].as_str().expect("id").to_owned())
        .collect();
    assert!(!listed.is_empty(), "Alice's seeded mailboxes");
    assert_eq!(fetched, listed, "the reference resolved to the query's ids");

    match response.result(&refused) {
        Err(JmapError::Method {
            call_id,
            error_type,
        }) => {
            eprintln!("a call to an unknown account answers {error_type:?}");
            assert_eq!(
                call_id, refused,
                "the error answers the call that caused it"
            );
            assert_eq!(
                JmapError::Method {
                    call_id,
                    error_type
                }
                .failure_class(),
                FailureClass::Permanent
            );
        }
        other => panic!("expected a method error, got {other:?}"),
    }
    assert_eq!(
        ids(&response.into_result(&after).expect("the call after it")["ids"]),
        listed,
        "a method error ends only its own call"
    );
}

#[tokio::test]
async fn live_an_upload_downloads_back_byte_for_byte_whole_and_streamed() {
    let Some(client) = connect().await else {
        return;
    };
    let session = client.session();
    let account = session.mail_account_id().expect("mail account");
    let run = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // Larger than one read, so the stream is shown to join several pieces.
    let bytes: Vec<u8> = format!("live_client run {run}\r\n")
        .into_bytes()
        .into_iter()
        .cycle()
        .take(256 * 1024)
        .collect();

    let upload = blob::upload_url(session.upload_url().expect("uploadUrl"), account);
    let id = client
        .upload(&upload, "application/octet-stream", &bytes)
        .await
        .expect("upload");
    let download = blob::download_url(
        session.download_url().expect("downloadUrl"),
        account,
        &id,
        "application/octet-stream",
        "probe.bin",
    );

    let whole = client.download(&download).await.expect("download");
    assert_eq!(whole, bytes, "whole");

    let mut stream = client.download_stream(&download).await.expect("stream");
    eprintln!("the server states the size: {:?}", stream.size());
    let mut streamed = Vec::new();
    let mut pieces = 0;
    while let Some(chunk) = stream.next_chunk().await.expect("a piece") {
        streamed.extend_from_slice(&chunk);
        pieces += 1;
    }
    eprintln!("streamed {} bytes in {pieces} pieces", streamed.len());
    assert_eq!(streamed, bytes, "streamed");
}
