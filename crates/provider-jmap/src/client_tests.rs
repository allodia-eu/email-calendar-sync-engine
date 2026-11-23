//! The public blob methods of [`JmapClient`] over a mock HTTP server: what comes back, and the
//! one rule they share, that the account's credential goes only to the origin the session
//! advertised for blobs.

use engine_core::{error::FailureClass, ids::BlobId};

use super::{RICH_SESSION_DOC, SESSION_DOC, http_bytes, http_ok, mock_server, rich_config};
use crate::{JmapAccountId, JmapClient, JmapError, blob};

/// A client over a mock that serves the rich session, then `replies` in order.
async fn client(replies: &[String]) -> (JmapClient, String) {
    let mut all = vec![http_ok(RICH_SESSION_DOC)];
    all.extend_from_slice(replies);
    let base = mock_server(all);
    let client = JmapClient::connect(rich_config(base.clone()))
        .await
        .unwrap();
    (client, base)
}

fn download_url(client: &JmapClient) -> String {
    let session = client.session();
    blob::download_url(
        session.download_url().unwrap(),
        session.mail_account_id().unwrap(),
        &BlobId::try_from("b1").unwrap(),
        "message/rfc822",
        "m.eml",
    )
}

#[tokio::test]
async fn a_streamed_download_arrives_whole_and_says_its_size() {
    let body = "From: a@example.com\r\n\r\nstreamed body";
    let (client, _) = client(&[http_bytes(body)]).await;

    let mut blob = client
        .download_stream(&download_url(&client))
        .await
        .unwrap();
    assert_eq!(blob.size(), Some(body.len() as u64));
    assert!(format!("{blob:?}").contains("BlobDownload"));
    let mut read = Vec::new();
    while let Some(chunk) = blob.next_chunk().await.unwrap() {
        read.extend_from_slice(&chunk);
    }
    assert_eq!(read, body.as_bytes());
}

#[tokio::test]
async fn a_streamed_download_that_fails_says_so_before_any_byte_is_read() {
    let missing = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (client, _) = client(&[missing.to_owned()]).await;

    let err = client
        .download_stream(&download_url(&client))
        .await
        .unwrap_err();
    assert!(
        matches!(err, JmapError::Status { status: 404, .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn an_upload_answers_the_blob_id_the_server_stored() {
    let stored = r#"{"accountId":"c","blobId":"G7","type":"message/rfc822","size":3}"#;
    let (client, _) = client(&[http_ok(stored)]).await;
    let session = client.session();
    let url = blob::upload_url(session.upload_url().unwrap(), &JmapAccountId::new("c"));

    let blob = client.upload(&url, "message/rfc822", b"abc").await.unwrap();
    assert_eq!(blob.as_str(), "G7");
}

/// The mock serves the session and nothing else, so a request that went out would fail as a
/// transport error; a refusal is the client declining before it sent anything.
#[tokio::test]
async fn a_blob_request_to_another_origin_is_refused_before_it_is_sent() {
    let (client, base) = client(&[]).await;
    let elsewhere = "http://elsewhere.example.com/download/c/b1/m.eml";
    let refused = |err: JmapError| {
        assert!(matches!(err, JmapError::Refused(_)), "{err:?}");
        assert_eq!(err.failure_class(), FailureClass::Permanent);
    };

    refused(client.download(elsewhere).await.unwrap_err());
    refused(client.download_stream(elsewhere).await.unwrap_err());
    refused(
        client
            .upload("http://elsewhere.example.com/upload/c/", "text/plain", b"x")
            .await
            .unwrap_err(),
    );
    // Same host, another port: a different origin too. Port 9 (discard) is in range and is not
    // the mock's.
    assert!(!base.ends_with(":9"));
    refused(
        client
            .upload("http://127.0.0.1:9/upload/c/", "text/plain", b"x")
            .await
            .unwrap_err(),
    );
}

/// A server that answers `replies` in order and hands back every request it read, so a test
/// sees what was sent where.
fn capturing_server(replies: Vec<String>) -> (String, std::sync::mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for reply in replies {
            let (mut stream, _) = listener.accept().unwrap();
            let request = engine_http::test_server::read_request_blocking(&mut stream);
            let _ = sent.send(String::from_utf8_lossy(&request).into_owned());
            let _ = std::io::Write::write_all(&mut stream, reply.as_bytes());
        }
    });
    (format!("http://{addr}"), received)
}

/// Fastmail's shape: the API on one origin, blobs on a separate user-content origin. The
/// credential goes to the blob origin the session named, and a blob path on the API origin,
/// which the session did not name for blobs, is refused.
#[tokio::test]
async fn blobs_on_their_own_origin_get_the_credential_and_the_api_origin_does_not() {
    let (blobs, sent) = capturing_server(vec![
        http_bytes("raw message"),
        http_ok(r#"{"blobId":"G9"}"#),
    ]);
    let session = format!(
        r#"{{"capabilities":{{"urn:ietf:params:jmap:core":{{}},"urn:ietf:params:jmap:mail":{{}}}},
            "primaryAccounts":{{"urn:ietf:params:jmap:mail":"c"}},
            "apiUrl":"https://mail.test.local/jmap/",
            "downloadUrl":"{blobs}/download/{{accountId}}/{{blobId}}/{{name}}?type={{type}}",
            "uploadUrl":"{blobs}/upload/{{accountId}}/"}}"#
    );
    let api = mock_server(vec![http_ok(&session)]);
    let client = JmapClient::connect(rich_config(api.clone())).await.unwrap();
    assert_eq!(
        client
            .session()
            .download_url()
            .unwrap()
            .split("/download")
            .next(),
        Some(blobs.as_str())
    );

    let bytes = client.download(&download_url(&client)).await.unwrap();
    assert_eq!(bytes, b"raw message");
    let upload = blob::upload_url(
        client.session().upload_url().unwrap(),
        &JmapAccountId::new("c"),
    );
    assert_eq!(
        client
            .upload(&upload, "text/plain", b"x")
            .await
            .unwrap()
            .as_str(),
        "G9"
    );
    for request in [sent.recv().unwrap(), sent.recv().unwrap()] {
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: basic"),
            "the blob origin gets the credential: {request}"
        );
    }

    // The same paths on the API origin: not where the session said blobs live.
    let on_api = format!("{api}/download/c/b1/m.eml");
    for err in [
        client.download(&on_api).await.unwrap_err(),
        client.download_stream(&on_api).await.unwrap_err(),
        client
            .upload(&format!("{api}/upload/c/"), "text/plain", b"x")
            .await
            .unwrap_err(),
    ] {
        assert!(matches!(err, JmapError::Refused(_)), "{err:?}");
    }
}

#[tokio::test]
async fn a_session_without_blob_urls_refuses_every_blob_request() {
    let base = mock_server(vec![http_ok(SESSION_DOC)]);
    let client = JmapClient::connect(rich_config(base.clone()))
        .await
        .unwrap();
    let url = format!("{base}/download/c/b1/m");

    for err in [
        client.download(&url).await.unwrap_err(),
        client.download_stream(&url).await.unwrap_err(),
        client.upload(&url, "text/plain", b"x").await.unwrap_err(),
    ] {
        assert!(matches!(err, JmapError::Session(_)), "{err:?}");
    }
}
