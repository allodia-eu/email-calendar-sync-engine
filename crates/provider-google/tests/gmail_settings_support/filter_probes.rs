//! Questions asked of Gmail by hand, each with a filter or label of the run's own: what the engine
//! holds back or relies on, asked as the raw request that asks for it, so each claim in
//! `google.md` and the design note is asserted here, not only printed.

use reqwest::Method;
use serde_json::{Value, json};

use super::{reason, send};

/// Creates `filter`, and deletes it again where Gmail stored it: the status and the answer.
async fn try_filter(token: &str, filter: &Value) -> (u16, Value) {
    let (status, answer) = send(token, Method::POST, "/settings/filters", Some(filter)).await;
    if let Some(id) = answer["id"]
        .as_str()
        .filter(|_| (200..300).contains(&status))
    {
        let (deleted, _) = send(
            token,
            Method::DELETE,
            &format!("/settings/filters/{id}"),
            None,
        )
        .await;
        assert!((200..300).contains(&deleted), "probe filter delete");
    }
    (status, answer)
}

/// Asks, with filters holding `marker` (and, for the one with no criteria, none, deleted by id):
/// whether a label just made reads as holding no message, whether Gmail stores two user labels, no
/// criteria, a second identical filter, only a `negatedQuery`, and a query bracketed as the writer
/// brackets a native one.
pub(crate) async fn ask(token: &str, marker: &str) {
    let mut labels = Vec::new();
    for n in 1..=2 {
        let body = json!({"name": format!("{marker}-L{n}"),
            "labelListVisibility": "labelHide", "messageListVisibility": "hide"});
        let (status, label) = send(token, Method::POST, "/labels", Some(&body)).await;
        assert_eq!(status, 200, "label create");
        let id = label["id"].as_str().unwrap().to_owned();
        // What the writer reads before deleting a label it made: a new one holds no message.
        let (status, read) = send(token, Method::GET, &format!("/labels/{id}"), None).await;
        assert_eq!(status, 200, "label read");
        eprintln!(
            "a label just made, messagesTotal: {}",
            read["messagesTotal"]
        );
        assert_eq!(read["messagesTotal"], 0, "a new label holds no message");
        labels.push(id);
    }
    let two = json!({"criteria": {"subject": format!("{marker}-two")},
        "action": {"addLabelIds": labels}});
    let (status, answer) = try_filter(token, &two).await;
    eprintln!(
        "a filter adding two user labels: {status} {}",
        reason(&answer)
    );
    for id in &labels {
        let (status, _) = send(token, Method::DELETE, &format!("/labels/{id}"), None).await;
        assert!((200..300).contains(&status), "label delete");
    }
    assert_eq!(
        (status, reason(&answer).as_str()),
        (400, "invalidArgument"),
        "two labels"
    );

    let empty = json!({"criteria": {}, "action": {"addLabelIds": ["STARRED"]}});
    let (status, answer) = try_filter(token, &empty).await;
    eprintln!("a filter with no criteria: {status} {}", reason(&answer));
    assert_eq!(
        (status, reason(&answer).as_str()),
        (400, "invalidArgument"),
        "no criteria"
    );

    let same = json!({"criteria": {"subject": format!("{marker}-same")},
        "action": {"addLabelIds": ["STARRED"]}});
    let (first, held) = send(token, Method::POST, "/settings/filters", Some(&same)).await;
    let (status, answer) = try_filter(token, &same).await;
    eprintln!(
        "a second identical filter: {first} then {status} {}",
        reason(&answer)
    );
    if let Some(id) = held["id"].as_str() {
        send(
            token,
            Method::DELETE,
            &format!("/settings/filters/{id}"),
            None,
        )
        .await;
    }
    assert_eq!(first, 200, "the first of two identical filters");
    assert_eq!(
        (status, reason(&answer).as_str()),
        (400, "failedPrecondition"),
        "duplicate"
    );

    // `RuleLimit::SomeTest` holds a rule whose only test is an exception.
    let negated = json!({"criteria": {"negatedQuery": format!("subject:{marker}-n")},
        "action": {"addLabelIds": ["STARRED"]}});
    let (status, _) = try_filter(token, &negated).await;
    eprintln!("a filter with only a negatedQuery: {status}");
    assert_eq!(status, 200, "only a negatedQuery");

    // A typed term after a native query, bracketed as the writer brackets it: stored as sent.
    let grouped = json!({"criteria": {
            "query": format!("(subject:{marker}-g1 OR subject:{marker}-g2) has:attachment"),
            "negatedQuery": format!("(subject:{marker}-g3 larger:9) OR subject:{marker}-g4")},
        "action": {"addLabelIds": ["STARRED"]}});
    let (status, answer) = try_filter(token, &grouped).await;
    eprintln!("a bracketed query and negatedQuery: {status}");
    assert_eq!(status, 200, "a bracketed query");
    assert_eq!(answer["criteria"], grouped["criteria"], "stored as sent");
}
