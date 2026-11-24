//! [`client_for`] and the environment's proxy: an account configured on plaintext loopback
//! reaches its server directly whatever `HTTP_PROXY` says, because its credential is the one
//! that goes in cleartext; every other account keeps the proxy.
//!
//! reqwest reads the proxy variables when a client is built, and changing this process's
//! environment would race every other test, so each case runs the `proxied_child` test in a
//! child process with the variables set there.

use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use reqwest::Url;

use super::client_for;

/// Set in the child: the URL to send one GET to.
const TARGET: &str = "ENGINE_HTTP_PROXY_TEST_TARGET";
/// Set in the child: the account's configured origin.
const CONFIGURED: &str = "ENGINE_HTTP_PROXY_TEST_CONFIGURED";

/// A listener that answers every connection `200` and counts them.
fn counting() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let count = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&count);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            counted.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    (format!("http://{addr}/"), count)
}

/// Runs `proxied_child` with `HTTP_PROXY` set to `proxy` and no other proxy variable.
fn in_child(target: &str, configured: &str, proxy: &str) {
    let mut child = Command::new(std::env::current_exe().expect("this test binary"));
    child
        .args([
            "--exact",
            "deadline::client_proxy_tests::proxied_child",
            "--test-threads=1",
        ])
        .env(TARGET, target)
        .env(CONFIGURED, configured)
        .env("HTTP_PROXY", proxy);
    for unset in [
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "NO_PROXY",
        "no_proxy",
        "REQUEST_METHOD",
    ] {
        child.env_remove(unset);
    }
    let status = child.status().expect("the child runs");
    assert!(status.success(), "the child's request failed: {status}");
}

/// The child's half: one GET to `TARGET` from a client built for `CONFIGURED`. Does nothing
/// when run as an ordinary test.
#[tokio::test]
async fn proxied_child() {
    let (Ok(target), Ok(configured)) = (std::env::var(TARGET), std::env::var(CONFIGURED)) else {
        return;
    };
    let configured = Url::parse(&configured).expect("a URL");
    let client = client_for(
        &engine_tls::TlsClientConfig::bundled(),
        &crate::test_server::user_agent(),
        &configured,
    )
    .build()
    .expect("a client");
    let reply = client.get(&target).send().await.expect("answered");
    assert_eq!(reply.status().as_u16(), 200);
}

#[test]
fn an_account_on_plaintext_loopback_bypasses_the_proxy() {
    let (server, direct) = counting();
    let (proxy, proxied) = counting();
    in_child(&server, &server, &proxy);
    assert_eq!(direct.load(Ordering::SeqCst), 1, "the server was reached");
    assert_eq!(proxied.load(Ordering::SeqCst), 0, "the proxy saw nothing");
}

#[test]
fn an_account_configured_over_https_keeps_the_proxy() {
    let (server, direct) = counting();
    let (proxy, proxied) = counting();
    in_child(&server, "https://jmap.example.com/", &proxy);
    assert_eq!(proxied.load(Ordering::SeqCst), 1, "the proxy carried it");
    assert_eq!(direct.load(Ordering::SeqCst), 0);
}
