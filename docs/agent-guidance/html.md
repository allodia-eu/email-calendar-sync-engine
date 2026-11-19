# HTML Rendering Guidance

Authoritative for how a message's HTML becomes something a host can render. Read before touching
`engine-html`, or before a host renders a message body, a printed message, or links in text it
draws itself.

Mail HTML is hostile input: it tries to run script, exfiltrate through remote loads, track the open
with remote images and pixels, navigate the host away, and escape its frame. This page is the one
bar every host meets, native (a platform web view) and web (an iframe in a browser) alike.

## What it is

`engine-html` is pure: no I/O, no async, no state beyond the sanitiser's immutable allowlist.

```rust
pub fn sanitize(html: &str) -> Sanitized          // html(), into_html(), has_remote_images(), has_cid_references()
pub fn sanitize_inert(html: &str) -> String
pub fn sanitize_for_composer(html: &str, parts: &[InlinePart]) -> String
pub fn inline_cid_images(html: &str, parts: &[InlinePart]) -> String
pub fn restore_cid_images<'a>(html: &str, parts: &[&'a InlinePart]) -> (String, Vec<&'a InlinePart>)
pub fn image_media_type(media_type: &str) -> Option<String>
pub fn render_document(body_fragment: &str, remote_images: RemoteImages, scheme: MessageScheme) -> String
pub fn should_open_external_link(url: &str) -> bool
impl MessageScheme { pub fn canvas(self) -> Canvas }  // background(), foreground(), link()

pub mod print   { PrintHeaderLine::new(label, value), render_print_document(subject, lines, html, plain, remote_images) }
pub mod linkify { find_links(text) -> Vec<FoundLink>, segments(text) -> Vec<TextSegment> }
pub mod link    { LINK_SCHEMES, has_link_scheme, is_link_target, is_openable_link, openable_link, without_number_spaces }
```

**Why the engine hosts it.** The body fetch returns HTML **unsanitised** by design (`mime.md`), and
that stays true: `engine-mime` interprets bytes and decides no rendering policy. A sanitiser per
host would be one bar per host, and the same message could then be inert on one and dangerous on
another; so the allowlist, the CSP, the inline-image policy and the link-scheme policy are written
once, here, and every host calls them. `engine-api` does not wrap it: a host depends on
`engine-html` directly, beside the facade.

## The layers

Defence in depth: each layer holds even if another is weakened. Layers 1 and 2 are this crate,
identical for every host; layer 3 is the host's renderer, and a host implements every one of its
gates.

### Layer 1: the sanitiser (`sanitize`)

- **Removed:** `<script>` and its content, event handlers (`on*`), `<iframe>`/`<object>`/`<embed>`/
  `<form>`, `<link>` (WebKit resolves a `dns-prefetch` host as the document parses, with no setting
  to stop it), `<base>`/`<meta>` (so a message cannot set its own refresh, CSP or viewport), any URL
  scheme other than `http(s)`/`mailto`/`tel`/`data`/`cid`, a `data:` link, and a `tel:` link that
  "Opening a link" below would not open (its words stay, as text).
- **Kept:** presentational HTML and CSS: inline `style`, `<style>` blocks, `class` and `id`
  (stripping CSS makes real mail illegible), and remote `<img>` sources (the load is gated by
  layer 2, not the markup). `id` is a selector hook exactly like `class`: a newsletter's mobile
  layout commonly hangs off it alone, so it is kept **unprefixed**, or a rule such as
  `table[id=templateContainer]{width:100% !important}` matches nothing and the message lays out
  600px wide in a phone's viewport. So is `<font>` with `color`, `face` and `size`, which
  Thunderbird still writes, and the markers a composer reads a reopened draft's quote and signature
  by: `type` on a `<blockquote>` and `name` on an `<a>`.
- **Linked:** a web or mail address written as text (`http://`, `https://`, `mailto:`, or a `www.`
  host, opened over `https`) is wrapped in an `<a href>`, on the sanitiser's own serialisation.
  Text already inside a link, attribute values and `<style>` are left alone. The finder is
  `linkify`, the one a host uses for text it draws itself, so an address is a link on every
  surface or on none. Bare domains and bare email addresses are not linked (too much prose matches
  them), and nor is a bare number as `tel:`: dialling what turns out to be something else is a
  call the reader did not ask for. A sender who wants a number dialled marks it up as a link.
