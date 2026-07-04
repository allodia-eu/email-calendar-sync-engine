//! The top-level `Event -> Vec<OccurrenceRow>` expander.
//!
//! Ties [`crate::rule`] (date generation) and [`crate::zone`] (UTC resolution)
//! together, applies per-instance overrides/exclusions, and filters to the
//! horizon. Pure and deterministic: the same event, horizon, and host zone always
//! produce byte-identical rows under a fixed [`crate::tzdata_version`].

use std::collections::BTreeSet;

use engine_core::calendar::{Event, RecurrenceBound, RecurrenceOverride};
use engine_core::ids::ProviderKey;
use engine_core::patch::PatchObject;
use engine_core::time::{CalendarDateTime, Duration, LocalDateTime, TimeZoneId, UtcDateTime};
use engine_store::OccurrenceRow;
use jiff::civil::{Date, Time};
use serde_json::Value;

use crate::{ExpandError, Horizon, TzdataVersion, rule, tzdata_version, zone};

/// A generous cap on instances generated per rule, a backstop against a rule that
/// matches implausibly often over the horizon (the horizon already bounds normal
/// generation).
const INSTANCE_CAP: usize = 100_000;

/// The resolution context shared by every instance of one event: the zone its
/// wall-clock times resolve through, plus the master start's date and time-of-day.
struct Context {
    zone: jiff::tz::TimeZone,
    date: Date,
    time: Time,
}

/// Expands `event` into the occurrences whose start falls within `horizon`.
///
/// Single events materialize one occurrence (so time-range search matches them
/// too); recurring masters expand their rules and inline overrides; a standalone
/// override-instance object (its `recurrence_id` set) expands to its own single
/// occurrence. Floating times resolve through `host_zone`; zoned times through the
/// event's own IANA zone; all-day values are zoneless UTC-midnight.
///
/// # Errors
///
/// Returns an [`ExpandError`] for an out-of-subset rule, an unresolvable zone, a
/// malformed override, an unrepresentable date, or expansion past the safety cap.
pub fn expand(
    event: &Event,
    horizon: &Horizon,
    host_zone: &TimeZoneId,
) -> Result<Vec<OccurrenceRow>, ExpandError> {
    let materializer = Materializer {
        event,
        horizon: *horizon,
        host_zone,
        key: event.id.key().clone(),
        version: tzdata_version(),
    };
    materializer.run()
}

/// Holds the per-event invariants so the per-instance helpers stay small.
struct Materializer<'a> {
    event: &'a Event,
    horizon: Horizon,
    host_zone: &'a TimeZoneId,
    key: ProviderKey,
    version: TzdataVersion,
}

