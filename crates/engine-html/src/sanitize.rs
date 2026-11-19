//! The sanitiser: a message's raw `text/html` stripped to an inert subset.
//!
//! What is removed ([`ammonia`]'s allowlist): scripts and their content, event handlers
//! (`on*`), `<iframe>`/`<object>`/`<embed>`/`<form>`, `<base>`/`<meta>` (so a message can't
//! set its own refresh/CSP), and any URL scheme other than http(s)/mailto/tel/data/cid.
//!
//! What is **kept**: presentational HTML and CSS; inline `style`, `<style>` blocks, and
//! `class`; because stripping CSS makes real mail illegible (no colours, fonts, or
//! layout), and remote `<img>` sources. CSS and remote loads are made safe not by stripping
//! but by the document and the host renderer around it, which are the security boundary: the
//! document's strict Content-Security-Policy ([`render_document`](crate::render_document)) blocks
//! script and every remote load **by default** (so remote images don't phone home / track opens
//! until the user explicitly loads them; `north-star.md`), and the host renders it with scripting
//! off and all navigation blocked, in a native web view or, for a web host, a sandboxed iframe on
//! an origin of its own. [`sanitize`] reports [`Sanitized::has_remote_images`] so a host can offer
//! that "load remote images" confirmation only when there is something to load.

use std::{collections::HashSet, sync::OnceLock};

use ammonia::Builder;
use engine_core::mail::InlinePart;

use crate::{cid, link, links};

/// The sanitised body, whether it references remote resources (remote `<img>` or a CSS
/// `url(...)`), so a host can gate loading behind a "load remote images" confirmation, and
/// whether it references inline `cid:` resources that may need resolving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sanitized {
    /// The inert, safe-to-render HTML (a body fragment).
    html: String,
    /// Whether the body would load a remote resource if the document's CSP allowed it.
    has_remote_images: bool,
    /// Whether the body references an inline `cid:` resource.
    has_cid_references: bool,
}

impl Sanitized {
    /// The inert, safe-to-render HTML (a body fragment).
    #[must_use]
    pub fn html(&self) -> &str {
        &self.html
    }

    /// The inert, safe-to-render HTML, owned.
    #[must_use]
    pub fn into_html(self) -> String {
        self.html
    }

    /// Whether the body would load a remote resource if the document's CSP allowed it; the
    /// signal to offer the "load remote images" prompt.
    #[must_use]
    pub fn has_remote_images(&self) -> bool {
        self.has_remote_images
    }

    /// Whether the body references an inline `cid:` resource: the signal for the reading path
    /// to fetch the message's inline parts and resolve them
    /// ([`inline_cid_images`](crate::inline_cid_images)), so an open with no inline images
    /// never pays for the parts fetch.
    #[must_use]
    pub fn has_cid_references(&self) -> bool {
        self.has_cid_references
    }
}

/// URL schemes a clicked link in a rendered message may be handed to the OS to open
/// ([`should_open_external_link`]), and the set the sanitiser keeps on `<a href>` (a link the
/// sanitiser strips can never be clicked, so the two must agree). The list itself is
/// [`link::LINK_SCHEMES`], which also bounds what a composer sends, so every host, every
/// direction and the linkifier ([`links`]) share one allowlist.
const EXTERNAL_LINK_SCHEMES: [&str; link::LINK_SCHEMES.len()] = link::LINK_SCHEMES;

/// Extra schemes the sanitiser keeps for **inline resources** referenced by `src` (not
/// links): `data:` for inline images and `cid:` for referenced message parts. These never
/// open externally: only [`EXTERNAL_LINK_SCHEMES`] do.
const INLINE_RESOURCE_SCHEMES: [&str; 2] = ["data", "cid"];

