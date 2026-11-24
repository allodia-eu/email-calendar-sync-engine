//! [`send_authenticated`](super::send_authenticated) after a refusal: the refused reply is read
//! before the source is asked to replace it, so its connection is free, and a source that fails
//! then ends the request with its own failure.

use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
};

use engine_core::error::FailureClass;

use super::tests::{Failure, OK, REFUSED, Tokens, get, server};

#[tokio::test]
async fn a_source_that_fails_to_replace_ends_the_request_with_its_own_failure() {
    let (url, seen) = server(vec![REFUSED, OK]);
    let mut source = Tokens::new("A", &[]);
    source.replace_fails = true;
    let failure = get(&source, &url).await.expect_err("the source failed");
    assert!(
        matches!(&failure, Failure::Source(err) if err.class() == FailureClass::Retryable),
        "{failure:?}"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "the refused request was sent"
    );
}

/// Serves `replies` in order over connections it keeps open, and counts the connections.
fn keep_alive_server(replies: Vec<&'static str>) -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let connections = Arc::new(Mutex::new(0));
    let counted = Arc::clone(&connections);
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            *counted.lock().unwrap() += 1;
            let replies = Arc::clone(&replies);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if line != "\r\n" {
                        continue;
                    }
                    let Some(reply) = replies.lock().unwrap().pop_front() else {
                        return;
                    };
                    // Larger than a read buffer, so the reply is not whole until it is read.
                    let body = "x".repeat(256 * 1024);
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 {reply}\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    );
                }
            });
        }
    });
    (format!("http://{addr}/"), connections)
}

#[tokio::test]
async fn the_refused_reply_is_read_before_the_replacement_so_its_connection_is_reused() {
    let (url, connections) = keep_alive_server(vec![REFUSED, OK]);
    let source = Tokens::new("A", &["B"]);
    assert_eq!(get(&source, &url).await.expect("sent"), 200);
    assert_eq!(
        *connections.lock().unwrap(),
        1,
        "the replay went over the same connection"
    );
}
