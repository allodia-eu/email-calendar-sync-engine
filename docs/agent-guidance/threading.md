# Email threading

How the engine groups messages into conversations (threads), across providers and
across folders. Read before touching `Message.thread_id`, the `Thread` model, the
message-id graph (`msgid_ref`), the assignment inside the apply (`store-sqlite`
`derived_ops/threading.rs` and its reference twin `engine-store` `mem/threading.rs`),
or `Engine::rebuild_thread_index` (`engine-sync` `threading.rs`).

## Model

- `Message.thread: Option<ThreadRef>` — the conversation a message belongs to, as
  `{ id: ThreadId, provenance: ThreadProvenance }`. `Message::thread_id()` reads just
  the id. `Thread` (engine-core) carries the same `ThreadProvenance`.
- `ThreadProvenance { ProviderAssigned, LocallyDerived }` — provider-assigned (JMAP
  `Thread.id`, Gmail `threadId`, Graph `conversationId`) vs derived by this engine.
  **The provenance is load-bearing, not decoration**: the engine re-groups its *own*
  ids whenever a component grows, so it must be able to tell them from the provider's,
  which it never touches. An id alone cannot say where it came from.
- The RFC 5322 `Message-ID` / `In-Reply-To` / `References` headers (`Envelope`) are
  threading hints, never identity (`modeling.md`).

## Where thread ids come from

- **Provider-native** (JMAP today; Gmail/Graph later): the adapter sets
  `Message.thread_id` during sync. Nothing else to do — it round-trips to
  `Engine::messages`.
- **Derived** (IMAP, and any provider without native threading): the engine assigns it
  **inside the apply that stores the message**, from the stored message-id graph. A
  message is on its conversation the moment the apply commits; there is no second pass
  to run.

## Derivation

- **Account-wide and cross-folder, inside the apply's transaction.** IMAP syncs per
  mailbox (each folder is a scope); a sent reply and its received original are distinct
  objects in distinct scopes, so a reply in Sent threads with the original in the Inbox
  (the Outlook/Gmail behavior). The lookup is keyed by **account**, not scope, for
  exactly that reason. This is the one place the store computes rather than persists a
  precomputed row (`store-and-sync.md`): a thread id is a function of the incoming
  object *and* of what is already stored across every scope, and only the apply's
  transaction can fence a concurrent scope of the same account merging into the same
  component.
- **The graph is a table.** `msgid_ref` holds, per message, every id it owns
  (`Message-ID`, `owned = 1`) and every id it references (`In-Reply-To`/`References`),
  so the component an arrival joins is an indexed lookup rather than a scan of every
  payload in the account. Only **derivable** messages are in it — a provider-threaded
  message projects no rows, so a forged `References` header has nothing to reach — and a
  message with no ids at all projects none either: nothing can share an id with it, so
  it is a singleton named after its own provider key.
- **Union-find over the Message-ID graph.** Two messages unite if they share any id they
  own or reference. A reply (whose `References`/`In-Reply-To` carry the parent's
  `Message-ID`) joins its parent; the same message copied into two folders (same
  `Message-ID`) is one conversation.
- **Which messages enter the graph.** Those with no thread id **and those the engine
  derived one for**. Re-grouping already-derived mail is the whole point: a reply that
  arrives in a later sync unites with the thread it belongs to, which is only possible
  if that thread's members are still in the graph. Provider-threaded messages stay out
  entirely and are never rewritten — a `References` header must not merge two threads
  the provider kept apart — so threading is a no-op for JMAP/Graph.
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
- **Persistence.** Only `message.thread_id` moves — no payload, no re-projected row. A derived
  thread id lives in the row alone; carrying every other column along with it is how a
  re-projection used to hand back the flags a mark-read had just moved.
- **The rebuild is the repair, not part of a sync.** `Engine::rebuild_thread_index` re-derives
  the whole account from the stored payloads and rewrites the graph rows as well as the thread
  ids. Reach for it when the index is suspect — after the migration that introduced the graph, or
  a repair — never after an ordinary sync, where it would re-read every payload in the account to
  confirm an answer already written. It writes **without advancing the scope cursor**, and it
  compares its computed assignment against the **stored row** rather than the payload it rebuilt
  the graph from, so a pass over mail that is already right writes nothing.

  Two shapes are deliberately left to it, because both need the whole component walked: a
  component a *deletion* split in two keeps one id between them, and one whose only owner was
  deleted keeps a name no remaining member owns. Both are rare (`References` accumulates every
  ancestor) and both leave a thread that is still unique and still stable.
- IMAP must fetch the `References` header for this to work — it is **not** in the IMAP
  `ENVELOPE`, so `provider-imap` fetches `BODY.PEEK[HEADER.FIELDS (REFERENCES)]` alongside
  `ENVELOPE` (`imap-smtp.md`).

## Host responsibility

**Nothing to run after a sync.** A sync threads what it applies, so a host calls no second
pass. `Engine::rebuild_thread_index` is the repair path only — a store that has just migrated
onto the graph, or an index a support case says is wrong.

**Not even the migration repair.** The migration that introduced the graph backfills its rows from
the stored payloads but assigns no thread ids, so a message the old whole-account pass had not yet
grouped stays ungrouped — and an arrival cannot adopt it, because the component lookup reaches a
stored message only through the thread id its row already carries. The engine repairs that itself:
it asks the store whether any message is *in the graph with no thread* and rebuilds when the answer
is yes.

The pass asks once, before it touches a folder. There is one mail entrypoint and the engine owns
the fan-out, so there is one place for this — the arrangement that let the question be asked in a
function the shipping client never called is gone.

That question is deliberately about the damage rather than a flag saying a repair is due. A flag has
to be set by whoever knew and cleared by whoever fixed it, and is wrong if either forgets; this is
true exactly when there is something to fix, so it also covers a rebuild that failed halfway and a
store damaged in some way nobody predicted. In steady state it is one indexed lookup that finds
nothing, because an ordinary page threads what it applies.

The engine derives and **persists** the grouping; it exposes the flat list
(`Engine::messages`, each row carrying `thread_id`) and a host groups by `thread_id`.
The **flat-vs-threaded toggle is a host/view-model concern** — the engine owns the data,
the host chooses how to render it (a flat list ordered by date, or threads ordered by
latest activity).
