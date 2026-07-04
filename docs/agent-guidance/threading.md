# Email threading

How the engine groups messages into conversations (threads), across providers and
across folders. Read before touching `Message.thread_id`, the `Thread` model, the
derivation pass (`engine-sync` `threading.rs`), or `Engine::derive_mail_threads`.

## Model

- `Message.thread_id: Option<ThreadId>` — the conversation a message belongs to.
- `Thread` (engine-core) carries `ThreadProvenance { Provider, Derived }`:
  provider-assigned (JMAP `Thread.id`, Gmail `threadId`, Graph `conversationId`) vs
  locally derived.
- The RFC 5322 `Message-ID` / `In-Reply-To` / `References` headers (`Envelope`) are
  threading hints, never identity (`modeling.md`).

## Where thread ids come from

- **Provider-native** (JMAP today; Gmail/Graph later): the adapter sets
  `Message.thread_id` during sync. Nothing else to do — it round-trips to
  `Engine::messages`.
- **Derived** (IMAP, and any provider without native threading): the engine derives it
  after sync via `Engine::derive_mail_threads` (`engine-sync::derive_mail_threads`).

## Derivation

- **Account-wide and cross-folder.** IMAP syncs per mailbox (each folder is a scope); a
  sent reply and its received original are distinct objects in distinct scopes.
  Derivation runs as a post-sync pass over **all** the account's stored mail, not inside
  one scope's `derive` step — so a reply in Sent threads with the original in the Inbox
  (the Outlook/Gmail behavior).
- **Union-find over the Message-ID graph.** Two messages unite if they share any id they
  own or reference. A reply (whose `References`/`In-Reply-To` carry the parent's
  `Message-ID`) joins its parent; the same message copied into two folders (same
  `Message-ID`) is one conversation. Each component gets a **stable** `ThreadId`: the
  lexicographically smallest owned `Message-ID`, falling back to the smallest provider
  key when a component owns none.
- **No subject linking.** JWZ-style subject merging over-merges unrelated mail; the
  header graph is the safe baseline. A guarded subject fallback is a possible future
  refinement.
- **Persistence.** `derive_mail_threads` re-applies the changed messages per scope (the
  object payload **and** the re-projected `mail_index.thread_id`) **without advancing
  the scope cursor** — it is a derivation, not a sync, so the next sync still resumes
  correctly. Messages already carrying a provider-assigned id are left untouched, so the
  pass is a no-op for JMAP.
- IMAP must fetch the `References` header for this to work — it is **not** in the IMAP
  `ENVELOPE`, so `provider-imap` fetches `BODY.PEEK[HEADER.FIELDS (REFERENCES)]` alongside
  `ENVELOPE` (`imap-smtp.md`).

## Host responsibility

The engine derives and **persists** the grouping; it exposes the flat list
(`Engine::messages`, each row carrying `thread_id`) and a host groups by `thread_id`.
The **flat-vs-threaded toggle is a host/view-model concern** — the engine owns the data,
the host chooses how to render it (a flat list ordered by date, or threads ordered by
latest activity).
