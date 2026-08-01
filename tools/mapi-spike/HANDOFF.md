# Handoff — continuing the MAPI spike on Windows x64 with Exchange Server SE

This picks up a spike that is **GO through CP4** against a Dockerized Gromox. Two things are left:
**CP5** (a paper exercise, no code) and **decoding real message rows**, which is blocked on a Gromox
harness quirk rather than on the protocol. Both are easier on Exchange.

Read `README.md` first — it holds the scope fence, the protocol facts, and the checkpoint table.
This file is only the "how do I continue on a different box" part.

---

## Where the spike got to

| CP | Result |
|---|---|
| CP0 endpoint answers | **GO** |
| CP1 Autodiscover | **GO** — returns MailStore URL + `LegacyDN` |
| CP2 `Connect` | **GO** — `ecSuccess`, session cookie, **empty aux buffer accepted** |
| CP3 `RopLogon` | **GO** — framing right first try, 13 FolderIds, **no compression/XOR needed** |
| CP4 table chain | **GO** — 4 ROPs in **one** `Execute`, **in-buffer handle chaining works**, 13 rows decoded |
| CP5 ICS distance | **not started — mandatory before any GO/NO-GO verdict** |

Measurements at CP4: **1,447 lines of Rust** excluding tests (2,083 with), **3 HTTP round trips** to
first row, **2 `Execute` calls**, **6 of 6 ROPs**, **6 OXCDATA property types**. Every threshold in
the README's go/no-go table is green.

**48 offline tests pass** (`cargo test`), including hand-computed golden byte vectors for the
`Connect` body, the `RopBuffer` framing, and `RopLogon`, plus never-panic tests over hostile input.

---

## The two open items

### 1. Message rows (blocked on Gromox, should just work on Exchange)

The contents-table path runs end-to-end against the real Inbox — all four ROPs return OK — but
yields **0 rows because the mailbox is empty**. Seeding it via `gromox-eml2mt | gromox-mt2exm`
fails: Gromox's exmdb IPC pins its listener to `[::1]:5000` regardless of `exmdb_listen`, while its
own `exmdb_client` resolves `localhost` → `127.0.0.1` and gets no answer. `/etc/hosts` is a
bind-mount inside the container, so it cannot be reordered in place.

**On Exchange this problem does not exist** — send the mailbox a few messages from anywhere and run
`--table contents`.

What is genuinely unverified until then, and is *not* a formality:

- whether the server returns **StandardPropertyRow (`0x00`) or FlaggedPropertyRow (`0x01`)** — both
  are implemented and unit-tested, neither is confirmed live for message rows;
- whether long subjects come back as **flag `0xA` + an error code** rather than a string (table
  values are length-limited, so this is expected, not exotic);
- `PidTagMessageDeliveryTime` as a real FILETIME.

The hierarchy table already decoded 13 live rows through the identical code, so the decoder itself
is proven against a real server.

If you want to unblock it on Gromox instead: try `extra_hosts` in the compose file, or find where
Gromox actually reads `exmdb_list.txt` (it ignored `/etc/gromox/exmdb_list.txt`).

### 2. CP5 — the ICS distance (paper only, ~half a day, **mandatory**)

CP4 is the **floor**; ICS is the **ceiling**. A GO based on CP4's numbers alone would be falsely
optimistic, which is exactly the failure this checkpoint exists to prevent. Enumerate — do not
implement — what a real `Provider` impl needs beyond CP4:

1. **Delta sync** = [MS-OXCFXICS]: `RopSynchronizationConfigure` (0x70),
   `RopSynchronizationGetTransferState` (0x82), `RopFastTransferSourceGetBuffer` (0x4E),
   `RopSynchronizationImportDeletes` (0x74) — plus the **FastTransfer stream**, a marker-driven
   serialization completely unlike `PropertyRow`, and **ICS state as GLOBSET/IDSET**. This is what
   `stream_email`'s resumable cursor would be built on.
2. **Bodies** = `RopOpenMessage` (0x03) + `RopGetPropertiesSpecific` (0x07) +
   `RopOpenStream`/`RopReadStream` (0x2B/0x2C), then **[MS-OXCMAIL]** to assemble RFC 5322 from MAPI
   properties — which is what `fetch_message_source` must return. Routinely underestimated.
