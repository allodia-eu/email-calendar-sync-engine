//! Gated live check that a first mail sync survives another device changing the mailbox mid-way,
//! against the Stalwart harness. Skips with no `STALWART_HTTP_ADDR`.
//!
//! `stream_email` fetches a page only when the next chunk is asked for, so the test pauses after
//! the first page and deletes two messages through the harness, as another device would: the
//! newest, which the sync has already read, and the one the next page is anchored after. Paging
//! by position would then skip a message; the walk is anchored and steps back past the deleted
//! anchor, so every message arrives, and the next delta takes the two deleted ones away.

use std::collections::BTreeSet;

use engine_core::sync::{SyncState, SyncUpdate, SyncWindow};
use engine_provider::Provider;
use futures_util::StreamExt;
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use serde_json::{Value, json};
use stalwart_harness::Harness;

const MAIL: &str = "urn:ietf:params:jmap:mail";
const COUNT: usize = 250;
const PAGE: usize = 100;

fn call(harness: &Harness, method: &str, args: &Value) -> Value {
    let body = json!({
        "using": ["urn:ietf:params:jmap:core", MAIL],
        "methodCalls": [[method, args, "0"]],
    });
    let response = harness
        .jmap_post(body.to_string().as_bytes())
        .expect("jmap post");
    let parsed: Value = serde_json::from_str(&response.body_text()).expect("json");
    let answer = &parsed["methodResponses"][0];
    assert_eq!(answer[0], method, "{method} answered {answer}");
    answer[1].clone()
}

/// The throwaway mailbox, destroyed with its messages when the guard drops.
struct Mailbox<'a> {
    harness: &'a Harness,
    account: String,
    id: String,
    name: &'static str,
}

impl<'a> Mailbox<'a> {
    fn fresh(harness: &'a Harness, account: String, name: &'static str) -> Self {
        let existing = call(
            harness,
            "Mailbox/get",
            &json!({ "accountId": account, "ids": null, "properties": ["name"] }),
        );
        let stale: Vec<Value> = existing["list"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|mailbox| mailbox["name"] == name)
            .map(|mailbox| mailbox["id"].clone())
            .collect();
        if !stale.is_empty() {
            call(
                harness,
                "Mailbox/set",
                &json!({ "accountId": account, "destroy": stale, "onDestroyRemoveEmails": true }),
            );
        }
        let created = call(
            harness,
            "Mailbox/set",
            &json!({ "accountId": account, "create": { "m": { "name": name } } }),
        );
        let id = created["created"]["m"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("mailbox created: {created}"))
            .to_owned();
        Self {
            harness,
            account,
            id,
            name,
        }
    }

    /// Creates `COUNT` messages newer than anything seeded, so they lead a newest-first walk,
    /// and answers their ids newest first.
    fn fill(&self) -> Vec<String> {
        let mut ids = Vec::with_capacity(COUNT);
        for batch in (0..COUNT).collect::<Vec<_>>().chunks(PAGE) {
            let create: serde_json::Map<String, Value> = batch
                .iter()
                .map(|&n| {
                    (
                        format!("e{n}"),
                        json!({
                            "mailboxIds": { &self.id: true },
                            "subject": format!("Walk {n}"),
                            "from": [{ "email": "walk@test.local" }],
                            "receivedAt": format!("2099-01-01T{:02}:{:02}:00Z", n / 60, n % 60),
                            "bodyValues": { "b": { "value": "x" } },
                            "textBody": [{ "partId": "b", "type": "text/plain" }],
                        }),
                    )
                })
                .collect();
            let result = call(
                self.harness,
                "Email/set",
                &json!({ "accountId": self.account, "create": create }),
            );
            for &n in batch {
                let id = result["created"][format!("e{n}")]["id"]
                    .as_str()
                    .unwrap_or_else(|| panic!("email created: {}", result["notCreated"]));
                ids.push(id.to_owned());
            }
        }
        ids.reverse();
        ids
    }

