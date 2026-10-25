//! Gated live check that a scope larger than one `/get` syncs whole against the Stalwart harness.
//! Skips with no `STALWART_HTTP_ADDR`.
//!
//! A server refuses a `/get` of more than `maxObjectsInGet` ids with `requestTooLarge` (RFC 8620
//! §5.1) rather than truncating it, so an adapter that asks for a whole scope in one call works
//! on every small account and fails on every large one. The seeded account is small, so each
//! scenario seeds a throwaway collection with more objects than one `/get` may carry, syncs it
//! through the adapter as a snapshot and then as a delta, and removes the collection again.

use engine_core::sync::{SyncState, SyncUpdate};
use engine_provider::{ContactSourceSync, ContactsProvider, Provider};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use serde_json::{Value, json};
use stalwart_harness::Harness;

const CALENDARS: &str = "urn:ietf:params:jmap:calendars";
const CONTACTS: &str = "urn:ietf:params:jmap:contacts";
const COLLECTION: &str = "Live scope limit";

fn account() -> engine_core::ids::AccountId {
    engine_core::ids::AccountId::try_from("live").unwrap()
}

/// One raw JMAP call, answered with its result arguments.
fn call(harness: &Harness, using: &str, method: &str, args: &Value) -> Value {
    let body = json!({
        "using": ["urn:ietf:params:jmap:core", using],
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

struct Scope {
    harness: Harness,
    session: Value,
}

impl Scope {
    async fn start(name: &str) -> Option<(Self, JmapProvider)> {
        let Some(harness) = Harness::from_env() else {
            eprintln!("skipping {name}: STALWART_HTTP_ADDR unset");
            return None;
        };
        harness
            .wait_until_ready(std::time::Duration::from_secs(30))
            .expect("ready");
        let session = harness.jmap_session().expect("session");
        let provider = JmapProvider::connect(JmapConfig::new(
            format!("http://{}", harness.http_addr),
            Credentials::basic(&harness.account, &harness.password),
            engine_http::test_server::user_agent(),
        ))
        .await
        .expect("connect");
        Some((Self { harness, session }, provider))
    }

    fn account_for(&self, urn: &str) -> String {
        self.session["primaryAccounts"][urn]
            .as_str()
            .expect("primary account")
            .to_owned()
    }

    /// One more object than a single `/get` may carry, and a few to spare.
    fn beyond_one_get(&self) -> usize {
        let max = self.session["capabilities"]["urn:ietf:params:jmap:core"]["maxObjectsInGet"]
            .as_u64()
            .expect("maxObjectsInGet");
        usize::try_from(max).unwrap() + 7
    }

    fn per_set(&self) -> usize {
        let max = self.session["capabilities"]["urn:ietf:params:jmap:core"]["maxObjectsInSet"]
            .as_u64()
            .unwrap_or(100);
        usize::try_from(max).unwrap()
    }

    /// Removes any collection of `type_name` a prior run left, then creates a fresh one, which
    /// is removed again when the returned guard drops, a failed assertion included.
    fn fresh_collection<'a>(
        &'a self,
        urn: &'static str,
        type_name: &'static str,
        remove_arg: &'static str,
    ) -> Collection<'a> {
        let account = self.account_for(urn);
        let existing = call(
            &self.harness,
            urn,
            &format!("{type_name}/get"),
            &json!({ "accountId": account, "ids": null, "properties": ["name"] }),
        );
        let stale: Vec<&Value> = existing["list"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["name"] == COLLECTION)
            .map(|c| &c["id"])
            .collect();
        if !stale.is_empty() {
            self.remove_collection(urn, type_name, remove_arg, &json!(stale));
        }
        let created = call(
            &self.harness,
            urn,
            &format!("{type_name}/set"),
            &json!({ "accountId": account, "create": { "c": { "name": COLLECTION } } }),
        );
        let id = created["created"]["c"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{type_name} created: {created}"))
            .to_owned();
        Collection {
            scope: self,
            urn,
            type_name,
            remove_arg,
            id,
        }
    }

    fn remove_collection(&self, urn: &str, type_name: &str, remove_arg: &str, ids: &Value) {
        let destroyed = call(
            &self.harness,
            urn,
            &format!("{type_name}/set"),
            &json!({ "accountId": self.account_for(urn), "destroy": ids, remove_arg: true }),
        );
        assert!(
            destroyed["notDestroyed"].is_null()
                || destroyed["notDestroyed"].as_object().unwrap().is_empty(),
            "{destroyed}"
        );
    }

    /// Creates `objects` in batches the server accepts.
    fn create(&self, urn: &str, type_name: &str, objects: &[Value]) {
        for batch in objects.chunks(self.per_set()) {
            let create: serde_json::Map<String, Value> = batch
                .iter()
                .enumerate()
                .map(|(n, object)| (format!("n{n}"), object.clone()))
                .collect();
            let result = call(
                &self.harness,
                urn,
                &format!("{type_name}/set"),
                &json!({ "accountId": self.account_for(urn), "create": create }),
            );
            assert_eq!(
                result["created"]
                    .as_object()
                    .map_or(0, serde_json::Map::len),
                batch.len(),
                "{type_name}/set created every object: {}",
                result["notCreated"]
            );
        }
    }
}

/// A collection a scenario created, removed with its contents when the guard drops.
struct Collection<'a> {
    scope: &'a Scope,
    urn: &'static str,
    type_name: &'static str,
    remove_arg: &'static str,
    id: String,
}

impl Drop for Collection<'_> {
    fn drop(&mut self) {
        // No assertion here: a drop during a failed test's unwind that panicked again would
        // abort the run and hide the first failure.
        let _ = self.scope.harness.jmap_post(
            json!({
                "using": ["urn:ietf:params:jmap:core", self.urn],
                "methodCalls": [[format!("{}/set", self.type_name), {
                    "accountId": self.scope.account_for(self.urn),
                    "destroy": [self.id],
                    self.remove_arg: true,
                }, "0"]],
            })
            .to_string()
            .as_bytes(),
        );
    }
}