/// Sanitises a message's raw `text/html` into an inert subset safe to render in a
/// locked-down renderer, preserving presentational CSS.
///
/// An address written as text in the body becomes a link (found by [`linkify`](crate::linkify)),
/// so a message whose sender did not mark it up reads like one whose sender did.
#[must_use]
pub fn sanitize(html: &str) -> Sanitized {
    let cleaned = links::linkify_html(&sanitizer().clean(html).to_string());
    let has_remote_images = references_remote_resource(&cleaned);
    let has_cid_references = cid::references_cid(&cleaned);
    Sanitized {
        html: cleaned,
        has_remote_images,
        has_cid_references,
    }
}

/// Sanitises a stored message's `text/html` for a composer that reopens it, inlining the
/// pictures `parts` carries as `data:` URIs so the editor can show them.
///
/// The reading view's allowlist, without its linkifying: an address the user typed as text stays
/// text in their own draft rather than becoming a link they did not make. Each inlined picture
/// keeps the `Content-ID` it was addressed by, as `data-cid`, so the editor hands the same one back
/// and the next save names the picture as the last did.
#[must_use]
pub fn sanitize_for_composer(html: &str, parts: &[InlinePart]) -> String {
    cid::inline_images(&sanitize_inert(html), parts, true)
}

/// The reading view's allowlist alone, without linkifying: what a composer's raw-HTML regions, a
/// quoted original and a signature, are re-sanitised with on every save and send. A region the
/// user did not touch then goes out as it came in, and a draft saved and reopened twice holds the
/// same markup both times (`html.md`, "The composer's raw-HTML regions").
#[must_use]
pub fn sanitize_inert(html: &str) -> String {
    sanitizer().clean(html).to_string()
}

/// The shared, immutable [`Builder`]: its allowlists never change, so it is configured once
/// and reused for every message open (ammonia's `clean` takes `&self`), rather than
/// rebuilt per call.
pub(crate) fn sanitizer() -> &'static Builder<'static> {
    static SANITIZER: OnceLock<Builder<'static>> = OnceLock::new();
    SANITIZER.get_or_init(|| {
        // Only safe URL schemes survive, notably no `javascript:`/`vbscript:`. Links use the
        // launchable allowlist (http(s)/mailto/tel); `data:`/`cid:` are kept for inline `src`
        // resources (and `data:` *links* are blocked below: an executable document). Remote
        // http(s) image sources are kept but gated by the document's CSP. The link half of this
        // set is shared with the host's launch policy (see `EXTERNAL_LINK_SCHEMES`).
        let schemes: HashSet<&str> = EXTERNAL_LINK_SCHEMES
            .into_iter()
            .chain(INLINE_RESOURCE_SCHEMES)
            .collect();
        let mut builder = Builder::new();
        builder
            .url_schemes(schemes)
            // Keep presentational CSS (inline + blocks) and the attributes mail uses for
            // layout. The document and its renderer (not stripping) contain it (scripting off,
            // remote loads blocked).
            .add_tags(["style"])
            .rm_clean_content_tags(["style"])
            // Drop `<title>`'s text too, not just the tag; otherwise the document title
            // (often a copy of the subject) leaks in as a stray line of body text.
            .add_clean_content_tags(["title"])
            // Presentational attributes mail uses for table layout. `border`/`cellspacing`/
            // `cellpadding` matter for correctness, not just looks: responsive mail commonly
            // sets `border="0"` and then `border-style: solid !important` in a mobile @media
            // rule, relying on the attribute's `border-width: 0` to keep it invisible; drop
            // the attribute and that rule paints a spurious box. (No URL or script.)
            //
            // `id` is a selector hook exactly like `class`, and a newsletter's mobile layout
            // routinely hangs off it alone: the container table carries a fixed `width="600"`
            // and only `table[id=templateContainer]{width:100% !important}`, inside a
            // `max-width:480px` @media block, narrows it. Without the attribute that rule
            // matches nothing, so the message lays out 600px wide in a phone's ~384px viewport
            // and every line runs off the right edge. It must be kept **unprefixed**
            // (ammonia's `id_prefix`, which exists to stop untrusted ids colliding with a host
            // page's, would rename the hook out from under the message's own CSS). Nothing here
            // collides: the reading document holds one message and no script, and in a
            // composer a quoted original's ids sit inside the editable region, after the
            // editor's own, where `getElementById` never reaches them.
            // The presentational markup older editors write and Thunderbird's still does: a
            // `<font>` with a colour, a typeface and a size, none of which loads or runs anything.
            // A quote is marked `<blockquote type="cite">` by Thunderbird and Apple Mail, and Word
            // names the signature it inserted with an `<a name>` bookmark; a composer reads both
            // to tell a reopened draft's quote and signature from its text.
            .add_tags(["font"])
            .add_tag_attributes("font", ["color", "face", "size"])
            .add_tag_attributes("blockquote", ["type"])
            .add_tag_attributes("a", ["name"])
            .add_generic_attributes([
                "style",
                "class",
                "id",
                "align",
                "valign",
                "bgcolor",
                "border",
                "cellspacing",
                "cellpadding",
                "width",
                "height",
            ])
            .attribute_filter(|element, attribute, value| match (element, attribute) {
                // A `data:` link can carry an executable document (e.g. data:text/html);
                // links may only point at real http(s)/mailto/tel targets, and a `tel:` one only
                // at a number the open gate would dial.
                ("a", "href") if is_data_uri(value) || is_refused_number(value) => None,
                _ => Some(value.into()),
            });
        builder
    })
}

