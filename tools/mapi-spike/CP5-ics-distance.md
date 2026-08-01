# CP5 — the ICS distance

**Paper exercise. No code was written for this checkpoint, by design.**

CP4 is the **floor**: the cheapest possible path to a decoded row. ICS is the **ceiling**. A GO based
on CP4's numbers alone would be falsely optimistic, which is the exact failure this checkpoint
exists to prevent. This document enumerates what a real `Provider` impl needs *beyond* CP4, prices
each area, and states a ratio.

The prior to be confirmed or refuted was: **CP4 is ~25–35% of a read-only provider and under 20% of
one with writes.** The result below **refutes it** — CP4 is roughly half that.

---

## What CP4 actually bought

Measured on the code as it stands, excluding tests and excluding `transcript.rs` (capture
instrumentation, not protocol code):

| Measure | Lines |
|---|---|
| Non-blank, non-comment | **1,298** |
| Including comments and blanks | 1,753 |
| Including tests and the recorder | 2,654 |

That buys exactly this: a Session Context, a logon, and a **table read** — `RopOpenFolder`,
`RopGetHierarchyTable`/`RopGetContentsTable`, `RopSetColumns`, `RopQueryRows`, over six OXCDATA
property types, all in one `Execute`.

It is worth being precise about what that is *not*. A contents table is a **view**, not a sync
primitive. It has no notion of "what changed since", its string values are **truncated at 255
characters** (measured — see README), and it cannot express a deletion. Every one of those is
load-bearing for a sync engine, and none of them is fixable within the table ROPs.

## The map: this repo's `Provider` trait vs what MAPI needs

`engine-provider`'s `Provider` trait is the actual contract. Mapping it to MAPI:

| Trait method | What MAPI needs | Covered by CP4? |
|---|---|---|
| `connection_info` | `Connect` + `RopLogon` | **yes** |
| `mailbox_scope` / `email_scope` | scope newtypes only | n/a (neutral) |
| `sync_mailboxes` | hierarchy table + ICS for *folder* deltas | **partly** — snapshot only |
| `stream_email` | **[MS-OXCFXICS]** in full | **no** |
| `sync_email` | default wrapper over `stream_email` | **no** |
| `fetch_message_source` | `RopOpenMessage`/`RopOpenStream` + **[MS-OXCMAIL]** | **no** |
| `submit_email` | `RopCreateMessage`/`RopModifyRecipients`/`RopSubmitMessage` | **no** |
| `edit_mail` | `RopSetProperties`/`RopSaveChangesMessage` + a guard concept | **no** |
| `sync_calendars` / `sync_events` | appointments-as-messages + **[MS-OXOCAL]** | **no** |
| `create/patch/put/rsvp/delete_event` | the above, plus iTIP | **no** |
| `ContactsProvider::*` | personal contacts, or **[MS-OXNSPI]** for the GAL | **no** |
| `Watch` | `RopRegisterNotification` + the `NotificationWait` request type | **no** |

## The estimate

Lines are non-blank, non-comment, excluding tests — the same basis as the 1,298 above, so the
ratio is like-for-like. Calibration anchor: this repo's existing providers run **7,107–11,404
source lines** (`provider-caldav` 7,107 · `provider-graph` 8,116 · `provider-google` 8,342 ·
`provider-jmap` 11,063 · `provider-imap` 11,404) — and every one of those gets JSON or line-based
text parsing for free.

