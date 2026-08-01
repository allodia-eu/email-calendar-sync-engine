# mapi-spike

A **throwaway** MAPI-over-HTTP client whose only job is to answer one question:

> What does the ROP / OXCDATA layer actually cost?

It is not a provider, it implements no `Provider` trait, and it touches no engine crate. It lives
in `tools/` on purpose — a detached `[workspace]` keeps half-finished binary parsing out of the
engine's fmt/clippy/coverage gates while it is being measured. The file-length gate still applies
(it walks `git ls-files`), which is deliberate: it forces the module split that makes graduation
to `crates/provider-mapi` mechanical.

## Why this exists

[MS-OXCMAPIHTTP] is only the envelope. Its mailbox endpoint has four request types — `Connect`,
`Execute`, `Disconnect`, `NotificationWait` (plus `PING`) — and `Execute` carries an **opaque
`RopBuffer`**. Everything that makes mail work lives in specs the PDF only references. For scale,
`provider-graph` is 9,292 lines for a JSON REST API; Microsoft's own client implementation of this
stack is ~1.1 MB of C#. This spike buys a real number before that bet.

**Exchange Online is out of scope and always will be.** Microsoft exposes no OAuth permission
model for Extended MAPI, and Basic auth is permanently disabled in all tenants. The target is
self-hosted Exchange-compatible groupware (Gromox/grommunio) and on-prem Exchange Server.

## Reference sources

