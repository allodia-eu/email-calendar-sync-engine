# Modeling Guidance

The domain model is the load-bearing part of this project. Do not implement or change model types until the relevant primary specs and provider docs have been checked.

For contacts, `ContactCard` is the normalized JSContact-shaped source record and
`Person` is a derived presentation record; they are not interchangeable.
`AddressBookId`, `ContactId`, and store-local `PersonId` prevent source,
collection, and derived identities from mixing. Address-book membership is
non-empty and separate from card identity. Raw vCard, raw JSContact, and raw
provider JSON remain beside normalized fields so a provider write never rebuilds
an authoritative document from a lossy projection — but "authoritative" means
*for what the model does not carry*: a write re-derives every modelled property
from the normalized value, or a host's edit would be silently discarded. See
`contacts.md`.

The model reports absence rather than inventing a value to fill it.
`Person::display_name` is `Option<String>` because a card with neither a name
nor an address has no name to show, and any placeholder the engine picked would
be untranslatable text minted in a provider-neutral core and rendered verbatim
by every host. Presentation defaults belong to the host.

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
- The displayable body is a *derived view*, `MessageBody { plain, html }` (`engine-core`), not stored state: the lossless source is the cached raw RFC 5322 (`RawMime`, Tier-3), and `engine-mime::extract_body` decodes the best `text/plain`/`text/html` parts out of it (content-transfer- and charset-decoded to UTF-8). `plain` is the reading-view text (a text rendering of an HTML-only message when there is no plain part); `html` is the **unsanitized** HTML, present only when a real `text/html` part exists — a host sanitizes before rendering. Its `Debug` is redacted, like the raw payloads.
- Calendar normalization must support floating times, all-day events, embedded timezones, recurrence overrides, exclusions, and cross-DST expansion.
- A calendar collection carries access rights, subscription/visibility, owner, default reminders (which events may inherit), and color — not only event membership.
- **A mail collection carries the caller's access rights too** — `Mailbox::access: MailboxAccess`, the nine RFC 8621 `MailboxRights` booleans plus `may_share`. This **reverses** the earlier decision to treat per-mailbox rights as provider-specific and leave them in `extended`, and the reversal is evidence-driven: two of the three mail protocols that support sharing standardise rights (JMAP `MailboxRights` RFC 8621 §2; IMAP ACL RFC 4314), the third grants them all-or-nothing per mailbox, and — decisively — there is **no usable signal above the collection**. Live against Stalwart, a JMAP account shared read-only reports `accounts.<id>.isReadOnly: false` while the single mailbox it exposes grants `lr` alone, so a caller asking "may I write here?" gets the wrong answer from the account and the right one only from the mailbox. Message *counts* remain provider-specific and stay in `extended`.
  - The nine rights are carried verbatim rather than collapsed to read/write, because they are genuinely independent and servers hand out arbitrary subsets: a shared mailbox commonly grants per-user `$seen` state (`maySetSeen`) without other keywords (`maySetKeywords`), and IMAP separates appending (`i`) from deleting a message (`t`) from creating a child mailbox (`k`) from deleting the mailbox (`x`).
  - Sourcing is per adapter: JMAP reads `myRights` directly; IMAP maps the `MYRIGHTS` letters (the mapping and its reasoning are tabulated in `provider-imap`'s `acl` module — note that *reading* needs `l`+`r` and *removing* needs `t`+`e`); Graph's Full Access is all-or-nothing per mailbox and Gmail labels carry no rights at all, so both report `owner()` — which is correct there, not optimistic.
  - Absent rights read as `owner()`, not as "no rights": a server with no way to report them (no `ACL` extension) must not have its mail hidden. The field is `#[serde(default)]` for the same reason, so a mailbox stored before rights existed still loads.
- Events carry a kind discriminator (default, plus provider kinds such as working-location, focus-time, out-of-office, birthday); the model records the kind and preserves its payload even when the JSCalendar projection cannot express the behavior.
- Recurring event range search uses bounded materialized occurrences.
- Thread ids carry provenance: provider-assigned or locally-derived.
- Writes are represented as durable pending operations before any provider side effect.
- Pending operations may have dependencies and local-id to provider-id resolution.
- Model types for sync/store contracts live in `engine-core` (provider-neutral data shapes) or `engine-store` (the lease, batch, and fencing vocabulary, beside the `Store` trait), never in `engine-sync`.

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
