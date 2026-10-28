//! Schema steps that add a column to `sync_scope`: facts about what one scope's rows currently
//! are, written under that scope's lease. v6, the expansion window; v19, the collection tag.

/// Migration v6: the per-scope **expansion window** — the horizon an event scope's
/// occurrence rows are materialized over, and the zone they were resolved through.
///
/// Nullable, and only ever set for event scopes: the rows were always *relative to* a
/// window, but the window itself was implicit, so a pass that re-derived one changed event
/// over whatever horizon its caller happened to hold deleted that event's rows outside it
/// and re-materialized only its own — silently dropping occurrences the host had already
/// expanded. Recording it lets a sync (and a post-write reconcile) re-expand a changed
/// event over the window the store actually holds.
pub(crate) const V6: &str = "\
ALTER TABLE sync_scope ADD COLUMN horizon_start TEXT;
ALTER TABLE sync_scope ADD COLUMN horizon_end   TEXT;
ALTER TABLE sync_scope ADD COLUMN expansion_zone TEXT;
";

/// Migration v19: the per-scope **collection tag**, the listing's marker for a collection's
/// contents (`CollectionTag`) as it stood when the scope's cursor was recorded.
///
/// Nullable, and set only for scopes whose listing carries one (CalDAV calendars). A pass that
/// finds the listing's tag equal to this one skips the collection without asking it anything.
pub(crate) const V19: &str = "\
ALTER TABLE sync_scope ADD COLUMN collection_tag TEXT;
";
