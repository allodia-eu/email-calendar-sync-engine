//! How an inline `cid:` image is resolved to a `data:` URI for reading and restored to a `cid:`
//! reference for sending.

use engine_core::mail::InlinePart;

use super::{base64_encode_into, inline_cid_images};
use crate::sanitize;

fn part(cid: &str, media: &str, bytes: &[u8]) -> InlinePart {
    InlinePart::new(cid, media, bytes.to_vec())
}

#[test]
fn sanitize_flags_inline_cid_references() {
    // A `cid:` image source sets the flag so the reading path fetches inline parts;
    // a body with none (and a plain remote image) does not.
    assert!(sanitize(r#"<img src="cid:logo@x">"#).has_cid_references());
    assert!(!sanitize(r#"<img src="https://x/a.png">"#).has_cid_references());
    assert!(!sanitize("<p>just text</p>").has_cid_references());
}

#[test]
fn inline_cid_resolves_to_a_data_uri_with_base64_bytes() {
    // `hello` base64-encodes to `aGVsbG8=`; the resolved `<img>` loads inline.
    let out = inline_cid_images(
        r#"<img src="cid:logo@x">"#,
        &[part("logo@x", "image/png", b"hello")],
    );
    assert!(
        out.contains("src=\"data:image/png;base64,aGVsbG8=\""),
        "{out}"
    );
    assert!(!out.contains("cid:"), "the cid reference is gone: {out}");
}

#[test]
fn inline_cid_leaves_unresolved_and_nonimage_refs_untouched() {
    let parts = [
        part("present@x", "image/gif", b"hello"),
        // A non-image inline part must never become a `data:` document.
        part("doc@x", "text/html", b"<script>evil()</script>"),
    ];
    // An id with no matching part stays a (broken) cid reference, as before.
    let missing = inline_cid_images(r#"<img src="cid:absent@x">"#, &parts);
    assert!(missing.contains(r#"src="cid:absent@x""#), "{missing}");
    // A matching but non-image part is left as cid: no data:text/html is emitted.
    let nonimage = inline_cid_images(r#"<img src="cid:doc@x">"#, &parts);
    assert!(nonimage.contains(r#"src="cid:doc@x""#), "{nonimage}");
    assert!(!nonimage.contains("data:text/html"), "{nonimage}");
    assert!(!nonimage.contains("data:"), "{nonimage}");
}

#[test]
fn inline_cid_resolves_every_reference() {
    let html = r#"<img src="cid:a"><p>x</p><img src="cid:b">"#;
    let out = inline_cid_images(
        html,
        &[
            part("a", "image/gif", b"hello"),
            part("b", "image/jpeg", b"world"),
        ],
    );
    assert!(out.contains("data:image/gif;base64,aGVsbG8="), "{out}");
    assert!(out.contains("data:image/jpeg;base64,d29ybGQ="), "{out}");
    assert!(!out.contains("cid:"), "{out}");
    assert!(
        out.contains("<p>x</p>"),
        "surrounding markup preserved: {out}"
    );
}

#[test]
fn inline_cid_allows_svg_but_rejects_injection_in_media_type() {
    // SVG via <img> is rendered non-scripted and the CSP has no script-src, so it is
    // allowed.
    let svg = inline_cid_images(
        r#"<img src="cid:s">"#,
        &[part("s", "image/svg+xml", b"<svg/>")],
    );
    assert!(svg.contains("data:image/svg+xml;base64,"), "{svg}");
    // A media type smuggling extra `data:` parameters / an attribute break is rejected
    // (not image/* after the safe-char check), leaving the ref inert.
    let evil = inline_cid_images(
        r#"<img src="cid:s">"#,
        &[part("s", "image/png\";onerror=alert(1)//", b"x")],
    );
    assert!(evil.contains(r#"src="cid:s""#), "{evil}");
    assert!(!evil.contains("onerror"), "{evil}");
}

#[test]
fn inline_cid_is_a_noop_without_parts_or_references() {
    assert_eq!(
        inline_cid_images("<p>no images</p>", &[]),
        "<p>no images</p>"
    );
    let parts = [part("x", "image/png", b"hello")];
    // Parts present but the body references none of them: unchanged.
    assert_eq!(
        inline_cid_images("<p>no images</p>", &parts),
        "<p>no images</p>"
    );
}

#[test]
fn inline_cid_never_panics_on_hostile_input() {
    let parts = [part("é@x", "image/png", b"hello")];
    // Multi-byte chars around the cid value, and a `src="cid:` with no closing quote,
    // must not slice a char boundary or panic.
    let _ = inline_cid_images(r#"<img alt="café" src="cid:é@x">"#, &parts);
    let _ = inline_cid_images(r#"<img src="cid:unterminated"#, &parts);
    let _ = inline_cid_images("src=\"cid:😀\"", &parts);
}

use super::restore_cid_images;

#[test]
fn restore_cid_round_trips_inline_resolution_preserving_the_original_id() {
    // cid -> data (reading) -> cid (send) returns the exact original markup, and the part
    // is reported so the caller can re-attach it with its original Content-ID.
    let parts = [part("logo@example.test", "image/png", b"hello")];
    let resolved = inline_cid_images(r#"<img src="cid:logo@example.test">"#, &parts);
    assert!(
        resolved.contains("data:image/png;base64,aGVsbG8="),
        "{resolved}"
    );

    let part_refs: Vec<&InlinePart> = parts.iter().collect();
    let (restored, matched) = restore_cid_images(&resolved, &part_refs);
    assert_eq!(restored, r#"<img src="cid:logo@example.test">"#);
    assert!(!restored.contains("data:"), "{restored}");
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].content_id(), "logo@example.test");
}

#[test]
fn restore_cid_leaves_foreign_data_images_and_is_a_noop_without_parts() {
    let parts = [part("a@x", "image/png", b"hello")];
    let part_refs: Vec<&InlinePart> = parts.iter().collect();
    // A data: image whose bytes match no part (base64 of "world") stays a data: URI.
    let foreign = r#"<img src="data:image/png;base64,d29ybGQ=">"#;
    let (restored, matched) = restore_cid_images(foreign, &part_refs);
    assert_eq!(restored, foreign);
    assert_eq!(matched, [] as [&InlinePart; 0]);
    // No parts at all: unchanged.
    let (r2, m2) = restore_cid_images(foreign, &[]);
    assert_eq!(r2, foreign);
    assert_eq!(m2, [] as [&InlinePart; 0]);
}

#[test]
fn restore_cid_anchors_on_the_full_src_so_a_prefix_part_cannot_corrupt_another() {
    // Part A's bytes ("hi", base64 "aGk="; wait: padded) vs a longer part B whose base64
    // starts with A's. Use lengths %3==0 so A's base64 has no padding and is a literal
    // prefix of B's: A = "abc" (YWJj), B = "abcdef" (YWJjZGVm). Anchoring the match on the
    // closing quote keeps A's URI from matching inside B's `src`.
    let parts = [
        part("a@x", "image/png", b"abc"),
        part("b@x", "image/png", b"abcdef"),
    ];
    let part_refs: Vec<&InlinePart> = parts.iter().collect();
    let html = concat!(
        r#"<img src="data:image/png;base64,YWJj">"#,
        r#"<img src="data:image/png;base64,YWJjZGVm">"#,
    );
    let (restored, matched) = restore_cid_images(html, &part_refs);
    // Each image becomes a cid: reference to its OWN part; B's src is not corrupted.
    assert_eq!(
        restored, r#"<img src="cid:a@x"><img src="cid:b@x">"#,
        "{restored}"
    );
    assert_eq!(matched.len(), 2);
}

#[test]
fn base64_matches_rfc4648_vectors() {
    let enc = |s: &[u8]| {
        let mut out = String::new();
        base64_encode_into(s, &mut out);
        out
    };
    assert_eq!(enc(b""), "");
    assert_eq!(enc(b"f"), "Zg==");
    assert_eq!(enc(b"fo"), "Zm8=");
    assert_eq!(enc(b"foo"), "Zm9v");
    assert_eq!(enc(b"foob"), "Zm9vYg==");
    assert_eq!(enc(b"fooba"), "Zm9vYmE=");
    assert_eq!(enc(b"foobar"), "Zm9vYmFy");
}

#[test]
fn restore_cid_skips_a_part_that_was_never_inlined_and_escapes_the_id() {
    let parts = [
        part("doc@x", "text/html", b"hi"),
        part("<a>&\"b\"@x", "image/png", b"hi"),
    ];
    let part_refs: Vec<&InlinePart> = parts.iter().collect();
    let (restored, matched) =
        restore_cid_images(r#"<img src="data:image/png;base64,aGk=">"#, &part_refs);
    assert_eq!(restored, r#"<img src="cid:&lt;a&gt;&amp;&quot;b&quot;@x">"#);
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].content_id(), "<a>&\"b\"@x");
}
