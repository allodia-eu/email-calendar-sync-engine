//! An offline JMAP server that answers what it is **asked**, and refuses what a real one would.
//!
//! [`FakeExecutor`](super::provider_test_support::FakeExecutor) replays canned bytes whatever it
//! is sent, so it cannot fail a request that is too large. This one holds objects per type and
//! evaluates each call: `Foo/query` pages by `position` or by `anchor`/`anchorOffset` (answering
//! `anchorNotFound` for an id it no longer holds) up to `limit`, optionally capping the page the
//! way a server may and ordering by a property an update can change, `Foo/get` resolves `#ids`
//! back-references and refuses more than `maxObjectsInGet` ids with `requestTooLarge` (RFC 8620
//! §5.1, including `ids: null`), and `Foo/changes` pages its log by `maxChanges` (§5.2). A test can
//! also change the data between two requests, which is how a pass that spans several requests is
//! shown to stay correct.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use reqwest::Url;
use serde_json::{Map, Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    request::{Request, Response},
    session::{Session, SessionUrlPolicy},
};

/// One change in a type's log; the log's index + 1 is the state it produced.
enum Change {
    Upserted { id: String, created: bool },
    Destroyed(String),
}

/// One JMAP type's objects (ordered by id, which is the query order) and its change log.
#[derive(Default)]
pub(crate) struct TypeStore {
    objects: BTreeMap<String, Value>,
    log: Vec<Change>,
    /// The state the store began at: a `/changes` from before it cannot be calculated.
    floor: usize,
}

impl TypeStore {
    fn state(&self) -> usize {
        self.floor + self.log.len()
    }

    /// Adds or replaces `object` (keyed by its `id`) and logs the change.
    pub(crate) fn upsert(&mut self, object: Value) {
        let id = object["id"].as_str().expect("object id").to_owned();
        let created = self.objects.insert(id.clone(), object).is_none();
        self.log.push(Change::Upserted { id, created });
    }

    /// Sets `property` of `id` without logging a change, as a server whose query index catches
    /// up with a write it has already answered for: the order moves, and no state does.
    pub(crate) fn resort(&mut self, id: &str, property: &str, value: Value) {
        if let Some(object) = self.objects.get_mut(id) {
            object[property] = value;
        }
    }

    /// Removes the object `id` and logs it.
    pub(crate) fn destroy(&mut self, id: &str) {
        self.objects.remove(id);
        self.log.push(Change::Destroyed(id.to_owned()));
    }
}

type Mutation = Box<dyn FnOnce(&mut HashMap<String, TypeStore>) + Send>;

pub(crate) struct BoundedServer {
    session: Session,
    max_objects_in_get: usize,
    query_cap: Option<usize>,
    order_by: Option<&'static str>,
    stores: Mutex<HashMap<String, TypeStore>>,
    /// Changes applied right after the request with that index (0-based) was answered.
    between: Mutex<Vec<(usize, Mutation)>>,
    pub(crate) requests: Mutex<Vec<Value>>,
}

