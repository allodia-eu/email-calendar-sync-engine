# Search Coverage Contract

A search answer must tell the caller how complete it is and why it might be
missing matches. Completeness is not one value: it is several independent axes,
and any subset can apply to a single query. This document defines the coverage
model that every search result carries. `north-star.md` states the requirement
at a high level; this document is authoritative. Read it before working on the
search executor (`engine-search`) or any provider-search fallback.

## Why several axes

Three unrelated mechanisms can each make an answer partial, and they routinely
co-occur:

- **Partial sync** (the Tier 1/2/3 policy in `north-star.md`) means some objects,
  bodies, or attachments are not local, so local FTS cannot match them.
- **Bounded recurrence materialization** (the `event_occurrence` horizon in
  `store-and-sync.md`) means a time-range query can fall outside the expanded
  window.
- **Optional provider-side search** can fill local gaps for some scopes but not
  others.

A single enum (`LocalComplete | LocalWindow | RemoteAugmented | Incomplete`)
conflates *what was searched*, *how it was produced*, and *whether it is
exhaustive*, and cannot represent the normal mobile-calendar state of "locally
windowed **and** time-bounded **and** remotely augmented." Coverage is therefore
a struct of orthogonal facts.

## Model

```rust
/// How complete a search answer is, and why it might be missing matches.
///
/// The axes are independent; any combination can hold for one query.
pub struct SearchCoverage {
    pub local: LocalCoverage,
    pub temporal: TemporalCoverage,
    pub remote: RemoteCoverage,
}

/// Local object and content availability across the query's scopes.
/// Both flags false means the local corpus was fully searchable.
pub struct LocalCoverage {
    /// Some in-scope objects are not local yet: initial backfill is incomplete,
    /// or a retention window excludes them.
    pub unsynced_objects: bool,
    /// Some in-scope objects are present as metadata only; their bodies or
    /// attachments were not indexed (partial-sync tiers), so text matches on
    /// them are missed.
    pub unindexed_content: bool,
}

/// Time-range coverage for results that depend on recurrence expansion.
/// Always `Full` for queries that do not expand occurrences (e.g. all mail
/// queries, and calendar queries with no time bound).
pub enum TemporalCoverage {
    /// The requested range is covered: within the materialized horizon, or
    /// expanded on demand up to the host's expansion cap.
    Full,
    /// The requested range exceeds the expansion cap. Recurring instances
    /// outside `covered` are missing.
    Bounded { covered: TimeRange },
}

/// Whether a provider-side search contributed to the answer. Informational:
/// any gap a non-exhaustive remote search left is already reflected in `local`
/// and `temporal` (see "Composition").
pub enum RemoteCoverage {
    /// Local data only.
    LocalOnly,
    /// A provider search (JMAP `Email/query`, IMAP `SEARCH`, CalDAV time-range
    /// `REPORT`) augmented the answer. `exhaustive` is what the provider
    /// reported about its own result.
    Augmented { exhaustive: bool },
}

impl SearchCoverage {
    /// True when no axis reports a known gap.
    pub fn is_complete(&self) -> bool {
        !self.local.unsynced_objects
            && !self.local.unindexed_content
            && matches!(self.temporal, TemporalCoverage::Full)
    }
}
```

## Axis semantics

### Local

`unsynced_objects` and `unindexed_content` are distinct gaps and may both hold:
the first means objects are missing entirely; the second means objects are known
but their text was not searchable. `unindexed_content` maps directly to Tier 2/3:
a metadata-only object, or one whose body has not been fetched, cannot match a
text query until it is fetched and indexed. Because on-demand fetched bodies are
indexed (`store-and-sync.md`), this axis shrinks over time — coverage is a
property of the answer, not a fixed property of the corpus.

### Temporal (recurrence horizon)

This axis only varies for queries whose results depend on expanded occurrences —
calendar time-range queries over recurring events. Everything else reports
`Full`.

Beyond-horizon behavior is decided here: **a range that exceeds the materialized
horizon is expanded from the master events on demand, up to a host-configured
expansion cap.** Within the cap the result is `Full`; only a range exceeding the
cap reports `Bounded { covered }`, naming the sub-range that is trustworthy. The
engine never silently returns empty for an out-of-horizon range, and never
expands an unbounded `RRULE` past the cap.

### Remote

Records whether a provider search ran and whether the provider called its own
result exhaustive. It is provenance, not a completeness verdict — see below.

## Composition

Remote augmentation is **compensated into the local and temporal axes at
assembly time**, not applied as an override in `is_complete`. When the executor
runs a provider search for a scope and the provider reports an exhaustive result,
that scope contributes no local/temporal gap, because the provider searched its
own full corpus (and CalDAV's time-range `REPORT` expands recurrence
server-side). A non-exhaustive remote result leaves the residual gap visible in
`local`/`temporal`. Either way `remote` records that augmentation happened.

This keeps `is_complete` a plain conjunction and makes it compose correctly
across multi-scope queries, which the override form could not:

- A query spans several scopes (mailboxes, calendars, accounts). `SearchCoverage`
  is the **conservative roll-up** over them: gap flags are OR-ed, and `Bounded`
  `covered` ranges are intersected. One incomplete scope makes the answer
  incomplete even if another scope was remotely augmented to completeness.

Caller pattern: check `is_complete()`; if false, inspect the axes to message the
user precisely ("older messages not downloaded" vs "showing events through 2027"
vs "searching server…") and to decide whether to trigger augmentation.

## Provenance

`engine-search` assembles `SearchCoverage` from three inputs: the per-scope
local object/index state the store reports for the queried scopes, whether
on-demand occurrence expansion was needed and how far it reached, and whether a
provider search was invoked and what it reported. The store and providers supply
facts; the search executor rolls them up.

## Required tests

Lock these before implementing the executor:

- A fully-local, fully-indexed query reports `is_complete()` true and
  `RemoteCoverage::LocalOnly`.
- A scope with metadata-only (un-fetched) bodies reports
  `unindexed_content == true` and `is_complete()` false.
- A calendar range inside the horizon, and one beyond the horizon but within the
  expansion cap, both report `TemporalCoverage::Full`; a range beyond the cap
  reports `Bounded { covered }` naming the trustworthy sub-range.
- An exhaustive provider search clears the gap it covered: the assembled `local`
  and `temporal` show no gap, `remote` is `Augmented { exhaustive: true }`, and
  `is_complete()` is true.
- A non-exhaustive provider search leaves the residual gap in `local`/`temporal`
  and reports `Augmented { exhaustive: false }`; `is_complete()` is false.
- A multi-scope query with one complete and one windowed scope rolls up to
  incomplete, and a `Bounded` range is the intersection across scopes.
