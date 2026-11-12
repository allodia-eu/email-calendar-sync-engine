//! `engine-html`: a message's HTML made safe to render, for every host.
//!
//! The engine returns a message's `text/html` part **unsanitised** (`mime.md`): mail HTML is
//! hostile input. Before any host renders it, it is stripped to a safe, inert subset here; one
//! hardened sanitiser shared by every host, not re-implemented per platform, so a malicious
//! message cannot run script, escape its frame, or navigate the host away.
//!
//! The crate is pure: no I/O, no async, no state beyond the sanitiser's immutable allowlist. In
//! the order a body goes through it:
//!
//! 1. [`sanitize`]: the allowlist, the addresses written as text turned into links, and the two
//!    signals a host gates on ([`Sanitized::has_remote_images`],
//!    [`Sanitized::has_cid_references`]).
//! 2. [`inline_cid_images`]: each inline `cid:` picture resolved to a self-contained `data:` URI.
//! 3. [`render_document`]: the fragment wrapped in a complete document whose strict
//!    Content-Security-Policy is what makes rendering the message's own CSS safe, drawn for the
//!    reader's [`MessageScheme`] and reflowed to fit a pane narrower than it was written for.
//!
//! Beside them: [`print`](mod@print), the page a printed message is drawn on; [`linkify`], the
//! address finder a host also uses for text it draws itself; and [`link`], the one scheme allowlist
//! a link in a message may carry, which [`should_open_external_link`] answers a click with.
//!
//! `docs/agent-guidance/html.md` is the contract: which layer holds which gate, and what a host
//! still has to do itself (scripting off, navigation blocked, an opaque origin, a second barrier
//! for remote loads).

mod cid;
mod document;
pub mod link;
pub mod linkify;
mod links;
pub mod print;
mod reflow;
mod sanitize;
mod scheme;

pub use cid::{image_media_type, inline_cid_images, restore_cid_images};
pub use document::{Canvas, RemoteImages, render_document};
pub use sanitize::{
    Sanitized, sanitize, sanitize_for_composer, sanitize_inert, should_open_external_link,
};
pub use scheme::MessageScheme;