impl Materializer<'_> {
    fn run(&self) -> Result<Vec<OccurrenceRow>, ExpandError> {
        // A cancelled event (or instance object) is a tombstone: no occurrences.
        if self.event.is_cancelled() {
            return Ok(Vec::new());
        }
        // A standalone override-instance object: exactly one occurrence.
        if let Some(recurrence_id) = &self.event.recurrence_id {
            return self.standalone_instance(recurrence_id);
        }
        // A recurring master: expand the rules and overrides.
        if let Some(recurrence) = self
            .event
            .recurrence
            .as_ref()
            .filter(|r| !r.rules.is_empty())
        {
            return self.recurring(recurrence);
        }
        // A plain single event: one occurrence so time-range search can match it.
        let ctx = self.context(&self.event.start)?;
        let (start, end) =
            zone::resolve_range(&ctx.zone, zone::at(ctx.date, ctx.time), self.event.duration)?;
        if self.in_window(start) {
            Ok(vec![self.row(start, end, None)])
        } else {
            Ok(Vec::new())
        }
    }

    /// A standalone override-instance object → its own occurrence, tagged with the
    /// recurrence id it replaces.
    fn standalone_instance(
        &self,
        recurrence_id: &CalendarDateTime,
    ) -> Result<Vec<OccurrenceRow>, ExpandError> {
        let ctx = self.context(&self.event.start)?;
        let (start, end) =
            zone::resolve_range(&ctx.zone, zone::at(ctx.date, ctx.time), self.event.duration)?;
        if !self.in_window(start) {
            return Ok(Vec::new());
        }
        let rid = self.resolve_point(recurrence_id)?;
        Ok(vec![self.row(start, end, Some(rid))])
    }

    /// A recurring master: union the rules (minus excluded rules), apply overrides
    /// and RDATE-like additions, and filter to the horizon.
    fn recurring(
        &self,
        recurrence: &engine_core::calendar::Recurrence,
    ) -> Result<Vec<OccurrenceRow>, ExpandError> {
        let ctx = self.context(&self.event.start)?;
        let window_end = zone::date(
            self.horizon.end().year(),
            self.horizon.end().month(),
            self.horizon.end().day(),
        )?;
        let window_end = window_end
            .checked_add(jiff::Span::new().days(1))
            .map_err(|_| ExpandError::OutOfRange)?;

        let mut base: BTreeSet<Date> = BTreeSet::new();
        for r in &recurrence.rules {
            for date in rule::occurrence_dates(ctx.date, r, window_end, INSTANCE_CAP)? {
                // Precise UNTIL: the instant's wall-clock (date + fixed time-of-day)
                // must be at or before UNTIL. The rule bounds at date granularity.
                if let RecurrenceBound::Until(until) = &r.bound
                    && instance_rid(&ctx, date)? > *until
                {
                    continue;
                }
                base.insert(date);
            }
        }
        for r in &recurrence.excluded_rules {
            for date in rule::occurrence_dates(ctx.date, r, window_end, INSTANCE_CAP)? {
                base.remove(&date);
            }
        }

        let mut rows: Vec<OccurrenceRow> = Vec::new();
        let mut covered: BTreeSet<LocalDateTime> = BTreeSet::new();
        for date in &base {
            let rid = instance_rid(&ctx, *date)?;
            covered.insert(rid);
            match recurrence.overrides.get(&rid) {
                Some(RecurrenceOverride::Excluded) => {}
                Some(RecurrenceOverride::Patch(patch)) => {
                    if let Some(row) = self.override_row(&ctx, rid, patch)? {
                        rows.push(row);
                    }
                }
                None => {
                    let (start, end) = zone::resolve_range(
                        &ctx.zone,
                        zone::at(*date, ctx.time),
                        self.event.duration,
                    )?;
                    if self.in_window(start) {
                        rows.push(self.row(start, end, None));
                    }
                }
            }
        }
        // RDATE-like additions: override entries that the rules did not produce.
        for (rid, over) in &recurrence.overrides {
            if covered.contains(rid) {
                continue;
            }
            if let RecurrenceOverride::Patch(patch) = over
                && let Some(row) = self.override_row(&ctx, *rid, patch)?
            {
                rows.push(row);
            }
        }

        rows.sort_by(|a, b| {
            a.start
                .cmp(&b.start)
                .then_with(|| a.recurrence_id.cmp(&b.recurrence_id))
        });
        Ok(rows)
    }

    /// Materializes one overridden (or RDATE-added) instance from its patch, or
    /// `None` if it is cancelled or out of the horizon.
    fn override_row(
        &self,
        ctx: &Context,
        rid: LocalDateTime,
        patch: &PatchObject,
    ) -> Result<Option<OccurrenceRow>, ExpandError> {
        if patch_cancelled(patch) {
            return Ok(None);
        }
        let rid_utc = zone::resolve(
            &ctx.zone,
            zone::at(zone::local_date(rid)?, zone::local_time(rid)?),
        )?;
        let start_local = patch_start(patch)
            .map_err(|reason| invalid(rid, reason))?
            .unwrap_or(rid);
        let zone = match patch_timezone(patch).map_err(|reason| invalid(rid, reason))? {
            Some(id) => zone::resolve_zone_id(&id)?,
            None => ctx.zone.clone(),
        };
        let duration = patch_duration(patch)
            .map_err(|reason| invalid(rid, reason))?
            .unwrap_or(self.event.duration);
        let dt = zone::at(
            zone::local_date(start_local)?,
            zone::local_time(start_local)?,
        );
        let (start, end) = zone::resolve_range(&zone, dt, duration)?;
        Ok(self
            .in_window(start)
            .then(|| self.row(start, end, Some(rid_utc))))
    }

    /// Builds the resolution context from a scheduled time.
    fn context(&self, value: &CalendarDateTime) -> Result<Context, ExpandError> {
        match value {
            CalendarDateTime::Date(date) => Ok(Context {
                zone: zone::utc(),
                date: zone::calendar_date(*date)?,
                time: Time::MIN,
            }),
            CalendarDateTime::Floating(local) => Ok(Context {
                zone: zone::resolve_zone_id(self.host_zone)?,
                date: zone::local_date(*local)?,
                time: zone::local_time(*local)?,
            }),
            CalendarDateTime::Zoned { local, zone } => Ok(Context {
                zone: zone::resolve_zone_id(zone)?,
                date: zone::local_date(*local)?,
                time: zone::local_time(*local)?,
            }),
        }
    }

    /// Resolves a single scheduled time to a UTC instant (for a recurrence id).
    fn resolve_point(&self, value: &CalendarDateTime) -> Result<UtcDateTime, ExpandError> {
        let ctx = self.context(value)?;
        zone::resolve(&ctx.zone, zone::at(ctx.date, ctx.time))
    }

    fn row(
        &self,
        start: UtcDateTime,
        end: UtcDateTime,
        recurrence_id: Option<UtcDateTime>,
    ) -> OccurrenceRow {
        OccurrenceRow {
            event: self.key.clone(),
            start,
            end,
            recurrence_id,
            tzdata_version: self.version.clone(),
        }
    }

    fn in_window(&self, start: UtcDateTime) -> bool {
        start >= self.horizon.start() && start < self.horizon.end()
    }
}

