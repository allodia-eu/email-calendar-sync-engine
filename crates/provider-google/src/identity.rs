//! Gmail send-as settings: the addresses this account may send as, and the display
//! name Gmail puts on each.
//!
//! `users.settings.sendAs` is a **settings** endpoint, not a mail one, so it needs the
//! `gmail.settings.basic` scope rather than the mail scope the rest of this adapter
//! runs on. A token without it fails here and nowhere else, which is why the refusal
//! has to arrive as a classified error rather than an empty list: an account whose
//! token is short one scope must not look like an account with no identities.
//!
//! Gmail returns every send-as alias, verified or not, so the list is genuinely a list
//! here where the Graph adapter's is always one entry. A caller finds the account's own
//! identity by matching an address, never by taking the first.

use engine_core::mail::EmailAddress;
use engine_provider::{SenderIdentity, SenderIdentityId};
use serde_json::{Value, json};

use crate::{
    error::GoogleError,
    json::opt_str,
    transport::{GoogleClient, encode_query_value},
};

/// The Gmail send-as settings collection.
const SEND_AS: &str = "/gmail/v1/users/me/settings/sendAs";

/// Reads every address this account may send as.
pub(crate) async fn list(client: &GoogleClient) -> Result<Vec<SenderIdentity>, GoogleError> {
    let body = client.get(&client.url(SEND_AS)).await?;
    let entries = body
        .get("sendAs")
        .and_then(Value::as_array)
        .ok_or_else(|| GoogleError::protocol("sendAs list has no sendAs array"))?;
    entries.iter().map(identity_from_json).collect()
}

/// Sets the display name Gmail sends `identity` under.
///
/// The identity handle is the send-as address, which is also the resource's path
/// segment. It is percent-encoded rather than spliced raw: an address may legally carry
/// characters (`+`, `/`) that would otherwise reshape the path.
pub(crate) async fn set_name(
    client: &GoogleClient,
    identity: &SenderIdentityId,
    name: &str,
) -> Result<(), GoogleError> {
    let url = client.url(&format!(
        "{SEND_AS}/{}",
        encode_query_value(identity.as_str())
    ));
    // `displayName` alone. A `PATCH` naming `sendAsEmail` would ask Gmail to change
    // which address the alias *is*, which is a different act from renaming the sender.
    let body = json!({ "displayName": name }).to_string().into_bytes();
    client
        .patch(&url, "application/json", None, body)
        .await
        .map(|_| ())
}

/// Normalizes one `SendAs` resource.
///
/// An empty `displayName` is Gmail holding nothing, not a name that is deliberately
/// blank, so it reaches a host as `None` — the difference between "we already know it"
/// and "ask", which is the whole question this read answers.
fn identity_from_json(entry: &Value) -> Result<SenderIdentity, GoogleError> {
    let email = opt_str(entry, "sendAsEmail")
        .filter(|email| !email.is_empty())
        .ok_or_else(|| GoogleError::protocol("sendAs entry has no sendAsEmail"))?;
    let name = opt_str(entry, "displayName").filter(|name| !name.is_empty());
    let address = match name {
        Some(name) => EmailAddress::named(name, email),
        None => EmailAddress::new(email),
    };
    Ok(SenderIdentity::new(
        SenderIdentityId::new(address.email.clone()),
        address,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_send_as_alias_is_returned_not_just_the_primary() {
        // Gmail's own aliases are how a user sends from a second address, so dropping
        // the non-primary entries would silently hide half the answer.
        let body = json!({ "sendAs": [
            { "sendAsEmail": "alice@example.com", "displayName": "Alice Smith", "isPrimary": true },
            { "sendAsEmail": "sales@example.com", "displayName": "Sales" }
        ]});
        let entries = body["sendAs"].as_array().unwrap();
        let identities: Vec<_> = entries
            .iter()
            .map(|e| identity_from_json(e).unwrap())
            .collect();
        assert_eq!(identities.len(), 2);
        assert_eq!(identities[1].address.email, "sales@example.com");
        assert_eq!(identities[1].address.name.as_deref(), Some("Sales"));
    }

    #[test]
    fn an_empty_display_name_reads_as_no_name() {
        let identity =
            identity_from_json(&json!({ "sendAsEmail": "alice@example.com", "displayName": "" }))
                .unwrap();
        assert!(identity.address.name.is_none());
    }

    #[test]
    fn an_entry_without_an_address_is_a_protocol_error() {
        assert!(identity_from_json(&json!({ "displayName": "Alice Smith" })).is_err());
    }

    #[test]
    fn the_handle_is_the_send_as_address() {
        // Gmail's settings resource is keyed by the address itself, so the handle a host
        // passes back to `set_sender_name` is that address and nothing derived from it.
        let identity = identity_from_json(&json!({ "sendAsEmail": "alice@example.com" })).unwrap();
        assert_eq!(identity.id, SenderIdentityId::new("alice@example.com"));
    }
}