- **[MS-OXCMAPIHTTP]** v20250520 — the transport (the PDF in the repo owner's Downloads).
- **[MS-OXCROPS]**, **[MS-OXCDATA]**, **[MS-OXCSTOR]**, **[MS-OXCRPC]** on Microsoft Learn.
- **`OfficeDev/Interop-TestSuites`** — **MIT**, Microsoft-authored, *client*-side C#.
  `ExchangeMAPI/Source/Common/Common/ExchangeMapiClient/`. MIT → MPL-2.0 is clean with attribution.
- **Gromox is AGPL-3.0 and is NEVER a source of code.** Read it to understand behaviour; do not
  copy types, tables, or structure. (*Running* it as a black-box fixture is fine and creates no
  derived work — this repo already runs Stalwart, which is also AGPL.)

## Protocol facts established before writing code

Recorded here so they are not re-derived. Each is verified against the spec or the MIT reference.

**Transport.** All requests are `POST`, `Content-Type: application/mapi-http`. Mandatory headers:
`Host`, `X-RequestType`, `X-RequestId` (`{GUID}:counter`, GUID stable for the session lifetime),
`Content-Length`; plus `X-ClientApplication: Outlook/15.00.0000.0000`. Session state is a
`Set-Cookie`/`Cookie` pair.

**The server returns HTTP 200 even for most failures.** The real status is the `X-ResponseCode`
header — 0 = success, 1–18 enumerated (6 = Invalid Context Cookie, 10 = Context Not Found,
15 = Invalid Sequence). Non-zero means the body is `text/html` diagnostics. Per §2.2.6 this header
is present on **both** chunked and non-chunked responses, so `resp.headers()` is the primary source
in both cases.

**Meta-tags sit inside the HTTP body**, after reqwest's transparent de-chunking: CRLF-delimited
ASCII lines `PROCESSING` / `PENDING` / `DONE`, then `Key:Value` additional headers, then a blank
line, then the binary body.

**The auxiliary buffer can be empty.** [MS-OXCRPC] §3.1.4.1 fails only when `cbAuxIn` is
*between 1 and 7* — zero is explicitly outside the failure band. The MIT reference sends
`AuxiliaryBufferSize = 0` on `Connect`, `Execute` and `Disconnect`. So the whole [MS-OXCRPC]
aux-block layer is deferred.

**There is no version negotiation.** `EcDoConnectEx` has `rgwClientVersion` as an RPC parameter,
but the MAPI/HTTP `Connect` body has no version fields at all — the client version is carried
solely by the `X-ClientApplication` header.

**Connect field values** (from the MIT reference, known to work): `Flags = 0x00000000`
(no admin privilege), `DefaultCodePage = 1252`, `LcidString = LcidSort = 0x00000409`.
`Execute`: `Flags` sets NoCompression|NoXorMagic, `MaxRopOut = 0x10000`.

**`RopLogon` hands you 13 FolderIds free.** The private-mailbox success response ([MS-OXCSTOR]
§2.2.1.1.3) carries 13 × 8-byte FIDs in a fixed order: `0` Mailbox Root, `1` Deferred Action,
`2` Spooler Queue, **`3` IPM subtree**, **`4` Inbox**, `5` Outbox, `6` Sent Items, `7` Deleted
Items, `8` Common Views, `9` Schedule, `10` Search, `11` Views, `12` Shortcuts. This deletes both
`RopGetReceiveFolder` and all EntryID parsing from the spike.

**`RopBuffer` framing** ([MS-OXCROPS] §2.2.1):

```
RPC_HEADER_EXT           8 bytes: Version u16=0, Flags u16, Size u16, SizeActual u16
RopSize                  u16 — counts ITSELF + RopsList, NOT the handle table
RopsList                 RopSize - 2 bytes
ServerObjectHandleTable  the remainder; count = (payload_len - RopSize) / 4
```

**The handle table** is a `u32` array. Every ROP's `InputHandleIndex`/`OutputHandleIndex` is a
**1-byte index into it** — handles never appear inside a ROP body. Size the table to
`max(index) + 1`, pre-fill unowned slots with `0xFFFFFFFF`, and persist the server's returned
table between `Execute` calls. **In-buffer chaining works**: the server processes ROPs in order
and updates the table in place, which is why the folder walk is one round trip and not three.

**`PropertyTag`** ([MS-OXCDATA] §2.9) is PropertyType (u16) then PropertyId (u16) — which is
exactly the conventional `0xIIIITTTT` constant written as a **little-endian u32**.
`PidTagSubject = 0x0037001F` serializes as `1F 00 37 00`.

**`PropertyRow` is not self-describing.** `RopQueryRows` rows are decoded entirely against the
column set the client last sent in `RopSetColumns`. The ROP layer is therefore *stateful*, and the
state that matters is `table handle → ordered column list`. A row's leading byte is `0x00` for a
StandardPropertyRow or `0x01` for a FlaggedPropertyRow, whose per-value flags are `0x0` (value
follows), `0x1` (**absent — consume nothing**), `0xA` (u32 error code). A client MUST handle both
forms; `0xA` is routine because table string values are length-limited.

## ROPs in scope — six sent, eight decoded

| ROP | RopId | | ROP | RopId |
|---|---|---|---|---|
| `RopRelease` | 0x01 | | `RopSetColumns` | 0x12 |
| `RopOpenFolder` | 0x02 | | `RopQueryRows` | 0x15 |
| `RopGetHierarchyTable` | 0x04 | | `RopLogon` | 0xFE |
| `RopGetContentsTable` | 0x05 | | *decode only:* `RopBackoff` 0xF9, `RopBufferTooSmall` 0xFF |

`RopRelease` returns no response on success, so responses are **decoded by reading each `RopId`
off the stream, never positionally**. A failing ROP's response is truncated after its `ReturnValue`
(u32) — read that first and stop on non-zero.

## Deliberately out of scope — the anti-drift fence

Restrictions ([MS-OXCDATA] §2.12) · EntryIDs (§2.2) · `RopSortTable`/bookmarks/`RopSeekRow`/
`RopFindRow` · named properties · multi-valued types · `PtypUnspecified` and `TypedPropertyValue` ·
`PtypBinary` (its COUNT is 2 bytes in a ROP buffer but 4 in a FastTransfer stream — a trap the
spike sidesteps by keeping it out of the column set) · **GLOBSET/IDSET and all of ICS/FastTransfer**
· message bodies and [MS-OXCMAIL] · attachments · notifications · **the entire address-book (NSPI)
endpoint** · TLS · NTLM/Negotiate · writes.

**Tripwire:** if anyone starts writing a marker table (`StartTopFld`, `IncrSyncChg`, `EndFolder`),
the spike has drifted into ICS. Stop and go to CP5, which is a *paper* exercise.

## Checkpoints

| | Goal | Time-box | The measurement |
|---|---|---|---|
| **CP0** | Gromox answers `PING` on loopback | 1d (hard stop 2) | **no Rust** — one `curl`. STOP if Basic is refused and SPNEGO is required |
| **CP1** | Autodiscover returns MailStore URL + LegacyDN | 0.5d | whether discovery is needed or the URL can be hardcoded |
| **CP2** | `Connect` succeeds | 1–1.5d | **did `AuxiliaryBufferSize = 0` work?** |
| **CP3** | `RopLogon` returns 13 FolderIds | 1–1.5d | framing right first try? **did NoCompression\|NoXorMagic hold?** |
| **CP4** | real message rows | 1.5–2d | **the go/no-go table below** |
| **CP5** | ICS distance | 0.5d | **paper only, mandatory** — CP4 is the floor, this is the ceiling |

### CP4 go/no-go

| Metric | GO | Amber | STOP |
|---|---|---|---|
| Lines of Rust to first row | ≤1,500 | 1,500–2,500 | >2,500 |
| HTTP round trips to first row | ≤4 | 5–8 | >8 |
| `Execute` calls to first row | 2 | 3 | >3 |
| Distinct ROPs implemented | 6 | 7–9 | >9 |
| OXCDATA property types | ≤7 | 8–10 | >10 |
| **In-buffer handle chaining worked?** | yes | — | **no** |
| Compression/XOR needed? | no | yes (+2d) | — |

Chaining is a hard gate: if Gromox will not chain within one `Execute`, every folder walk becomes
N round trips and the latency story for a sync engine collapses.

**Overall: time-box the whole spike at 8 working days.** At day 8 with no decoded row, that is
itself the answer.

## The artifact that survives a NO-GO

`transcripts/` — raw request+response byte pairs from a real Gromox. Per AGENTS.md the offline
fakes serve canned bytes regardless of request and so cannot catch a wrong request shape; these
transcripts are the only thing here that cannot be re-derived from a spec, and they are what any
future attempt starts from.

## Running

```sh
# CP0 — no Rust
curl -i -u alice@spike.test:alicepass -X POST \
  -H 'Content-Type: application/mapi-http' -H 'X-RequestType: PING' \
  -H 'X-RequestId: {00000000-0000-0000-0000-000000000001}:1' \
  -H 'X-ClientApplication: Outlook/15.00.0000.0000' \
  --data-binary '' http://127.0.0.1:18082/mapi/emsmdb/

# CP1-CP4
cargo run -- autodiscover --url http://127.0.0.1:18082 --user … --pass …
cargo run -- connect  --url http://127.0.0.1:18082/mapi/emsmdb/ …
cargo run -- logon    …
cargo run -- folders  …
cargo run -- rows     --folder inbox
cargo test            # golden vectors, round-trips, hostile input
```