impl BoundedServer {
    pub(crate) fn new(max_objects_in_get: usize) -> Self {
        let session_doc = json!({
            "capabilities": {
                "urn:ietf:params:jmap:core": { "maxObjectsInGet": max_objects_in_get },
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:calendars": {},
                "urn:ietf:params:jmap:contacts": {}
            },
            "primaryAccounts": {
                "urn:ietf:params:jmap:mail": "c",
                "urn:ietf:params:jmap:calendars": "c",
                "urn:ietf:params:jmap:contacts": "c"
            },
            "apiUrl": "https://jmap.test.local/api/",
            "downloadUrl": "https://jmap.test.local/download/{accountId}/{blobId}/{name}",
            "uploadUrl": "https://jmap.test.local/upload/{accountId}/"
        });
        let base = Url::parse("https://jmap.test.local").unwrap();
        Self {
            session: Session::parse(&session_doc, &base, SessionUrlPolicy::TrustAdvertised)
                .unwrap(),
            max_objects_in_get,
            query_cap: None,
            order_by: None,
            stores: Mutex::new(HashMap::new()),
            between: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Answers at most `cap` ids per `Foo/query`, whatever `limit` asked for (RFC 8620 §5.5
    /// lets a server cap it, and it says so in the result's `limit`).
    pub(crate) fn with_query_cap(mut self, cap: usize) -> Self {
        self.query_cap = Some(cap);
        self
    }

    /// Orders every query by `property` (then id) instead of by id alone, so an update to that
    /// property moves an object, as a server's own order may.
    pub(crate) fn with_order_by(mut self, property: &'static str) -> Self {
        self.order_by = Some(property);
        self
    }

    /// Seeds `objects` of `type_name` as the store's starting state, with no logged changes.
    pub(crate) fn seed(&self, type_name: &str, objects: impl IntoIterator<Item = Value>) {
        let mut stores = self.stores.lock().unwrap();
        let store = stores.entry(type_name.to_owned()).or_default();
        for object in objects {
            store.upsert(object);
        }
        store.floor += store.log.len();
        store.log.clear();
    }

    /// Changes `type_name`'s store now, as another client would.
    pub(crate) fn change(&self, type_name: &str, change: impl FnOnce(&mut TypeStore)) {
        change(
            self.stores
                .lock()
                .unwrap()
                .entry(type_name.to_owned())
                .or_default(),
        );
    }

    /// Changes `type_name`'s store right after request number `index` (0-based) is answered.
    pub(crate) fn change_after(
        &self,
        index: usize,
        type_name: &'static str,
        change: impl FnOnce(&mut TypeStore) + Send + 'static,
    ) {
        self.between.lock().unwrap().push((
            index,
            Box::new(move |stores| change(stores.entry(type_name.to_owned()).or_default())),
        ));
    }

    /// The state string `type_name` is at now.
    pub(crate) fn state(&self, type_name: &str) -> String {
        let stores = self.stores.lock().unwrap();
        stores
            .get(type_name)
            .map_or(0, TypeStore::state)
            .to_string()
    }

    pub(crate) fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn answer(&self, request: &Value) -> Value {
        let mut stores = self.stores.lock().unwrap();
        let mut responses: Vec<Value> = Vec::new();
        for call in request["methodCalls"].as_array().expect("methodCalls") {
            let (method, args, id) = (call[0].as_str().unwrap(), &call[1], &call[2]);
            let args = resolve_back_references(args, &responses);
            let (type_name, verb) = method.split_once('/').expect("Type/verb");
            let store = stores.entry(type_name.to_owned()).or_default();
            let result = match verb {
                "query" => self.query(store, &args),
                "get" => self.get(store, &args),
                "changes" => changes(store, &args),
                _ => Err("unknownMethod"),
            };
            responses.push(match result {
                Ok(result) => json!([method, result, id]),
                Err(error_type) => json!(["error", { "type": error_type }, id]),
            });
        }
        json!({ "methodResponses": responses, "sessionState": "s" })
    }

    fn query(&self, store: &TypeStore, args: &Value) -> Result<Value, &'static str> {
        let mut ordered: Vec<&String> = store.objects.keys().collect();
        if let Some(property) = self.order_by {
            ordered.sort_by_key(|id| (store.objects[id.as_str()][property].to_string(), *id));
        }
        let position = match args["anchor"].as_str() {
            Some(anchor) => {
                let at = ordered
                    .iter()
                    .position(|id| id.as_str() == anchor)
                    .ok_or("anchorNotFound")?;
                let offset = args["anchorOffset"].as_i64().unwrap_or(0);
                usize::try_from(i64::try_from(at).unwrap() + offset).unwrap_or(0)
            }
            None => args["position"].as_u64().map_or(0, as_count),
        };
        let asked = args["limit"].as_u64().map(as_count);
        let limit = match (asked, self.query_cap) {
            (Some(asked), Some(cap)) => Some(asked.min(cap)),
            (asked, cap) => asked.or(cap),
        };
        let ids: Vec<&String> = ordered
            .into_iter()
            .skip(position)
            .take(limit.unwrap_or(usize::MAX))
            .collect();
        let mut result = json!({
            "accountId": args["accountId"],
            "queryState": store.state().to_string(),
            "canCalculateChanges": false,
            "position": position,
            "ids": ids,
        });
        if args["calculateTotal"].as_bool() == Some(true) {
            result["total"] = json!(store.objects.len());
        }
        if let Some(limit) = limit.filter(|&limit| Some(limit) != asked) {
            result["limit"] = json!(limit);
        }
        Ok(result)
    }

    fn get(&self, store: &TypeStore, args: &Value) -> Result<Value, &'static str> {
        let ids: Vec<String> = match args.get("ids") {
            None | Some(Value::Null) => store.objects.keys().cloned().collect(),
            Some(ids) => ids
                .as_array()
                .expect("ids array")
                .iter()
                .map(|id| id.as_str().unwrap().to_owned())
                .collect(),
        };
        if ids.len() > self.max_objects_in_get {
            return Err("requestTooLarge");
        }
        let (list, not_found): (Vec<_>, Vec<_>) = ids
            .iter()
            .partition(|id| store.objects.contains_key(id.as_str()));
        Ok(json!({
            "accountId": args["accountId"],
            "state": store.state().to_string(),
            "list": list.iter().map(|id| &store.objects[id.as_str()]).collect::<Vec<_>>(),
            "notFound": not_found,
        }))
    }
}

fn changes(store: &TypeStore, args: &Value) -> Result<Value, &'static str> {
    let since: usize = args["sinceState"]
        .as_str()
        .and_then(|state| state.parse().ok())
        .filter(|&state| state >= store.floor && state <= store.state())
        .ok_or("cannotCalculateChanges")?;
    let pending = &store.log[since - store.floor..];
    let max = args["maxChanges"].as_u64().map_or(usize::MAX, as_count);
    let taken = &pending[..pending.len().min(max)];
    let (mut created, mut updated, mut destroyed) = (Vec::new(), Vec::new(), Vec::new());
    for change in taken {
        match change {
            Change::Upserted { id, created: true } => created.push(id),
            Change::Upserted { id, created: false } => updated.push(id),
            Change::Destroyed(id) => destroyed.push(id),
        }
    }
    Ok(json!({
        "accountId": args["accountId"],
        "oldState": since.to_string(),
        "newState": (since + taken.len()).to_string(),
        "hasMoreChanges": taken.len() < pending.len(),
        "created": created,
        "updated": updated,
        "destroyed": destroyed,
    }))
}

