//! Applying a [`MailEdit`] to an already-synced message via `Email/set`
//! (RFC 8621 §4.6).
//!
//! JMAP folds all three provider-neutral mail edits (`modeling.md`) onto **one**
//! `Email/set` call:
//!
//! - [`MailEdit::SetKeywords`] → a `keywords/<kw>` [PatchObject](https://www.rfc-editor.org/rfc/rfc8620#section-5.3)
//!   (`true` to set a keyword, `null` to clear it) — mark read/unread (`$seen`), flag/unflag
//!   (`$flagged`), or any user keyword.
//! - [`MailEdit::MoveTo`] → a `mailboxIds` **replacement** so the message ends up in exactly the
//!   destination (the neutral meaning of a move, and the single-membership common case).
//! - [`MailEdit::Delete`] → a `destroy`.
//!
//! A JMAP object id is account-global and **stable across a move**, so the receipt's
//! key is the (unchanged) target id and the next sync reconciles the new membership —
//! unlike IMAP, where a move synthesizes a new `(mailbox, UIDVALIDITY, UID)` key. A
//! per-object `SetError` (RFC 8620 §5.3) maps into the engine failure taxonomy through
//! [`JmapError::Set`]: a `notFound`/`stateMismatch` is a [`Conflict`] (re-sync, then
//! retry), matching the IMAP stale-UID contract.
//!
//! [`Conflict`]: engine_core::error::FailureClass::Conflict

use engine_provider::{MailEdit, MailEditReceipt};
use serde_json::{Map, Value, json};

use crate::{
    error::JmapError,
    provider::Executor,
    request::{Request, capability},
};

/// Applies `edit` to its target message under `mail_account` via `Email/set`,
/// returning a receipt carrying the (unchanged) target key.
///
/// # Errors
///
/// Returns [`JmapError`] on a transport/method failure, or [`JmapError::Set`] when the
/// server rejects the object with a `SetError` (or silently drops it — treated as a
/// `notFound` conflict).
pub(crate) async fn edit_mail(
    executor: &dyn Executor,
    mail_account: &str,
    edit: &MailEdit,
) -> Result<MailEditReceipt, JmapError> {
    let target = edit.target().as_str();
    let mut req = Request::new([capability::CORE, capability::MAIL]);
    let call = req.invoke("Email/set", set_arguments(mail_account, edit, target));
    let resp = executor.execute(&req).await?;
    check_set_result(resp.result(&call)?, edit, target)?;
    Ok(MailEditReceipt::new(edit.target().clone()))
}

/// Builds the `Email/set` arguments for one edit.
fn set_arguments(mail_account: &str, edit: &MailEdit, target: &str) -> Value {
    match edit {
        MailEdit::SetKeywords { add, remove, .. } => {
            let mut patch = Map::new();
            for keyword in add {
                patch.insert(keyword_pointer(keyword.as_str()), Value::Bool(true));
            }
            for keyword in remove {
                patch.insert(keyword_pointer(keyword.as_str()), Value::Null);
            }
            update_args(mail_account, target, Value::Object(patch))
        }
        MailEdit::MoveTo { destination, .. } => {
            // Replace the whole membership set: the message ends up in exactly the
            // destination. This is the neutral meaning of `MoveTo` and the
            // single-membership common case, regardless of prior memberships.
            let mailbox_ids = json!({ destination.as_str(): true });
            update_args(mail_account, target, json!({ "mailboxIds": mailbox_ids }))
        }
        MailEdit::Delete { .. } => json!({ "accountId": mail_account, "destroy": [target] }),
    }
}

/// Wraps a per-object patch in `{ accountId, update: { <target>: <patch> } }`.
pub(crate) fn update_args(mail_account: &str, target: &str, patch: Value) -> Value {
    let mut update = Map::new();
    update.insert(target.to_owned(), patch);
    json!({ "accountId": mail_account, "update": update })
}

/// The PatchObject key for a keyword, JSON-Pointer-escaped (RFC 6901 §3): `~` → `~0`,
/// `/` → `~1`. JMAP keywords may contain both (RFC 8621 §4.1.1 permits every printable
/// ASCII bar a short blocklist), so an unescaped `keywords/<kw>` pointer would be
/// ambiguous.
pub(crate) fn keyword_pointer(keyword: &str) -> String {
    format!("keywords/{}", keyword.replace('~', "~0").replace('/', "~1"))
}

/// Verifies the `Email/set` acted on `target`, mapping a `notUpdated`/`notDestroyed`
/// `SetError` (RFC 8620 §5.3) into a classified [`JmapError::Set`]. A target that is
/// neither applied nor reported failed (the server silently dropped our id) is treated
/// as a `notFound` conflict — never a false success.
fn check_set_result(result: &Value, edit: &MailEdit, target: &str) -> Result<(), JmapError> {
    let (applied, failed) = if matches!(edit, MailEdit::Delete { .. }) {
        ("destroyed", "notDestroyed")
    } else {
        ("updated", "notUpdated")
    };
    check_set_result_for(result, target, applied, failed)
}

