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
    // Same host, another port: a different origin too.
    let other_port = format!("{}0/upload/c/", base.trim_end_matches('/'));
    refused(
        client
            .upload(&other_port, "text/plain", b"x")
            .await
            .unwrap_err(),
    );
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
