# Modeling Guidance

The domain model is the load-bearing part of this project. Do not implement or change model types until the relevant primary specs and provider docs have been checked.

## Required Sources

Use primary sources first:
- JMAP Core, Mail, and Submission RFCs.
- JSCalendar RFC for the normalized calendar data model.
- JMAP Calendars draft when implementing that transport; do not treat it as equally mature with JMAP Mail.
- IMAP, SMTP, iCalendar, iTIP/iMIP, CalDAV/CardDAV RFCs.
- Provider-specific docs for each adapter when implementation starts (for example, the Gmail API and Microsoft Graph references for those adapters).

Capture any provider-specific assumption in tests or fixtures. If a provider behavior is observed but not documented, label it as observed behavior and keep it out of generic invariants unless at least two providers prove it portable.

## Core Invariants

- Provider object identity and collection membership are separate.
- Stored mail objects are provider objects. Do not coalesce IMAP copies into one row by `Message-ID`.
- IMAP messages are distinct per `(mailbox, UIDVALIDITY, UID)`.
- JMAP/Gmail-style objects may have multiple mailbox/label memberships.
- UI/search deduplication is presentation policy, not storage identity.
- Events may have multiple calendar memberships where a provider supports it; one-calendar membership remains the common case.
- Keywords (user-settable state such as read/flagged) and membership (collection placement) are distinct axes. A provider's flag/label namespace partitions across both, plus role: JMAP keywords and IMAP flags are keywords; mailboxes/folders and most labels are membership; some Gmail system labels are keywords (`UNREAD`, `STARRED`, `IMPORTANT`), not membership.
- Collections carry a normalized role (inbox, sent, drafts, trash, junk, archive, all) mapped from provider roles — JMAP `role`, IMAP SPECIAL-USE, Gmail system labels, Graph well-known names — distinct from id and display name.
- User-set tags differ from provider-assigned classifications: user categories/keywords (which may span mail and calendar and reference a per-account registry of name and color) versus classifications the user does not set directly (focused/other, inbox tabs).
- `Message-ID` is a threading/reconciliation hint, not hard identity.
- Raw provider payloads are preserved for lossless re-derivation: MIME, iCalendar, JSCalendar, vCard.
- Provider-defined extended properties and extensions (Microsoft Graph extended properties and open extensions, Google Calendar `extendedProperties`) are preserved as normalized, namespaced key-value data — distinct from raw payloads and from first-class fields.
- Provider object keys are stable across moves; where a provider's natural id is not (Graph default ids), the adapter uses its immutable-id form, with a version token (ETag, `changeKey`, MODSEQ) tracking revisions.
- Attachments span kinds — file (bytes), item (an embedded message/event), reference (an external/cloud link with no bytes), and inline (CID); quota and host-open policy apply to byte content only.
- Normalized messages expose distinct sent, received, and last-modified timestamps, and separate the full body from a reply-unique body used for snippets and indexing.
- Calendar normalization must support floating times, all-day events, embedded timezones, recurrence overrides, exclusions, and cross-DST expansion.
- A calendar collection carries access rights, subscription/visibility, owner, default reminders (which events may inherit), and color — not only event membership.
- Events carry a kind discriminator (default, plus provider kinds such as working-location, focus-time, out-of-office, birthday); the model records the kind and preserves its payload even when the JSCalendar projection cannot express the behavior.
- Recurring event range search uses bounded materialized occurrences.
- Thread ids carry provenance: provider-assigned or locally-derived.
- Writes are represented as durable pending operations before any provider side effect.
- Pending operations may have dependencies and local-id to provider-id resolution.
- Model types for sync/store contracts live in `engine-core` or a dedicated async-free contracts crate, not in `engine-sync`.

## Test Requirements

Before model implementation, create fixtures for:
- JMAP Email with multiple `mailboxIds`.
- JMAP Email keywords with system and arbitrary values.
- JMAP CalendarEvent with multiple `calendarIds`.
- JSCalendar recurrence rules, recurrence overrides, excluded overrides, participants, and virtual locations.
- iCalendar RRULE/RDATE/EXDATE/RECURRENCE-ID with embedded VTIMEZONE.
- IMAP UID identity across folder moves and UIDVALIDITY reset.
- Duplicate or missing `Message-ID` values.
- Partial-sync bodies where search coverage is incomplete.
- Create-then-edit offline write dependency chains.
- Occurrence expansion across timezone changes and DST boundaries.
- Embedded `VTIMEZONE` that disagrees with the IANA definition of the same `TZID`.
- iMIP scheduling messages (`METHOD:REQUEST`/`REPLY`/`CANCEL`) reconciled by `UID`/`SEQUENCE`/`RECURRENCE-ID`.
- Collection role mapping across JMAP `role`, IMAP SPECIAL-USE, Gmail system labels, and Graph well-known names.
- Provider extended properties and extensions preserved and re-derived without loss.
- Event kinds (working-location, focus-time, out-of-office, birthday) preserved through normalization.
- Reference and item attachments represented distinctly from byte attachments.

Every model conversion should have at least:
- Parse/normalize test.
- Raw preservation test.
- Round-trip or re-derivation test where the protocol permits it.
- Negative test for malformed or ambiguous data.

## Review Questions

Ask these before merging model changes:
- Does this type encode a real invariant, or just mirror one provider?
- Can this survive JMAP, IMAP/CalDAV, Gmail, and Graph?
- Does it preserve provider object identity without unsafe coalescing?
- Is absence represented precisely enough?
- Are provider keys impossible to mix by accident?
- Can partial-sync and search-coverage states be represented honestly?
- Does the type survive provider extended properties, collection roles, event kinds, and non-byte attachments?
- Do tests prove both the clean JMAP case and a messy legacy case?
