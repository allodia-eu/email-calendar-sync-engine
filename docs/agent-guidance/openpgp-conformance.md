# OpenPGP Conformance

The human-readable side of `crates/engine-openpgp/tests/conformance/`. The JSON tables are the
source of truth; this page says how they were made, how they are checked, and where they stand.
Policy decisions themselves are in `openpgp.md`.

## The tables

| Table | Rows | library | policy | not applicable |
|---|---|---|---|---|
| `rfc9580.json` | 221 | 103 | 87 | 31 |
| `rfc3156.json` | 16 | 0 | 15 | 1 |

A row is one normative paragraph: `id`, `section`, the BCP 14 `keyword`, the `excerpt` verbatim,
its `scope`, the `area` where the work lands (`parsing`, `armor`, `recognise`, `certificates`,
`verify`, `decrypt`, `compose`, `generate`, `secret-keys`, `cleartext`), a `note` where the
decision needs one, a `status` and the `tests` that pin it.

`tests/conformance.rs` fails when:

- a row that applies is `covered` and names no test, or is `planned` and names one;
- a named test has no `fn <name>(` anywhere under `crates/` (a renamed test must take its rows
  with it);
- a `not-applicable` row has no reason, or has a status;
- an id repeats, or RFC 9580 stops having 221 rows (the extraction is fixed; a row lost in an
  edit is a requirement lost).

Moving a row from `planned` to `covered` is part of the change that makes it true.

## Status

Everything not listed as covered is planned, and lands with the area it names. At the time of
writing, covered:

- RFC 3156 §4 and §5 recognition rules, and the signed-data byte rules the neutral walk enforces.
- RFC 9580 §3.3 and §14.1: identity is the whole fingerprint, never a key ID.

## Recipe

The RFC 9580 rows came from the plain-text RFC (fetched from rfc-editor.org) by this extraction,
then scoped by hand:

```python
import re
text = open('rfc9580.txt').read()
lines = [l for l in text.split('\n')
         if not re.match(r'^(RFC 9580 {2}|Wouters, et al\.)', l) and '\f' not in l]
section, rows = '', []
for para in re.split(r'\n\s*\n', '\n'.join(lines)):
    s = para.strip()
    m = re.match(r'^(\d+(\.\d+)*\.|A\.\d+(\.\d+)*\.)\s+(.*)', s)
    if m and len(s) < 120:
        section = s
        continue
    if s.startswith(('|', '+')):
        continue  # tables: read by hand
    if re.search(r'\b(MUST|SHOULD|REQUIRED|SHALL|RECOMMENDED)\b', s):
        rows.append((section, ' '.join(s.split())))
```

The extraction skips tables, so these carry requirements in their "Generate?" columns and were
read by hand into the policy in `openpgp.md`: Table 1 (S2K types), Table 2 (S2K usage), Table 18
(public-key algorithms), Table 23 (hash algorithms and salt sizes), Table 26 (packet versions in
encrypted messages), Table 27 (key and signature versions).

The RFC 3156 rows were taken by the same method and are few enough to have been written out
directly.
