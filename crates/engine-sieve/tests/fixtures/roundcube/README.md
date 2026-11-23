# Scripts Roundcube's managesieve plugin wrote

The `*.sieve` files are the exact bytes Roundcube's managesieve plugin writes for the rule sets in
[`generate.php`](generate.php): the dialect the engine reads and writes
(`docs/design/filters-and-out-of-office.md`, decision 3). They are kept as observed, CRLF included
(`.gitattributes` marks them `-text`), so a test can say "the plugin wrote this", not "we think it
would".

## How they were made

On 2026-11-23, with `roundcube/roundcubemail:1.6.11-apache`
(`sha256:7191357d2ee2b50e4068571f683a85fec86f5b2f27b764e5850c5799f0ad89df`), PHP 8.4.15:

```sh
docker run --rm -i --entrypoint php roundcube/roundcubemail:1.6.11-apache < generate.php > out.txt
while read -r name written rewritten; do
  printf '%s' "$written" | base64 -d > "$name.sieve"
  [ "$written" = "$rewritten" ] || echo "$name: the plugin rewrites its own output differently"
done < out.txt
```

`generate.php` builds each rule set as the plugin's forms do (its rule array: tests, actions,
`join`, `disabled`, `name`), and prints the plugin's `as_text()` of it. It also reads that text
back with the plugin's own parser and prints `as_text()` again; for every file here the two were
identical, so these are scripts the plugin also reads as the rules it wrote.

The plugin is GPL. Nothing of it is in the engine: `generate.php` only calls it, and the engine's
reader and writer are written from the observed bytes. Values use reserved domains only.

## What the bytes show

- One `require ["a","b"];` first, names sorted, no spaces; omitted when nothing is required.
- Each rule: `# rule:[name]` on its own line (no line for an unnamed rule), then
  `if <test>`, a line with `{`, one action per line indented by a tab and ended with `;`, and a
  line with `}`. Every line ends in CRLF.
- Several tests: `allof (a, b)` or `anyof (a, b)`, a space before the parenthesis and `, `
  between tests. A rule for every message: `if true`.
- A disabled rule: `if false # <the tests as they would be written>`, the tests kept in the
  comment.
- String lists: `["a","b"]`. A value with a line end: a `text:` block, with periods doubled; the
  token after a block in a test starts on the next line.
- `address` writes its part before its match type (`address :domain :is`), and leaves `:all` out.

## The out-of-office reply: `reply_*.sieve`

The `reply_*` sets are built as the plugin's vacation engine builds its rule
(`rcube_sieve_vacation::set_vacation`, read in the same image): `currentdate` tests on the
`iso8601` part with the zone given (`+0000`, the value then ending in `Z`), `allof` for two tests
and `true` for none, the rule named `Out-of-Office`, and a disabled reply as any disabled rule. The
engine's reply reader and writer (`engine_sieve::dialect::reply`) are pinned to them:

- `reply_window`: both ends, `:days 7` and a subject, a two-line reason, then an ordinary rule;
- `reply_start`: a start alone, no subject;
- `reply_always`: no window (`if true`);
- `reply_disabled`: switched off, `if false # allof (…)`;
- `reply_date_zone`: the form the plugin's settings page writes when no time is given, the `date`
  part in a zone of `+0100`;
- `reply_redirect`: a reply that also redirects, which the engine reads as a reply it cannot show.

The plugin takes as the reply the first rule whose first action is `vacation`, writes a new one
first, and writes the end of the window with `le`, holding through the second it names. Its own
`vacation_rule` set above has no `:zone`, so the date there is the server's local one: the engine
does not read that window.

## A disabled rule whose test spans lines: not one Roundcube keeps disabled

`if false # <test>` holds a test on one line. For a test with a value that spans lines (a
`text:` block), the engine considered writing `if allof (false, <test>)`. Run through the same
image, the plugin read this script

```sieve
# rule:[Fwd]
if allof (false, header :contains "subject" text:
two
lines
.
)
{
	redirect "x@example.com";
}
```

as a rule that runs: `disabled: false`, `join: true`, and the tests `not true` (its spelling of
`false`) and the header test. Written back unchanged, it kept `false`, so a save alone does not
re-arm it. But the plugin shows it as an enabled rule with an odd first test, and removing that
test there re-arms it, so a forwarding rule an import stored disabled could start forwarding from
an edit in Roundcube. The engine therefore never writes this form: the plan keeps no disabled rule
whose test spans lines (`RuleFeature::DisabledMultiLine`, which no Sieve vocabulary lists). The
reader still reads a hand-written one as disabled.
