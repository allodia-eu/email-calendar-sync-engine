//! The document a sanitised fragment is wrapped in before a host renders it: the strict CSP that
//! makes the message's own CSS safe to run, the page it is drawn on, the base stylesheet, and the
//! per-message sheet that reflows a message written for a wider pane ([`super::reflow`]).
//!
//! Layer 2 of `html.md`, and its sizing rules 1 to 3. What the sanitiser does to the fragment
//! before any of this is next door, in [`crate::sanitize`].

use std::sync::OnceLock;

use super::{reflow, scheme, scheme::MessageScheme};

/// The canvas a message is drawn on in each scheme ([`MessageScheme::canvas`]).
const LIGHT_CANVAS: Canvas = Canvas {
    background: "#ffffff",
    foreground: "#1a1a1a",
    link: "#2864d6",
};

/// Where a dark render moves white and black to (`scheme::colour`), so a message's own white
/// page and the canvas around it are one colour.
const DARK_CANVAS: Canvas = Canvas {
    background: "#1e1e1e",
    foreground: "#e3e3e3",
    link: "#8ab4f8",
};

impl MessageScheme {
    /// The canvas a message is drawn on: the page's background, and the ink that stays legible
    /// on it. `#rrggbb`, the one hex form every host's colour parser reads.
    ///
    /// It is **chrome the host also has to draw**: the body area is this canvas from the moment a
    /// message is opened, while the body is still resolving, for a plain-text body, for a load
    /// error, so an open that resolves in tens of milliseconds changes the text on the page and
    /// never repaints the page itself. A host that left the gap transparent instead punched a hole
    /// in the canvas: on a dark theme the body area went white, black, white, which reads as a
    /// flicker rather than as a message opening. Which canvas depends only on the scheme the host
    /// renders in, which it knows before the body arrives (`html.md`, "Reading in a dark theme").
    ///
    /// Shared rather than restated per host for the same reason the CSP is: the host's half and
    /// the document's half cannot drift into two slightly different whites. The base stylesheet
    /// interpolates these constants, so there is only ever one of each.
    #[must_use]
    pub fn canvas(self) -> Canvas {
        match self {
            Self::Light => LIGHT_CANVAS,
            Self::Dark => DARK_CANVAS,
        }
    }
}

/// A background and the ink that stays legible on it, both `#rrggbb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Canvas {
    background: &'static str,
    foreground: &'static str,
    link: &'static str,
}

impl Canvas {
    /// The page's fill.
    #[must_use]
    pub fn background(self) -> &'static str {
        self.background
    }

    /// The text colour on that fill.
    #[must_use]
    pub fn foreground(self) -> &'static str {
        self.foreground
    }

    /// A link's colour on that fill, where the message gives it none.
    #[must_use]
    pub fn link(self) -> &'static str {
        self.link
    }
}

/// Whether the document may load the message's remote images, which is the reader's choice for
/// this message and nobody else's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemoteImages {
    /// Only inline `data:` images render; every remote load is blocked, so a message cannot phone
    /// home or track the open. Every message opens this way.
    #[default]
    Blocked,
    /// Remote `http(s)` images and CSS backgrounds load too: the reader asked for them.
    Allowed,
}

/// How far the document insets itself from the renderer's edges, on every side.
///
/// Shared with [`reflow`], whose breakpoint is the width at which a message stops fitting: what it
/// has to fit inside is the pane **less this, twice**. A second copy of the number would put that
/// breakpoint 28px out, leaving a band of pane widths where a message is clipped and nothing
/// reflows it.
pub(super) const BODY_PADDING: u32 = 14;

/// The base stylesheet for the reading document: a readable default that the message's own
/// CSS overrides. `color-scheme` names the scheme being drawn, so the renderer's own controls and
/// scrollbars match the page rather than the system, and no renderer darkens a page on its own.
///
/// `img{max-width:100%;height:auto!important}` is the **one** rule the message may not
/// override: `max-width:100%` shrinks a wide image to fit a narrow reading pane, and the
/// `!important` `height:auto` forces the height to follow that shrunk width so the aspect
/// ratio is preserved. Without `!important`, an image whose height is pinned by the
/// message's own CSS or inline style (corporate signatures routinely do; `<img>` with a
/// fixed `height`) keeps that height while only its width shrinks, squashing it
/// horizontally. Auto height is always proportional scaling, so this never distorts an
/// image; it only overrides an explicit height that the width constraint would otherwise
/// fight.
///
/// Built once per scheme rather than per render: it interpolates constants, so every message
/// would otherwise pay for the same string.
fn base_css(scheme: MessageScheme) -> &'static str {
    static LIGHT: OnceLock<String> = OnceLock::new();
    static DARK: OnceLock<String> = OnceLock::new();
    let (cell, name) = match scheme {
        MessageScheme::Light => (&LIGHT, "light"),
        MessageScheme::Dark => (&DARK, "dark"),
    };
    cell.get_or_init(|| {
        let Canvas {
            background,
            foreground,
            link,
        } = scheme.canvas();
        format!(
            ":root{{color-scheme:{name}}}\
             body{{margin:0;padding:{BODY_PADDING}px;background:{background};color:{foreground};\
             font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;\
             font-size:15px;line-height:1.5;overflow-wrap:break-word;\
             -webkit-text-size-adjust:100%}}\
             img{{max-width:100%;height:auto!important}}a[href]{{color:{link}}}"
        )
    })
}