- **Reported:** `has_remote_images`, whether the body *would* load a remote resource (a remote
  `<img>`, a CSS `url(...)` or `@import`, a protocol-relative `//…`, inline or in `<style>`), so a
  host offers "load remote images" only when there is something to load. A link loads nothing and
  never sets it. `has_cid_references`, whether the body names an inline `cid:` picture, so a host
  fetches the inline parts (`Engine::message_inline_parts`) only when there is one.
- **Resolved:** `inline_cid_images` rewrites an `<img src="cid:…">` to a self-contained `data:` URI
  from the matching part, so it renders under layer 2's `img-src data:` with no network load and
  no opt-in: an inline picture is part of the message, not remote content. Two hard limits keep it
  safe: only an `image/*` part whose subtype is RFC 6838 restricted-name characters is inlined
  (`image_media_type`; the rewrite can never emit `data:text/html` or inject a parameter), and only
  an `<img src>` is rewritten, the one place the sanitiser keeps `cid:`. An unresolved or non-image
  `cid:` stays an inert broken image.

### Layer 2: the strict-CSP document (`render_document`)

The sanitised fragment is wrapped in a complete document with a strict Content-Security-Policy
and base styling, so the security boundary and the presentation are identical on every host:

- `default-src 'none'`: nothing loads unless allowed below. `base-uri 'none'` and
  `form-action 'none'` are set explicitly, because neither falls back to `default-src`.
- `img-src data:` under `RemoteImages::Blocked`, which every message opens with: only inline
  images render, so a message cannot phone home or track the open. `RemoteImages::Allowed` adds
  `https: http:`, on the reader's choice for that message; the host re-renders. Plain `http` is in
  because senders still write it, mostly in signatures. While blocked, every image that is not a
  `data:` one is `visibility:hidden`: its box keeps the layout, and no broken-image icon or alt
  text suggests the message is at fault.
- `style-src 'unsafe-inline'` for the message's own CSS, `font-src data:`, and no `script-src`.
- The base sheet's one rule a message cannot override is `img{max-width:100%;height:auto!important}`:
  without the `!important` an image whose height the message pins keeps it while its width shrinks,
  and is squashed.
- A dark render moves colours **after** sanitisation and writes only colour values and
  `prefers-color-scheme` features: never an element, an attribute or a URL, so it cannot reopen
  anything layer 1 closed (`a_dark_render_adds_no_markup`).

⚠️ **The directives are one policy, separated by semicolons.** A `content` attribute holds a policy
*list*, and a comma starts a second policy enforced alongside the first, so the document gets the
intersection. A comma in place of a semicolon cuts `default-src 'none'` off from the `style-src` and
`img-src` that soften it, both fall back to `'none'`, and every message renders with no CSS at all
and not even a `data:` image. It reads as a rendering bug, and an assertion that each directive is
*present* passes throughout, which is why `the_document_carries_one_policy_rather_than_two`
asserts on the separators.

### Layer 3: the host's renderer

The CSP is the primary boundary; these gates are mandatory defence in depth, so one weakness (a
future CSP regression) cannot leak data. The mechanism differs per host; the outcome may not.

1. **Scripting off for the page.** Nothing the message carries ever runs. A native host turns
   the web view's script off; a web host grants the iframe's `sandbox` no `allow-scripts`.
2. **No bridge into the host.** No script channel, no injected host objects, and no inspector or
   remote-debugging channel in a build a user installs: a debugging port hands every page, a
   message being read included, to whatever connects.
3. **No navigation inside the renderer.** Redirects, form posts and link taps never load in place;
   the document is inert. The one exception is a user-activated link tap, handed off under gate 4a.
4. **No new windows or popups** (a web host: no `allow-popups`, no `allow-top-navigation*`).
   - **4a. External link handoff:** see "Opening a link".
5. **No file or content access** from the document.
6. **An opaque origin.** A native host loads the document from a string with no base URL; a web
   host serves it in a sandboxed iframe without `allow-same-origin`, from an origin of its own
   (a separate user-content origin), so even a mistake in the sandbox reaches none of the app's
   cookies or storage.
7. **Remote sub-resources blocked by default**, until the reader opts in for that message (the
   choice resets per message). Prefer a second barrier beside the CSP: a native request
   interceptor, or for a web host the same policy sent as a response header by the frame's origin.

