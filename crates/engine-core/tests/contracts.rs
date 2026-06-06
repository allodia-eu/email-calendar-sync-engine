//! Conformance tests for the sync, search, and write contract types, plus
//! scheduling reconciliation and trust.

use std::collections::BTreeSet;

use engine_core::coverage::{LocalCoverage, RemoteCoverage, SearchCoverage, TemporalCoverage};
use engine_core::error::FailureClass;
use engine_core::ids::{AccountId, DavCollectionId, MailboxId, ProviderKey, Uid};
use engine_core::patch::PatchObject;
use engine_core::scheduling::{
    ImipTrust, ImipUntrusted, InstanceKey, Revision, ScheduleMethod, SchedulingMode,
    evaluate_imip_trust,
};
use engine_core::sync::{JmapDataType, SyncScope, SyncState, SyncUpdate};
use engine_core::time::CalendarDateTime;
use engine_core::version::{ChangeKey, ETag, ModSeq, RevisionTokens, ScheduleTag};
use engine_core::write::{
    CreationId, IdempotencyKey, PendingOp, PendingOpId, PendingOutcome, ResourceKey, WriteKeyError,
};

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

#[test]
fn sync_scope_variants_share_account() {
    let scopes = [
        SyncScope::JmapType {
            account: account(),
            data_type: JmapDataType::CalendarEvent,
        },
        SyncScope::ImapMailbox {
            account: account(),
            mailbox: MailboxId::try_from("inbox").unwrap(),
        },
        SyncScope::DavCollection {
            account: account(),
            collection: DavCollectionId::try_from("/calendars/work/").unwrap(),
        },
    ];
    for scope in &scopes {
        assert_eq!(scope.account(), &account());
    }
    // Container types apply before member types.
    assert!(JmapDataType::Mailbox.is_container());
    assert!(JmapDataType::Calendar.is_container());
    assert!(!JmapDataType::EmailSubmission.is_container());
    assert!(!JmapDataType::Thread.is_container());
    assert_eq!(
        JmapDataType::from_wire("VacationResponse"),
        JmapDataType::Other("VacationResponse".into())
    );
}

#[test]
fn sync_state_is_opaque() {
    let state = SyncState::new("cursor-123");
    assert_eq!(state.as_str(), "cursor-123");
    assert_eq!(state, SyncState::new("cursor-123"));
}

#[test]
fn sync_update_delta_and_snapshot() {
    let delta: SyncUpdate<String> = SyncUpdate::delta(
        vec!["changed".to_owned()],
        vec![ProviderKey::new("gone").unwrap()],
    );
    assert!(!delta.is_snapshot());

    let present: BTreeSet<ProviderKey> = [ProviderKey::new("x").unwrap()].into_iter().collect();
    let snapshot: SyncUpdate<String> = SyncUpdate::snapshot(vec!["x".to_owned()], present);
    assert!(snapshot.is_snapshot());
}

#[test]
fn search_coverage_axes_and_rollup() {
    assert!(SearchCoverage::complete().is_complete());

    let local_gap = SearchCoverage {
        local: LocalCoverage {
            unsynced_objects: true,
            unindexed_content: true,
        },
        temporal: TemporalCoverage::Full,
        remote: RemoteCoverage::LocalOnly,
    };
    assert!(!local_gap.is_complete());

    let rolled = SearchCoverage::roll_up([SearchCoverage::complete(), local_gap]);
    assert!(!rolled.is_complete());
    assert!(rolled.local.unsynced_objects);
    assert_eq!(rolled.remote, RemoteCoverage::LocalOnly);
}

#[test]
fn revision_tokens_cover_every_provider_shape() {
    assert!(RevisionTokens::none().is_empty());
    assert!(!RevisionTokens::from_etag(ETag::new("v1")).is_empty());
    let full = RevisionTokens {
        etag: Some(ETag::new("e")),
        schedule_tag: Some(ScheduleTag::new("s")),
        change_key: Some(ChangeKey::new("c")),
        mod_seq: Some(ModSeq::new(7)),
    };
    assert!(!full.is_empty());
    assert_eq!(full.etag.as_ref().unwrap().as_str(), "e");
    assert_eq!(full.schedule_tag.as_ref().unwrap().as_str(), "s");
    assert_eq!(full.change_key.as_ref().unwrap().as_str(), "c");
    assert_eq!(full.mod_seq.unwrap().get(), 7);
}

