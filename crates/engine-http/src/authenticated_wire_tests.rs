//! What [`AuthenticatedClient`](super::AuthenticatedClient) puts on the wire whatever built the
//! request, and how it reads a refusal's body: best effort, capped in size and time, so a `401`
//! stays the refusal it is.

use std::{
    io::Write,
    net::TcpListener,
    sync::{Arc, Mutex},
    time::Duration,
};

use engine_provider::{HandOver, Unrecorded};

use super::tests::{Failure, OK, Tokens, account, client, judge, server, token};
use crate::Exchange;

/// Serves `replies` in order, one per connection, each written whole and the connection then
/// closed; counts the connections.
fn raw_server(replies: Vec<Vec<u8>>) -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let count = Arc::new(Mutex::new(0));
    let counted = Arc::clone(&count);
    std::thread::spawn(move || {
        for (index, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { break };
            *counted.lock().unwrap() += 1;
            let reply = replies[index.min(replies.len() - 1)].clone();
            std::thread::spawn(move || {
                let _ = crate::test_server::read_request_blocking(&mut stream);
                let _ = stream.write_all(&reply);
            });
        }
    });
    (format!("http://{addr}/"), count)
}

fn refusal_head(length: usize) -> String {
    format!(
        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Refused\r\nContent-Length: {length}\r\n\
         Connection: close\r\n\r\n"
    )
}

async fn send_as(
    source: &Arc<Tokens>,
    url: &str,
    exchange: Exchange<'_>,
) -> Result<crate::Sent, Failure> {
    account(source, url)
        .send(
            client().post(url).body("the request"),
            exchange,
            |builder, credential| Ok::<_, Failure>(builder.bearer_auth(token(credential))),
            judge,
        )
        .await
}

/// A GET to `url` from an account configured there, and the reply as it came.
async fn get_sent(source: &Arc<Tokens>, url: &str) -> Result<crate::Sent, Failure> {
    account(source, url)
        .send(
            client().get(url),
            Exchange::Ordinary,
            |builder, credential| Ok::<_, Failure>(builder.bearer_auth(token(credential))),
            judge,
        )
        .await
}

#[tokio::test]
async fn a_refusal_cut_off_mid_body_is_still_a_refusal_of_a_submission() {
    // Content-Length 100, ten bytes, then the connection closes. A 401 says the request was
    // not processed: a submission refused so must not read as possibly sent.
    let mut reply = refusal_head(100).into_bytes();
    reply.extend_from_slice(b"0123456789");
    let (url, _) = raw_server(vec![reply]);
    let source = Arc::new(Tokens::new("A", &[]));
    let hand_over = HandOver::new(&Unrecorded);
    let sent = send_as(&source, &url, Exchange::Submission(&hand_over))
        .await
        .expect("the refusal comes back");
    assert_eq!(sent.status().as_u16(), 401);
    assert_eq!(sent.read_body(), Some(&b"0123456789"[..]));
}

#[tokio::test]
async fn a_huge_refusal_is_read_no_further_than_its_cap() {
    let length = 4 * 1024 * 1024;
    let mut reply = refusal_head(length).into_bytes();
    reply.resize(reply.len() + length, b'x');
    let (url, _) = raw_server(vec![reply]);
    let source = Arc::new(Tokens::new("A", &[]));
    let sent = send_as(&source, &url, Exchange::Ordinary)
        .await
        .expect("the refusal comes back");
    assert_eq!(sent.status().as_u16(), 401);
    assert_eq!(sent.read_body().map(<[u8]>::len), Some(64 * 1024));
}

#[tokio::test]
async fn a_huge_refusal_still_lets_the_replacement_through() {
    let length = 4 * 1024 * 1024;
    let mut refusal = refusal_head(length).into_bytes();
    refusal.resize(refusal.len() + length, b'x');
    let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
    let (url, connections) = raw_server(vec![refusal, ok]);
    let source = Arc::new(Tokens::new("A", &["B"]));
    let sent = send_as(&source, &url, Exchange::Ordinary)
        .await
        .expect("replayed");
    assert_eq!(sent.status().as_u16(), 200);
    assert_eq!(*connections.lock().unwrap(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_trickling_refusal_is_cut_off_and_still_a_refusal() {
    let url = crate::test_server::trickling(
        refusal_head(1_000_000),
        "x".to_owned(),
        1000,
        Duration::from_secs(1),
    )
    .await;
    let source = Arc::new(Tokens::new("A", &[]));
    let started = tokio::time::Instant::now();
    let sent = get_sent(&source, &url)
        .await
        .expect("the refusal comes back");
    assert_eq!(sent.status().as_u16(), 401);
    assert!(
        started.elapsed() <= Duration::from_secs(6),
        "cut off after its five seconds: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_request_built_by_a_client_that_follows_redirects_does_not_follow_one() {
    let (elsewhere, seen) = server(vec![OK]);
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nLocation: {elsewhere}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    let (url, _) = raw_server(vec![redirect]);
    let source = Arc::new(Tokens::new("A", &[]));
    // reqwest's default client follows up to ten redirects.
    let following = reqwest::Client::builder()
        .build()
        .expect("a client that follows redirects");
    let sent = account(&source, &url)
        .send(
            following.get(&url),
            Exchange::Ordinary,
            |builder, credential| Ok::<_, Failure>(builder.bearer_auth(token(credential))),
            judge,
        )
        .await
        .expect("answered");
    assert_eq!(sent.status().as_u16(), 302, "handed back, not followed");
    assert_eq!(
        seen.lock().unwrap().len(),
        0,
        "the credential went nowhere else"
    );
}

/// `Debug` shows where the account is configured, never what its URL may carry besides: a host
/// that put a password in the userinfo or a token in the query does not see it printed.
#[test]
fn debug_names_the_configured_origin_only() {
    let source = Arc::new(Tokens::new("A", &[]));
    let client = account(
        &source,
        "https://someone:hunter2@jmap.example.com:8443/jmap/session?access_token=t0ken#frag",
    );
    let printed = format!("{client:?}");
    assert!(
        printed.contains("https://jmap.example.com:8443"),
        "{printed}"
    );
    for secret in ["someone", "hunter2", "t0ken", "/jmap/session", "frag"] {
        assert!(!printed.contains(secret), "{secret} printed: {printed}");
    }
}
