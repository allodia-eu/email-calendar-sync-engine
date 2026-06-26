# MIME Body Extraction Guidance

Read before touching `engine-mime`, the `MessageBody` type (`engine-core`), or any
code that turns a raw RFC 5322 message into displayable text.

## What it is

`engine-mime` is a pure, I/O-free, async-free crate with one public function:

```rust
pub fn extract_body(raw: &RawMime) -> MessageBody
```

It interprets a message's cached raw source (`RawMime`, the Tier-3 blob the store
caches on demand — `store-and-sync.md`) into `MessageBody { plain, html }`:

- `plain` — the canonical text rendering: the decoded `text/plain` body, or a text
  rendering of an HTML-only message, so a plain-text reading view always has
  something to show.
- `html` — the decoded **unsanitized** `text/html`, captured **only** when the
  message carries a real `text/html` part (the parser maps a text-only message's
  text part into its HTML body list too, so the list being non-empty does not prove
  a real HTML part — check the part type). A host **must sanitize** before rendering
  (`north-star.md` security: HTML mail is sanitized, remote images blocked by
  default). Rendering HTML is a later slice.

The fetching and caching of the raw bytes are **not** this crate's job — the
provider layer fetches (`Provider::fetch_message_source`) and the store caches
(`MessageSourceCache`); `engine-mime` only *interprets*.

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
(no panic). Add a fixture for any new decoding behavior.
