//! A fake Gmail that keeps the filters as a real collection, the way the API documents them: a
//! create stores a filter under a new id, a delete removes it (`404` where it is gone), and there
//! is no update. Writes are recorded, and any request can be made to fail, so a test asserts the
//! collection a write leaves, not only the requests it sent.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{
    error::GoogleError,
    transport::{GoogleClient, GoogleTransport},
};

/// The fake's state, shared with the test that built it.
#[derive(Default)]
pub(crate) struct Filters {
    pub(crate) list: Vec<Value>,
    next: usize,
    /// The account's forwarding addresses, as `forwardingAddresses.list` answers them.
    pub(crate) forwarding: Vec<Value>,
    /// The account's labels, as `labels.list` answers them.
    pub(crate) labels: Vec<Value>,
    /// Requests that fail: `(method, filter id, or "new" for a create)` to the status answered.
    pub(crate) failing: BTreeMap<(&'static str, String), u16>,
    /// Every write, as `(method, filter id or "new")`; a label removed as `("DELETE label", id)`.
    pub(crate) writes: Vec<(&'static str, String)>,
    /// Whether a create is stored and then answered with a body that does not read.
    pub(crate) unreadable_create: bool,
    /// How often the forwarding addresses were asked.
    pub(crate) forwarding_reads: usize,
}

impl Filters {
    /// A collection holding `filters` (each needs its `id`).
    pub(crate) fn holding(filters: Vec<Value>) -> Self {
        Self {
            next: filters.len(),
            list: filters,
            ..Self::default()
        }
    }

    /// The ids in the collection, sorted.
    pub(crate) fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .list
            .iter()
            .map(|filter| filter["id"].as_str().unwrap_or_default().to_owned())
            .collect();
        ids.sort();
        ids
    }

    fn fail(&self, method: &'static str, id: &str) -> Result<(), GoogleError> {
        match self.failing.get(&(method, id.to_owned())) {
            Some(status) => Err(GoogleError::status(
                *status,
                json!({"error": {"code": status, "status": "INJECTED"}}).to_string(),
            )),
            None => Ok(()),
        }
    }
}

struct Fake(Arc<Mutex<Filters>>);

const FILTERS: &str = "/settings/filters";

fn id_of(url: &str) -> Option<&str> {
    url.split_once(&format!("{FILTERS}/")).map(|(_, id)| id)
}

#[async_trait]
impl GoogleTransport for Fake {
    async fn get(&self, url: &str) -> Result<Value, GoogleError> {
        let mut state = self.0.lock().expect("fake");
        if url.ends_with(FILTERS) {
            state.fail("GET", "list")?;
            if state.list.is_empty() {
                return Ok(json!({}));
            }
            return Ok(json!({ "filter": state.list }));
        }
        if url.ends_with("/settings/forwardingAddresses") {
            state.forwarding_reads += 1;
            state.fail("GET", "forwarding")?;
            return Ok(json!({ "forwardingAddresses": state.forwarding }));
        }
        if url.ends_with("/labels") {
            return Ok(json!({ "labels": state.labels }));
        }
        Err(GoogleError::protocol(format!(
            "no fake route for GET {url}"
        )))
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, GoogleError> {
        Err(GoogleError::protocol(format!("no fake bytes for {url}")))
    }

    async fn get_bytes_unauthenticated(&self, url: &str) -> Result<Vec<u8>, GoogleError> {
        Err(GoogleError::protocol(format!("no fake bytes for {url}")))
    }

    async fn post(
        &self,
        url: &str,
        _content_type: &str,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GoogleError> {
        let mut state = self.0.lock().expect("fake");
        if url.ends_with("/labels") {
            let mut label: Value = serde_json::from_slice(&body)?;
            let id = format!("Label_{}", 900 + state.labels.len());
            label["id"] = json!(id);
            state.labels.push(label.clone());
            return Ok(Some(label));
        }
        if !url.ends_with(FILTERS) {
            return Err(GoogleError::protocol(format!(
                "no fake route for POST {url}"
            )));
        }
        state.writes.push(("POST", "new".to_owned()));
        state.fail("POST", "new")?;
        let mut filter: Value = serde_json::from_slice(&body)?;
        // Gmail refuses a filter identical to one it holds (observed).
        if state.list.iter().any(|held| {
            held["criteria"] == filter["criteria"] && held["action"] == filter["action"]
        }) {
            let body = json!({"error": {"code": 400, "status": "FAILED_PRECONDITION",
                "errors": [{"reason": "failedPrecondition"}]}});
            return Err(GoogleError::status(400, body.to_string()));
        }
        state.next += 1;
        filter["id"] = json!(format!("new{}", state.next));
        state.list.push(filter.clone());
        if state.unreadable_create {
            return Err(serde_json::from_str::<Value>("not json")
                .unwrap_err()
                .into());
        }
        Ok(Some(filter))
    }

    async fn patch(
        &self,
        url: &str,
        _content_type: &str,
        _if_match: Option<&str>,
        _body: Vec<u8>,
    ) -> Result<Option<Value>, GoogleError> {
        Err(GoogleError::protocol(format!(
            "filters have no update: PATCH {url}"
        )))
    }

    async fn delete(&self, url: &str, _if_match: Option<&str>) -> Result<(), GoogleError> {
        let mut state = self.0.lock().expect("fake");
        if let Some((_, label)) = url.split_once("/labels/") {
            state.writes.push(("DELETE label", label.to_owned()));
            state.labels.retain(|held| held["id"] != label);
            return Ok(());
        }
        let id = id_of(url).ok_or_else(|| GoogleError::protocol("no filter id"))?;
        state.writes.push(("DELETE", id.to_owned()));
        state.fail("DELETE", id)?;
        let at = state
            .list
            .iter()
            .position(|filter| filter["id"] == id)
            .ok_or_else(|| GoogleError::status(404, json!({"error": {"code": 404}}).to_string()))?;
        state.list.remove(at);
        Ok(())
    }
}

/// A client over a fake holding `filters`, and the fake's state.
pub(crate) fn client(filters: Filters) -> (GoogleClient, Arc<Mutex<Filters>>) {
    let state = Arc::new(Mutex::new(filters));
    let client = GoogleClient::with_transport(
        Box::new(Fake(Arc::clone(&state))),
        "https://fake.example".to_owned(),
    );
    (client, state)
}
