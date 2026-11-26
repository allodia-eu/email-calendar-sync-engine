//! Reading and changing the harness server's own settings, through Stalwart's management API.
//!
//! The entrypoint raises Stalwart's per-account limits so that a migration-sized copy fits
//! (`docs/agent-guidance/stalwart-harness.md`, "Rate limiters are disarmed on purpose"). A
//! live suite that measures one of those limits puts it back to what Stalwart ships for its
//! own run, and raises it again after. These are the management JMAP methods `entrypoint.sh`
//! sends, on a settings singleton (`x:Http`, `x:Jmap`, …), followed by the `ReloadSettings`
//! action.
//!
//! A change here is server-wide and outlives the test that made it, so a caller restores it
//! even when its measurement panics, and restores a known value rather than the one it read: a
//! run killed while the limit was lowered would otherwise read the lowered value next time and
//! make it permanent. A harness left lowered is fixed by restarting the container: the
//! entrypoint raises the limits again on every start.

use serde_json::{Value, json};

use crate::{Harness, HarnessError, http};

/// The fallback administrator `docker-compose.yml` provisions (`STALWART_RECOVERY_ADMIN`).
///
/// Not env-overridable, like [`Harness::scratch`]: it exists only because the bundled compose
/// file sets it, so another credential could only ever be refused.
const ADMIN: (&str, &str) = ("admin", "harness-admin-pw");

/// Signed-in requests a minute per account that Stalwart allows as shipped (v1.0.2,
/// `x:Http.rateLimitAuthenticated`).
pub const SHIPPED_REQUESTS_PER_MINUTE: u64 = 1_000;

/// What `entrypoint.sh` (`raise_per_account_limits`) raises that limit to; the two must agree.
/// A suite that lowers the limit for itself restores this, never the value it read.
pub const RAISED_REQUESTS_PER_MINUTE: u64 = 1_000_000;

/// The period both limits above are counted over, in the milliseconds Stalwart states it in.
const MINUTE_MS: u64 = 60_000;

impl Harness {
    /// The settings singleton `object` (`"Http"`, `"Jmap"`, …), as the management API's
    /// `x:<object>/get` returns it.
    ///
    /// # Errors
    /// Propagates transport failures, and returns [`HarnessError::Protocol`] when the answer
    /// does not carry the singleton.
    pub fn settings(&self, object: &str) -> Result<Value, HarnessError> {
        let [answer] = self.manage(&[json!([
            format!("x:{object}/get"),
            { "ids": ["singleton"] },
            "c0",
        ])])?;
        singleton_of(&answer).cloned().ok_or_else(|| {
            HarnessError::protocol("management", format!("x:{object}/get: {answer}"))
        })
    }

    /// Set the properties in `patch` on the settings singleton `object`, and have Stalwart
    /// reload its settings so the change applies at once, with no restart.
    ///
    /// The reload is not optional: a `set` alone is stored and reported `updated`, and the
    /// server goes on enforcing the old value until it next starts. Measured on v1.0.2: with
    /// `rateLimitAuthenticated` set from 1,000,000 down to 1,000 a minute, 1,100 requests in
    /// three seconds all passed; after a reload, the 1,001st was refused.
    ///
    /// # Errors
    /// Propagates transport failures, and returns [`HarnessError::Protocol`] when the server
    /// does not report the singleton updated and the reload done.
    pub fn update_settings(&self, object: &str, patch: &Value) -> Result<(), HarnessError> {
        let [set, reload] = self.manage(&[
            json!([
                format!("x:{object}/set"),
                { "update": { "singleton": patch } },
                "c0",
            ]),
            json!(["x:Action/set", { "create": { "r": { "@type": "ReloadSettings" } } }, "c1"]),
        ])?;
        if updated(&set) && reloaded(&reload) {
            Ok(())
        } else {
            Err(HarnessError::protocol(
                "management",
                format!("x:{object}/set: {set}; reload: {reload}"),
            ))
        }
    }

