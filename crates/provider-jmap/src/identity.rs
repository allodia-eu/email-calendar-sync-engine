//! `Identity/get` and `Identity/set`: the addresses this account may send as, and the
//! name the server holds for each (RFC 8621 §6).
//!
//! `Identity` belongs to the **submission** capability, not to mail, so every request
//! here names `urn:ietf:params:jmap:submission` and is addressed to the submission
//! account. That is a different `primaryAccounts` entry from the mail one; they
//! coincide on the servers this repo tests against and are not required to.
//!
//! Nothing here decides what goes on the wire. The `From` this adapter sends is built
//! from the caller's [`Draft`](engine_provider::Draft) (`crate::submit`), so writing a
//! name back keeps the account's *other* clients in step and changes nothing about what
//! we ourselves send.

use engine_core::mail::EmailAddress;
use engine_provider::{SenderIdentity, SenderIdentityId};
use serde_json::{Map, Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    mutate::check_set_result_for,
    request::{Request, capability},
    sync_ops::objects,
};

/// Reads every identity the submission account can send as.
pub(crate) async fn list(
    executor: &dyn Executor,
    submission_account: &str,
) -> Result<Vec<SenderIdentity>, JmapError> {
    let mut req = Request::new([capability::CORE, capability::SUBMISSION]);
    let call = req.invoke("Identity/get", json!({ "accountId": submission_account }));
    let resp = executor.execute(&req).await?;
    objects(resp.result(&call)?, identity_from_json)
}

/// Sets `identity`'s display name.
pub(crate) async fn set_name(
    executor: &dyn Executor,
    submission_account: &str,
    identity: &SenderIdentityId,
    name: &str,
) -> Result<(), JmapError> {
    let mut patch = Map::new();
    // `name` alone: naming `email` here would ask the server to change which address the
    // account sends as, which is a different act from renaming the sender.
    patch.insert("name".to_owned(), Value::String(name.to_owned()));
    let mut update = Map::new();
    update.insert(identity.as_str().to_owned(), Value::Object(patch));

    let mut req = Request::new([capability::CORE, capability::SUBMISSION]);
    let call = req.invoke(
        "Identity/set",
        json!({ "accountId": submission_account, "update": update }),
    );
    let resp = executor.execute(&req).await?;
    check_set_result_for(
        resp.result(&call)?,
        identity.as_str(),
        "updated",
        "notUpdated",
    )
}

/// Normalizes one `Identity` object (RFC 8621 §6.1).
///
/// An empty `name` is the server holding nothing rather than a name that is
/// deliberately blank, so it reaches a host as `None`: that is the difference between
/// "we already know it" and "ask", and it is the whole question this read exists to
/// answer.
fn identity_from_json(value: &Value) -> Result<SenderIdentity, JmapError> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| JmapError::protocol("identity has no id"))?;
    let email = value
        .get("email")
        .and_then(Value::as_str)
        .ok_or_else(|| JmapError::protocol("identity has no email"))?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty());
    let address = match name {
        Some(name) => EmailAddress::named(name, email),
        None => EmailAddress::new(email),
    };
    Ok(SenderIdentity::new(SenderIdentityId::new(id), address))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_server_name_reads_as_no_name() {
        let identity =
            identity_from_json(&json!({ "id": "a", "email": "alice@example.com", "name": "" }))
                .unwrap();
        assert!(identity.address.name.is_none());
    }

    #[test]
    fn an_absent_name_property_reads_as_no_name() {
        let identity =
            identity_from_json(&json!({ "id": "a", "email": "alice@example.com" })).unwrap();
        assert!(identity.address.name.is_none());
    }

    #[test]
    fn an_identity_without_an_email_is_a_protocol_error() {
        // RFC 8621 §6.1 makes `email` mandatory. An identity we cannot address is not a
        // usable one, and dropping it silently would hide a server that broke its own
        // contract.
        assert!(identity_from_json(&json!({ "id": "a", "name": "Alice" })).is_err());
    }
}
