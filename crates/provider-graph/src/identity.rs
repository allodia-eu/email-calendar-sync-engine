//! The mailbox's own sender identity: `GET {principal}?$select=displayName,mail,userPrincipalName`.
//!
//! Read-only, and not because this adapter is incomplete. A Graph mailbox's display
//! name is a directory attribute that a tenant administrator owns, so an account holder
//! changing it is not a smaller version of the same operation — it is somebody else's
//! operation. Hence [`IdentityControls::ReadOnly`](engine_provider::IdentityControls)
//! and no write path at all.
//!
//! One identity, never a list: the principal *is* the mailbox
//! ([`MailboxPrincipal`](crate::MailboxPrincipal)), and a shared mailbox the signed-in
//! user also reaches is a separate engine account with its own provider. A `/users/{…}`
//! principal therefore reads that mailbox's own name, which needs the directory-read
//! scope the shared-mailbox setup already grants.

use engine_core::mail::EmailAddress;
use engine_provider::{SenderIdentity, SenderIdentityId};
use serde_json::Value;

use crate::{error::GraphError, json::opt_str, transport::GraphClient};

/// The mailbox's address and the name the directory holds for it.
pub(crate) async fn sender_identity(
    client: &GraphClient,
) -> Result<Vec<SenderIdentity>, GraphError> {
    let url = client.url("?$select=displayName,mail,userPrincipalName");
    Ok(vec![identity_from_json(&client.get(&url).await?)?])
}

/// Normalizes a `user` resource into the one identity it stands for.
///
/// `mail` is the SMTP address and is **null** on a mailbox the tenant has not given
/// one; `userPrincipalName` is the sign-in name and is always present. Falling back is
/// what the rest of this workspace already does for a Graph address, so the two agree.
fn identity_from_json(user: &Value) -> Result<SenderIdentity, GraphError> {
    let address = opt_str(user, "mail")
        .filter(|mail| !mail.is_empty())
        .or_else(|| opt_str(user, "userPrincipalName"))
        .ok_or_else(|| GraphError::protocol("user resource carries no address"))?;
    let name = opt_str(user, "displayName").filter(|name| !name.is_empty());
    let address = match name {
        Some(name) => EmailAddress::named(name, address),
        None => EmailAddress::new(address),
    };
    // The address is the handle: nothing here is writable, so there is no server-side id
    // a later call would need, and inventing one would suggest otherwise.
    Ok(SenderIdentity::new(
        SenderIdentityId::new(address.email.clone()),
        address,
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_smtp_address_wins_over_the_sign_in_name() {
        let identity = identity_from_json(&json!({
            "displayName": "Alice Smith",
            "mail": "alice@example.com",
            "userPrincipalName": "alice@tenant.example.test"
        }))
        .unwrap();
        assert_eq!(identity.address.email, "alice@example.com");
        assert_eq!(identity.address.name.as_deref(), Some("Alice Smith"));
    }

    #[test]
    fn a_mailbox_with_no_smtp_address_falls_back_to_the_sign_in_name() {
        let identity = identity_from_json(&json!({
            "displayName": "Alice Smith",
            "mail": Value::Null,
            "userPrincipalName": "alice@tenant.example.test"
        }))
        .unwrap();
        assert_eq!(identity.address.email, "alice@tenant.example.test");
    }

    #[test]
    fn an_empty_display_name_reads_as_no_name() {
        let identity = identity_from_json(&json!({
            "displayName": "",
            "userPrincipalName": "alice@tenant.example.test"
        }))
        .unwrap();
        assert!(identity.address.name.is_none());
    }

    #[test]
    fn a_resource_with_no_address_at_all_is_a_protocol_error() {
        assert!(identity_from_json(&json!({ "displayName": "Alice Smith" })).is_err());
    }
}
