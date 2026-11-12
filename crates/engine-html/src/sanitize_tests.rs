//! What the sanitiser does to a message's `text/html`: what it strips, what it keeps because
//! stripping it makes real mail illegible, and what it reports. Layer 1 of `html.md`.
//!
//! How it resolves an inline `cid:` image is `crate::cid`'s tests; the document it is then wrapped
//! in, its CSP, stylesheet and viewport, is `crate::document`'s.

use super::sanitize;

#[test]
fn strips_script_tags_and_their_content() {
    let out = sanitize("<p>hi</p><script>steal(document.cookie)</script>").html;
    assert!(out.contains("hi"));
    assert!(!out.contains("<script"));
    assert!(!out.contains("steal"));
}

#[test]
fn strips_inline_event_handlers() {
    let out = sanitize(r#"<a href="https://ok.example" onclick="evil()">x</a>"#).html;
    assert!(!out.contains("onclick"));
    assert!(!out.contains("evil"));
    assert!(out.contains("https://ok.example"));
}

/// WebKit resolves a `<link rel=dns-prefetch>` host as the document parses, and since 2.48 no
/// setting turns that off: stripping the element is the only thing keeping a message from
/// announcing that it was opened to the sender's resolver.
#[test]
fn strips_link_elements_so_no_host_is_prefetched() {
    let out = sanitize(r#"<link rel="dns-prefetch" href="//tracker.example"><p>hi</p>"#).html;
    assert!(out.contains("hi"));
    assert!(!out.contains("<link"));
    assert!(!out.contains("tracker.example"));
}

#[test]
fn preserves_inline_css_so_mail_is_legible() {
    // The reported bug: styling was stripped, so blue text rendered black. Inline
    // colours/fonts must survive (the document and its renderer, not stripping, are the boundary).
    let out = sanitize(r#"<p style="color:#0000ff;font-weight:bold">blue</p>"#).html;
    assert!(out.contains("blue"));
    assert!(out.contains("style="));
    assert!(out.contains("#0000ff") || out.contains("color"));
}

#[test]
fn preserves_style_blocks_and_classes() {
    // Marketing mail puts media queries / class rules in <style>; child combinators and
    // @media must survive un-escaped, or the CSS breaks.
    let out = sanitize(
        "<style>@media (max-width:600px){.x{color:blue}} .a>.b{margin:0}</style>\
             <p class=\"x\">y</p>",
    )
    .html;
    assert!(out.contains("<style"));
    assert!(out.contains("@media"));
    assert!(out.contains(".a>.b") || out.contains(".a >.b") || out.contains(".a > .b"));
    assert!(out.contains("class=\"x\""));
}

#[test]
fn keeps_remote_images_but_flags_them() {
    // Remote images are kept (the host's CSP gates the actual load); the flag tells the
    // host to offer the "load remote images" prompt.
    let result = sanitize(r#"<img src="https://tracker.example/p.gif?id=42">"#);
    assert!(result.html.contains("tracker.example"));
    assert!(result.has_remote_images);
    // A CSS background image is flagged too.
    assert!(
        sanitize(r#"<div style="background:url(https://x.example/a.png)">z</div>"#)
            .has_remote_images
    );
}

#[test]
fn flags_remote_css_variants_a_flat_scan_misses() {
    // url() with whitespace after the paren, @import string form, protocol-relative src,
    // and a remote ref inside a <style> block; all must set has_remote_images.
    assert!(
        sanitize(r#"<div style="background:url( https://x.example/a.png )">z</div>"#)
            .has_remote_images
    );
    assert!(
        sanitize(
            "<style>@media all{body{background:url('https://x.example/a.png')}}</style><p>x</p>"
        )
        .has_remote_images
    );
    assert!(
        sanitize("<style>@import \"https://x.example/x.css\";</style><p>x</p>").has_remote_images
    );
    assert!(sanitize(r#"<img src="//x.example/a.png">"#).has_remote_images);
    // A plain remote link is still not a load.
    assert!(!sanitize(r#"<a href="https://x.example">l</a>"#).has_remote_images);
}

#[test]
fn inline_data_images_and_links_are_not_flagged_as_remote() {
    let result = sanitize(
        r#"<img src="data:image/png;base64,iVBORw0KGgo="><a href="https://x.example">l</a>"#,
    );
    assert!(result.html.contains("data:image/png;base64"));
    // A plain link is not a remote *load*, and a data: image is local: no prompt.
    assert!(!result.has_remote_images);
}

#[test]
fn preserves_presentational_table_attributes() {
    // Responsive mail relies on `border="0"` keeping border-width 0 so a mobile
    // `border-style: solid !important` rule stays invisible; dropping it paints a box.
    let out = sanitize(
            r#"<table border="0" cellspacing="0" cellpadding="0" width="600"><tr><td>x</td></tr></table>"#,
        )
        .html;
    assert!(out.contains(r#"border="0""#));
    assert!(out.contains(r#"cellspacing="0""#));
    assert!(out.contains(r#"cellpadding="0""#));
}

/// A list that carries on from an earlier one is numbered from where it left off, in the reading
/// view and in a draft reopened in the composer, which reads the number back into the document.
#[test]
fn preserves_an_ordered_lists_first_number() {
    let out = sanitize(r#"<ol start="3"><li>three</li></ol>"#).html;
    assert!(out.contains(r#"<ol start="3">"#), "{out}");
}

/// Thunderbird still writes colours, sizes and typefaces as `<font>`; dropped, its text arrives in
/// the body's colour and size, in the reading view and in a reopened draft alike.
#[test]
fn preserves_the_font_element_thunderbird_writes() {
    let out = sanitize(
        r##"<font color="#ff0000" size="5" face="Comic Sans MS" onclick="x()">red</font>"##,
    )
    .html;
    assert!(out.contains(r##"color="#ff0000""##), "{out}");
    assert!(out.contains(r#"size="5""#), "{out}");
    assert!(out.contains(r#"face="Comic Sans MS""#), "{out}");
    assert!(!out.contains("onclick"), "{out}");
}

/// The markers a composer reads a reopened draft's quote and signature by.
#[test]
fn preserves_the_quote_and_signature_markers_other_clients_write() {
    let out = sanitize(
        r#"<blockquote type="cite" cite="mid:x@y">quoted</blockquote><a name="_MailAutoSig">Ada</a>"#,
    )
    .html;
    assert!(out.contains(r#"type="cite""#), "{out}");
    assert!(out.contains(r#"name="_MailAutoSig""#), "{out}");
}

#[test]
fn preserves_the_id_a_responsive_media_query_selects_on() {
    // A newsletter's whole mobile layout hangs off `id`: the container table is a fixed 600px
    // and only the `@media` rule below narrows it. Strip the attribute and the rule matches
    // nothing, so the message lays out 600px wide inside a phone's ~384px viewport and every
    // line of text runs off the right edge.
    let out = sanitize(
        "<style>@media only screen and (max-width:480px){\
             table[id=templateContainer]{width:100% !important}}</style>\
         <table id=\"templateContainer\" width=\"600\"><tr><td id=\"bodyCell\">x</td></tr></table>",
    )
    .html;
    assert!(out.contains(r#"id="templateContainer""#));
    assert!(out.contains(r#"id="bodyCell""#));
}

#[test]
fn drops_the_document_title_text() {
    // The title (often a subject copy) must not leak in as body text, but a <style>
    // sibling in <head> must still survive.
    let out = sanitize(
        "<head><title>Subject leak</title><style>.x{color:red}</style></head>\
             <body><p>visible</p></body>",
    )
    .html;
    assert!(!out.contains("Subject leak"));
    assert!(out.contains("visible"));
    assert!(out.contains(".x{color:red}"));
}

#[test]
fn strips_iframes_and_objects() {
    let out = sanitize(r#"<iframe src="https://x.example"></iframe><b>kept</b>"#).html;
    assert!(out.contains("kept"));
    assert!(!out.contains("iframe"));
}

#[test]
fn blocks_data_uri_links() {
    let out = sanitize(r#"<a href="data:text/html,<script>alert(1)</script>">x</a>"#).html;
    assert!(!out.contains("data:text/html"));
}

#[test]
fn strips_custom_scheme_links_so_they_match_the_launch_policy() {
    // A custom app-scheme href is dropped by the sanitiser (it isn't in the launchable
    // allowlist), so it can't even be clicked; keeping the sanitiser and the launch
    // policy (`should_open_external_link`) in agreement on the same scheme set.
    let out = sanitize(r#"<a href="myapp://home">open</a>"#).html;
    assert!(out.contains("open")); // link text survives
    assert!(!out.contains("myapp://")); // but the unlaunchable href is gone
}

#[test]
fn keeps_a_telephone_link_a_sender_marked_up() {
    let out = sanitize(r#"<a href="tel:+15032223333">call us</a>"#).html;
    assert!(out.contains(r#"href="tel:+15032223333""#), "{out}");
    assert!(out.contains("call us"));
}

#[test]
fn keeps_a_telephone_link_written_with_spaces_and_strips_a_service_code() {
    let out = sanitize(r#"<a href="tel:+1 503 222 3333">call us</a>"#).html;
    assert!(out.contains(r#"href="tel:+1 503 222 3333""#), "{out}");
    let out = sanitize(r#"<a href="tel:*21*+31612345678%23">claim your prize</a>"#).html;
    assert!(!out.contains("href"), "{out}");
    assert!(out.contains("claim your prize"));
}

use super::should_open_external_link;

#[test]
fn opens_only_safe_link_schemes() {
    assert!(should_open_external_link("https://example.com/path?q=1"));
    assert!(should_open_external_link("http://example.com"));
    assert!(should_open_external_link("mailto:a@b.example"));
    assert!(should_open_external_link("tel:+15032223333"));
    // The scheme is matched case-insensitively.
    assert!(should_open_external_link("HTTPS://Example.COM"));
    assert!(should_open_external_link("TEL:+15032223333"));
    // A web view hands over a sender's spaces as `%20`.
    assert!(should_open_external_link("tel:+1%20503%20222%203333"));
}

#[test]
fn does_not_open_unsafe_relative_or_custom_schemes() {
    // Mail is hostile input: custom app deep-links, executable/document schemes, other
    // schemes outside the strict allowlist, and non-`scheme:` inputs never open. See
    // `html.md`, "Opening a link".
    for url in [
        "myapp://home",
        "javascript:alert(1)",
        "data:text/html,<script>",
        "file:///etc/passwd",
        "sms:+15551234567",
        "tel:*21*+31612345678%23",
        "tel:112",
        "callto:+15551234567",
        "//example.com/proto-relative",
        "/relative/path",
        "notaurl",
        ":no-scheme",
        "",
    ] {
        assert!(!should_open_external_link(url), "should not open: {url:?}");
    }
}

#[test]
fn does_not_panic_on_non_ascii_link_href() {
    // Regression: is_data_uri must not byte-slice across a UTF-8 char boundary: a link
    // href with a multi-byte char around byte 5 used to panic the whole sanitise pass.
    assert!(
        sanitize(r#"<a href="abcdé fghij">x</a>"#)
            .html
            .contains('x')
    );
    // A char straddling byte 5 (the historical crash) and a short multi-byte value.
    let _ = sanitize("<a href=\"aaa😀bbb\">y</a>");
    let _ = sanitize("<a href=\"é\">z</a>");
}

#[test]
fn empty_or_textonly_input_is_safe() {
    assert_eq!(sanitize("").html, "");
    assert_eq!(sanitize("just text").html, "just text");
    assert!(!sanitize("just text").has_remote_images);
}

use engine_core::mail::InlinePart;

use super::{sanitize_for_composer, sanitize_inert};

#[test]
fn the_inert_allowlist_strips_as_the_reading_one_does_and_links_nothing() {
    let raw = r#"<p onclick="x()">see https://example.com</p><script>steal()</script>"#;
    let out = sanitize_inert(raw);
    assert_eq!(out, "<p>see https://example.com</p>");
    // The reading pass is the same allowlist plus the links.
    assert_eq!(
        sanitize(raw).html(),
        "<p>see <a href=\"https://example.com\" rel=\"noopener noreferrer\">https://example.com</a></p>"
    );
    // A region re-sanitised on every save comes out as it went in.
    assert_eq!(sanitize_inert(&out), out);
}

#[test]
fn a_reopened_draft_keeps_each_pictures_content_id_beside_it() {
    let parts = [InlinePart::new(
        "logo@example.test",
        "image/png",
        b"hi".to_vec(),
    )];
    let raw =
        r#"<p>see https://example.com</p><img src="cid:logo@example.test"><script>x()</script>"#;
    let out = sanitize_for_composer(raw, &parts);
    assert_eq!(
        out,
        r#"<p>see https://example.com</p><img src="data:image/png;base64,aGk=" data-cid="logo@example.test">"#
    );
}
