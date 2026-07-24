# Contacts, Unified People, and Recipient Suggestions

This document is authoritative for contact modeling, provider mappings, people
derivation, contact writes, photos, and recipient suggestions. Read it before
touching `engine_core::contact`, contact scopes or provider code, the people
index, recipient observations, or contact-facing `engine-api` methods.

## Standards and product boundary

The normalized contact spine follows:

- JSContact Card, RFC 9553: <https://www.rfc-editor.org/rfc/rfc9553>
- JMAP Contacts, RFC 9610: <https://www.rfc-editor.org/rfc/rfc9610>
- vCard 4.0, RFC 6350: <https://www.rfc-editor.org/rfc/rfc6350>
- CardDAV, RFC 6352: <https://www.rfc-editor.org/rfc/rfc6352>
- WebDAV Collection Synchronization, RFC 6578:
  <https://www.rfc-editor.org/rfc/rfc6578>

Contacts are the engine's third provider-neutral domain. An `AddressBook` and a
`ContactCard` are account-scoped provider records, not global people. A separate
derived `Person` view joins source records conservatively. Provider records
always remain individually addressable and lossless.

Groups and address books are readable. Creating, editing, deleting, or mutating
group membership and address books is deferred. Contact photos are readable and
cached; photo upload and deletion are deferred.

## Identity and membership

- `AddressBookId` and `ContactId` wrap immutable provider keys. They are unique
  only within `(account, object kind)`.
- `PersonId` is a store-local persistent identity for a derived person. It is
  never sent to a provider.
- A contact card has one or more address-book memberships. Membership is
  separate from the card's provider identity and uses the same non-empty
  `Memberships<T>` invariant as mail and calendar.
- JSContact/vCard `uid` is preserved as content identity. It does not replace
  the provider-assigned `ContactId`.
- Person, organization, and group cards share `ContactCard`; `ContactKind`
  carries the distinction. Google contact groups and JMAP/vCard group cards
  normalize to group cards with member references.

## Normalized card and raw preservation

`ContactCard` follows JSContact property-map semantics. Property ids, contexts,
preference values, labels, and unknown per-property data survive normalization.
The first-class projection covers:

- kind, structured name components, full name, nicknames, and phonetic/sort data;
- email addresses, phones, postal addresses, preferred languages, and time zone;
- organizations, departments, titles, roles, and personal information;
- anniversaries/dates, notes, URLs, media, online services, and crypto keys;
- relations, group members, keywords, calendars, scheduling addresses, and
  directory URIs;
- created/updated timestamps, revision tokens, localizations, and extensions.

