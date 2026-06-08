# JMAP offline fixtures

Real JMAP JSON **captured from the deterministic Stalwart v0.16 harness**
(`docker/stalwart/`, account `alice@test.local`) and committed so the
parse/normalize and orchestration tests run offline with no Docker. They are the
ground truth the normalizers and the generic fetch orchestration are tested
against; the live integration tests re-verify the same invariants against the
running server.

**Secrets:** none. Authentication is an HTTP request header, never part of these
response *bodies*; the only addresses present are the harness's throwaway
`@test.local` accounts. The server holds no real data (see
`docs/agent-guidance/stalwart-harness.md`). Determinism rule: tests assert on
harness-controlled content (subjects, `Message-ID`s, iCalendar UIDs, membership,
roles), never on the server-assigned opaque ids these files happen to contain.

Two shapes are stored: a **method result** (the `args` object of one
`methodResponse`, with a `list`) for normalization tests, and a full
**response document** (`{ methodResponses, sessionState }`) for orchestration
tests driven through a fake executor.

| Fixture | Captured from | Protects |
| ------- | ------------- | -------- |
| `mailbox_get.json` | `Mailbox/get` result | Mailbox normalization: roles (`inbox`/`sent`/`trash`) and the roleless custom `Archive`/`Projects`. |
| `email_get.json` | `Email/get` result (all 9 seed emails) | Email normalization: the COPY as one multi-membership object, the duplicate-`Message-ID` pair as two distinct objects, missing `Message-ID`, keywords, the move. |
| `mailbox_snapshot_response.json` | `[Mailbox/get]` response | Container snapshot orchestration. |
| `email_snapshot_response.json` | `[Email/query, Email/get(#ids)]` response | Member snapshot via result back-reference. |
| `email_changes_response.json` | `[Email/changes, Email/get(#created), Email/get(#updated)]` response | Empty-delta orchestration (the changes→get back-reference path). |
| `submit_context_response.json` | `[Mailbox/get, Identity/get]` response | Resolving the Drafts/Sent mailboxes + submission identity before a send. |
| `submit_send_response.json` | `[Email/set, EmailSubmission/set]` response | Submission: the created email key, plus the implicit `Email/set` from `onSuccessUpdateEmail` (two responses share call id `1`). |
| `calendar_get.json` | `Calendar/get` result | Calendar-container normalization. |
| `calendarevent_get.json` | `CalendarEvent/get` result (all 6 seed events) | JSCalendar normalization: zoned/floating/all-day time model, recurrence rule + overrides, participants, virtual location. |
| `calendar_snapshot_response.json` | `[Calendar/get]` response | Calendar container snapshot orchestration. |
| `event_snapshot_response.json` | `[CalendarEvent/query, CalendarEvent/get(#ids)]` response | Event member snapshot orchestration. |

## Re-capturing

Bring up the harness (`cd docker/stalwart && docker compose up -d --wait`) and
re-issue the matching request against `http://127.0.0.1:18080/jmap/` with basic
auth. Calendar fixtures use fixed 2026 dates so occurrence expansion stays
stable; never re-capture against a server that has been mutated (e.g. after the
live submission test files mail in `Sent`).
