#!/usr/bin/env python3
"""Draws the two diagrams in `docs/outbox-and-sending.md`: `outbox.svg` and `send-timeline.svg`.

Edit the text here and re-run `python3 docs/images/outbox-diagrams.py`, rather than editing the
SVGs by hand: the layout (row heights, card sizes, the point-of-no-return line) is computed from
the text. Standard library only. Each SVG carries its own light and dark palette
(`prefers-color-scheme`) on an opaque card, so it reads on either GitHub theme.
"""
import html, pathlib, re

FONT = "-apple-system, BlinkMacSystemFont, 'Segoe UI', 'Noto Sans', Helvetica, Arial, sans-serif"
MONO = "ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, 'Liberation Mono', monospace"

STYLE = f"""
  <style>
    svg {{ font-family: {FONT}; }}
    .bg {{ fill: #ffffff; stroke: #d0d7de; }}
    .ink {{ fill: #1f2328; }}
    .muted {{ fill: #59636e; }}
    .code {{ font-family: {MONO}; font-size: 0.92em; }}
    .h1 {{ font-size: 24px; font-weight: 700; }}
    .h2 {{ font-size: 17px; font-weight: 700; }}
    .h3 {{ font-size: 16px; font-weight: 650; }}
    .body {{ font-size: 14px; }}
    .small {{ font-size: 13px; }}
    .badge {{ font-size: 13px; font-weight: 700; letter-spacing: 0.02em; }}
    .panel {{ fill: #f6f8fa; stroke: #d0d7de; }}
    .box {{ fill: #ffffff; stroke: #d0d7de; }}
    .rail {{ stroke: #d0d7de; }}
    .wire {{ stroke: #8c959f; fill: none; }}
    .wire-head {{ fill: #8c959f; }}
    .blue-f {{ fill: #ddf4ff; }} .blue-s {{ stroke: #0969da; }} .blue-t {{ fill: #0969da; }} .blue-solid {{ fill: #0969da; }}
    .green-f {{ fill: #dafbe1; }} .green-s {{ stroke: #1a7f37; }} .green-t {{ fill: #116329; }} .green-solid {{ fill: #1a7f37; }}
    .amber-f {{ fill: #fff8c5; }} .amber-s {{ stroke: #9a6700; }} .amber-t {{ fill: #7d4e00; }} .amber-solid {{ fill: #9a6700; }}
    .red-f {{ fill: #ffebe9; }} .red-s {{ stroke: #cf222e; }} .red-t {{ fill: #a40e26; }} .red-solid {{ fill: #cf222e; }}
    .gray-f {{ fill: #f6f8fa; }} .gray-s {{ stroke: #8c959f; }} .gray-t {{ fill: #59636e; }} .gray-solid {{ fill: #6e7781; }}
    .zone-safe {{ fill: #1a7f37; fill-opacity: 0.05; }}
    .zone-risk {{ fill: #bf8700; fill-opacity: 0.07; }}
    .on-solid {{ fill: #ffffff; }}
    @media (prefers-color-scheme: dark) {{
      .bg {{ fill: #0d1117; stroke: #30363d; }}
      .ink {{ fill: #e6edf3; }}
      .muted {{ fill: #9198a1; }}
      .panel {{ fill: #151b23; stroke: #30363d; }}
      .box {{ fill: #0d1117; stroke: #3d444d; }}
      .rail {{ stroke: #3d444d; }}
      .wire {{ stroke: #656c76; }}
      .wire-head {{ fill: #656c76; }}
      .blue-f {{ fill: #121d2f; }} .blue-s {{ stroke: #4493f8; }} .blue-t {{ fill: #79c0ff; }} .blue-solid {{ fill: #1f6feb; }}
      .green-f {{ fill: #12261e; }} .green-s {{ stroke: #2ea043; }} .green-t {{ fill: #56d364; }} .green-solid {{ fill: #238636; }}
      .amber-f {{ fill: #272115; }} .amber-s {{ stroke: #bb8009; }} .amber-t {{ fill: #e3b341; }} .amber-solid {{ fill: #9e6a03; }}
      .red-f {{ fill: #25171c; }} .red-s {{ stroke: #f85149; }} .red-t {{ fill: #ff7b72; }} .red-solid {{ fill: #da3633; }}
      .gray-f {{ fill: #151b23; }} .gray-s {{ stroke: #656c76; }} .gray-t {{ fill: #9198a1; }} .gray-solid {{ fill: #656c76; }}
      .zone-safe {{ fill: #2ea043; fill-opacity: 0.07; }}
      .zone-risk {{ fill: #bb8009; fill-opacity: 0.09; }}
    }}
  </style>"""


