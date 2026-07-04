//! Offline tests for the JMAP EventSource watcher: SSE parsing, `StateChange`
//! classification, and the [`Watch`] event loop driven over scripted chunks — no
//! network, so they run in the always-green suite. The live counterpart (a real change
//! seen over a real stream) is `tests/live_provider.rs`.

use super::*;
use engine_core::error::FailureClass;

/// A [`ChunkSource`] that replays scripted results, FIFO, then reports end-of-stream.
struct ScriptedSource {
    chunks: VecDeque<Result<Option<Vec<u8>>, JmapError>>,
}

#[async_trait]
impl ChunkSource for ScriptedSource {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, JmapError> {
        self.chunks.pop_front().unwrap_or(Ok(None))
    }
}

fn watcher(chunks: Vec<Result<Option<Vec<u8>>, JmapError>>, types: &[JmapDataType]) -> JmapWatcher {
    JmapWatcher::from_source(
        Box::new(ScriptedSource {
            chunks: chunks.into_iter().collect(),
        }),
        types,
    )
}

/// One SSE `data` chunk for a `StateChange` naming the given `type_name`'s new state.
fn state_frame(account: &str, type_name: &str) -> Vec<u8> {
    format!(
        "event: state\ndata: {{\"@type\":\"StateChange\",\"changed\":{{\"{account}\":{{\"{type_name}\":\"s2\"}}}}}}\n\n"
    )
    .into_bytes()
}

// A chunk element for the scripted source. `Result`-typed so it can sit in the same
// vec as the `Err(..)`/end-of-stream cases; clippy's "unnecessary wrap" does not apply
// to that shared element type.
#[allow(
    clippy::unnecessary_wraps,
    reason = "shares the scripted-source element type with Err cases"
)]
fn ok(bytes: Vec<u8>) -> Result<Option<Vec<u8>>, JmapError> {
    Ok(Some(bytes))
}

#[tokio::test]
async fn state_change_for_a_watched_type_is_changed() {
    let mut w = watcher(vec![ok(state_frame("c", "Email"))], &[JmapDataType::Email]);
    assert_eq!(w.next_event().await.unwrap(), WatchEvent::Changed);
}

#[tokio::test]
async fn ping_is_a_keepalive() {
    let mut w = watcher(
        vec![ok(b"event: ping\ndata: {\"interval\":30}\n\n".to_vec())],
        &[JmapDataType::Email],
    );
    assert_eq!(w.next_event().await.unwrap(), WatchEvent::KeepAlive);
}

#[tokio::test]
async fn a_state_change_for_an_unwatched_type_is_skipped() {
    // Watching Email, but Calendar changed: keep reading past it to the following ping.
    let mut w = watcher(
        vec![
            ok(state_frame("c", "Calendar")),
            ok(b"event: ping\ndata: {}\n\n".to_vec()),
        ],
        &[JmapDataType::Email],
    );
    assert_eq!(w.next_event().await.unwrap(), WatchEvent::KeepAlive);
}

#[tokio::test]
async fn a_frame_split_across_chunks_is_buffered() {
    // Split the state frame mid-line; the parser reassembles it into one event.
    let frame = state_frame("c", "Email");
    let (head, tail) = frame.split_at(20);
    let mut w = watcher(
        vec![ok(head.to_vec()), ok(tail.to_vec())],
        &[JmapDataType::Email],
    );
    assert_eq!(w.next_event().await.unwrap(), WatchEvent::Changed);
}

#[tokio::test]
async fn multiple_events_in_one_chunk_are_returned_in_order() {
    let mut combined = state_frame("c", "Email");
    combined.extend_from_slice(b"event: ping\ndata: {}\n\n");
    let mut w = watcher(vec![ok(combined)], &[JmapDataType::Email]);
    assert_eq!(w.next_event().await.unwrap(), WatchEvent::Changed);
    assert_eq!(w.next_event().await.unwrap(), WatchEvent::KeepAlive);
}

#[tokio::test]
async fn a_closed_stream_is_retryable() {
    // No chunks → immediate end-of-stream → the host reconnects.
    let mut w = watcher(vec![], &[JmapDataType::Email]);
    let err = w.next_event().await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Retryable);
}

#[tokio::test]
async fn a_stream_error_propagates_classified() {
    let mut w = watcher(
        vec![Err(JmapError::status(503, "unavailable"))],
        &[JmapDataType::Email],
    );
    let err = w.next_event().await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Retryable);
}

#[tokio::test]
async fn empty_types_watches_every_change() {
    // No explicit types (a `*` subscription): any state change wakes the host.
    let mut w = watcher(vec![ok(state_frame("c", "CalendarEvent"))], &[]);
    assert_eq!(w.next_event().await.unwrap(), WatchEvent::Changed);
}

#[tokio::test]
async fn drives_through_the_watch_trait_object() {
    // Hosts hold the session behind `dyn Watch`; the trait method must work too.
    let mut w: Box<dyn Watch> = Box::new(watcher(
        vec![ok(state_frame("c", "Email"))],
        &[JmapDataType::Email],
    ));
    assert_eq!(w.next().await.unwrap(), WatchEvent::Changed);
}

// --- SSE parser units ---