/// The acknowledgement rule shared by every `Email/set` this adapter sends: a
/// `SetError` under `failed` is a classified [`JmapError::Set`], and a target that is
/// neither applied nor reported failed (the server silently dropped our id) is treated
/// as a `notFound` conflict — never a false success.
pub(crate) fn check_set_result_for(
    result: &Value,
    target: &str,
    applied: &str,
    failed: &str,
) -> Result<(), JmapError> {
    let destroy = applied == "destroyed";

    if let Some(error_type) = result
        .get(failed)
        .and_then(|f| f.get(target))
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
    {
        return Err(JmapError::set(target, error_type));
    }

    // `destroyed` is an array of ids; `updated` is an object keyed by id (its value may
    // be `null` when the server made no extra server-set changes — still an ack).
    let acknowledged = if destroy {
        result
            .get(applied)
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(target)))
    } else {
        result.get(applied).and_then(|u| u.get(target)).is_some()
    };

    if acknowledged {
        Ok(())
    } else {
        Err(JmapError::set(target, "notFound"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use engine_core::{
        error::FailureClass,
        ids::{MailboxId, ProviderKey},
        mail::{Keyword, SystemKeyword},
    };

    use super::*;

    fn target() -> ProviderKey {
        ProviderKey::new("eaaaaab").unwrap()
    }

    fn keywords(add: &[Keyword], remove: &[Keyword]) -> MailEdit {
        MailEdit::SetKeywords {
            target: target(),
            add: add.iter().cloned().collect::<BTreeSet<_>>(),
            remove: remove.iter().cloned().collect::<BTreeSet<_>>(),
        }
    }

    #[test]
    fn set_keywords_builds_a_patch_of_true_and_null() {
        let edit = keywords(
            &[Keyword::system(SystemKeyword::Seen)],
            &[Keyword::system(SystemKeyword::Flagged)],
        );
        let args = set_arguments("c", &edit, "eaaaaab");
        assert_eq!(args["accountId"], "c");
        let patch = &args["update"]["eaaaaab"];
        // Add → `true`, remove → `null`.
        assert_eq!(patch["keywords/$seen"], json!(true));
        assert_eq!(patch["keywords/$flagged"], Value::Null);
    }

    #[test]
    fn keyword_pointer_is_json_pointer_escaped() {
        // A keyword may contain `/` and `~`; both must be escaped in the pointer so the
        // server does not read them as pointer separators.
        let edit = keywords(&[Keyword::new("a/b~c").unwrap()], &[]);
        let args = set_arguments("c", &edit, "eaaaaab");
        let patch = args["update"]["eaaaaab"].as_object().unwrap();
        assert!(patch.contains_key("keywords/a~1b~0c"));
    }

    #[test]
    fn move_replaces_the_whole_mailbox_membership() {
        let edit = MailEdit::move_to(target(), MailboxId::try_from("dest").unwrap());
        let args = set_arguments("c", &edit, "eaaaaab");
        let mailbox_ids = &args["update"]["eaaaaab"]["mailboxIds"];
        assert_eq!(mailbox_ids["dest"], json!(true));
        // Exactly one membership — the destination.
        assert_eq!(mailbox_ids.as_object().unwrap().len(), 1);
    }

    #[test]
    fn delete_destroys_the_target() {
        let edit = MailEdit::delete(target());
        let args = set_arguments("c", &edit, "eaaaaab");
        assert_eq!(args["destroy"], json!(["eaaaaab"]));
        assert!(args.get("update").is_none());
    }

    #[test]
    fn updated_ack_is_success_even_when_null() {
        // The server returns the id under `updated` with a `null` value (no extra
        // server-set changes) — still an acknowledgement.
        let result = json!({ "updated": { "eaaaaab": null } });
        assert!(check_set_result(&result, &keywords(&[], &[]), "eaaaaab").is_ok());
    }

    #[test]
    fn destroyed_ack_is_success() {
        let result = json!({ "destroyed": ["eaaaaab"], "notDestroyed": {} });
        assert!(check_set_result(&result, &MailEdit::delete(target()), "eaaaaab").is_ok());
    }

    #[test]
    fn not_updated_set_error_classifies() {
        let result = json!({ "notUpdated": { "eaaaaab": { "type": "stateMismatch" } } });
        let err = check_set_result(&result, &keywords(&[], &[]), "eaaaaab").unwrap_err();
        assert_eq!(err.failure_class(), FailureClass::Conflict);
    }

    #[test]
    fn not_destroyed_set_error_classifies() {
        let result = json!({ "notDestroyed": { "eaaaaab": { "type": "forbidden" } } });
        let err = check_set_result(&result, &MailEdit::delete(target()), "eaaaaab").unwrap_err();
        assert_eq!(err.failure_class(), FailureClass::Permanent);
    }

    #[test]
    fn silently_dropped_target_is_a_conflict() {
        // Neither `updated` nor `notUpdated` mentions our id — never a false success.
        let result = json!({ "updated": { "other": null }, "notUpdated": {} });
        let err = check_set_result(&result, &keywords(&[], &[]), "eaaaaab").unwrap_err();
        assert_eq!(err.failure_class(), FailureClass::Conflict);
    }
}