    /// The account's newest `limit` message ids, as the walk's own sort lists them.
    fn newest(&self, limit: usize) -> Vec<String> {
        let listed = call(
            self.harness,
            "Email/query",
            &json!({
                "accountId": self.account,
                "sort": [{ "property": "receivedAt", "isAscending": false }],
                "limit": limit,
            }),
        );
        listed["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect()
    }

    /// Waits until `listed` holds for the sorted query. Stalwart sorts a write into
    /// `Email/query` a moment after answering it, so a query asked at once may list a new
    /// message last, or a destroyed one still.
    fn wait_until(&self, what: &str, listed: impl Fn(&[String]) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !listed(&self.newest(COUNT)) {
            assert!(
                std::time::Instant::now() < deadline,
                "the query never {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    /// Creates one message newer than any `fill` makes, and answers its id.
    fn create_newest(&self) -> String {
        let result = call(
            self.harness,
            "Email/set",
            &json!({ "accountId": self.account, "create": { "late": {
                "mailboxIds": { &self.id: true },
                "subject": format!("{} newest", self.name),
                "from": [{ "email": "walk@test.local" }],
                "receivedAt": "2099-12-31T23:59:00Z",
                "bodyValues": { "b": { "value": "x" } },
                "textBody": [{ "partId": "b", "type": "text/plain" }],
            } } }),
        );
        result["created"]["late"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("email created: {}", result["notCreated"]))
            .to_owned()
    }

    fn destroy_emails(&self, ids: &[&str]) {
        let result = call(
            self.harness,
            "Email/set",
            &json!({ "accountId": self.account, "destroy": ids }),
        );
        assert_eq!(
            result["destroyed"].as_array().map_or(0, Vec::len),
            ids.len(),
            "{result}"
        );
    }
}

impl Drop for Mailbox<'_> {
    fn drop(&mut self) {
        // No assertion: a second panic during a failed test's unwind would hide the first.
        let _ = self.harness.jmap_post(
            json!({
                "using": ["urn:ietf:params:jmap:core", MAIL],
                "methodCalls": [["Mailbox/set", {
                    "accountId": self.account,
                    "destroy": [self.id],
                    "onDestroyRemoveEmails": true,
                }, "0"]],
            })
            .to_string()
            .as_bytes(),
        );
    }
}

/// Held for the length of each scenario: both rank their messages first in the account's whole
/// newest-first list, so two at once would interleave there.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The harness, its mail account id and a connected provider; `None` when the harness is unset.
async fn start(name: &str) -> Option<(Harness, String, JmapProvider)> {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping {name}: STALWART_HTTP_ADDR unset");
        return None;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("ready");
    let session = harness.jmap_session().expect("session");
    let account_id = session["primaryAccounts"][MAIL]
        .as_str()
        .expect("mail account")
        .to_owned();
    let provider = JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
        engine_http::test_server::user_agent(),
    ))
    .await
    .expect("connect");
    Some((harness, account_id, provider))
}

fn ids(chunk: &engine_provider::EmailChunk) -> impl Iterator<Item = String> + '_ {
    chunk.present.iter().map(|key| key.as_str().to_owned())
}

#[tokio::test]
async fn a_first_sync_misses_nothing_when_another_device_deletes_mid_way() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some((harness, account_id, provider)) =
        start("a_first_sync_misses_nothing_when_another_device_deletes_mid_way").await
    else {
        return;
    };
    let mailbox = Mailbox::fresh(&harness, account_id, "Live snapshot walk");
    let created = mailbox.fill();
    mailbox.wait_until("sorted the new messages first", |listed| listed == created);
    let account = engine_core::ids::AccountId::try_from("live").unwrap();

    let mut stream = provider.stream_email(&account, None, SyncWindow::full(), PAGE, 0);
    let first = stream
        .next()
        .await
        .expect("a first chunk")
        .expect("first page");
    let first_page: BTreeSet<String> = ids(&first).collect();
    assert!(
        first_page.contains(&created[0]),
        "the newest message leads the walk"
    );
    assert!(
        first_page.contains(&created[PAGE - 1]),
        "the page's last message"
    );
    // Another device deletes the newest message, already read, and the next page's anchor.
    let (newest, anchor) = (created[0].as_str(), created[PAGE - 1].as_str());
    mailbox.destroy_emails(&[newest, anchor]);
    mailbox.wait_until("dropped the deleted messages", |listed| {
        !listed.iter().any(|id| id == newest || id == anchor)
    });

    let mut present = first_page;
    let mut cursor: Option<SyncState> = first.advance_to;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("later page");
        present.extend(ids(&chunk));
        cursor = chunk.advance_to.or(cursor);
    }
    drop(stream);
    let missed: Vec<&String> = created.iter().filter(|id| !present.contains(*id)).collect();
    assert!(missed.is_empty(), "skipped: {missed:?}");

    let delta = provider
        .sync_email(&account, Some(&cursor.expect("the pass's cursor")))
        .await
        .expect("delta");
    let SyncUpdate::Delta { removed, .. } = &delta.update else {
        panic!("a cursor gives a delta");
    };
    let removed: BTreeSet<&str> = removed
        .iter()
        .map(engine_core::ids::ProviderKey::as_str)
        .collect();
    assert!(
        removed.contains(newest) && removed.contains(anchor),
        "{removed:?}"
    );
}

/// Stalwart lists a message it has just stored last and sorts it into place a moment later,
/// leaving `queryState` as it was. A message stored just before the first page is therefore read
/// at the end of the list, moves to the top while the walk is on its later pages, and is in no
/// delta, having been stored before the cursor's state: only the closing listing finds it.
#[tokio::test]
async fn a_message_the_server_sorts_late_is_not_lost() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some((harness, account_id, provider)) =
        start("a_message_the_server_sorts_late_is_not_lost").await
    else {
        return;
    };
    let mailbox = Mailbox::fresh(&harness, account_id, "Live snapshot walk late");
    let created = mailbox.fill();
    mailbox.wait_until("sorted the new messages first", |listed| listed == created);
    let account = engine_core::ids::AccountId::try_from("live").unwrap();

    let late = mailbox.create_newest();
    let mut stream = provider.stream_email(&account, None, SyncWindow::full(), PAGE, 0);
    let first = stream
        .next()
        .await
        .expect("a first chunk")
        .expect("first page");
    let mut present: BTreeSet<String> = ids(&first).collect();
    assert!(
        !present.contains(&late),
        "the server had sorted it already, so the race was not reproduced"
    );
    mailbox.wait_until("sorted the late message first", |listed| {
        listed.first() == Some(&late)
    });
    while let Some(chunk) = stream.next().await {
        present.extend(ids(&chunk.expect("later page")));
    }
    assert!(present.contains(&late), "the late message was lost");
}