#[test]
fn parser_reads_event_and_joins_data_lines() {
    let mut parser = SseParser::default();
    let events = parser.push(b"event: state\ndata: line1\ndata: line2\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "state");
    assert_eq!(events[0].data, "line1\nline2");
}

#[test]
fn parser_defaults_event_to_message_and_ignores_comments() {
    let mut parser = SseParser::default();
    // A comment (`:` line) is a keep-alive with no fields; the data-only record still
    // dispatches with the default `message` event name.
    let events = parser.push(b": keep-alive\ndata: hi\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "message");
    assert_eq!(events[0].data, "hi");
}

#[test]
fn parser_handles_crlf_line_endings() {
    let mut parser = SseParser::default();
    let events = parser.push(b"event: ping\r\ndata: {}\r\n\r\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "ping");
    assert_eq!(events[0].data, "{}");
}

#[test]
fn parser_treats_a_bare_field_as_empty_value() {
    let mut parser = SseParser::default();
    // A `data` line with no colon is the `data` field with an empty value.
    let events = parser.push(b"event: state\ndata\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "");
}

#[test]
fn parser_dispatches_nothing_for_an_empty_record() {
    let mut parser = SseParser::default();
    // A lone blank line with nothing pending yields no event.
    assert!(parser.push(b"\n").is_empty());
}

// --- StateChange classification ---

#[test]
fn state_change_hits_matches_a_watched_type() {
    let data = r#"{"@type":"StateChange","changed":{"c":{"Email":"s2","Thread":"t2"}}}"#;
    assert!(state_change_hits(data, &["Email".to_owned()]));
    assert!(!state_change_hits(data, &["Mailbox".to_owned()]));
}

#[test]
fn state_change_hits_matches_across_any_account() {
    let data = r#"{"changed":{"other-acct":{"Mailbox":"m2"}}}"#;
    assert!(state_change_hits(data, &["Mailbox".to_owned()]));
}

#[test]
fn state_change_hits_is_false_for_malformed_or_empty() {
    assert!(!state_change_hits("not json", &["Email".to_owned()]));
    assert!(!state_change_hits(
        r#"{"changed":5}"#,
        &["Email".to_owned()]
    ));
    assert!(!state_change_hits(
        r#"{"noChanged":{}}"#,
        &["Email".to_owned()]
    ));
}

#[test]
fn classify_ignores_unknown_and_empty_state_events() {
    // An unknown event name is not a wake signal.
    let unknown = SseEvent {
        event: "message".to_owned(),
        data: "{}".to_owned(),
    };
    assert!(classify(&unknown, &["Email".to_owned()]).is_none());
    // A `state` event whose payload names no watched type is skipped.
    let state = SseEvent {
        event: "state".to_owned(),
        data: r#"{"changed":{"c":{"Calendar":"s"}}}"#.to_owned(),
    };
    assert!(classify(&state, &["Email".to_owned()]).is_none());
}

#[test]
fn map_open_error_reports_missing_event_source_as_not_watchable() {
    // A server without an EventSource endpoint is not-watchable (InvalidState → poll),
    // not a permanent protocol error.
    let err = map_open_error(JmapError::session("server advertised no eventSourceUrl"));
    assert_eq!(err.class(), FailureClass::InvalidState);
    // Any other failure passes through with its own classification.
    let other = map_open_error(JmapError::status(503, "down"));
    assert_eq!(other.class(), FailureClass::Retryable);
}

#[test]
fn debug_is_redaction_safe() {
    let w = watcher(vec![], &[JmapDataType::Email]);
    assert!(format!("{w:?}").contains("JmapWatcher"));
}

// --- Real transport path (no Docker): a one-shot mock HTTP server drives session
// discovery + the EventSource GET through the actual reqwest client and SSE reader.

/// A blocking mock server that serves canned raw HTTP responses, FIFO, one per
/// connection — enough to exercise the streaming path offline.
fn mock_server(responses: Vec<String>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
        }
    });
    format!("http://{addr}")
}

fn http_response(content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn connect_reads_a_change_over_a_real_event_stream() {
    let session = r#"{"capabilities":{"urn:ietf:params:jmap:core":{},"urn:ietf:params:jmap:mail":{}},"primaryAccounts":{"urn:ietf:params:jmap:mail":"c"},"apiUrl":"https://mail.test.local/jmap/","eventSourceUrl":"https://mail.test.local/eventsource/?types={types}&closeafter={closeafter}&ping={ping}"}"#;
    let sse_body = "event: state\ndata: {\"@type\":\"StateChange\",\"changed\":{\"c\":{\"Email\":\"s2\"}}}\n\n";
    let base = mock_server(vec![
        http_response("application/json", session),
        http_response("text/event-stream", sse_body),
    ]);

    // The whole path: session discovery → open EventSource (URL substitution + status
    // check) → real reqwest streaming chunks → SSE parse → classify → Changed.
    let mut watcher = JmapWatcher::connect(
        crate::JmapConfig::new(base, crate::Credentials::basic("a", "b"))
            .with_session_path("/jmap/session"),
        &[JmapDataType::Email],
        Duration::from_secs(30),
    )
    .await
    .expect("open event source");
    assert_eq!(watcher.next_event().await.unwrap(), WatchEvent::Changed);
}

#[tokio::test]
async fn connect_without_event_source_url_is_not_watchable() {
    // A session with no eventSourceUrl → InvalidState (the host polls), through the
    // real connect path.
    let session = r#"{"capabilities":{"urn:ietf:params:jmap:mail":{}},"primaryAccounts":{"urn:ietf:params:jmap:mail":"c"},"apiUrl":"https://mail.test.local/jmap/"}"#;
    let base = mock_server(vec![http_response("application/json", session)]);
    let err = JmapWatcher::connect(
        crate::JmapConfig::new(base, crate::Credentials::basic("a", "b"))
            .with_session_path("/jmap/session"),
        &[JmapDataType::Email],
        Duration::from_secs(30),
    )
    .await
    .unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
}