def rich(s):
    """Escapes text, turning `code` spans into monospace tspans."""
    out = []
    for i, part in enumerate(re.split(r"`", s)):
        part = html.escape(part, quote=False)
        out.append(f'<tspan class="code">{part}</tspan>' if i % 2 else part)
    return "".join(out)


def text(x, y, s, cls="body ink", anchor=None):
    a = f' text-anchor="{anchor}"' if anchor else ""
    return f'<text x="{x}" y="{y}" class="{cls}"{a}>{rich(s)}</text>'


def lines(x, y, ls, cls="body ink", lh=20, anchor=None):
    return "\n".join(text(x, y + i * lh, s, cls, anchor) for i, s in enumerate(ls))


def rect(x, y, w, h, cls, rx=10, extra=""):
    return f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}" class="{cls}"{extra}/>'


def arrow(d, width=2, dash=None):
    ds = f' stroke-dasharray="{dash}"' if dash else ""
    return f'<path d="{d}" class="wire" stroke-width="{width}"{ds} marker-end="url(#head)"/>'


def svg(w, h, title, desc, body):
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" role="img" aria-labelledby="t d">
  <title id="t">{html.escape(title)}</title>
  <desc id="d">{html.escape(desc)}</desc>{STYLE}
  <defs>
    <marker id="head" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
      <path d="M0,0 L10,5 L0,10 z" class="wire-head"/>
    </marker>
  </defs>
{rect(0.5, 0.5, w - 1, h - 1, "bg", 16)}
{body}
</svg>
"""


def badge(x, y, label, color):
    w = int(len(label) * 7.6) + 22
    return (rect(x, y, w, 24, f"{color}-solid", 12)
            + text(x + w / 2, y + 16.5, label, "badge on-solid", "middle")), w


# --------------------------------------------------------------------------- outbox
def outbox():
    W = 960
    out = [text(32, 50, "The outbox: saved first, sent later", "h1 ink"),
           lines(32, 78, ["Every write a host asks for (a send, a flag, a move, an RSVP, a calendar edit) takes",
                          "the same four steps, and its outcome decides what happens to it next."], "body muted")]

    # Four steps.
    steps = [
        ("1", "Save", ["Stored as an op and", "synced to disk before", "any network I/O. The", "same idempotency key", "is the same op."]),
        ("2", "Claim", ["A worker leases it with", "a fencing token. It waits", "for its dependencies and", "for other writes to the", "same resource."]),
        ("3", "Call", ["The provider does the", "work: SMTP, JMAP,", "Graph, Gmail, CalDAV,", "CardDAV."]),
        ("4", "Record", ["The outcome is stored", "under the lease. A", "stale token is refused,", "so a late worker cannot", "overwrite a newer one."]),
    ]
    bx, by, bw, bh, gap = 32, 120, 200, 168, 32
    for i, (n, name, ls) in enumerate(steps):
        x = bx + i * (bw + gap)
        out.append(rect(x, by, bw, bh, "box", 12, ' stroke-width="1.5"'))
        out.append(f'<circle cx="{x + 30}" cy="{by + 30}" r="14" class="blue-solid"/>')
        out.append(text(x + 30, by + 35, n, "badge on-solid", "middle"))
        out.append(text(x + 54, by + 36, name, "h2 ink"))
        out.append(lines(x + 18, by + 70, ls, "body ink"))
        if i < 3:
            out.append(arrow(f"M{x + bw + 4},{by + bh / 2} L{x + bw + gap - 4},{by + bh / 2}"))

    # Outcomes panel.
    py = by + bh + 56
    outcomes = [
        ("Succeeded", "green", ["Done. It leaves the queue, and the drain report says what it became",
                                "(its provider key)."]),
        ("Retry later", "blue", ["A dropped connection, a `503`, a rate limit. Back to Pending with a backoff:",
                                 "30 s doubling to 30 min, or the server's own `Retry-After`. Other writes give",
                                 "up after 8 attempts; a send never does."]),
        ("Needs confirmation", "amber", ["Sends only: it may already be in front of its recipients. Parked, never",
                                         "retried blindly, until its copy in Sent, the host, or the attempt itself",
                                         "settles it."]),
        ("Failed", "red", ["A conflict, a bad login, a permanent refusal: waiting would not help.",
                           "A failed send stays listed, for the user to send again or delete."]),
    ]
    row_h = [len(o[2]) * 20 + 22 for o in outcomes]
    ph = 52 + sum(row_h) + 8
    out.append(rect(32, py, W - 64, ph, "panel", 12))
    out.append(text(52, py + 32, "What the outcome does", "h2 ink"))
    # Connector from Record down into the panel.
    rx = bx + 3 * (bw + gap) + bw / 2
    out.append(arrow(f"M{rx},{by + bh + 4} L{rx},{py - 4}"))
    y = py + 56
    retry_y = None
    for (label, color, ls), h in zip(outcomes, row_h):
        b, _ = badge(52, y, label, color)
        out.append(b)
        out.append(lines(252, y + 17, ls, "body ink"))
        if label == "Retry later":
            retry_y = y + 12
        y += h
    # Retry loops back to Claim.
    cx = bx + (bw + gap) + bw / 2
    out.append(arrow(f"M52,{retry_y} L40,{retry_y} Q20,{retry_y} 20,{retry_y - 20} "
                     f"L20,{by + bh + 40} Q20,{by + bh + 28} 32,{by + bh + 28} "
                     f"L{cx - 12},{by + bh + 28} Q{cx},{by + bh + 28} {cx},{by + bh + 16} L{cx},{by + bh + 4}",
                     1.5, "5 4"))
    out.append(text(cx + 10, by + bh + 24, "when its backoff has passed", "small muted"))

    # Who drives it.
    dy = py + ph + 44
    out.append(text(32, dy, "Who runs the queue", "h2 ink"))
    out.append(text(32, dy + 24, "The engine holds no timer: the host knows when the network is back, so the host decides when a pass runs.", "body muted"))
    drivers = [
        ("The write itself", ["A host call (send, flag,", "RSVP, …) saves its op and", "runs it straight away."]),
        ("`drain_outbox`", ["Picks up whatever is", "queued: on reconnect,", "after a sync, on request."]),
        ("Start-up recovery", ["`recover_interrupted_ops`", "settles what the last", "process left in flight."]),
        ("The user, via the host", ["`outbox` lists the queue;", "cancel, retry now and", "confirm act on one op."]),
    ]
    ty = dy + 46
    for i, (name, ls) in enumerate(drivers):
        x = bx + i * (bw + gap)
        out.append(rect(x, ty, bw, 112, "box", 12, ' stroke-width="1.5"'))
        out.append(text(x + 16, ty + 30, name, "h3 ink"))
        out.append(lines(x + 16, ty + 56, ls, "small ink", 19))
    H = ty + 112 + 32
    return svg(W, H, "The outbox: saved first, sent later",
               "A write is saved, claimed under a lease, sent by the provider, and its outcome recorded. "
               "The outcome decides whether it is done, retried after a backoff, parked for confirmation, or failed.",
               "\n".join(out))


# --------------------------------------------------------------------------- send timeline
def timeline():
    W = 960
    out = [text(32, 50, "Sending one message: what if it stops here?", "h1 ink"),
           lines(32, 78, ["Each step of a send, and what happens if the process dies, the device sleeps, or the",
                          "connection drops at that moment. Nothing is lost, and nothing is delivered twice."],
                 "body muted")]
    hy = 132
    out.append(text(100, hy, "STEP", "badge muted"))
    out.append(text(548, hy, "IF IT STOPS HERE", "badge muted"))

    G, A, R, B = "green", "amber", "red", "blue"
    before = [
        ("You press Send",
         ["`submit_mail` saves the message to the outbox,", "synced to disk, before any network I/O. It is",
          "keyed by its `Message-ID`, so it is queued once."],
         [("Nothing lost", G, ["Not saved yet: nothing was sent, and the draft", "is untouched. Saved: the next drain sends it."])]),
        ("A worker claims it",
         ["It takes a 5-minute lease with a fresh fencing", "token, renewed every 1⅔ minutes while the",
          "attempt runs. One write per resource at a time."],
         [("Sent again at once", G, ["The lease lapses. Recovery finds no hand-over", "record, so the send is due again right away."])]),
        ("Connect, log in, upload",
         ["Dial, TLS, log in. SMTP: `EHLO`, `MAIL`, `RCPT` and", "all of the message text. JMAP, Graph and Gmail:",
          "the request, all but the last piece of its body."],
         [("Retried", B, ["A dropped connection or a timeout: back in the", "queue with a backoff (or at once, if the process", "ended)."]),
          ("Kept for the user", R, ["A bad login or a permanent refusal: Failed,", "but still listed, to fix and send again."])]),
        ("Record the hand-over",
         ["The store marks the op `handed_over`, under its", "lease, synced to disk. Only once that succeeds",
          "may the final byte be written."],
         [("Retried, nothing sent", G, ["If the record fails (a store error, a lease", "another worker took), the attempt stops before", "the server has the whole message."])]),
    ]
    after = [
        ("Send the final byte",
         ["SMTP: the `<CRLF>.<CRLF>` that ends `DATA`.", "JMAP, Graph, Gmail: the last piece of the",
          "submitting request's body."],
         [("Needs confirmation", A, ["The connection drops or the process ends: the", "message may have been delivered. It is never", "retried blindly."])]),
        ("The server answers",
         ["Accepted: Succeeded, with the provider's key.", "A clear refusal (SMTP `4xx`/`5xx`, an HTTP error,",
          "a JMAP `SetError`) clears the hand-over record", "and is retried or failed like any other error."],
         [("Needs confirmation", A, ["No answer, one nobody can read, or a gateway's", "`502`/`504`: nobody knows whether it went."])]),
        ("Record the outcome",
         ["`mark_pending_op`, under the same lease. If the", "lease was recovered meanwhile, only the attempt",
          "that handed over may still report."],
         [("Needs confirmation", A, ["Recovery finds the hand-over record. The next", "sync usually settles it (below)."])]),
    ]

    def card_h(cards):
        return sum(30 + len(c[2]) * 19 + 16 for c in cards) + 10 * (len(cards) - 1)

    def row(y, n, title, ls, cards):
        h = max(26 + len(ls) * 20, card_h(cards))
        out.append(f'<circle cx="60" cy="{y + 10}" r="16" class="blue-solid"/>')
        out.append(text(60, y + 15, str(n), "badge on-solid", "middle"))
        out.append(text(100, y + 16, title, "h3 ink"))
        out.append(lines(100, y + 42, ls, "body ink"))
        cy = y - 6
        for label, color, cl in cards:
            ch = 30 + len(cl) * 19 + 16
            out.append(rect(536, cy, 392, ch, f"{color}-f {color}-s", 10, ' stroke-width="1"'))
            out.append(rect(536, cy, 5, ch, f"{color}-solid", 2))
            out.append(text(556, cy + 24, label, f"h3 {color}-t"))
            out.append(lines(556, cy + 46, cl, "small ink", 19))
            cy += ch + 10
        return h

    # Rows before the line.
    y = hy + 36
    zone_top = y - 22
    ys = []
    for i, (t, ls, cards) in enumerate(before):
        ys.append(y)
        y += row(y, i + 1, t, ls, cards) + 30
    zone_mid = y - 12
    # Point of no return.
    line_y = zone_mid + 14
    y = line_y + 56
    for i, (t, ls, cards) in enumerate(after):
        ys.append(y)
        y += row(y, len(before) + i + 1, t, ls, cards) + 30
    zone_bot = y - 12
    # Zones and rail drawn behind (inserted at the front of the body).
    back = [rect(16, zone_top, W - 32, zone_mid - zone_top, "zone-safe", 12),
            rect(16, line_y, W - 32, zone_bot - line_y, "zone-risk", 12),
            f'<line x1="60" y1="{ys[0] + 10}" x2="60" y2="{ys[-1] + 10}" class="rail" stroke-width="3"/>']
    out[4:4] = back
    out.append(f'<line x1="16" y1="{line_y}" x2="{W - 16}" y2="{line_y}" class="red-s" stroke-width="2.5" stroke-dasharray="8 6"/>')
    lab = "POINT OF NO RETURN"
    b, bw = badge(100, line_y - 12, lab, "red")
    out.append(b)
    out.append(text(100 + bw + 14, line_y - 9, "Above: safe to repeat, so it is simply sent again.", "small muted"))
    out.append(text(100 + bw + 14, line_y + 21, "Below: it may already be in front of its recipients.", "small muted"))

    # Resolution panel.
    py = zone_bot + 30
    out.append(text(32, py + 4, "Settling a send that needs confirmation", "h2 ink"))
    out.append(text(32, py + 28, "It is parked, not retried: no claim takes it. One of three things settles it.", "body muted"))
    ways = [
        ("Its copy shows up in Sent", ["A sync finds a copy in Sent with", "the same `Message-ID`: Succeeded.", "A copy in Drafts proves nothing."]),
        ("The host asks the user", ["`confirm_pending_op`: Delivered", "settles it; Not delivered sends it", "again, under a new token."]),
        ("The attempt wakes up", ["A process suspended mid-send", "that resumes can still report", "what the server said."]),
    ]
    bw3, gap = 285, 20
    ty = py + 50
    for i, (name, ls) in enumerate(ways):
        x = 32 + i * (bw3 + gap)
        out.append(rect(x, ty, bw3, 116, "box", 12, ' stroke-width="1.5"'))
        out.append(rect(x, ty, bw3, 5, "amber-solid", 2))
        out.append(text(x + 18, ty + 34, name, "h3 ink"))
        out.append(lines(x + 18, ty + 60, ls, "small ink", 19))
    fy = ty + 116 + 36
    out.append(lines(32, fy, [
        "A send never gives up on a count: it stays queued until it goes, or the user withdraws it. Recovery runs",
        "wherever a dead attempt is met: both claims, cancel, retry now, confirm, and `recover_interrupted_ops`."],
        "small muted", 19))
    H = fy + 19 + 30
    return svg(W, H, "Sending one message: what if it stops here?",
               "A send's seven steps. Before the hand-over is recorded and the final byte written, an interrupted "
               "send is simply sent again. After it, the send waits for confirmation: its copy in Sent, the host, "
               "or the original attempt settles it.",
               "\n".join(out))


if __name__ == "__main__":
    here = pathlib.Path(__file__).parent
    (here / "outbox.svg").write_text(outbox())
    (here / "send-timeline.svg").write_text(timeline())