**Text a host draws itself** (a plain-text body, an event's notes, an invitation's summary and
description) bypasses all three layers, so it has its own gate: it is passed to a **text**
primitive, never to one that parses markup, markdown or a format string. Its links come from
`linkify::segments`: each run is appended as text with its target attached as a link attribute;
the host never looks for an address itself, and a tap opens only through gate 4a.

## Opening a link

`should_open_external_link` is the one decision, so every host decides identically: `true` only
for `link::LINK_SCHEMES` (`http`, `https`, `mailto`, `tel`), the same set the sanitiser keeps on an
`<a href>` (a link the sanitiser strips can never be clicked, so the two must agree). `data:`,
`cid:`, `file:`, `javascript:` and custom application schemes never open.

A `tel:` link opens only when it is a number RFC 3966 can write and holds no `*` or `#`, written out
or percent-encoded anywhere in the URI. The RFC allows both and leaves them undefined; in practice
they are service codes, which can forward the reader's calls elsewhere, and Apple's Phone app
refuses to dial them for that reason. The spaces a sender's number often carries, which the RFC
forbids, are ignored rather than refused (a renderer hands them over as `%20`); `openable_link`
gives the URI without them, since some platforms will not parse one with them.

The host opens the URL only on **user activation**, so a redirect or a programmatic navigation can
never launch one, and only through the platform's own launcher. A composer sends only what this
opens (`link::is_link_target`), so a host never sends a link its own reader would refuse.

## The composer's raw-HTML regions

A composer carries two blocks of HTML it emits verbatim rather than building from nodes: a quoted
original and a signature. Both are hostile input the editor round-trips, so a host re-sanitises
both on every save and send with `sanitize_inert`: the reading allowlist without its linkifying.
Linkifying adds rather than removes, and adding links on every save would make a region the user
never touched differ from the copy it replaces; without it, sanitising is idempotent, and a draft
saved and reopened twice holds the same markup both times.

A reopened draft's HTML reaches the editor through `sanitize_for_composer`, which inlines its
pictures as `data:` URIs and notes each one's `Content-ID` beside it as `data-cid`, so the next
save names the picture as the last did. On send, `restore_cid_images` turns each quoted `data:`
picture back into a `cid:` reference to its original part (Outlook's reader blocks `data:`
images; `multipart/related` is the interoperable form), and reports the parts to re-attach.

## Reading in a dark theme

`MessageScheme` is the page the reader sees the message on. The host decides it, from its own
resolved appearance and the reader's preference; never the message (a sender's
`<meta name="color-scheme" content="light only">` is not honoured, as Outlook and Thunderbird do
not). Because it is known before the body arrives, `MessageScheme::canvas` is what the host paints
behind the body area from the moment a message opens, the same constant the document paints, so
an open never flickers white, black, white. The dark canvas is `#1e1e1e` under `#e3e3e3` ink;
every canvas colour is `#rrggbb`.

A dark render moves each colour the message sets, in its `<style>` sheets, its `style` attributes,
`bgcolor` and `<font color>`, keeping its hue and changing only its perceived lightness (OKLab), by
what the colour paints:

| Role | Properties | Moves to |
|---|---|---|
| Fill | `background*`, `bgcolor` | A light fill becomes dark; white becomes the canvas exactly. A dark fill stays. |
| Ink | `color`, `<font color>`, `-webkit-text-fill-color`, `caret-color` | Dark ink becomes light; black becomes the canvas's ink. Light ink stays, raised to a floor that reads on the canvas. |
| Line | `border*`, `outline*`, `text-decoration*`, `column-rule*` | Lightness swaps end for end, so a pale divider stays subtle and a black frame stays visible. |

It never moves **images**; a declaration block whose background is an image (`url()`,
`image-set()`), whose colours were chosen for the picture; a value it cannot read (`var()`,
`currentColor`, a malformed colour); or the rules inside a sender's own
`@media (prefers-color-scheme: dark)` block. A fill or ink already right for a dark page is left
alone, so a sender's own dark design passes through. Every pair the sender made legible on white
(4.5:1) stays legible (`a_pair_that_read_on_white_still_reads_on_the_dark_page`).

In **both** schemes every `prefers-color-scheme` feature is replaced with one that is always or
never true for the scheme drawn, because each renderer decides that feature from its host, and
left alone a light render on a dark system would apply the sender's dark rules to a light page.
The document's own `color-scheme` names the scheme, so scrollbars and controls match the page.

