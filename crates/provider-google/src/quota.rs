//! What each Gmail call costs against a user's per-minute quota, and the pace that keeps an
//! account under it.
//!
//! Google bills every Gmail call in query-cost units against **6,000 per user per minute** for
//! each Cloud project, and refuses the call that would go over with a `403 rateLimitExceeded`
//! (`crate::throttle`). The prices are Google's published table
//! (<https://developers.google.com/workspace/gmail/api/reference/quota>), which applies to every
//! project created on or after 1 May 2026; an older project keeps a larger, older quota, and
//! this pace is merely slower than it needs to be there.
//!
//! The price that decides everything is the message fetch: **20 units**, and 60 for a message's
//! source. A snapshot page of a hundred messages costs 2,005, so a rested quota covers about
//! three pages a minute, and a fan-out twenty wide spends a whole minute's budget in seconds.
//! Measured on the test account: 299 metadata fetches succeeded in the minute before the first
//! refusal.
//!
//! Calendar and People are billed under quotas of their own, so their calls cost nothing here.

use core::time::Duration;

use engine_http::RequestGate;
use reqwest::Method;

/// The budget the adapter holds an account to: nine tenths of Google's 6,000, leaving the rest
/// to the same user's other clients of the same project (the app on another device) and to the
/// slack between this device's clock and the window Google counts in.
pub(crate) const UNITS_PER_MINUTE: u32 = 5400;

/// The window [`UNITS_PER_MINUTE`] is counted over.
pub(crate) const WINDOW: Duration = Duration::from_mins(1);

/// What a call Google's table does not list is charged: the dearest single call it does list.
/// An unlisted call can only make the pace more cautious than it needs to be, never less.
const UNLISTED: u32 = 100;

/// The Gmail API root every billed path sits under, `upload/` variants included.
const GMAIL: &str = "/gmail/v1/users/me/";

/// Holds the account `gate` belongs to under [`UNITS_PER_MINUTE`]: stated once, as a client
/// connects, on the gate every provider and core of that account spends through.
pub(crate) fn pace(gate: &RequestGate) {
    gate.pace_to(UNITS_PER_MINUTE, WINDOW);
}

/// Waits until the account's budget covers `method` on `url`, and charges it.
pub(crate) async fn charge(gate: &RequestGate, method: &Method, url: &str) {
    gate.spend(cost(method, url)).await;
}

/// What `method` on `url` costs against the Gmail quota; `0` for anything outside Gmail.
pub(crate) fn cost(method: &Method, url: &str) -> u32 {
    let Some((_, rest)) = url.split_once(GMAIL) else {
        return 0;
    };
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let segments: Vec<&str> = path.split('/').collect();
    match (segments.as_slice(), method.as_str()) {
        (["profile"] | ["labels", ..] | ["settings", "sendAs", ..], "GET") => 1,
        (["history"], "GET") => 2,
        (["labels", ..], _)
        | (["messages" | "drafts"], "GET")
        | (["messages", _, "modify" | "untrash"], "POST")
        | (["settings", "sendAs", _], "DELETE") => 5,
        (["messages" | "drafts", _], "DELETE") | (["drafts"], "POST") => 10,
        (["drafts", _], "PUT") => 15,
        (["messages", _, "trash"], "POST") | (["messages", _, "attachments", _], "GET") => 20,
        (["messages"] | ["messages", "import" | "insert"], "POST") => 25,
        (["messages", "batchModify" | "batchDelete"], "POST") => 50,
        (["messages" | "drafts", "send"], "POST")
        | (["settings", "sendAs", _], "PUT" | "PATCH") => 100,
        (["messages" | "drafts", _], "GET") => fetch(query),
        _ => UNLISTED,
    }
}

/// A message or draft fetch: 20 units, or 60 for the formats that carry the body. Google's
/// default format is `full`, so a fetch that names none is the dear one.
fn fetch(query: &str) -> u32 {
    let format = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("format="))
        .unwrap_or("full");
    match format {
        "metadata" | "minimal" => 20,
        _ => 60,
    }
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
