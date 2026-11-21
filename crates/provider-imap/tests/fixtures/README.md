# IMAP offline fixtures

## `attachment-shapes/`

One message per MIME shape that decides whether a message carries a download, as a pair:

- `NN-name.eml` — the message, written for the purpose (the harness's `@test.local`
  addresses only);
- `NN-name.bodystructure` — the `BODYSTRUCTURE` the **Stalwart harness** at its pinned version (v1.0.2 when captured)
  (`docker/stalwart/`, account `alice@test.local`) built for it: the `.eml` was `APPEND`ed
  to a scratch mailbox and read back with `UID FETCH 1:* (UID BODYSTRUCTURE)`, keeping the
  value after `BODYSTRUCTURE` and dropping the `FETCH` list's closing parenthesis.

`src/bodystructure_agreement_tests.rs` asserts that the list-time flag read from each
structure and `engine_mime::extract_attachments` on the bytes give the same answer, and the
one its table expects. A new shape needs both files, captured the same way, and a row in
that table; the test fails on a file without one.

**Secrets:** none. The messages are synthetic and the server holds no real data.

## `sent_at_fetch.txt`

The five rows the **Stalwart harness** (v1.0.2) answered the metadata `UID FETCH` with
(`FETCH_ITEMS`, one UID per command) for messages whose `Date` header was
`Thu, 29 Oct 2026 10:00:00 +0100`, `Thu, 29 Oct 2026 10:00:00 EST`, `29 Oct 2026 10:00:00 GMT`,
`Thu, 29 Oct 26 10:00:00 +0000 (UTC)` and `not a date`, `APPEND`ed to a scratch mailbox that was
deleted afterwards. Stalwart re-renders the four it could parse in the canonical form and passes
the fifth through verbatim. Stored LF; `src/mail_dates_tests.rs` restores the CRLF the literals'
lengths count, and strips the untagged `* ` as the transport does.

## `importance_fetch.txt`

The five rows the **Stalwart harness** (v1.0.2) answered the metadata `UID FETCH` with
(`FETCH_ITEMS`, one UID per command) for messages carrying `Importance: High` with `X-Priority`
beside it, `X-Priority: 5 (Lowest)` alone, neither, `Importance` twice (low, then high, both
returned in order), and `X-MSMail-Priority: Low` alone, `APPEND`ed to a scratch mailbox that was
deleted afterwards. The capture's client dropped each row's closing parenthesis, which is put
back. Stored LF like every text file here: `src/mail_importance_tests.rs` restores the CRLF the
literals' lengths count, and strips the untagged `* ` as the transport does.
