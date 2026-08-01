# Handoff — the Exchange leg is **done**. Both open items are closed.

This file used to say "continue on a box with Exchange Server". That happened on **2026-08-01**,
against Exchange Server SE (`Microsoft.Exchange.MapiHttp`, MAPI/HTTP v1), mailbox `dev.local`.

**Read [`README.md`](README.md) for the findings table and
[`CP5-ics-distance.md`](CP5-ics-distance.md) for the verdict.** This file is now only the record of
what closed and what is still genuinely unverified.

---

## Both blockers closed

### 1. Message rows — **done**

Blocked on Gromox only because seeding its mailbox failed (an exmdb IPC binding quirk). On Exchange
the mailbox was seeded over plain SMTP and **5 real message rows decoded on the first attempt**,
with `PidTagMid`, `PidTagSubject`, `PidTagMessageDeliveryTime` and `PidTagMessageFlags`. The
FILETIME decodes to the correct wall-clock time (`134300850968907102` → `2026-08-01 19:11:36Z`).

The three things listed as "genuinely unverified, and not a formality" are now answered:

| Was unverified | Answer |
|---|---|
| Standard (`0x00`) or Flagged (`0x01`) row? | **Standard, always** — all 20 rows across both tables, on both servers. The Flagged path is **still unconfirmed live.** |
| Do long subjects come back as flag `0xA` + an error? | **No — the prediction was wrong.** Silently truncated to 255 chars with a literal `...`, in a Standard row, no error. See README; this is a correctness hazard, not a curiosity. |
| `PidTagMessageDeliveryTime` as a real FILETIME? | **Yes**, decodes correctly. |

### 2. CP5 — **done**

[`CP5-ics-distance.md`](CP5-ics-distance.md). The prior (CP4 ≈ 25–35% of a read-only provider) is
**refuted**: measured against this repo's actual `Provider` trait it is **~15% read-only, ~9% at
parity**, putting the remainder past the 4× tripwire.

## What is still unverified

Stated plainly, because everything else here is green and that is misleading on its own:

- **`FlaggedPropertyRow` has never been seen from a real server.** Implemented, unit-tested against
  hand-built vectors, unconfirmed live. Do not assume it is dead code — it is required by the spec
  and two servers choosing not to use it proves nothing about a third.
- **Nothing about ICS or FastTransfer.** Zero markers parsed, zero GLOBSETs decoded.
- **Nothing about message bodies.** Not one byte of content fetched.
- **Only two implementations.** Both honoured `NoCompression|NoXorMagic` and both accepted an empty
  aux buffer. That is evidence, not proof, that a third would.
- **Basic auth only.** NTLM/Negotiate is untouched, and Basic is *not* on by default on an Exchange
  MAPI vdir — it had to be enabled for this run. Any real deployment needs SPNEGO.

## Reproducing the Exchange run

Autodiscover **must** carry `X-MapiHttpCapability: 1` or the `mapiHttp` block is omitted entirely:

```powershell
$body = @'
<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/outlook/requestschema/2006">
  <Request>
    <EMailAddress>USER@DOMAIN</EMailAddress>
    <AcceptableResponseSchema>http://schemas.microsoft.com/exchange/autodiscover/outlook/responseschema/2006a</AcceptableResponseSchema>
  </Request>
</Autodiscover>
'@
Invoke-WebRequest -Uri "https://<SERVER>/Autodiscover/Autodiscover.xml" -Method Post `
  -ContentType "text/xml" -Body $body -Credential (Get-Credential) -SkipCertificateCheck `
  -Headers @{ 'X-MapiHttpCapability' = '1' }
```

Take `<User><LegacyDN>` → `--dn`, and `<Protocol Type="mapiHttp"><MailStore><InternalUrl>` →
`--url` (verbatim, including `?MailboxId=`). Server-side prerequisites and the run commands are in
[`README.md`](README.md) → "Running".

Seeding the mailbox is just SMTP — no Exchange tooling needed:

```powershell
Send-MailMessage -SmtpServer <SERVER> -From seeder@DOMAIN -To USER@DOMAIN -Subject "…" -Body "…"
```

## The trap that is still worth knowing

**`Connect`'s `Flags` is not `Execute`'s `Flags`.** On `Connect`, bit 0 requests *administrator
privilege* ([MS-OXCRPC] §3.1.4.1 `ulFlags`); on `Execute` it means "do not compress". The MIT
reference sets Connect's to `1` under a comment about compression, which makes an ordinary user's
logon fail with `ecLoginPerm` (0x000003F2) and reads like an auth error. Keep Connect's at `0`.

## Reference sources

- **`OfficeDev/Interop-TestSuites`** — MIT, Microsoft-authored, **client**-side C#:
  `ExchangeMAPI/Source/Common/Common/ExchangeMapiClient/`. MIT → MPL-2.0 is clean with attribution.
  **This is the source to port from.**
- **Gromox is AGPL-3.0 and is never a source of code.** Read it to understand behaviour only.
  Running it as a fixture creates no derived work (this repo already runs Stalwart, also AGPL).
- `[MS-OXCMAPIHTTP]` v20250520, plus `[MS-OXCROPS]`, `[MS-OXCDATA]`, `[MS-OXCSTOR]`, `[MS-OXCRPC]`,
  and — for anything past CP4 — `[MS-OXCFXICS]`, `[MS-OXCMAIL]`, `[MS-OXOCAL]`, `[MS-OXNSPI]`.

## The Gromox harness (keep it)

`docker/gromox/` — MariaDB + `gromox-http`. Two implementations disagreeing is *the* way to tell a
protocol requirement from a vendor quirk, and it earned its keep here: the `X-ClientInfo`
requirement and the `X-ResponseCode` numbering both turned out to be Gromox-specific, which a
single-server run would have baked into a provider as universal truth.

```sh
cd docker/gromox && docker compose up -d --wait
# alice@spike.test / alicepass on http://127.0.0.1:18082
```