#[test]
fn failure_classes_classify() {
    assert!(FailureClass::Retryable.is_retryable());
    assert!(FailureClass::RateLimited.is_retryable());
    assert!(!FailureClass::Permanent.is_retryable());
    assert!(FailureClass::NeedsResync.requires_resync());
    assert!(!FailureClass::Conflict.requires_resync());
}

#[test]
fn pending_ops_and_outcomes() {
    let mut op = PendingOp::new(
        IdempotencyKey::new("idem-1").unwrap(),
        ResourceKey::new("event:uid-1").unwrap(),
        serde_json::json!({ "op": "rsvp", "status": "accepted" }),
    );
    op.depends_on.push(PendingOpId::new(3));
    assert_eq!(op.depends_on[0].get(), 3);
    assert_eq!(op.idempotency_key.as_str(), "idem-1");
    assert_eq!(op.resource_key.as_str(), "event:uid-1");

    let creation = CreationId::new("#local-1").unwrap();
    assert_eq!(creation.as_str(), "#local-1");

    let succeeded = PendingOutcome::Succeeded {
        provider_key: ProviderKey::new("server-id").unwrap(),
    };
    let failed = PendingOutcome::Failed {
        class: FailureClass::Conflict,
        retry_after: None,
    };
    let needs = PendingOutcome::NeedsConfirmation {
        detail: "ambiguous send".into(),
    };
    assert_ne!(succeeded, failed);
    assert_ne!(failed, needs);

    // The write keys reject empty values.
    assert_eq!(IdempotencyKey::new(""), Err(WriteKeyError));
    assert_eq!(ResourceKey::new(""), Err(WriteKeyError));
    assert_eq!(CreationId::new(""), Err(WriteKeyError));
    assert!(WriteKeyError.to_string().contains("must not be empty"));
}

#[test]
fn patch_object_accessors() {
    let empty = PatchObject::default();
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    let patch = PatchObject::new([("title".to_owned(), serde_json::json!("x"))]).unwrap();
    assert!(!patch.is_empty());
    assert_eq!(patch.len(), 1);
    assert_eq!(patch.iter().count(), 1);
}

#[test]
fn scheduling_modes_and_instance_keys() {
    assert_ne!(
        SchedulingMode::ServerAutoSchedule,
        SchedulingMode::ClientImip
    );
    let uid = Uid::new("uid-1").unwrap();
    let series = InstanceKey::series(uid.clone());
    assert!(series.is_series());
    let instance = InstanceKey::instance(
        uid,
        CalendarDateTime::Floating("2021-06-07T09:00:00".parse().unwrap()),
    );
    assert!(!instance.is_series());
}

#[test]
fn revision_reconciliation() {
    let dtstamp = |s: &str| s.parse().unwrap();
    let older = Revision::new(1, dtstamp("2021-01-01T00:00:00Z"));
    let newer = Revision::new(2, dtstamp("2020-01-01T00:00:00Z"));
    assert!(newer.supersedes(&older));
    assert!(!older.supersedes(&newer));
}

#[test]
fn imip_trust_covers_every_verdict() {
    // Trusted organizer.
    assert_eq!(
        evaluate_imip_trust(
            &ScheduleMethod::Request,
            Some("boss@example.com"),
            None,
            Some("boss@example.com"),
        ),
        ImipTrust::Trusted
    );
    // Mismatched sender.
    assert_eq!(
        evaluate_imip_trust(
            &ScheduleMethod::Cancel,
            Some("boss@example.com"),
            None,
            Some("evil@example.com"),
        ),
        ImipTrust::Untrusted(ImipUntrusted::SenderMismatch {
            expected: "organizer"
        })
    );
    // Unauthenticated.
    assert_eq!(
        evaluate_imip_trust(
            &ScheduleMethod::Request,
            Some("boss@example.com"),
            None,
            None
        ),
        ImipTrust::Untrusted(ImipUntrusted::Unauthenticated)
    );
    // Missing body identity to verify against.
    let missing = evaluate_imip_trust(
        &ScheduleMethod::Reply,
        Some("boss@example.com"),
        None,
        Some("guest@example.com"),
    );
    assert_eq!(
        missing,
        ImipTrust::Untrusted(ImipUntrusted::MissingIdentity)
    );

    // Every untrusted reason renders a message.
    for reason in [
        ImipUntrusted::Unauthenticated,
        ImipUntrusted::MissingIdentity,
        ImipUntrusted::SenderMismatch {
            expected: "attendee",
        },
    ] {
        assert!(!reason.to_string().is_empty());
    }
}
