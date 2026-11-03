# MIME Body Extraction Guidance

Read before touching `engine-mime`, the `MessageBody` type (`engine-core`), or any
code that turns a raw RFC 5322 message into displayable text.

## What it is

`engine-mime` is a pure, I/O-free, async-free crate; its core functions:

```rust
pub fn extract_body(raw: &RawMime) -> MessageBody
pub fn extract_inline_parts(raw: &RawMime) -> Vec<InlinePart>
pub fn extract_attachments(raw: &RawMime) -> Vec<MessageAttachment>
pub fn extract_attachment(raw: &RawMime, id: AttachmentPartId) -> Option<MessageAttachmentContent>
pub fn has_downloadable_attachment(raw: &RawMime) -> bool
pub fn encoded_word::decode(input: &str) -> String
pub fn html_to_text(html: &str) -> String
```

`html_to_text` is the text rendering an HTML-only body gets, for HTML that arrives outside a
message (`provider-google` renders a conference's HTML notes with it).

`extract_body` interprets a message's cached raw source (`RawMime`, the Tier-3 blob the
store caches on demand — `store-and-sync.md`) into `MessageBody { plain, html }`:

- `plain` — the canonical text rendering: the decoded `text/plain` parts of the text
  body (RFC 8621 `textBody`) in reading order, an HTML part converted where it has no
  text version, so a plain-text reading view always has something to show. A body is a
  list of parts — an iPhone puts its text on both sides of a photo, a mailing list
  appends its footer as a part — so every text part is kept, a blank line between two;
  a single part is kept exactly as written. The rendering leaves out what `<head>`, `<style>`, `<script>` and
  `<template>` hold, whatever attributes the tag carries. That is mail-parser's own
  converter since 1.0; 0.11 recognised only the bare tag and an `html_text` module here
  stripped the attributes first. A test through `extract_body` pins it, so a release that
  loses it fails rather than putting stylesheets into the snippet and the search index.
- `html` — the decoded **unsanitized** `text/html`, captured **only** when the
  message carries a real `text/html` part (the parser maps a text-only message's
  text part into its HTML body list too, so the list being non-empty does not prove
  a real HTML part — check the part type). It is then the whole HTML body list in
  reading order, a text part standing in for a missing HTML version converted to HTML
  in its place (so a `multipart/mixed` of an alternative and a plain footer shows both,
  and two alternatives around a photo show both halves). Its `<meta>` charset declarations are
  rewritten to `utf-8` (`MessagePart::html_utf8`), because the text is UTF-8 once
  decoded and a stale `charset=iso-8859-1` would have a browser decode it twice. The HTML
  version of a `multipart/alternative` is the HTML body even when it carries a `name`, and
  neither it nor a named `text/plain` version is ever listed as a download:
  RFC 8621, which mail-parser 1.0 follows exactly, makes such a part an attachment and
  copies the `text/plain` in as the HTML stand-in, but an alternative's children are
  versions of one body by definition (RFC 2046 §5.1.4), and IMAP's list-time
  `has_attachment` already never counts a `text/html` on a bare name. A host **must sanitize** before rendering
  (`north-star.md` security: HTML mail is sanitized, remote images blocked by
  default). Rendering HTML is a later slice.

`extract_inline_parts` decodes a message's inline (`cid:`-referenced) parts into
`Vec<InlinePart>` — one per **binary** leaf part of the message itself (not of a message it
forwards) that declares a `Content-ID`, the only
parts a `cid:` URL can address (RFC 2392). Each `InlinePart` carries the id with angle
brackets stripped, the `Content-Type` media type (parameters stripped), and the
content-transfer-decoded bytes. A host inlines these for `<img src="cid:…">` references
in the (sanitized) HTML body. **Policy stays with the host**: which media types are safe
to inline, and the inert form they are inlined as (e.g. an `image/*`-only `data:` URI),
are decided by the renderer, not here — the bytes are hostile input. Text and
`multipart/*` parts, and parts without a `Content-ID`, are skipped.

`encoded_word::decode` is the same decoding one level up, on **header** text: it resolves
RFC 2047 encoded-words (`=?iso-2022-jp?B?…?=`) in a subject or display name. It lives here
rather than in an adapter because two adapters read RFC 5322 headers themselves and must
agree with each other, and with a body, about what a charset label means:

| Adapter | Header text | Decodes here? |
|---|---|---|
| `provider-imap` | IMAP `ENVELOPE`, verbatim header text | yes |
| `provider-google` | Gmail `payload.headers[].value`, raw | yes |
| `provider-jmap` | JMAP `subject` / `from.name`, server-decoded | no, nothing to do |
| `provider-graph` | Graph `subject` / `from…name`, server-decoded | no, nothing to do |

