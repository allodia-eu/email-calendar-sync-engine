# provider-graph test fixtures

Real Microsoft Graph **v1.0** JSON responses captured from a throwaway personal
account (`outlook.com`) via `tools/graph-oauth`, then **scrubbed** of PII. The
object *shapes* are verbatim from the live API; only account-identifying values
were mapped to deterministic fakes, consistently, so cross-references survive:

- emails → `testuser@example.test`, names → `Test User`, user id → `00000000feedface`
- folder ids → role names (`folder-inbox`, `folder-sentitems`, …; `folder-root`
  for the `msgfolderroot` parent), message ids → `message-N`
- `conversationId`/`@odata.etag`/`changeKey`/`internetMessageId`/`conversationIndex`
  → ordinal fakes; opaque `$deltatoken`/`$skiptoken` payloads → `opaque-token-N`
- body/`bodyPreview`/`webLink` content → fixed placeholders

The scrub is reproducible: `scratchpad/scrub.py` (kept out of the repo) maps the
gitignored raw captures under `tools/graph-oauth/.local/raw/` to these files. The
3 message fixtures are deterministic self-sent messages ("Fixture: …").

## Files

| Fixture | Real Graph call | Protects |
| --- | --- | --- |
| `mail/mailfolders.json` | `GET /me/mailFolders?$top=50` | folder → `Mailbox` normalization (8 folders) |
| `mail/mailfolders_delta.json` | `GET /me/mailFolders/delta` | folder container delta + `deltaLink` cursor |
| `mail/messages_delta_snapshot.json` | `GET /me/mailFolders/inbox/messages/delta?$select=…` | **initial** sync: full message objects + `deltaLink` |
| `mail/messages_delta_nochange.json` | replay the snapshot `deltaLink` | incremental no-op (`value:[]` + new `deltaLink`) |
| `mail/messages_delta_changed.json` | replay after `PATCH isRead` | **lightweight partial** changed entry — no `@odata.etag` (see Finding 4) → re-fetched |
| `mail/messages_delta_changed_full.json` | replay after `PATCH flag` | **full** changed entry (has `@odata.etag`) → used directly, no re-fetch |
| `mail/messages_delta_removed.json` | replay after `DELETE` | `{ id, @removed:{reason} }` tombstone shape |
| `mail/messages_list_page1.json` / `_page2.json` | `GET …/messages?$top=2` + its `@odata.nextLink` | real `nextLink` pagination chain |
| `mail/message_detail.json` | `GET /me/messages/{id}` | full single-message shape (the changed-id re-fetch) |
| `wellknown/*.json` | `GET /me/mailFolders/{inbox,drafts,…}` | well-known-name → id role resolution |
| `error/bad_request.json` / `unauthorized.json` | a 400 and a 401 | `error` envelope → `FailureClass` mapping |
| `me.json` | `GET /me` | account identity probe |

## Real-behavior findings (captured, not assumed)

1. **Personal `mailFolder` has no `wellKnownName`** (work/school-only) — selecting
   it 400s. Role mapping must resolve the well-known *aliases* (`/me/mailFolders/inbox`
   …) to ids and match, not read a role property.
2. **Folder `displayName`s are localized** (these are Dutch: "Postvak IN" = Inbox).
   Never parse display names for roles.
3. **`messages/delta` `$top` does not paginate on consumer.** Page size is
   server-controlled; `@odata.nextLink` appears only on large result sets. The
   `nextLink`-following path is therefore exercised via the *list* endpoint, whose
   `$top` does paginate.
4. **Incremental `delta` — full objects, except lightweight changes.** Per
   Microsoft's delta-query-messages doc a changed entry is a *full* object, and it
   is for substantive edits (a `flag` change → all selected fields + `@odata.etag`:
   `messages_delta_changed_full.json`). The undocumented exception, on consumer
   mailboxes, is a *lightweight* `isRead` change → only the changed property + `id`,
   **no** `@odata.etag` (`messages_delta_changed.json`). So the provider uses an
   entry with `@odata.etag` directly and **re-fetches only the etag-less partials**.
   *Snapshot* (initial) entries are always full. `@removed` items carry only `id` +
   `@removed`.
5. **Immutable ids** (requested via `Prefer: IdType="ImmutableId"`) are stable
   across calls and URL-safe — the right `ProviderKey` for Graph mail.
