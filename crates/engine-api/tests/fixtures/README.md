# engine-api offline fixtures

Real bytes **captured from the deterministic Stalwart v0.16 harness**
(`docker/stalwart/`) and committed so the facade reads can be tested offline with
no Docker. The live scheduling suite
(`crates/provider-caldav/tests/scheduling/mod.rs`) re-verifies the same
invariants against the running server; these files are how the next agent
inherits the *observed* shape rather than a guess about it.

**Secrets:** none. The only addresses are the harness's throwaway `@test.local`
accounts, and the server holds no real data (see
`docs/agent-guidance/stalwart-harness.md`). Determinism rule: tests assert on
harness-controlled content (the iCalendar `UID`, the `TZID`, the participant
addresses, the MIME nesting), never on the server-assigned `Message-ID`,
`Date`, or MIME boundary strings these files happen to contain.

| Fixture | Captured from | Protects |
| ------- | ------------- | -------- |
| `stalwart-invitation.eml` | The iMIP invitation Stalwart **generates and mails** to an attendee when an organizer `PUT`s an event naming them (RFC 6047). | `Engine::message_scheduling` against a real server-authored invitation: the part is found three levels down a `multipart/mixed` → `related` → sibling tree, quoted-printable-decoded, and parsed — and it arrives `Content-Disposition: attachment`, so the RSVP gate must key on `METHOD`, not on "was it a body part". |

## What was trimmed, and what was not

`stalwart-invitation.eml` is the captured message with two **bodies** shortened,
from 20 512 to ~3 000 bytes: the `text/html` part (a ~17 KB MJML card) and the
inline `image/png` logo. Every header, the whole MIME skeleton, the boundary
nesting, the `Content-Transfer-Encoding`s, and the **entire `text/calendar`
part** are byte-for-byte as the server emitted them — including the
quoted-printable escaping (`=0D=0A`, `=3D`), the DQUOTE-quoted Windows `TZID`,
and the `ATTENDEE` line folded mid-`mailto:` (`mailt` + CRLF + ` o:carol@…`).
Those are the details the read has to survive, so none of them are synthetic.

## Re-capturing

Bring up the harness (`scripts/ci/stalwart-live.sh up`), have the organizer
scratch account `PUT` an event naming the attendee scratch account (the
`invitation()` helper in the live scheduling suite writes exactly this
document), then read the attendee's INBOX over IMAP. The trimming above is
mechanical — shorten the two body payloads and leave everything else alone.
