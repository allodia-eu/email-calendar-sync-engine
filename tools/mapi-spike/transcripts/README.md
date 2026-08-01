# Transcripts

Raw request/response byte pairs from **real servers**. Per AGENTS.md the offline provider fakes
answer canned bytes regardless of the request they receive, so they cannot catch a wrong *request*
shape. These captures are the only artifact in the spike that cannot be re-derived from a spec, and
they are what a future `provider-mapi`'s offline fixture suite would be built from.

## Layout

```
exchange-se/<scenario>/NN-<requesttype>.request.bin    the POST body, byte-exact
exchange-se/<scenario>/NN-<requesttype>.response.bin   the HTTP payload after de-chunking
exchange-se/<scenario>/NN-<requesttype>.meta.txt       HTTP status, X-ResponseCode, paired hexdump
```

`NN` is the request order within one session, so `01-connect` → `02-execute` (logon) →
`03-execute` (the four-ROP chain) reads top to bottom as one Session Context.

| Scenario | What it captures |
|---|---|
| `exchange-se/hierarchy` | `Connect` → `RopLogon` → OpenFolder/GetHierarchyTable/SetColumns/QueryRows over the IPM subtree (15 rows) |
| `exchange-se/contents` | the same chain over the Inbox contents table (5 message rows) |

Captured against **Exchange Server SE** (`Microsoft.Exchange.MapiHttp`, MAPI/HTTP v1) on
2026-08-01.

## How they were produced

```sh
cargo run -- rows --url <mapi-url> --user <u> --pass <p> --dn <legacy-dn> \
  --insecure --table contents --transcript transcripts/exchange-se/contents \
  --scrub '<mailbox-guid>=00000000-0000-0000-0000-000000000000' \
  --scrub '<dn-guid>=ffffffffffffffffffffffffffffffff' \
  --scrub '<hostname>=exchange-lab-01' \
  --scrub '<HOSTNAME>=EXCHANGE-LAB-01' \
  --scrub 'Developer User=Spike Test Usr'
```

Run this from PowerShell, not Git Bash — MSYS rewrites the `/o=…` LegacyDN into a Windows path and
Exchange then fails the `Connect` with `ecUnknownUser`. See `../README.md` → "Running".

## Scrubbing

Following the rules in `crates/provider-graph/tests/fixtures/README.md`.

- **Credentials never reach a capture.** The recorder is handed the request body and the response
  payload only — the `Authorization` header is not passed to it. That is structural, not a
  scrubbing pass that could be forgotten.
- **Identifiers are rewritten** via `--scrub from=to`: the mailbox GUID, the LegacyDN's recipient
  GUID, the server hostname, and the mailbox display name.
- **Replacements are length-preserving** (padded or truncated to the needle). A transcript is read
  by byte offset, and a capture whose offsets had shifted would be actively misleading — worse
  than an unscrubbed name. `transcript::tests::scrubbing_preserves_length` pins this.
- **Both encodings are scrubbed.** MAPI bodies carry ASCII (the LegacyDN on `Connect`) *and*
  UTF-16LE (strings inside a `RopBuffer`), so each needle is replaced in both or half the
  occurrences survive. `transcript::tests::scrubs_ascii_and_utf16_occurrences_alike` pins this.
- **Matching is case-sensitive, so every case a server uses needs its own rule.** This already bit
  once: the `--url` carries the host lowercase, but Exchange echoes it **uppercase** in the
  `Connect` response (`WIN-…​.dev.local` at offset 151), so a single lowercase rule left the real
  hostname in the capture. Both forms are passed above. Case-folding is deliberately *not* done in
  the scrubber — it replaces raw bytes, and folding UTF-16LE bytes correctly is a different problem
  than the one a spike should solve. **Grep a fresh capture for each secret before committing it**;
  a rule that silently matched nothing looks exactly like a rule that worked.

The source was a throwaway lab box (`dev.local`) with no real accounts, so `dev.local`,
`/o=Dev/...` and the seeded message subjects are synthetic and deliberately left readable — they
are what makes the captures useful as fixtures.

## Re-verifying a capture

The bodies are byte-exact, so they can be replayed into the decoders directly:

```rust
let raw = include_bytes!("../transcripts/exchange-se/contents/03-execute.response.bin");
let exec = ropbuf::ExecuteResponse::parse(raw)?;      // meta-tag split already applied
let (rops, handles) = ropbuf::RopBuffer::parse(&exec.rop_buffer)?;
```

Note the `.response.bin` is the payload **after** reqwest's transparent de-chunking but **before**
the meta-tag preamble is stripped — i.e. it still begins `PROCESSING\r\nDONE\r\n`. That is
deliberate: the preamble split is itself parsing worth testing.
