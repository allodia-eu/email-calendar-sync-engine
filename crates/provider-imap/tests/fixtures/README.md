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

## `select_inbox_*.txt`

The untagged responses each harness server answered `SELECT INBOX` with, as `alice@test.local`:
`select_inbox_stalwart.txt` from the **Stalwart harness** (v1.0.2), `select_inbox_dovecot_rev1.txt`
and `select_inbox_dovecot_rev2.txt` from the **Dovecot harness** speaking IMAP4rev1 and IMAP4rev2.
Captured on 2026-11-23 with `curl -X 'SELECT INBOX'`; the tagged completion is not part of them.
All three list every system flag in `PERMANENTFLAGS` and end it with `\*`. `src/permanent_tests.rs`
reads them as a `SELECT` answer and asserts which keywords each server keeps. Stored LF; nothing
in them is content.

## `managesieve/`

What the two harness servers' ManageSieve services (RFC 5804) answered, byte for byte, CRLF
included (`.gitattributes` marks them `-text`, since a literal's length counts its line ends):
`stalwart_*.txt` from the **Stalwart harness** (v1.0.2, `127.0.0.1:14190`, as
`carol@test.local`) and `dovecot_*.txt` from the **Dovecot harness** (2.4.4 with Pigeonhole, the
rev1 service, `127.0.0.1:14191`, as `alice@test.local`). Captured on 2026-11-23 by a script that
sent each command over one session and wrote each response to its file:

| File | Command |
|---|---|
| `*_greeting.txt` | none: the greeting on connect, before TLS |
| `*_starttls.txt` | `STARTTLS` |
| `*_greeting_tls.txt` | none: the capabilities the server sends once TLS is up (RFC 5804 §2.2) |
| `*_authenticate.txt` | `AUTHENTICATE "PLAIN"` with the password, as an initial response |
| `*_authenticate_refused.txt` | the same with a wrong password, on a connection of its own |
| `*_capability_authenticated.txt` | `CAPABILITY` once signed in |
| `*_listscripts.txt` | `LISTSCRIPTS` with one script, `zfixture`, active |
| `*_getscript.txt` | `GETSCRIPT "zfixture"` (a one-rule script in Roundcube's dialect) |
| `*_getscript_nonexistent.txt` | `GETSCRIPT` of a script that does not exist |
| `*_checkscript_unknown_extension.txt` | `CHECKSCRIPT` of a script requiring `imapsieve` |
| `*_checkscript_editheader.txt` | `CHECKSCRIPT` of a script requiring `editheader` |
| `*_havespace_large.txt` | `HAVESPACE` for 999,999,999 bytes |
| `*_deletescript_active.txt` | `DELETESCRIPT` of the active script |
| `*_listscripts_empty.txt` | `LISTSCRIPTS` once the script was deactivated and deleted |
| `*_logout.txt` | `LOGOUT` |

What they show: mechanisms are offered only after `STARTTLS` (`SASL ""` before it); Dovecot lists
`MAXREDIRECTS` only once signed in; Dovecot sends a refusal's prose as a literal, naming its
temporary file; the response codes seen are `NONEXISTENT`, `ACTIVE` and `QUOTA/MAXSIZE`. Nothing
in them is content: the script is the capture's own, and its words are reserved names.