3. **Named properties** = `RopGetPropertyIdsFromNames` (0x56) + a per-store id cache.
4. **Attachments** = `RopGetAttachmentTable` (0x21) + `RopOpenAttachment` (0x22).
5. **Address book** = the whole NSPI endpoint (`/mapi/nspi/`), only if contacts/GAL are in scope.

Then state a ratio. Prior to be confirmed or refuted: **CP4 is ~25–35% of a read-only provider and
under 20% of one with writes.** If the remainder is >4× the spike, a GO must be an explicit decision
to fund a multi-month protocol implementation — not an incremental "we already have rows".

---

## Setting up Exchange Server SE

Trial is 180 days. Supported until **at least 2035-12-31** under the Modern Lifecycle Policy, and
the MAPI-over-HTTP docs list "APPLIES TO: 2016, 2019, **Subscription Edition**".

**Why on-prem works when Exchange Online cannot:** Microsoft exposes no OAuth permission model for
Extended MAPI, and Basic auth is permanently removed from Exchange Online. On-prem, the auth methods
on the MAPI virtual directory are **admin-settable**, so Basic stays available — the same path this
spike already uses against Gromox. No auth work is needed.

In the Exchange Management Shell:

```powershell
# 1. MAPI/HTTP on at the org level (default is on for a clean SE install; verify).
Get-OrganizationConfig | Select-Object MapiHttpEnabled
Set-OrganizationConfig -MapiHttpEnabled $true

# 2. Allow Basic on the MAPI vdir so the spike can authenticate.
Get-MapiVirtualDirectory | Format-List Identity,InternalUrl,ExternalUrl,IISAuthenticationMethods
Set-MapiVirtualDirectory -Identity "<SERVER>\mapi (Default Web Site)" `
    -InternalUrl https://<SERVER-FQDN>/mapi `
    -IISAuthenticationMethods Basic,Ntlm,Negotiate

# 3. A test mailbox.
New-Mailbox -Name spiketest -UserPrincipalName spiketest@<domain> -Password (Read-Host -AsSecureString)

# 4. Sanity check, and restart IIS after vdir changes.
iisreset /noforce
Test-OutlookConnectivity -RunFromServerId <SERVER> -ProbeIdentity OutlookMapiHttpSelfTestProbe
```

Then get the two values the spike needs from Autodiscover (PowerShell, from any box that can reach
the server):

```powershell
$cred = Get-Credential
$body = @'
<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/outlook/requestschema/2006">
  <Request>
    <EMailAddress>spiketest@DOMAIN</EMailAddress>
    <AcceptableResponseSchema>http://schemas.microsoft.com/exchange/autodiscover/outlook/responseschema/2006a</AcceptableResponseSchema>
  </Request>
</Autodiscover>
'@
Invoke-RestMethod -Uri "https://<SERVER-FQDN>/Autodiscover/Autodiscover.xml" `
  -Method Post -ContentType "text/xml" -Body $body -Credential $cred -SkipCertificateCheck
```

Take from the response:

- `<User><LegacyDN>` → the `--dn` argument (**never hand-construct this**);
- `<Protocol Type="mapiHttp">` → `<MailStore><InternalUrl>` → the `--url` argument, **verbatim,
  including its query parameters**.

## Running the spike on Windows

`rustup` + a normal stable toolchain is all that is needed; the spike is a detached workspace with
one dependency (`reqwest`, blocking + rustls), so it does not build the engine.

```powershell
cd tools\mapi-spike
cargo test          # 48 offline tests, no server needed

$url  = "https://<SERVER-FQDN>/mapi/emsmdb/?MailboxId=<guid>@<domain>"   # from Autodiscover
$dn   = "/o=First Organization/ou=Exchange Administrative Group (FYDIBOHF23SPDLT)/cn=Recipients/cn=..."

cargo run -- connect --url $url --user "DOMAIN\spiketest" --pass "..." --dn $dn --insecure
cargo run -- logon   --url $url --user "DOMAIN\spiketest" --pass "..." --dn $dn --insecure
cargo run -- rows    --url $url --user "DOMAIN\spiketest" --pass "..." --dn $dn --insecure --table hierarchy
cargo run -- rows    --url $url --user "DOMAIN\spiketest" --pass "..." --dn $dn --insecure --table contents
```