/// The original wall-clock recurrence id of a base instance: its date plus the
/// master start's time-of-day.
fn instance_rid(ctx: &Context, date: Date) -> Result<LocalDateTime, ExpandError> {
    let out_of_range = |_| ExpandError::OutOfRange;
    LocalDateTime::new(
        i32::from(date.year()),
        u8::try_from(date.month()).map_err(out_of_range)?,
        u8::try_from(date.day()).map_err(out_of_range)?,
        u8::try_from(ctx.time.hour()).map_err(out_of_range)?,
        u8::try_from(ctx.time.minute()).map_err(out_of_range)?,
        u8::try_from(ctx.time.second()).map_err(out_of_range)?,
    )
    .map_err(ExpandError::from)
}

/// Builds an [`ExpandError::InvalidOverride`] for the given recurrence id.
fn invalid(rid: LocalDateTime, reason: &'static str) -> ExpandError {
    ExpandError::InvalidOverride {
        recurrence_id: rid.to_string(),
        reason,
    }
}

/// Whether an override patch cancels its instance (`status: cancelled`).
fn patch_cancelled(patch: &PatchObject) -> bool {
    patch.get("status").and_then(Value::as_str) == Some("cancelled")
}

/// Reads an optional, non-null string-valued patch field. An absent field and an
/// explicit `null` (reset-to-default) both yield `Ok(None)`.
fn patch_str<'a>(patch: &'a PatchObject, field: &str) -> Result<Option<&'a str>, &'static str> {
    match patch.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or("override field must be a string"),
    }
}

/// The override's moved `start`, if it patches one.
fn patch_start(patch: &PatchObject) -> Result<Option<LocalDateTime>, &'static str> {
    patch_str(patch, "start")?
        .map(|text| text.parse().map_err(|_| "malformed override start"))
        .transpose()
}

/// The override's patched `duration`, if any.
fn patch_duration(patch: &PatchObject) -> Result<Option<Duration>, &'static str> {
    patch_str(patch, "duration")?
        .map(|text| text.parse().map_err(|_| "malformed override duration"))
        .transpose()
}

/// The override's patched `timeZone`, if any (a non-null IANA name).
fn patch_timezone(patch: &PatchObject) -> Result<Option<TimeZoneId>, &'static str> {
    patch_str(patch, "timeZone")?
        .map(|text| TimeZoneId::iana(text).map_err(|_| "empty override timeZone"))
        .transpose()
}
