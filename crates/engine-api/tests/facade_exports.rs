//! Every calendar-write intent a host states must be **nameable** through `engine_api`.
//!
//! Nothing here asserts behaviour. The gate is the compile: `engine-api` re-exports the
//! types a host passes to the facade, and a type left off that list makes the variant that
//! carries it unconstructable — `PatchTarget` was exported while `Occurrence` was not, so
//! `PatchTarget::Instance` could not be built at all, and every behavioural test still
//! passed because none of them named it.
//!
//! So: `use engine_api::…` only. Reaching into `engine_provider` or `engine_core` here
//! would defeat the point.

use engine_api::{
    CalendarDateTime, Capabilities, DeleteTarget, DraftRecurrence, Frequency, LocalDateTime,
    Occurrence, OverrideSurvival, PatchObject, PatchTarget, Recurrence, RecurrenceEdit,
    RecurrenceOverride, RecurrenceRule, TimeZoneId, UtcDateTime, WriteGuard,
};

fn wall_clock() -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: LocalDateTime::new(2026, 8, 24, 9, 0, 0).unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

fn instant() -> UtcDateTime {
    UtcDateTime::new(2026, 8, 24, 7, 0, 0).unwrap()
}

#[test]
fn one_occurrence_can_be_named_for_a_patch() {
    let target = PatchTarget::Instance(Occurrence::at(wall_clock(), instant()));
    assert_ne!(target, PatchTarget::Series);
}

#[test]
fn one_occurrence_can_be_named_for_a_delete() {
    let target = DeleteTarget::Occurrence {
        occurrence: Occurrence::starting(wall_clock()),
        stamp: instant(),
    };
    assert_ne!(target, DeleteTarget::Series);
}

#[test]
fn a_recurrence_can_be_set_and_cleared() {
    let rule = RecurrenceRule::new(Frequency::Weekly);
    let set = RecurrenceEdit::Set(Box::new(DraftRecurrence::ending_at(rule, instant())));
    assert_ne!(set, RecurrenceEdit::Clear);
}

#[test]
fn what_a_series_edit_costs_can_be_read_and_named() {
    // A host reads this before offering the edit and words its warning from it, so it has to
    // be able to hold the answer — not just call the method and match inline.
    let caps = Capabilities::none().with_calendar_writes(WriteGuard::Enforced, GOOGLE_SHAPED);
    let survival: OverrideSurvival = caps.override_survival().expect("stated with the writes");
    assert!(survival.warns_on_series_edit());
    assert_ne!(survival, OverrideSurvival::kept());
}

/// The survival answer of the one transport that overwrites an override's own fields.
const GOOGLE_SHAPED: OverrideSurvival = OverrideSurvival {
    survives_time_change: false,
    survives_rule_change: true,
    clobbers_own_fields: true,
};

#[test]
fn what_one_occurrence_changed_can_be_read_off_the_series() {
    // `RecurrenceOverride::Patch` is exported; without `PatchObject` a host could match the
    // variant and then not name what it was holding.
    let patch: PatchObject =
        PatchObject::new(vec![("title".to_owned(), "Moved".into())]).expect("a well-formed patch");
    let mut recurrence = Recurrence::default();
    recurrence.overrides.insert(
        LocalDateTime::new(2026, 8, 24, 9, 0, 0).unwrap(),
        RecurrenceOverride::Patch(patch),
    );

    let stored = recurrence.overrides.values().next().expect("one override");
    let RecurrenceOverride::Patch(patch) = stored else {
        panic!("expected a patch");
    };
    assert_eq!(patch.get("title").and_then(|v| v.as_str()), Some("Moved"));
}