`--insecure` skips certificate validation, which a lab Exchange with a self-signed cert needs. It is
a spike-only affordance — a real `provider-mapi` would take an `engine_tls::TlsClientConfig` and
have no such switch.

## What to expect to differ from Gromox — and to write down

These are the Gromox findings most likely to be Gromox-specific. **Each one is a reason the Exchange
run is worth doing**; confirming or refuting them is the main value of this leg.

| Finding on Gromox | What to check on Exchange |
|---|---|
| **`X-ClientInfo` required**, though absent from §2.2.2.1's mandatory list | Does Exchange also require it? (Outlook always sends it, so probably yes.) |
| **`X-ResponseCode` values disagree with the spec table** — Gromox returns 3/6/5 where the spec says 7/13/12 | Exchange should match the spec. If so, **Gromox is the outlier** and a provider must not hard-map codes. |
| **`?MailboxId=` required or the router 404s** | Exchange's Autodiscover URL also carries `?MailboxId=`; check whether a bare `/mapi/emsmdb/` works. That decides whether a provider can ever skip discovery. |
| **Empty auxiliary buffer accepted** | If Exchange rejects it, [MS-OXCRPC]'s `AUX_*` blocks come back into scope (port from the MIT reference — see below). |
| **`NoCompression\|NoXorMagic` honoured** | If Exchange compresses anyway, budget ~250 lines for LZ77+DIRECT2 and the 0xA5 XOR, plus a fuzz target. The code **fails loudly** rather than mis-parsing, so this shows up as a clear error. |
| **13 FolderIds, Inbox at slot 4** | Should be identical — it is a fixed layout in [MS-OXCSTOR] §2.2.1.1.3. |
| **In-buffer handle chaining works** | The hard gate. If Exchange refuses it, folder walks cost N round trips and that flips the recommendation. |
| `LogonTime` is **8 bytes**, private-mailbox logon response is **exactly 166 bytes** | Pinned by a test; a different total means a layout difference worth finding. |

One trap already paid for: **`Connect`'s `Flags` is not `Execute`'s `Flags`.** On `Connect`, bit 0
requests *administrator privilege* ([MS-OXCRPC] §3.1.4.1 `ulFlags`); on `Execute` it means "do not
compress". The MIT reference sets Connect's to `1` under a comment about compression, which makes an
ordinary user's logon fail with `ecLoginPerm` (0x000003F2) and reads like an auth error. Keep
Connect's at `0`.

## Reference sources

- **`OfficeDev/Interop-TestSuites`** — MIT, Microsoft-authored, **client**-side C#:
  `ExchangeMAPI/Source/Common/Common/ExchangeMapiClient/` has `MapiHttpAdapter.cs`,
  `OxcropsClient.cs`, `RopMessages/**`, `Enum.cs`. MIT → MPL-2.0 is clean with attribution. **This
  is the source to port from.**
- **Gromox is AGPL-3.0 and is never a source of code.** Read it to understand behaviour only.
  Running it as a fixture is fine and creates no derived work (this repo already runs Stalwart,
  also AGPL).
- `[MS-OXCMAPIHTTP]` v20250520 (the transport), plus `[MS-OXCROPS]`, `[MS-OXCDATA]`, `[MS-OXCSTOR]`,
  `[MS-OXCRPC]` on Microsoft Learn.

## Capture transcripts

Whatever the verdict, **save the raw request/response byte pairs** from Exchange under
`tools/mapi-spike/transcripts/`. Per AGENTS.md the offline fakes serve canned bytes regardless of
the request and so cannot catch a wrong request shape; these transcripts are the only artifact here
that cannot be re-derived from a spec, and they are what a future `provider-mapi`'s offline fixture
suite would be built from. Scrub per `crates/provider-graph/tests/fixtures/README.md`'s rules.

## The Gromox harness (still useful as a second implementation)

`docker/gromox/` — MariaDB + `gromox-http` only, not the official 18-daemon stack. Builds natively
on arm64 and x86_64.

```sh
cd docker/gromox && docker compose up -d --wait
# alice@spike.test / alicepass on http://127.0.0.1:18082
```

Two implementations disagreeing is *the* way to tell a protocol requirement from a vendor quirk, so
keep running both.