**Why this transform.** A renderer's own darkening is one platform's feature and absent on the
others. Inverting with a CSS filter (Thunderbird's first attempt) turns photographs into negatives
and makes a transparent dark logo vanish. Stripping the sender's colours (Thunderbird now) loses
every designed background. Moving lightness and keeping hue is what Outlook and Apple Mail do. No
transform reads every message correctly (a dark logo on a transparent background, text baked into
a picture), so a host should keep the light page one press away.

## Sizing a message

The width a message lays out against decides which of its own `@media` rules apply, so it decides
what the message *is*; two hosts that disagree show two different newsletters from the same bytes.

1. **A message lays out against the pane's own width.** The document declares
   `<meta name="viewport" content="width=device-width">`, and `device-width` is the renderer's CSS
   width, not the display's. A host must let that declaration reach its renderer (Android's
   `WebView` ignores it unless `useWideViewPort` is on).
2. **The declaration names a width and no scale.** An `initial-scale` pins the page at 1:1, the
   condition under which Blink declines to shrink an over-wide message; `user-scalable` and
   `maximum-scale` would take the reader's pinch away.
3. **A message written for a wider pane is reflowed to fit, not clipped.** Half of real mail has
   no `@media` rules: a table pinned to 600px in the markup and again inline. The reflow sheet is
   built per message, from its own widths, and is CSS, so it needs no script and no measurement
   (the mail clients' "munger", from AOSP Email through K-9 and Thunderbird for Android, does the
   same job as script inside the message, which gate 1 forbids). `max-width:100%` is always on,
   because it can only shrink. Clearing the fixed widths on a table and its cells is behind a media
   query whose breakpoint is the widest fixed width the message asks for **plus the document's
   inset on both edges**, so a three-column newsletter keeps its columns in a pane that holds it.
   - ⚠️ Without the inset a 600px message in a 612px pane has 584px to lay out in, and a breakpoint
     at 600 leaves a 28px band where nothing fits and nothing fires; a 13-inch iPad's reading pane
     lands in it.
   - ⚠️ Only a width attribute that is **not** a percentage is cleared. The full-bleed band,
     `<table width="100%" bgcolor>` around a column, is in half the newsletters ever sent; clear it
     and its colour stops reaching the edges.
4. **The reader can zoom** with the platform's own gesture; a zoom control is the host's, never a
   page element or an injected script.
5. **A zoom belongs to the message it was made on.** Opening another message starts again; the
   same message re-rendered in another scheme keeps its place and its zoom.
6. **A message cannot change any of this**, because layer 1 drops its `<meta>`, which is what makes
   it safe for a host to honour a viewport declaration at all.

What the reflow cannot narrow (a cell holding a 300px word) still overflows; Blink can scale that
remainder down, other engines scroll. Whether a real message reflows is a question about a browser
engine, so the Rust suite holds what the sheet *says*, and each host checks the rendering by eye on
both sides of the breakpoint.

## Printing a message

`print::render_print_document` lays a message out for a print dialog: its header as labelled,
escaped text above the body (the labels are the host's, already localised), wrapped by the same
document, so a print carries the reading CSP and the reader's remote-image choice unchanged, and a
print is never the way a tracking pixel fires. It is always light: paper is white and the message
was written for it. The subject is the page's `<title>`, which WebView2 and WebKitGTK name the print
job after (without one, after the URL: the whole message as base64).

## Known gaps

- **The image height pin is overridden globally.** `height:auto!important` also overrides a message
  that pins **only** the height (a signature's `<img style="height:32px">` with auto width), which
  then renders at its intrinsic height. A narrower rule would fix it, but this CSS is load-bearing
  for the corporate-logo aspect-ratio case: a change lands with tests across all four pin
  combinations (width only, height only, both, neither).
- **A fixed width written only in an inline style is left to `max-width`.** The reflow clears
  widths by attribute (`table[width]`), which is what tells a relative width from a fixed one
  without parsing CSS text; a `<table style="width:600px">` with no attribute can still overflow.
  Mail writes table widths as the attribute because Outlook honours nothing else.
- **The print page's class names** (`mailcal-print-*`) still carry the prefix of the host this code
  was written in, kept so moving it here changed no output. They are internal to the page (its own
  stylesheet is the only thing that selects on them), so renaming them is a free cleanup for a
  change that touches the print page anyway.