/// What a blocked image leaves behind: its box, so the layout holds, and nothing in it. A renderer
/// otherwise draws a refused load as a broken-image icon beside the alt text, which reads as a
/// fault in the message rather than a choice the reader has not made yet. Every image not inline is
/// blocked under [`RemoteImages::Blocked`], so `data:` is the one source left showing; an
/// unresolved `cid:` is hidden as well, being as broken. A screen reader skips the image too, as a
/// sighted reader sees nothing there; the host's "load remote images" offer is what says they are
/// held back.
const BLOCKED_IMAGES_CSS: &str = "img:not([src^=\"data:\" i]){visibility:hidden!important}";

/// Wraps a sanitised body fragment in a complete HTML document; with a strict
/// Content-Security-Policy and base styling; ready for a host's renderer to load. Shared in
/// Rust so the security boundary (the CSP) and the base presentation are **identical across
/// every host**, a native web view and a web host's sandboxed iframe alike; a host only adds
/// what the document cannot say for itself (scripting off, navigation blocked, an opaque origin).
///
/// The CSP is the boundary that makes rendering the message's CSS safe: `default-src 'none'`
/// blocks scripts, frames, and every remote load; `style-src 'unsafe-inline'` allows the
/// message's presentational CSS. With [`RemoteImages::Blocked`] (the default) `img-src data:`
/// blocks every remote image: so a message cannot phone home or track the open until the user
/// explicitly loads images (`north-star.md`), and a blocked image is drawn as an empty box rather
/// than a broken one; with [`RemoteImages::Allowed`], remote http(s) images (and CSS backgrounds)
/// load. The host passes the user's per-message choice here and re-renders.
///
/// `scheme` is the page the reader is looking at it on: a dark render moves the message's own
/// colours to suit a dark page (`html.md`, "Reading in a dark theme").
///
/// The stylesheet is in two parts: the base sheet, the same for every message, and the reflow
/// sheet, built from **this** message's own widths, which is what makes a message written for
/// 600px readable in a narrower pane (`html.md`, sizing rule 3).
///
/// The viewport declares a width and **no scale** (`html.md`, "Sizing a message").
/// `width=device-width` is what puts the message's own `@media (max-width: …)` rules in front of
/// the pane the reader actually has. Naming an `initial-scale` as well would pin the page at 1:1,
/// which is the condition under which Blink declines to shrink an over-wide message to fit, so a
/// fixed-width newsletter would hang off the right edge of a phone. Leaving the scale free is also
/// what leaves the reader their pinch, so no `user-scalable` or `maximum-scale` belongs here
/// either. A message cannot override any of this: the sanitiser drops `<meta>`.
#[must_use]
pub fn render_document(
    body_fragment: &str,
    remote_images: RemoteImages,
    scheme: MessageScheme,
) -> String {
    render_titled_document("", body_fragment, remote_images, scheme)
}

/// [`render_document`] with a `<title>`, for a page a renderer names after its title: a print
/// job in WebView2 and WebKitGTK, which otherwise falls back to the document's URL.
/// `escaped_title` is already escaped text; an empty one writes no `<title>`.
pub(super) fn render_titled_document(
    escaped_title: &str,
    body_fragment: &str,
    remote_images: RemoteImages,
    scheme: MessageScheme,
) -> String {
    let title = if escaped_title.is_empty() {
        String::new()
    } else {
        format!("<title>{escaped_title}</title>")
    };
    let (img_src, blocked_css) = match remote_images {
        RemoteImages::Allowed => ("https: http: data:", ""),
        RemoteImages::Blocked => ("data:", BLOCKED_IMAGES_CSS),
    };
    let base_css = base_css(scheme);
    let body_fragment = scheme::adapt(body_fragment, scheme);
    // Built per message rather than once, because its breakpoint is this message's own width
    // (`reflow`). A message with nothing fixed in it produces no breakpoint and pays for a scan.
    let reflow_css = reflow::reflow_css(reflow::natural_width(&body_fragment));
    // `base-uri`/`form-action` do NOT fall back to `default-src`, so set them explicitly:
    // even if a `<base>` or `<form>` ever survived sanitisation, it can't rebase relative
    // URLs or POST data off-host.
    //
    // ⚠️ Every separator below is a SEMICOLON. A `content` attribute is a policy *list*, and a
    // comma starts a second policy that is enforced alongside the first, so the document gets
    // the intersection. One comma here splits `default-src 'none'` away from the `style-src`
    // and `img-src` that soften it, both fall back to `'none'`, and the message renders with no
    // CSS at all and not even a `data:` image.
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
         base-uri 'none'; form-action 'none'; \
         img-src {img_src}; style-src 'unsafe-inline'; font-src data:\">\
         <meta name=\"viewport\" content=\"width=device-width\">{title}\
         <style>{base_css}{blocked_css}{reflow_css}</style></head><body>{body_fragment}</body></html>"
    )
}

#[cfg(test)]
#[path = "document_tests.rs"]
mod document_tests;
