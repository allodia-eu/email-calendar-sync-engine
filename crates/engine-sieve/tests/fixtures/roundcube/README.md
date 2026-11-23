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