| # | Area | Lines | Why |
|---|---|---:|---|
| 1 | **ICS / FastTransfer delta sync** | 2,000–3,000 | `RopSynchronizationConfigure` (0x70), `RopSynchronizationUploadStateStreamBegin/Continue/End` (0x75–0x77), `RopSynchronizationGetTransferState` (0x82), `RopFastTransferSourceGetBuffer` (0x4E), `RopSynchronizationImportDeletes` (0x74). Plus the **FastTransfer stream** — a ~40-marker, nested-scope serialization *completely unlike* `PropertyRow` — and **GLOBSET/IDSET**, a bit-packed codec with PUSH/POP/BITMASK/RANGE/END commands ([MS-OXCFXICS] §2.2.2.5). Also forces the **full** OXCDATA type set, including `PtypBinary`'s 4-byte COUNT in a stream vs 2 in a ROP buffer — the trap CP4 deliberately fenced out. |
| 2 | **Bodies + RFC 5322 assembly** | 1,500–2,500 | `RopOpenMessage` (0x03), `RopGetPropertiesSpecific` (0x07), `RopOpenStream`/`RopReadStream` (0x2B/0x2C), then **[MS-OXCMAIL]** to rebuild RFC 5322 from MAPI properties. Address rewriting (EX-type `PidTagSenderEmailAddress` vs SMTP), `PidTagTransportMessageHeaders` when present, and — when `PidTagBody` is absent — **`PidTagRtfCompressed`, which needs its own RTF decompression codec and RTF→HTML de-encapsulation**. That last item is a sub-project, and it is why this line is routinely underestimated. |
| 3 | **Named properties** | 300–500 | `RopGetPropertyIdsFromNames` (0x56), `RopGetNamesFromPropertyIds` (0x55), plus a per-store cache keyed by (GUID, kind, id/name). Unavoidable the moment anything Outlook-specific is touched — categories, flags, every calendar extension. |
| 4 | **Attachments** | 400–700 | `RopGetAttachmentTable` (0x21), `RopOpenAttachment` (0x22), streamed attachment data, and **embedded-message attachments, which recurse**. |
| 5 | **Writes** | 800–1,200 | `RopCreateMessage` (0x06), `RopSetProperties` (0x0A), `RopSaveChangesMessage` (0x0C), `RopDeleteMessages` (0x1E), `RopMoveCopyMessages` (0x33), `RopSubmitMessage` (0x32), `RopModifyRecipients` (0x0E). The conflict guard #99 wants maps to `PidTagChangeKey`/`PidTagPredecessorChangeList` — itself ICS-adjacent, so it is not independent of row 1. |
| 6 | **Calendar** | 2,000–3,000 | Appointments are messages plus named properties. Recurrence is **[MS-OXOCAL]**'s binary `PidLidAppointmentRecur` blob — a wholly different encoding from RRULE, needing lossless conversion **both ways** — plus `PidLidTimeZoneStruct` blobs. After bodies, the most underestimated area here. |
| 7 | **Contacts / GAL** | 1,500–2,500 | Personal contacts are messages and comparatively cheap. The **GAL is not**: `/mapi/nspi/` is a *separate protocol* ([MS-OXNSPI]) with its own binding, row format and restriction language. Only if GAL is in scope. |
| 8 | **Notifications / `Watch`** | 400–600 | `RopRegisterNotification` (0x29), the `NotificationWait` request type, long-poll lifecycle. |
| 9 | **Transport hardening** | 800–1,500 | [MS-OXCRPC] `AUX_*` blocks if any server demands them; **LZ77+DIRECT2 + the 0xA5 XOR** if any server ignores `NoCompression\|NoXorMagic` (Exchange and Gromox both honoured it — but that is two servers, not all); `RopBufferTooSmall` retry loop; `RopBackoff`; session re-establishment on `ContextNotFound`; **NTLM/Negotiate for any server without Basic — which includes every default-configured Exchange**. |
| — | Provider glue: error classification, cursors, scope wiring | ~800 | What every adapter in this repo pays. |

### Totals

| Target | Lines | CP4 as a share |
|---|---:|---:|
| CP4 as built | 1,298 | — |
| **Read-only mail** (1+2+3+4+9+glue) | **~8,500** | **≈15%** |
| **Full parity** with the other providers (+5+6+7+8) | **~14,500** | **≈9%** |

## Verdict on the prior

**Refuted.** The prior was 25–35% read-only and under 20% with writes. Measured: **~15% and ~9%**
— roughly half the optimistic end.

The remainder is **6.5× the spike for read-only mail, and ~10× for parity**. HANDOFF.md set the
tripwire at 4×: *"If the remainder is >4× the spike, a GO must be an explicit decision to fund a
multi-month protocol implementation — not an incremental 'we already have rows'."* That threshold
is exceeded on both targets.

Two things make the ratio worse than the raw line count suggests, and neither is captured above:

1. **Line-for-line, MAPI lines are more expensive.** ~14,500 lines would make `provider-mapi` the
   largest adapter in the repo (~1.3× `provider-imap`) — while being the only one that is a
   *binary* RPC protocol with a stateful handle table, where a one-byte framing error yields a
   plausible-looking wrong answer rather than a parse failure.
2. **The offline-fake problem is sharper here.** Per AGENTS.md the fakes answer canned bytes
   regardless of the request. For JSON providers a wrong request is usually still readable by
   inspection; for a `RopBuffer` it is not. Nearly every line above needs a **live** server to be
   trusted at all, and the two available implementations already disagree (see the README's
   findings table).

## What CP4 does *not* de-risk

Honest list, because the CP4 numbers are green and that is misleading on its own:

- **Nothing about ICS.** No marker was parsed, no GLOBSET was decoded. Row 1 is entirely unproven
  and is the single largest item.
- **Nothing about bodies.** Not one byte of message content was fetched.
- **`FlaggedPropertyRow` was never observed.** Both servers sent `StandardPropertyRow` for every
  row of both tables. The flagged path is implemented and unit-tested against hand-built vectors,
  but is **unconfirmed against any real server**.
- **Truncation is a real correctness hazard.** Exchange silently truncated a 392-character subject
  to 255 with a literal `...` and **no error flag**. A provider that trusted table values would
  index corrupted subjects and never know — so bodies/properties must come from
  `RopOpenMessage`/ICS, not the table. This *adds* to row 2's cost rather than being covered by it.

## Recommendation

Treat CP4 as what it is: proof that **the transport, framing, handle table and row decoding are
tractable and correct**, established against two independent implementations. That was the
question the spike was chartered to answer, and the answer is yes.

It is *not* evidence that a provider is close. On the measured ratio, a `provider-mapi` is a
**multi-month, ~8.5k-line commitment for read-only mail alone**, with ICS and [MS-OXCMAIL] — the
two areas with zero spike coverage — as the dominant risks.

So: **GO on the protocol, NO-GO on an incremental framing.** If MAPI is funded, it should be
scoped and staffed as a protocol implementation in its own right, with ICS prototyped *first*
(it is the long pole and the thing most likely to change the estimate), not as an extension of
this spike.
