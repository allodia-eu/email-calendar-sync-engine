//! Unit tests for [`MailboxEdit`](super::MailboxEdit), its role rule and receipt. A sibling
//! file so `mailbox_edit.rs` stays under the line limit.

use engine_core::error::FailureClass;

use super::*;

fn id(value: &str) -> MailboxId {
    MailboxId::try_from(value).unwrap()
}

#[test]
fn a_create_targets_nothing_and_every_other_edit_its_folder() {
    let create = MailboxEdit::create("Receipts", None);
    assert_eq!(create.target(), None);
    let edits = [
        MailboxEdit::Update {
            target: id("f1"),
            name: "Bills".into(),
            parent: Some(id("f0")),
        },
        MailboxEdit::Trash {
            target: id("f1"),
            trash: id("trash"),
            name: "Bills".into(),
        },
        MailboxEdit::Delete { target: id("f1") },
    ];
    for edit in &edits {
        assert_eq!(edit.target(), Some(&id("f1")));
    }
}

#[test]
fn a_refused_name_is_invalid_state_and_its_detail_is_the_rule() {
    let error = ProviderError::from(MailboxNameError::Separator);
    assert_eq!(error.class(), FailureClass::InvalidState);
    assert_eq!(error.detail(), MailboxNameError::Separator.to_string());
}

#[test]
fn a_refused_name_carries_the_typed_rule_as_its_source() {
    let error = ProviderError::from(MailboxNameError::Control);
    let source = std::error::Error::source(&error).expect("the typed error");
    assert_eq!(
        source.downcast_ref::<MailboxNameError>(),
        Some(&MailboxNameError::Control)
    );
}

#[test]
fn an_edit_survives_the_outbox_round_trip() {
    let edit = MailboxEdit::Update {
        target: id("Work/2024"),
        name: "2025".into(),
        parent: None,
    };
    let json = serde_json::to_value(&edit).unwrap();
    assert_eq!(serde_json::from_value::<MailboxEdit>(json).unwrap(), edit);
}

#[test]
fn the_constructors_build_a_create_with_and_without_a_role() {
    assert_eq!(
        MailboxEdit::create("Receipts", Some(id("f0"))),
        MailboxEdit::Create {
            name: "Receipts".into(),
            parent: Some(id("f0")),
            role: None,
        }
    );
    assert_eq!(
        MailboxEdit::create_with_role("Sent", None, MailboxRole::Sent),
        MailboxEdit::Create {
            name: "Sent".into(),
            parent: None,
            role: Some(MailboxRole::Sent),
        }
    );
}

#[test]
fn a_create_without_a_role_serializes_as_it_did_before_roles_and_reads_back() {
    let plain = MailboxEdit::create("Receipts", None);
    let json = serde_json::to_value(&plain).unwrap();
    // What an outbox queued before a create could carry a role: no `role` key at all.
    assert_eq!(
        json,
        serde_json::json!({ "Create": { "name": "Receipts", "parent": null } })
    );
    assert_eq!(serde_json::from_value::<MailboxEdit>(json).unwrap(), plain);

    let sent = MailboxEdit::create_with_role("Sent", None, MailboxRole::Sent);
    let json = serde_json::to_value(&sent).unwrap();
    assert_eq!(json["Create"]["role"], "sent");
    assert_eq!(serde_json::from_value::<MailboxEdit>(json).unwrap(), sent);
}

/// A target that can give a folder a role as it makes it.
fn sets_roles() -> Capabilities {
    Capabilities::none()
        .with_mail()
        .with_mailbox_writes()
        .with_mailbox_create_role()
}

#[test]
fn a_create_says_which_role_it_asks_for_and_no_other_edit_asks_for_one() {
    let sent = MailboxEdit::create_with_role("Sent", None, MailboxRole::Sent);
    assert_eq!(sent.requested_role(), Some(&MailboxRole::Sent));
    assert_eq!(MailboxEdit::create("Receipts", None).requested_role(), None);
    assert_eq!(
        MailboxEdit::Delete { target: id("f1") }.requested_role(),
        None
    );
}