    /// `x:Http.rateLimitAuthenticated`, as signed-in requests a minute per account.
    ///
    /// # Errors
    /// Propagates [`settings`](Self::settings) failures, and returns
    /// [`HarnessError::Protocol`] when the setting is not a count per minute.
    pub fn requests_per_minute(&self) -> Result<u64, HarnessError> {
        let http = self.settings("Http")?;
        let limit = &http["rateLimitAuthenticated"];
        match (limit["count"].as_u64(), limit["period"].as_u64()) {
            (Some(count), Some(MINUTE_MS)) => Ok(count),
            _ => Err(HarnessError::protocol(
                "management",
                format!("rateLimitAuthenticated is not a count per minute: {limit}"),
            )),
        }
    }

    /// Set `x:Http.rateLimitAuthenticated` to `count` signed-in requests a minute per account,
    /// and apply it ([`update_settings`](Self::update_settings)).
    ///
    /// # Errors
    /// As [`update_settings`](Self::update_settings).
    pub fn set_requests_per_minute(&self, count: u64) -> Result<(), HarnessError> {
        self.update_settings(
            "Http",
            &json!({ "rateLimitAuthenticated": { "count": count, "period": MINUTE_MS } }),
        )
    }

    /// Management method calls in one request, as the administrator; their answers in order.
    fn manage<const N: usize>(&self, calls: &[Value; N]) -> Result<[Value; N], HarnessError> {
        let body = json!({
            "using": ["urn:ietf:params:jmap:core", "urn:stalwart:jmap"],
            "methodCalls": calls.as_slice(),
        });
        let resp = http::request(
            &self.http_addr,
            "POST",
            "/jmap/",
            Some(ADMIN),
            &[("Content-Type", "application/json")],
            body.to_string().as_bytes(),
        )?;
        if resp.status != 200 {
            return Err(HarnessError::protocol(
                "management",
                format!("HTTP {}", resp.status),
            ));
        }
        let mut answer: Value = serde_json::from_slice(&resp.body)?;
        let responses = answer
            .get_mut("methodResponses")
            .and_then(Value::as_array_mut)
            .map(core::mem::take)
            .unwrap_or_default();
        <[Value; N]>::try_from(responses).map_err(|got| {
            HarnessError::protocol(
                "management",
                format!("{N} method answers expected, got {}", got.len()),
            )
        })
    }
}

/// The singleton in a `get` answer (`["x:…/get", {"list": [{…}]}, "c0"]`).
fn singleton_of(answer: &Value) -> Option<&Value> {
    answer[1]["list"]
        .as_array()?
        .iter()
        .find(|entry| entry["id"] == "singleton")
}

/// Whether an `x:Action/set` answer reports the action (created as `r`) carried out.
fn reloaded(answer: &Value) -> bool {
    answer[1]["created"]["r"]["id"].is_string()
}

/// Whether a `set` answer reports the singleton updated.
fn updated(answer: &Value) -> bool {
    answer[1]["updated"]
        .as_object()
        .is_some_and(|updated| updated.contains_key("singleton"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{reloaded, singleton_of, updated};

    #[test]
    fn reads_the_singleton_out_of_a_get_answer() {
        let answer = json!([
            "x:Http/get",
            { "list": [{ "rateLimitAuthenticated": { "count": 1000, "period": 60000 }, "id": "singleton" }], "notFound": [] },
            "c0",
        ]);
        assert_eq!(
            singleton_of(&answer).unwrap()["rateLimitAuthenticated"]["count"],
            1000
        );
        let error = json!(["error", { "type": "unknownMethod" }, "c0"]);
        assert!(singleton_of(&error).is_none());
    }

    #[test]
    fn only_an_updated_singleton_counts_as_set() {
        assert!(updated(
            &json!(["x:Http/set", { "updated": { "singleton": null } }, "c0"])
        ));
        assert!(!updated(&json!([
            "x:Http/set",
            { "notUpdated": { "singleton": { "type": "invalidPatch" } } },
            "c0",
        ])));
        assert!(!updated(
            &json!(["error", { "type": "unknownMethod" }, "c0"])
        ));
    }

    #[test]
    fn only_a_created_action_counts_as_reloaded() {
        assert!(reloaded(&json!([
            "x:Action/set",
            { "created": { "r": { "warnings": {}, "id": "bvqqpvv" } } },
            "c1",
        ])));
        assert!(!reloaded(&json!([
            "x:Action/set",
            { "notCreated": { "r": { "type": "forbidden" } } },
            "c1",
        ])));
    }
}
