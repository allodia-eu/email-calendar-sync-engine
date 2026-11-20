# The outbox and sending mail, at a glance

Two pictures for a contributor's first hour: how a write gets from a host to a server, and why
a message the user pressed Send on is never lost and never delivered twice. They describe what
the code does; the contract, with every rule and its reason, is
[`agent-guidance/store-and-sync.md`](agent-guidance/store-and-sync.md) → "The outbox" and "A send
survives its process". Read that before changing anything shown here.

## The outbox

![The outbox: a write is saved, claimed under a lease, sent by the provider, and its outcome
recorded; the outcome decides whether it is done, retried after a backoff, parked for
confirmation, or failed](images/outbox.svg)

The idea in one line: **the engine writes down what it is about to do before it does it**, so a
crash, a dead network or a suspended phone can interrupt a write but never make it vanish.

- **Save.** A write is stored as an *op* (its kind, plus the user's intent as a payload) and synced
  to disk before any network I/O. An idempotency key makes saving the same write twice return the
  op that is already there.
- **Claim.** A worker takes a *lease* on the op: a time limit plus a fencing token. Writes to one
  resource (`mail:<key>`, `event:<uid>`, `draft:<Message-ID>`) run one at a time, and an op that
  depends on another waits for it to succeed.
- **Call.** The provider adapter does the work against the server.
- **Record.** The outcome is stored, and only under the lease that claimed it: a worker whose
  lease was taken over meanwhile is refused, so it cannot overwrite a newer attempt.

Ops reach a worker three ways: the host call that made the write runs it straight away;
`drain_outbox` picks up whatever is queued when the host says so; and `recover_interrupted_ops`
settles at start-up what the last process left half-done. The engine runs no timer of its own,
because only the host knows when the network is back.

## Sending one message

![The seven steps of a send. Before the hand-over is recorded and the final byte written, an
interrupted send is simply sent again; after it, the send waits for confirmation, which its copy
in Sent, the host, or the original attempt provides](images/send-timeline.svg)

A send is the one write that cannot be blindly retried: a second attempt after the server took
the first one is a duplicate in somebody's inbox. So a send draws one line, the **point of no
return**: the last byte without which the server cannot deliver the message (the `.` that ends
SMTP `DATA`, or the last piece of an HTTP submission's body).

Just before crossing it, the attempt records the **hand-over** in the store, synced to disk.
Everything else follows from that one record:

| The attempt stopped… | The record says | So the engine |
|---|---|---|
| before the hand-over | nothing was handed over | sends it again; nothing reached the server |
| after it, with a clear answer | the server said yes, or said no | records `Succeeded`, or clears the record and treats the refusal like any other error |
| after it, with no answer | it *may* have gone | parks it as `NeedsConfirmation` and never retries on its own |

A parked send is settled by evidence, not by guessing: its copy turning up in Sent (matched by
`Message-ID`), the host asking the user (`confirm_pending_op`), or the original attempt waking up
and reporting what the server said.

## Where to go next

| To change… | Read |
|---|---|
| the outbox states, claims, leases, recovery | [`store-and-sync.md`](agent-guidance/store-and-sync.md) → "The outbox" |
| where each transport's point of no return is | [`providers.md`](agent-guidance/providers.md) → "A submission records its hand-over" |
| how long a send waits, and what a timeout means | [`deadlines.md`](agent-guidance/deadlines.md) |
| the host verbs (`drain_outbox`, `outbox`, cancel, retry, confirm) | [`engine-api.md`](agent-guidance/engine-api.md) |

## Updating the pictures

The SVGs are generated. Edit the text in [`images/outbox-diagrams.py`](images/outbox-diagrams.py)
and run `python3 docs/images/outbox-diagrams.py` (standard library only); the layout follows
from the text. A change to any step shown here changes the picture in the same commit, like any
other guidance doc.
