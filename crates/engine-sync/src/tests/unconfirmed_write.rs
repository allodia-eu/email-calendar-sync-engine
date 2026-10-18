//! What a calendar or contact write tells its caller when the answer is not a plain success or
//! a plain refusal: one whose outcome the provider could not learn is returned as such and
//! settles its op rather than waiting for a retry nothing should make, and a failure to record
//! the outcome never turns either that or an applied write into "nothing happened".

use engine_core::{
    contact::{ContactCard, ContactDraft, ContactPatch},
    ids::{AddressBookId, ContactId},
};
use engine_provider::{ContactWriteReceipt, ContactsProvider};

use super::*;
use crate::{SyncError, create_contact, delete_contact, patch_contact};

/// A provider whose every calendar and contact write is `applied` or loses its answer. With
/// `lapse`, the write also outlives its lease and is recovered by the store before it returns,
/// so the driver's own record of the outcome is refused.
struct Writes<'a> {
    applied: bool,
    lapse: Option<(&'a SqliteStore<ManualClock>, ManualClock)>,
}

impl Writes<'_> {
    async fn attempt(&self) -> ProviderResult<()> {
        if let Some((store, clock)) = &self.lapse {
            clock.advance(Duration::from_mins(2));
            store.recover_interrupted_ops().await.unwrap();
        }
        if self.applied {
            return Ok(());
        }
        Err(ProviderError::needs_confirmation(
            "the reply never came; the change may have been applied",
        ))
    }

    async fn card(&self) -> ProviderResult<ContactWriteReceipt> {
        self.attempt().await?;
        Ok(ContactWriteReceipt::new(card().id))
    }
}

impl Provider for Writes<'_> {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_contacts())
    }
}

impl engine_provider::MailboxWrites for Writes<'_> {}

#[async_trait::async_trait]
impl CalendarWrites for Writes<'_> {
    async fn create_event(
        &self,
        _account: &AccountId,
        draft: &EventDraft,
    ) -> ProviderResult<EventWriteReceipt> {
        self.attempt().await?;
        Ok(EventWriteReceipt::new(
            event(),
            draft.uid.clone(),
            RevisionTokens::none(),
        ))
    }

    async fn delete_event(
        &self,
        _account: &AccountId,
        _base: Option<&Event>,
        _deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        self.attempt().await
    }
}

#[async_trait::async_trait]
impl ContactsProvider for Writes<'_> {
    async fn create_contact(
        &self,
        _account: &AccountId,
        _draft: &ContactDraft,
    ) -> ProviderResult<ContactWriteReceipt> {
        self.card().await
    }

    async fn patch_contact(
        &self,
        _account: &AccountId,
        _base: &ContactCard,
        _patch: &ContactPatch,
    ) -> ProviderResult<ContactWriteReceipt> {
        self.card().await
    }

    async fn delete_contact(
        &self,
        _account: &AccountId,
        _base: &ContactCard,
    ) -> ProviderResult<()> {
        self.attempt().await
    }
}

const TTL: Duration = Duration::from_mins(1);

fn book() -> AddressBookId {
    AddressBookId::try_from("personal").unwrap()
}

fn card() -> ContactCard {
    let mut card = ContactCard::new(
        ContactId::try_from("card-1").unwrap(),
        Memberships::of_one(book()),
    );
    card.is_writable = true;
    card
}

fn event() -> EventId {
    EventId::try_from("/cal/default/evt-9.ics").unwrap()
}

fn draft() -> EventDraft {
    let at = |hour: u8| {
        CalendarDateTime::utc(
            format!("2026-08-01T{hour:02}:00:00")
                .parse::<LocalDateTime>()
                .unwrap(),
        )
    };
    EventDraft::new(
        CalendarId::new(ProviderKey::new("/cal/default/").unwrap()),
        Uid::new("evt-9@test.local").unwrap(),
        "Planning",
        at(9),
        at(10),
        "2026-07-14T10:00:00Z".parse().unwrap(),
    )
}

/// Runs each write the drivers offer against `provider`, returning what each one answered
/// under the idempotency key it used.
async fn every_write(
    provider: &Writes<'_>,
    store: &SqliteStore<ManualClock>,
) -> Vec<(&'static str, Option<SyncError>)> {
    let deletion = EventDeletion::unconditional(event(), Uid::new("evt-9@test.local").unwrap());
    let contact = ContactDraft {
        address_book: book(),
        card: card(),
    };
    let (a, w) = (account(), worker());
    let patch = ContactPatch::default();
    vec![
        (
            "event-create",
            create_calendar_event(
                provider,
                store,
                &a,
                w.clone(),
                TTL,
                "event-create",
                &draft(),
            )
            .await
            .err(),
        ),
        (
            "event-delete",
            delete_calendar_event(
                provider,
                store,
                &a,
                w.clone(),
                TTL,
                "event-delete",
                None,
                &deletion,
            )
            .await
            .err(),
        ),
        (
            "card-create",
            create_contact(provider, store, &a, w.clone(), TTL, "card-create", &contact)
                .await
                .err(),
        ),
        (
            "card-patch",
            patch_contact(
                provider,
                store,
                &a,
                w.clone(),
                TTL,
                "card-patch",
                &card(),
                &patch,
            )
            .await
            .err(),
        ),
        (
            "card-delete",
            delete_contact(provider, store, &a, w, TTL, "card-delete", &card())
                .await
                .err(),
        ),
    ]
}

fn assert_unconfirmed(key: &str, err: Option<SyncError>) {
    match err {
        Some(SyncError::Provider(e)) => assert!(e.requires_confirmation(), "{key}: {e}"),
        other => panic!("{key}: expected the provider's answer, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unconfirmed_write_reaches_the_caller_and_settles_its_op() {
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let provider = Writes {
        applied: false,
        lapse: None,
    };

    for (key, err) in every_write(&provider, &store).await {
        assert_unconfirmed(key, err);
        // Settled, not parked for a retry: a replay of a create the server may already hold
        // would make it twice.
        let op = store
            .enqueue_pending_op(
                account(),
                PendingOp::new(
                    IdempotencyKey::new(key).unwrap(),
                    PendingOpKind::CalendarCreate,
                    ResourceKey::new(key).unwrap(),
                    serde_json::Value::Null,
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            store.pending_op_state(op).await.unwrap(),
            Some(PendingOpState::Failed),
            "{key}"
        );
    }
}

#[tokio::test]
async fn a_record_that_fails_never_hides_that_the_write_may_have_been_applied() {
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let provider = Writes {
        applied: false,
        lapse: Some((&store, clock)),
    };

    for (key, err) in every_write(&provider, &store).await {
        assert_unconfirmed(key, err);
    }
}

#[tokio::test]
async fn an_applied_write_whose_success_cannot_be_recorded_says_it_was_applied() {
    let clock = clock();
    let store = SqliteStore::open_in_memory(clock.clone()).unwrap();
    let provider = Writes {
        applied: true,
        lapse: Some((&store, clock)),
    };

    for (key, err) in every_write(&provider, &store).await {
        assert!(
            matches!(err, Some(SyncError::Unrecorded(_))),
            "{key}: expected the write to be reported applied, got {err:?}"
        );
    }
}