The scanning and the `B`/`Q` payloads are `encodify`'s RFC 2047 decoder, the one
mail-parser runs on a parsed message's own headers (1.0 moved the codecs there); it joins
adjacent words of one charset before converting, so a character split across two words
decodes, and it trims the ends and turns folding whitespace into one space. The charset
table is `mail-parser`'s, shared with body decoding. One label is overridden:
`ISO-8859-1` is read as its `Windows-1252` superset, because that is what browsers do and
what real mail means (without it an Outlook en-dash, `0x96`, is an unrenderable C1
control). That override is stated in code rather than left to the table, which has already
changed its answer once across a `mail-parser` release.

The fetching and caching of the raw bytes are **not** this crate's job — the
provider layer fetches (`Provider::fetch_message_source`) and the store caches
(`MessageSourceCache`); `engine-mime` only *interprets*. Note inline bytes are kept **out**
of the SQLite body cache (`MessageBodyStore`): `engine-sync::fetch_inline_parts` re-derives
them from the immutable raw blob on demand, so a large inline image never bloats the
relational store (`MessageSourceCache` doc).

`extract_attachments` lists ordinary downloadable attachments from the same raw source,
returning metadata only (`MessageAttachment`). The candidates are the message's own leaf
parts (a forwarded `message/rfc822` is one download; its parts are not listed beside it)
that are an RFC 8621 attachment **or** inline media shown inside the body (`PartRole::MediaBody`).
The second half is ours, not the RFC's: RFC 8621 drops an image shown in both body lists from
`attachments`, and mail-parser 1.0 follows it, but no host renders body media inline, so the
photo an iPhone places between two paragraphs (inline, named, no `Content-ID`) would be
reachable nowhere. Its CID rule is the exact complement of
`extract_inline_parts`: a **binary** part carrying a `Content-ID` is a body resource
resolved through `extract_inline_parts` and is omitted here (so an embedded `cid:` image is
not also listed as a file), **unless** the sender explicitly marked it
`Content-Disposition: attachment`, which keeps it downloadable even with a `Content-ID`.
The rule keys on both `Binary` and `InlineBinary` because the kind follows the disposition
(mail-parser 1.0, issue #70): an undispositioned `cid:` image in a `multipart/mixed` is
`Binary`, so keying on `InlineBinary`/inline disposition alone would leak embedded images
into the list. Parts without a `Content-ID` (for example an inline-displayed PDF) remain
downloadable. The message-scoped `AttachmentPartId` is the part's MIME part number (document
order from the root's `0`, nested messages' parts included), which depends on the structure
alone: a classification rule that changes between releases can make an id stop resolving,
never make it name a different file. (It was the index into the parser's attachment list
before mail-parser 1.0.) Listing and fetching read one sequence of parts, so they agree.
`has_downloadable_attachment` answers whether that list is non-empty without building it; it
is what a row's paperclip is set to once the source is read (`store-and-sync.md`, "The source
answers the row's paperclip").
`extract_attachment` takes that id and returns the selected metadata plus decoded bytes
(`MessageAttachmentContent`) for a host save/open action; suggested file names are sanitized
(path separators, control codes, and Unicode bidi/zero-width format controls neutralized).

## Key decision: depend on `mail-parser`, don't hand-roll

Unlike the IMAP/SMTP/CalDAV wire parsers (hand-rolled, to keep protocol invariants
under our control), MIME body decoding is delegated to **`mail-parser`** (Stalwart,
the same authors as our test target):

- Mail bodies are **hostile input** and charset/encoded-word/nested-multipart
  decoding is a spec-heavy correctness/safety minefield; a hardened, fuzzed parser is
  the right tool. The `full_encoding` feature (not default since 0.11.2) is enabled
  for the full legacy charset tables, so non-UTF-8 bodies decode correctly.
- It is pure Rust — no C, no new cross-compile surface for iOS/Android.
- **Read the message's own parts.** From 1.0, `Message::parts()` walks every nested
  message too, with one global numbering; a read that is about *this* message (its inline
  parts, its attachments, its invitation) iterates `message.root().parts()`, or a forwarded
  invitation becomes this user's.

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
HTML-only fallback (non-text elements left out of the text, whatever their attributes),
the text on both sides of a photo, every HTML part of a body in order,
the `<meta>` charset rewrite, a named HTML alternative, `multipart/mixed` past an
attachment, and adversarial/empty input (no panic). `extract_inline_parts` is covered by fixtures for a `multipart/related`
inline image (decoded bytes, stripped `cid`), an attachment without a `Content-ID` (not
returned), plain/HTML-only messages (no inline parts), multiple inline parts in order, and
adversarial/empty input (no panic). Attachment extraction is covered for metadata-only
listing, selected byte extraction, CID-image exclusion, inline non-CID files, a photo shown
inside the body, a forwarded message's parts, filename sanitization, and adversarial input. Add a fixture for any new decoding behavior.