The model is a useful portable projection, not a round-trip serializer.
`RawVcard`, `RawJsContact`, and `RawProviderJson` preserve the original provider
document beside it. Writes apply targeted intent to the raw document (or use a
provider's patch verb), then refetch the server's canonical card. Unknown
extensions and fields outside the normalized projection must not disappear.
All raw types use redacted `Debug`.

Malformed legacy vCards are best-effort normalized when the record remains
addressable. The raw bytes/text and parse diagnostics are retained. A malformed
optional property does not discard an otherwise useful card.

## Source classes and availability

Every contact source has a `ContactSourceClass`:

1. writable personal contacts;
2. read-only personal or suggested contacts;
3. organizational/directory contacts;
4. mail-history observations.

Optional sources degrade independently. Missing Graph organization/directory or
Google Other Contacts/directory permission produces a source-level
`Unavailable` result while owned contacts continue. Unavailable is persisted
separately from an empty successful snapshot, so a UI never represents missing
permission as "the directory has no people".

Address-book discovery runs before card sync. Account-global adapters expose a
combined convenience, but stores and reports retain separate source scopes.

## Provider matrix

### Microsoft Graph

- Recursively discover personal `contactFolders`; sync root contacts and each
  folder using their delta endpoints.
- Sync organizational contacts and directory users as independent, read-only
  sources. They are not inserted into a personal folder.
- Request immutable Outlook ids where supported. Preserve `changeKey` and the
  complete raw Graph JSON.
- Personal contacts support create, patch, and delete. Their current Graph
  update contract documents no conditional revision guard, so capability
  metadata reports `WriteGuard::Absent`. Do not infer a guard from `changeKey`.
- Graph's writable-field set is deliberately narrower than its read projection:
  birthday and homepage values are normalized on read, but the neutral
  multi-value anniversary/link maps are not advertised for writes because Graph
  exposes only one scalar of each. Never silently choose one value.
- Fetch personal-contact and directory-user photos on demand. A missing photo is
  a normal absence, not a failed contact sync.

Relevant permissions include `Contacts.Read`/`Contacts.ReadWrite`,
`OrgContact.Read.All`, directory user permissions appropriate to the host, and
`ProfilePhoto.Read.All` for cross-user photos. The capture helper currently asks
for `Contacts.ReadWrite`, `OrgContact.Read.All`, `User.ReadBasic.All`, and
`ProfilePhoto.Read.All`.

### Google People

- Owned connections, Other Contacts, directory people, and contact groups are
  independent sources.
- Page tokens and sync tokens are opaque. The request parameters used with a
  token must match the initial request.
- Owned connections and Other Contacts sync tokens expire after seven days.
  `EXPIRED_SYNC_TOKEN` restarts that source with a full snapshot. Deleted-person
  markers become tombstones.
- Directory people retain their own sync tokens. `contactGroups.list` has only
  pagination, so group passes are generation-stamped full snapshots and never
  send the local snapshot sentinel as a Google `syncToken`.
- Only owned contacts are writable. Updates carry the source `Person.etag`; a
  stale ETag is a conflict. Capability metadata
  reports `WriteGuard::Enforced`.
- Owned-contact writes translate normalized name, nickname, email, phone,
  address, organization/title, birth date, note, URL, relation, and keyword
  intent into People API JSON. Patches begin with the preserved provider JSON,
  so unknown fields survive; duplicate provider update-mask names are collapsed.
- Because People API list synchronization can lag after a write, reconcile the
  written object with a direct `people.get`; do not advance the normal sync
  cursor.

Other Contacts and directory entries remain read-only. Contact group membership
writes remain deferred.

The Google capture helper asks for
`https://www.googleapis.com/auth/contacts`,
`https://www.googleapis.com/auth/contacts.other.readonly`, and
`https://www.googleapis.com/auth/directory.readonly` in addition to its
mail/calendar/identity scopes.

### JMAP Contacts

- Advertise and use `urn:ietf:params:jmap:contacts`.
- Sync `AddressBook/get|changes` before `ContactCard/get|changes`.
- Preserve the server-assigned card `id`, `uid`, non-empty multi-address-book
  membership, rights, JSContact property ids, groups, and extensions.
- Use `ContactCard/set` for CRUD. JMAP state protects the set operation at a
  collection state boundary, but the neutral capability reports
  `WriteGuard::Absent`: it is not a per-card ETag guard.
- After discovery, bind a source adapter to the chosen book with
  `JmapProvider::with_contact_address_book` before exposing its destination to
  host writes. This replaces the constructor's compatibility `default`
  destination with the real opaque server id.
- Media/blob references use authenticated JMAP blob retrieval.

### CardDAV

- `CardDavProvider` shares DAV HTTP/TLS primitives with CalDAV but has separate
  discovery, normalization, and write code.
- Discover address-book homes and collections. Prefer RFC 6578 `sync-collection`;
  use CTag/per-resource ETag comparison when the server lacks it.
- Fetch changed cards with `addressbook-multiget`, requesting `getetag` and
  `address-data`.
- Parse vCard 4 and supported vCard 3 compatibility forms while retaining the
  original vCard.
- Create with `If-None-Match: *`; update and delete with `If-Match` using the
  resource ETag. Capability metadata reports `WriteGuard::Enforced`.
- Embedded and authenticated URI photos are fetched on demand.

The Stalwart fixture writes the shared person/group cards over CardDAV; the
gated `provider-caldav/tests/live_contacts.rs` reads them through both JMAP and
CardDAV and compares normalized identity, kind, names, emails, and members.

## Writable-field capabilities and write lifecycle

Each destination reports account, address book, source class, writability,
guard strength, and an explicit `ContactFieldSet`. A create or patch is rejected
before enqueue if it requests an unsupported field. Fields are never silently
dropped and never stored as local overlays.

`ContactDraft` and `ContactPatch` are intent. Writes:

1. name one account and one writable address book/source card;
2. validate fields and enqueue a durable outbox operation;
3. perform exactly one provider side effect under the outbox lease;
4. refetch the provider's canonical card;
5. apply that card without advancing the source's normal sync cursor;
6. finish the pending operation.

A stale enforced guard is `Conflict`: refetch, let the user rebase intent, and
never blind-retry. Delete is idempotent when the record is already absent.

## Unified people derivation

People are a replaceable derived index. Source contact rows are not rewritten or
coalesced.

### Conservative joins

Two source cards join only when they share an exact canonical email.

No provider currently exposes a stable cross-source person handle, so shared
canonical email is the **only** join signal. An earlier `PersonSource::explicit_links`
field modelled a second one, but nothing ever populated it — the branch was dead — so
it was removed rather than left as a speculative seam. If a provider does grow such a
handle, reintroduce the field *and* the code that fills it in the same change.

Canonicalization trims surrounding whitespace, preserves the local part exactly
(including case), converts an internationalized domain to ASCII, and
case-folds only the domain. It never applies Gmail dot removal, plus stripping,
phone matching, fuzzy names, or provider-specific alias rules. Invalid addresses
do not become join keys. Cards without an email remain separate unless
explicitly linked.

Connected components are transitive: if A shares one exact email with B and B
shares another with C, the three source cards form one person. Every selected
value retains source provenance.

Preferred display names are deterministic:

1. writable personal contact;
2. read-only personal/suggested contact;
3. directory contact;
4. mail-history display name;
5. canonical email.

The same priority, then account/source/contact ids, breaks ties for other
preferred values.

### Generations and stable ids

Applying contact changes increments a contact-source generation. Rebuilding
reads one consistent generation and atomically replaces people only if the
generation is still current. A failed compare-and-swap retries from fresh
sources.

`PersonId` survives ordinary rebuilds:

- an unchanged component keeps its id;
- a merge keeps the oldest id and records aliases from retired ids;
- a split deterministically keeps the prior id on the lexicographically first
  component and mints ids for the rest;
- a new component receives the next store-local id.

Aliases are resolved by `person(PersonId)` so stale UI references remain useful.

## Photo cache

`ContactStore::contact_photo` and `put_contact_photo` keep only metadata in the
database; bytes live in the same content-addressed blob area as raw mail source.
Entries are keyed by account/source-card **plus a digest of the media resource's
URI**, and validated by the media fingerprint when present, otherwise by the card
ETag/changeKey, otherwise by the media URI. The resource component is load-bearing: a
card can carry several media resources (a `PHOTO` and a `LOGO` both land in
`ContactCard::media`) and the ETag fallback is identical for every resource on one
card, so a card-only key let a `LOGO` fetch satisfy a later `PHOTO` read.
A changed fingerprint is a cache miss and triggers a provider fetch.
Binary DAV reads must not pass through UTF-8 decoding.

A photo URI comes from remote *content* and may name any host, so the fetch is
**not** unconditionally authenticated — see "Credentials and remote-content URLs"
in `providers.md`.

## Recipient observations

Recipient history is derived from mail encountered by the configured normal,
windowed mail sync. It is not lifetime history.

Before applying changed messages, resolve current mailboxes with the normalized
`Sent` role. A changed message that belongs to a Sent mailbox contributes its
`To`, `Cc`, and `Bcc` recipients, deduplicated by canonical email per source
message. The apply transaction commits message rows, sync state, and recipient
observations atomically.

Observations are keyed by `(account, source message, canonical email)`, so replay
does not increase counts. Moving or deleting a message does not delete an
observation. Self-addressed recipients are retained. Existing stored messages
are backfilled once through an interaction-index version marker; no mail resync
is forced.

Forgetting history marks matching existing observations suppressed rather than
deleting source identities. A replay therefore cannot resurrect them. A newly
encountered source message for the address is eligible. Per-account and global
clear operations use the same suppression rule. Synced contacts are unaffected.

## Queries and suggestions

People pages use stable keyset cursors bound to query, filters, ordering, and
people-index generation. A changed generation invalidates the cursor. Search
covers normalized name, email, phone, organization, and title. Filters include
account, address book, source class, card kind, group, and writability. Ordering
is deterministic by display name then `PersonId`.

Recipient suggestions emit one result per canonical email with optional
`PersonId`, preferred name, provenance, saved-contact status, sent count, and
last-sent time. Matching and ranking are:

1. exact email;
2. name/email token prefix;
3. email substring;
4. source priority;
5. interaction recency;
6. interaction frequency;
7. canonical email.

An empty query returns recent/frequent observed recipients before contacts
without interactions. Results include per-account coverage: the configured
observed mail window and whether a Sent collection was identifiable. The API
must never call that coverage complete lifetime history.

## Required tests and evidence

Lock invariants in pure tests before implementation: raw preservation,
JSContact/vCard parity, exact-email transitive joins, false-merge prevention,
no-email cards, provenance, stable ids/aliases/splits, and deterministic
preferences.

Store contracts cover contact tombstones, generation CAS, migration from every
prior schema, account purge, observation idempotency/suppression, and atomic
object/cursor/index application. Suggestion tests cover matching, ranking,
cross-account uniqueness, all recipient fields, retained deleted-message
history, self-addresses, empty queries, cursor validation, and coverage.

Every provider needs snapshot/delta/tombstone/cursor-recovery, permission
degradation, write conflict, unsupported-field, group, photo, malformed-payload,
and raw-preservation tests. Offline fakes must assert exact outbound requests.
Any changed provider request bytes also require a scrubbed transcript or the
appropriate token-gated/live server test.