fn as_count(n: u64) -> usize {
    usize::try_from(n).expect("a count that fits")
}

/// Replaces each `#name` argument with the value its result reference names (RFC 8620 §3.7):
/// the referenced call's result, at the reference's path (one level, as this crate uses).
fn resolve_back_references(args: &Value, responses: &[Value]) -> Value {
    let mut resolved = Map::new();
    for (key, value) in args.as_object().expect("arguments object") {
        let Some(name) = key.strip_prefix('#') else {
            resolved.insert(key.clone(), value.clone());
            continue;
        };
        let earlier = responses
            .iter()
            .find(|response| response[2] == value["resultOf"])
            .expect("referenced call answered earlier");
        let path = value["path"].as_str().unwrap().trim_start_matches('/');
        resolved.insert(name.to_owned(), earlier[1][path].clone());
    }
    Value::Object(resolved)
}

#[async_trait]
impl Executor for Arc<BoundedServer> {
    async fn execute(&self, request: &Request) -> Result<Response, JmapError> {
        let request = request.to_json();
        let answer = self.answer(&request);
        let index = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            requests.len() - 1
        };
        let due: Vec<Mutation> = {
            let mut between = self.between.lock().unwrap();
            let (due, later) = between.drain(..).partition(|(at, _)| *at == index);
            *between = later;
            due.into_iter().map(|(_, change)| change).collect()
        };
        for change in due {
            change(&mut self.stores.lock().unwrap());
        }
        Response::parse(&answer)
    }

    async fn download(&self, _url: &str) -> Result<Vec<u8>, JmapError> {
        Err(JmapError::status(404, "no blobs here"))
    }

    async fn upload(&self, _url: &str, _media: &str, _bytes: &[u8]) -> Result<String, JmapError> {
        Err(JmapError::status(404, "no uploads here"))
    }

    fn session(&self) -> &Session {
        &self.session
    }
}

/// A minimal valid JSCalendar event, `id` `e<n>`.
pub(crate) fn event(n: usize) -> Value {
    json!({
        "id": format!("e{n:05}"),
        "@type": "Event",
        "uid": format!("uid-{n}@example.com"),
        "calendarIds": { "cal": true },
        "title": format!("Event {n}"),
        "start": "2026-01-05T10:00:00",
        "timeZone": "Europe/Amsterdam",
        "duration": "PT1H"
    })
}

/// A minimal valid JSContact card, `id` `k<n>`.
pub(crate) fn card(n: usize) -> Value {
    json!({
        "id": format!("k{n:05}"),
        "@type": "Card",
        "version": "1.0",
        "uid": format!("urn:uuid:card-{n}"),
        "addressBookIds": { "book": true },
        "name": { "full": format!("Person {n}") }
    })
}

/// A minimal mailbox, `id` `m<n>`.
pub(crate) fn mailbox(n: usize) -> Value {
    json!({ "id": format!("m{n:05}"), "name": format!("Folder {n}") })
}

/// A minimal email, `id` `x<n>`, in one mailbox.
pub(crate) fn email(n: usize) -> Value {
    json!({
        "id": format!("x{n:05}"),
        "mailboxIds": { "inbox": true },
        "keywords": {},
        "receivedAt": "2026-01-05T10:00:00Z",
        "size": 100
    })
}
