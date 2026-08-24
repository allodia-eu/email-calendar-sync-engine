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
    CalendarDateTime, DeleteTarget, DraftRecurrence, Frequency, LocalDateTime, Occurrence,
    PatchTarget, RecurrenceEdit, RecurrenceRule, TimeZoneId, UtcDateTime,
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
