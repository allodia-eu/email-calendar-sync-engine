# Attributions

## mailkit_arkts (MPL-2.0)

This crate is ported from **mailkit_arkts**, a HarmonyOS email client whose
Exchange ActiveSync (EAS) implementation provides the WBXML codec and command
coverage this crate is built on.

- Reference tree: `common/MailKit/src/main/ets/protocols/activesync/` in the
  mailkit_arkts project (user-owned).
- License: confirmed 2026-08-12 that mailkit_arkts is user-owned and the ported
  EAS code may be relicensed under MPL-2.0 — the basis for every ported file's
  `SPDX-License-Identifier: MPL-2.0` header.
- Provenance: ported into the Kylins client (kylins-client commit `0dc611d`),
  then imported into this engine as a standalone crate (engine import commit
  `f7db44d`) and retrofitted to engine standards (edition 2024, workspace
  lints, the 500-line module split, the `engine-tls` transport).

Ported modules (under `src/`, post-split layout):

- `wbxml/` — WBXML serializer/deserializer, 26 code pages, tag tables
- `types/`, `status.rs` — folder, message, sync state models + status classifier
- `commands/` — FolderSync, Sync (incl. Change upsync), SendMail,
  SmartForward/Reply, ItemOperations (Fetch / EmptyFolderContents /
  conversation Move), MoveItems, GetItemEstimate, Ping, MeetingResponse,
  Search, Settings, ResolveRecipients, ValidateCert, FolderCreate/Delete/Update
- `client/`, `provision.rs`, `autodiscover/`, `multipart.rs`, `auth.rs` —
  transport, Provision handshake, AutoDiscover, multipart responses, auth
  strategies
- `calendar/`, `calendar_write/`, `contacts/`, `meeting_uid.rs` — class-typed
  conversion and meeting-uid mapping

Each ported file carries a header comment indicating its origin.