#[test]
fn a_role_is_named_in_the_transports_form() {
    let sent = MailboxEdit::create_with_role("Sent", None, MailboxRole::Sent);
    assert_eq!(
        sent.role_as(&sets_roles(), MailboxRole::jmap_role),
        Ok(Some("sent"))
    );
    assert_eq!(
        sent.role_as(&sets_roles(), MailboxRole::as_imap_special_use),
        Ok(Some("\\Sent"))
    );
}

#[test]
fn an_edit_that_asks_for_no_role_passes_whatever_the_target() {
    let edits = [
        MailboxEdit::create("Receipts", None),
        MailboxEdit::Delete { target: id("f1") },
    ];
    for edit in &edits {
        assert_eq!(
            edit.role_as(&Capabilities::none(), MailboxRole::jmap_role),
            Ok(None)
        );
        assert_eq!(
            edit.role_as(&sets_roles(), MailboxRole::jmap_role),
            Ok(None)
        );
    }
}

#[test]
fn a_role_asked_of_a_target_whose_capabilities_set_none_is_not_supported() {
    let archive = MailboxEdit::create_with_role("Archive", None, MailboxRole::Archive);
    // The transport could name it; the capabilities say this account cannot set it: an
    // account that cannot change its folders, or one whose target makes no roles.
    for capabilities in [
        Capabilities::none().with_mail(),
        Capabilities::none().with_mail().with_mailbox_writes(),
        Capabilities::none().with_mail().with_mailbox_create_role(),
    ] {
        assert_eq!(
            archive.role_as(&capabilities, MailboxRole::jmap_role),
            Err(CreateRoleError::NotSupported)
        );
    }
}

#[test]
fn a_role_the_transport_cannot_name_is_refused_on_every_transport() {
    for role in [
        MailboxRole::ServerRecords,
        MailboxRole::ChatHistory,
        MailboxRole::Other("snoozed".into()),
    ] {
        let edit = MailboxEdit::create_with_role("X", None, role);
        for form in [MailboxRole::jmap_role, MailboxRole::as_imap_special_use] {
            assert_eq!(
                edit.role_as(&sets_roles(), form),
                Err(CreateRoleError::NoTransportForm)
            );
        }
    }
    // IMAP names the inbox by its name, not by an attribute.
    let inbox = MailboxEdit::create_with_role("X", None, MailboxRole::Inbox);
    assert_eq!(
        inbox.role_as(&sets_roles(), MailboxRole::as_imap_special_use),
        Err(CreateRoleError::NoTransportForm)
    );
}

#[test]
fn a_role_not_given_is_invalid_state_naming_which_with_the_typed_error_as_source() {
    for rule in [
        CreateRoleError::NotSupported,
        CreateRoleError::NoTransportForm,
        CreateRoleError::NotKept {
            mailbox: id("Archive"),
        },
    ] {
        let error = ProviderError::from(rule.clone());
        assert_eq!(error.class(), FailureClass::InvalidState);
        assert_eq!(error.detail(), rule.to_string());
        let source = std::error::Error::source(&error).expect("the typed error");
        assert_eq!(source.downcast_ref::<CreateRoleError>(), Some(&rule));
    }
    // The detail is the rule, never the role: a server's own spelling stays out of it.
    let edit = MailboxEdit::create_with_role("X", None, MailboxRole::Other("snoozed".into()));
    let refusal = edit
        .role_as(&sets_roles(), MailboxRole::jmap_role)
        .unwrap_err();
    let error = ProviderError::from(refusal);
    assert!(!error.detail().contains("snoozed"), "{}", error.detail());
}

#[test]
fn a_receipt_names_the_folder_or_says_none_remains() {
    assert_eq!(
        MailboxEditReceipt::resolved(id("Trash/Bills")).mailbox,
        Some(id("Trash/Bills"))
    );
    assert_eq!(MailboxEditReceipt::removed().mailbox, None);
}
