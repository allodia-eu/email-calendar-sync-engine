# MIME Body Extraction Guidance

Read before touching `engine-mime`, the `MessageBody` type (`engine-core`), or any
code that turns a raw RFC 5322 message into displayable text.

## What it is

`engine-mime` is a pure, I/O-free, async-free crate with two public functions:

```rust
pub fn extract_body(raw: &RawMime) -> MessageBody
pub fn extract_inline_parts(raw: &RawMime) -> Vec<InlinePart>
pub fn extract_attachments(raw: &RawMime) -> Vec<MessageAttachment>
pub fn extract_attachment(raw: &RawMime, id: AttachmentPartId) -> Option<MessageAttachmentContent>
```

`extract_body` interprets a message's cached raw source (`RawMime`, the Tier-3 blob the
store caches on demand — `store-and-sync.md`) into `MessageBody { plain, html }`:

- `plain` — the canonical text rendering: the decoded `text/plain` body, or a text
  rendering of an HTML-only message, so a plain-text reading view always has
  something to show.
- `html` — the decoded **unsanitized** `text/html`, captured **only** when the
  message carries a real `text/html` part (the parser maps a text-only message's
  text part into its HTML body list too, so the list being non-empty does not prove
  a real HTML part — check the part type). A host **must sanitize** before rendering
  (`north-star.md` security: HTML mail is sanitized, remote images blocked by
  default). Rendering HTML is a later slice.

`extract_inline_parts` decodes a message's inline (`cid:`-referenced) parts into
`Vec<InlinePart>` — one per **binary** leaf part that declares a `Content-ID`, the only
parts a `cid:` URL can address (RFC 2392). Each `InlinePart` carries the id with angle
brackets stripped, the `Content-Type` media type (parameters stripped), and the
content-transfer-decoded bytes. A host inlines these for `<img src="cid:…">` references
in the (sanitized) HTML body. **Policy stays with the host**: which media types are safe
to inline, and the inert form they are inlined as (e.g. an `image/*`-only `data:` URI),
are decided by the renderer, not here — the bytes are hostile input. Text and
`multipart/*` parts, and parts without a `Content-ID`, are skipped.

The fetching and caching of the raw bytes are **not** this crate's job — the
provider layer fetches (`Provider::fetch_message_source`) and the store caches
(`MessageSourceCache`); `engine-mime` only *interprets*. Note inline bytes are kept **out**
of the SQLite body cache (`MessageBodyStore`): `engine-sync::fetch_inline_parts` re-derives
them from the immutable raw blob on demand, so a large inline image never bloats the
relational store (`MessageSourceCache` doc).

`extract_attachments` lists ordinary downloadable attachments from the same raw source,
returning metadata only (`MessageAttachment`). It skips inline CID image parts because
those are body resources handled by `extract_inline_parts`; inline parts without a
`Content-ID` (for example an inline-displayed PDF) remain downloadable. The
message-scoped `AttachmentPartId` is the parser attachment index for that immutable raw
source. `extract_attachment` takes that id and returns the selected metadata plus decoded
bytes (`MessageAttachmentContent`) for a host save/open action.

## Key decision: depend on `mail-parser`, don't hand-roll

Unlike the IMAP/SMTP/CalDAV wire parsers (hand-rolled, to keep protocol invariants
under our control), MIME body decoding is delegated to **`mail-parser`** (Stalwart,
the same authors as our test target):

- Mail bodies are **hostile input** and charset/encoded-word/nested-multipart
  decoding is a spec-heavy correctness/safety minefield; a hardened, fuzzed parser is
  the right tool. The `full_encoding` feature (not default since 0.11.2) is enabled
  for the full legacy charset tables, so non-UTF-8 bodies decode correctly.
- It is pure Rust — no C, no new cross-compile surface for iOS/Android.

## Invariants

- **Hostile input never panics.** Malformed, truncated, or non-UTF-8 bytes yield an
  empty `MessageBody`, never a panic — matched by adversarial unit tests (the repo's
  "hostile input rejected, never panicked on" posture, like the IMAP parser).
- **Body text is sensitive.** `MessageBody`'s `Debug` is redacted (lengths only,
  never content), like the raw payloads — logs are redacted by default.
- **Pure and derived.** `MessageBody` is a derived view, not stored state. The raw is
  the single source of truth; re-extraction is cheap, so nothing caches the decoded
  text (yet — body→FTS indexing is a separate follow-up).

## Tests

`extract_body` is covered by fixtures for plain text, `multipart/alternative`
(text+html), quoted-printable, base64, a non-UTF-8 charset (proving `full_encoding`),
HTML-only fallback, `multipart/mixed` past an attachment, and adversarial/empty input
(no panic). `extract_inline_parts` is covered by fixtures for a `multipart/related`
inline image (decoded bytes, stripped `cid`), an attachment without a `Content-ID` (not
returned), plain/HTML-only messages (no inline parts), multiple inline parts in order, and
adversarial/empty input (no panic). Attachment extraction is covered for metadata-only
listing, selected byte extraction, CID-image exclusion, inline non-CID files, filename
sanitization, and adversarial input. Add a fixture for any new decoding behavior.
