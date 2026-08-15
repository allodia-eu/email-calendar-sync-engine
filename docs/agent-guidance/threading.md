# Email threading

How the engine groups messages into conversations (threads), across providers and
across folders. Read before touching `Message.thread_id`, the `Thread` model, the
derivation pass (`engine-sync` `threading.rs`), or `Engine::derive_mail_threads`.

## Model

- `Message.thread: Option<ThreadRef>` — the conversation a message belongs to, as
  `{ id: ThreadId, provenance: ThreadProvenance }`. `Message::thread_id()` reads just
  the id. `Thread` (engine-core) carries the same `ThreadProvenance`.
- `ThreadProvenance { ProviderAssigned, LocallyDerived }` — provider-assigned (JMAP
  `Thread.id`, Gmail `threadId`, Graph `conversationId`) vs derived by this engine.
  **The provenance is load-bearing, not decoration**: derivation re-runs after every
  sync and re-groups its *own* ids, so it must be able to tell them from the
  provider's, which it never touches. An id alone cannot say where it came from — that
  is what let a reply synced after its thread was derived become a singleton thread
  (issue #53).
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
  `Message-ID`) is one conversation.
- **Which messages enter the graph.** Those with no thread id **and those the engine
  derived one for**. Re-grouping already-derived mail is the whole point: a reply that
  arrives in a later sync unites with the thread it belongs to, which is only possible
  if that thread's members are still in the graph. Provider-threaded messages stay out
  entirely and are never rewritten — a `References` header must not merge two threads
  the provider kept apart — so the pass is a no-op for JMAP/Graph.
- **The thread id is a function of the component**, not of arrival order: the
  lexicographically smallest owned `Message-ID`, falling back to the smallest provider
  key when a component owns none. A full resync therefore reproduces the same ids. The
  price is that a merge can **re-key** a thread (a late message owning a smaller id, or
  two components joining): every member is then re-applied with the new id, and a host
  keying list rows on `thread_id` sees those rows change identity. This was chosen over
  letting the incumbent thread keep its id, which would make ids depend on sync order
  and diverge from what a resync produces.
- **No subject linking.** JWZ-style subject merging over-merges unrelated mail; the
  header graph is the safe baseline. A guarded subject fallback is a possible future
  refinement.
- **Persistence.** `derive_mail_threads` writes `message.thread_id` for the messages whose id
  changed, **and nothing else about them** — no payload, no re-projected row. The pass decided a
  thread id; carrying every other column along with it is how it used to hand back the flags a
  mark-read had just moved, since it re-projects from payloads that no longer carry keywords at
  all. It writes **without advancing the scope cursor** — it is a derivation, not a sync — so
  the next sync still resumes correctly.

  It compares its computed assignment against the **stored row**, not against the payload it
  rebuilt the graph from. The payload's thread is the *provider's* (present only when the
  provider assigned it); the derived id lives in the row, so comparing against the payload would
  re-assign every message on every pass and never converge. A pass over unchanged mail writes
  nothing, and there is a test that says so.
- IMAP must fetch the `References` header for this to work — it is **not** in the IMAP
  `ENVELOPE`, so `provider-imap` fetches `BODY.PEEK[HEADER.FIELDS (REFERENCES)]` alongside
  `ENVELOPE` (`imap-smtp.md`).

## Host responsibility

Run `Engine::derive_mail_threads` after **every** mail sync of a derived-threading
account, not only the first: a pass groups the mail that is in the store when it runs,
so mail synced since the last pass is unthreaded until the next one.

The engine derives and **persists** the grouping; it exposes the flat list
(`Engine::messages`, each row carrying `thread_id`) and a host groups by `thread_id`.
The **flat-vs-threaded toggle is a host/view-model concern** — the engine owns the data,
the host chooses how to render it (a flat list ordered by date, or threads ordered by
latest activity).