/// Whether a link the user clicked in a rendered message should be handed to the OS default
/// browser/handler. The host calls this for a **user-activated** link tap and, on `true`,
/// opens the URL with the platform's launcher (a native host's OS handler, a web host's new
/// browsing context); on `false` it ignores the tap. The policy, the `EXTERNAL_LINK_SCHEMES`
/// allowlist, lives here so every host decides identically and consistently with what the
/// sanitiser keeps (mail is hostile input; see `html.md`, "Opening a link").
#[must_use]
pub fn should_open_external_link(url: &str) -> bool {
    link::is_openable_link(url)
}

/// Whether `value` is a `tel:` link [`should_open_external_link`] refuses, such as a service code.
fn is_refused_number(value: &str) -> bool {
    value
        .trim_start()
        .get(..4)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("tel:"))
        && !should_open_external_link(value)
}

/// Whether `value` is an inline `data:` URI (scheme-only check, leading space tolerant).
///
/// Uses `get(..5)` rather than slicing `[..5]`: the value is hostile input, and a byte
/// slice would panic when byte 5 falls inside a multi-byte char (e.g. a link `href` with a
/// non-ASCII character), which would unwind through the whole sanitise pass.
fn is_data_uri(value: &str) -> bool {
    value
        .trim_start()
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
}

/// Whether the sanitised HTML references a remote resource that *loads*: a remote `<img>`
/// source, or a CSS `url(...)`/`@import` (inline **or** in a `<style>` block). Plain links
/// (`href`) are excluded: they don't auto-fetch, so they aren't a tracking vector. Scanned
/// on the serialized output so `<style>` blocks are covered (an attribute filter can't see
/// their text); ammonia normalises attribute quoting to `"`, so the `src="…"` checks hold.
fn references_remote_resource(html: &str) -> bool {
    let h = html.to_ascii_lowercase();
    h.contains("src=\"http")
        || h.contains("src=\"//")
        || css_marker_loads_remote(&h, "url(")
        || css_marker_loads_remote(&h, "@import")
}

/// Whether any `marker` in `h` is followed (after optional CSS whitespace and quotes) by a
/// remote URL (`http…` or a protocol-relative `//…`). Catches the variants a flat substring
/// match misses: `url( http`, `url('http')`, `@import "https://…"`. `h` must be lowercased.
fn css_marker_loads_remote(h: &str, marker: &str) -> bool {
    let mut rest = h;
    while let Some(i) = rest.find(marker) {
        let after = rest[i + marker.len()..].trim_start_matches([' ', '\t', '\n', '\r', '\'', '"']);
        if after.starts_with("http") || after.starts_with("//") {
            return true;
        }
        rest = &rest[i + marker.len()..];
    }
    false
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod tests;