fn events(calendar: &str, prefix: &str, count: usize) -> Vec<Value> {
    (0..count)
        .map(|n| {
            json!({
                "@type": "Event",
                "uid": format!("{prefix}-{n}@test.local"),
                "calendarIds": { calendar: true },
                "title": format!("Scope limit {n}"),
                "start": "2026-03-02T09:00:00",
                "timeZone": "Europe/Amsterdam",
                "duration": "PT30M"
            })
        })
        .collect()
}

fn uids_with(events: &[engine_core::calendar::Event], prefix: &str) -> usize {
    events
        .iter()
        .filter(|event| event.uid.as_str().starts_with(prefix))
        .count()
}

#[tokio::test]
async fn an_event_scope_larger_than_one_get_syncs_whole() {
    let Some((scope, provider)) =
        Scope::start("an_event_scope_larger_than_one_get_syncs_whole").await
    else {
        return;
    };
    let count = scope.beyond_one_get();
    let collection = scope.fresh_collection(CALENDARS, "Calendar", "onDestroyRemoveEvents");
    let calendar = collection.id.clone();
    scope.create(
        CALENDARS,
        "CalendarEvent",
        &events(&calendar, "scope-snap", count),
    );

    let snapshot = provider
        .sync_events(&account(), None)
        .await
        .expect("snapshot");
    let SyncUpdate::Snapshot { objects, .. } = &snapshot.update else {
        panic!("a pass over an unchanging account is a snapshot");
    };
    assert_eq!(uids_with(objects, "scope-snap-"), count);

    scope.create(
        CALENDARS,
        "CalendarEvent",
        &events(&calendar, "scope-delta", count),
    );
    let delta = provider
        .sync_events(&account(), Some(&snapshot.next_cursor))
        .await
        .expect("delta");
    let SyncUpdate::Delta { changed, .. } = &delta.update else {
        panic!("a cursor gives a delta");
    };
    assert_eq!(uids_with(changed, "scope-delta-"), count);
}

#[tokio::test]
async fn a_contact_scope_larger_than_one_get_syncs_whole() {
    let Some((scope, provider)) =
        Scope::start("a_contact_scope_larger_than_one_get_syncs_whole").await
    else {
        return;
    };
    let count = scope.beyond_one_get();
    let collection = scope.fresh_collection(CONTACTS, "AddressBook", "onDestroyRemoveContents");
    let book = collection.id.clone();
    let cards = |prefix: &str| -> Vec<Value> {
        (0..count)
            .map(|n| {
                json!({
                    "@type": "Card",
                    "version": "1.0",
                    "uid": format!("urn:uuid:{prefix}-{n}"),
                    "addressBookIds": { book.as_str(): true },
                    "name": { "full": format!("Scope Limit {n}") }
                })
            })
            .collect()
    };
    let in_scope = |cards: &[engine_core::contact::ContactCard], prefix: &str| {
        cards
            .iter()
            .filter(|card| card.uid.as_deref().is_some_and(|uid| uid.contains(prefix)))
            .count()
    };
    scope.create(CONTACTS, "ContactCard", &cards("scope-snap"));

    let ContactSourceSync::Available { sync: snapshot, .. } = provider
        .sync_contacts(&account(), None)
        .await
        .expect("snapshot")
    else {
        panic!("the harness account has contacts");
    };
    let SyncUpdate::Snapshot { objects, .. } = &snapshot.update else {
        panic!("a pass over an unchanging account is a snapshot");
    };
    assert_eq!(in_scope(objects, "scope-snap-"), count);

    scope.create(CONTACTS, "ContactCard", &cards("scope-delta"));
    let cursor: &SyncState = &snapshot.next_cursor;
    let ContactSourceSync::Available { sync: delta, .. } = provider
        .sync_contacts(&account(), Some(cursor))
        .await
        .expect("delta")
    else {
        panic!("the harness account has contacts");
    };
    let SyncUpdate::Delta { changed, .. } = &delta.update else {
        panic!("a cursor gives a delta");
    };
    assert_eq!(in_scope(changed, "scope-delta-"), count);
}
