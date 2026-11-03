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
