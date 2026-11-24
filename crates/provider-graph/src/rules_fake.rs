//! A fake Graph that keeps the inbox rules as a real list, the way Exchange was observed to
//! (`graph.md`, "Mail rules and the out-of-office reply"): `sequence` is a rule's position, an
//! insert or a move at a number shifts the others, a number past the end is clamped, a delete
//! closes the gap, and a `PATCH` keeps what it is sent as `null`. The order tests assert the list
//! a write leaves, not only the requests it sent.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::{
    error::GraphError,
    transport::{GraphClient, GraphTransport},
};

/// The fake's state, shared with the test that built it.
#[derive(Default)]
pub(crate) struct Rules {
    list: Vec<Value>,
    next: usize,
    /// Requests that fail: `(method, rule id)` to the status answered.
    pub(crate) failing: BTreeMap<(&'static str, String), u16>,
    /// Every write, as `(method, rule id or "new")`.
    pub(crate) writes: Vec<(&'static str, String)>,
}

impl Rules {
    /// The display names in list order.
    pub(crate) fn names(&self) -> Vec<String> {
        self.list
            .iter()
            .map(|rule| rule["displayName"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn renumber(&mut self) {
        for (position, rule) in self.list.iter_mut().enumerate() {
            rule["sequence"] = json!(position + 1);
        }
    }

    fn fail(&self, method: &'static str, id: &str) -> Result<(), GraphError> {
        match self.failing.get(&(method, id.to_owned())) {
            Some(status) => Err(GraphError::status(
                *status,
                json!({"error": {"code": "Injected"}}).to_string(),
            )),
            None => Ok(()),
        }
    }

    fn position(&self, id: &str) -> Option<usize> {
        self.list.iter().position(|rule| rule["id"] == id)
    }
}

/// A client whose rules are `rules`, each named and numbered in order.
pub(crate) fn client(rules: &[Value]) -> (GraphClient, Arc<Mutex<Rules>>) {
    let state = Arc::new(Mutex::new(Rules {
        list: rules.to_vec(),
        ..Rules::default()
    }));
    state.lock().unwrap().renumber();
    let client = GraphClient::with_transport(
        Box::new(Fake(Arc::clone(&state))),
        "https://graph.test".to_owned(),
    );
    (client, state)
}

/// Rules named `names`, with ids `id-<name>`, each marking mail read.
pub(crate) fn named(names: &[&str]) -> Vec<Value> {
    names
        .iter()
        .map(|name| {
            json!({
                "id": format!("id-{name}"),
                "displayName": name,
                "isEnabled": true,
                "conditions": {"subjectContains": [format!("marker-{name}")]},
                "actions": {"markAsRead": true}
            })
        })
        .collect()
}

struct Fake(Arc<Mutex<Rules>>);

/// The rule id a URL ends in, decoded.
fn id_of(url: &str) -> String {
    let tail = url.rsplit('/').next().unwrap_or_default();
    let mut out = Vec::new();
    let bytes = tail.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// Where a `sequence` puts a rule in a list of `len`: clamped to its ends.
fn slot(body: &Map<String, Value>, len: usize) -> usize {
    let asked = body
        .get("sequence")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(len + 1);
    asked.clamp(1, len + 1) - 1
}

#[async_trait]
impl GraphTransport for Fake {
    async fn get(&self, url: &str) -> Result<Value, GraphError> {
        if url.contains("/mailFolders/") && !url.contains("messageRules") {
            return Ok(json!({"id": "folder-id"}));
        }
        Ok(json!({ "value": self.0.lock().unwrap().list.clone() }))
    }

    async fn get_bytes(&self, _url: &str) -> Result<Vec<u8>, GraphError> {
        Err(GraphError::protocol("no bytes here"))
    }

    async fn get_bytes_unauthenticated(&self, _url: &str) -> Result<Vec<u8>, GraphError> {
        Err(GraphError::protocol("no bytes here"))
    }

    async fn post(
        &self,
        _url: &str,
        _content_type: &str,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GraphError> {
        let mut rules = self.0.lock().unwrap();
        let Value::Object(mut body) = serde_json::from_slice(&body)? else {
            return Err(GraphError::protocol("not an object"));
        };
        let name = body
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        rules.fail("POST", &name)?;
        rules.writes.push(("POST", name));
        rules.next += 1;
        body.insert("id".to_owned(), json!(format!("new-{}", rules.next)));
        let at = slot(&body, rules.list.len());
        let rule = Value::Object(body);
        rules.list.insert(at, rule.clone());
        rules.renumber();
        Ok(Some(rule))
    }

    async fn patch(
        &self,
        url: &str,
        _content_type: &str,
        _if_match: Option<&str>,
        body: Vec<u8>,
    ) -> Result<Option<Value>, GraphError> {
        let mut rules = self.0.lock().unwrap();
        let id = id_of(url);
        rules.fail("PATCH", &id)?;
        rules.writes.push(("PATCH", id.clone()));
        let Value::Object(body) = serde_json::from_slice(&body)? else {
            return Err(GraphError::protocol("not an object"));
        };
        let at = rules
            .position(&id)
            .ok_or_else(|| GraphError::status(404, "{}"))?;
        let mut rule = rules.list.remove(at);
        for (key, value) in &body {
            match (key.as_str(), value) {
                ("sequence", _) | (_, Value::Null) => {}
                (_, Value::Object(parts)) => {
                    // A part sent as `null` stays as it was (observed).
                    for (part, value) in parts.iter().filter(|(_, v)| !v.is_null()) {
                        rule[key.as_str()][part.as_str()] = value.clone();
                    }
                }
                _ => rule[key.as_str()] = value.clone(),
            }
        }
        let to = if body.contains_key("sequence") {
            slot(&body, rules.list.len())
        } else {
            at
        };
        rules.list.insert(to, rule.clone());
        rules.renumber();
        Ok(Some(rule))
    }

    async fn delete(&self, url: &str, _if_match: Option<&str>) -> Result<(), GraphError> {
        let mut rules = self.0.lock().unwrap();
        let id = id_of(url);
        rules.fail("DELETE", &id)?;
        rules.writes.push(("DELETE", id.clone()));
        let at = rules
            .position(&id)
            .ok_or_else(|| GraphError::status(404, "{}"))?;
        rules.list.remove(at);
        rules.renumber();
        Ok(())
    }
}
