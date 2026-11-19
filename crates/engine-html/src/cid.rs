//! Inline `cid:` pictures: resolved to `data:` URIs for reading, and restored to `cid:`
//! references for sending.
//!
//! A message's inline parts come from the engine (`Engine::message_inline_parts`, decoded by
//! `engine-mime`); which of them may be inlined, and in what form, is decided here, because the
//! bytes are hostile input (`mime.md`: policy stays with the renderer).

use engine_core::mail::InlinePart;

/// Whether the sanitised HTML references an inline `cid:` image source; `src="cid:…"`, the
/// only place the sanitiser keeps a `cid:` scheme (an `<img>`'s `src`). The cheap signal the
/// reading path uses to decide whether to fetch the message's inline parts at all, so a
/// message with no inline images never pays for that fetch. Matched case-insensitively
/// because a host may author `CID:`/`SRC=`.
pub(crate) fn references_cid(html: &str) -> bool {
    html.to_ascii_lowercase().contains(CID_SRC_MARKER)
}

/// The `src="cid:` token (lowercased) that fronts every inline-image reference in ammonia's
/// serialized output (attributes are normalised to double quotes). `cid:` is kept by the
/// sanitiser only on an `<img src>`, so anchoring on `src="` confines the rewrite to image
/// sources and never touches a surviving `cid:` elsewhere.
const CID_SRC_MARKER: &str = "src=\"cid:";

/// Rewrites each resolvable inline `cid:` image reference in `html` to a self-contained
/// `data:` URI built from `parts`, so the reading document's CSP (`img-src data:`)
/// renders it without a network load and without the "load remote images" opt-in; inline
/// images are part of the message, not remote content.
///
/// Only an `<img src="cid:ID">` is rewritten (the sole place the sanitiser keeps `cid:`),
/// and only when `ID` resolves to an inline part whose media type is a safe `image/*`
/// (defensive: the rewrite must never emit a non-image `data:` document such as
/// `data:text/html`). An unresolved id, or a non-image part, is left untouched: an inert
/// broken image, exactly as before this resolution existed. Operates on byte indices found
/// at ASCII boundaries, so it never slices a multi-byte char in hostile input.
#[must_use]
pub fn inline_cid_images(html: &str, parts: &[InlinePart]) -> String {
    inline_images(html, parts, false)
}

/// [`inline_cid_images`], writing ` data-cid="ID"` after each rewritten `src` when `mark_cid`.
pub(crate) fn inline_images(html: &str, parts: &[InlinePart], mark_cid: bool) -> String {
    if parts.is_empty() {
        return html.to_owned();
    }
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0;
    while let Some(rel) = lower[cursor..].find(CID_SRC_MARKER) {
        let token_start = cursor + rel; // index of `s` in `src="cid:`
        let value_start = token_start + CID_SRC_MARKER.len(); // first char of the id
        // The id runs to the next double quote that closes the attribute; ammonia escapes
        // any literal `"` in a value to `&quot;`, so the first quote is the real terminator.
        let Some(quote_rel) = html[value_start..].find('"') else {
            break; // malformed (no closing quote): copy the remainder untouched below.
        };
        let value_end = value_start + quote_rel;
        let token_end = value_end + 1; // past the closing quote
        let cid = html[value_start..value_end].trim();

        out.push_str(&html[cursor..token_start]);
        match resolve_image(parts, cid) {
            Some((media_type, bytes)) => {
                out.push_str("src=\"data:");
                out.push_str(&media_type);
                out.push_str(";base64,");
                base64_encode_into(bytes, &mut out);
                out.push('"');
                if mark_cid {
                    out.push_str(" data-cid=\"");
                    escape_attr_value(cid, &mut out);
                    out.push('"');
                }
            }
            // Unresolved or non-image: leave the original `src="cid:ID"` token as-is.
            None => out.push_str(&html[token_start..token_end]),
        }
        cursor = token_end;
    }
    out.push_str(&html[cursor..]);
    out
}

/// Resolves a `cid:` token to an inline part's `(image media type, bytes)`, or `None` when
/// no part matches or its media type is not a safe `image/*`. The id match is exact and
/// case-sensitive (RFC 2392 `cid:` references a case-sensitive `Content-ID`).
fn resolve_image<'a>(parts: &'a [InlinePart], cid: &str) -> Option<(String, &'a [u8])> {
    let part = parts.iter().find(|part| part.content_id() == cid)?;
    let media_type = image_media_type(part.media_type())?;
    Some((media_type, part.bytes()))
}

/// The normalised `image/<subtype>` media type to place in a `data:` URI, or `None` if
/// `media_type` is not an image type or carries any character unsafe in a `data:` URL.
///
/// Restricting to `image/*` keeps the rewrite from ever producing an executable/document
/// `data:` URI, and the subtype allowlist (RFC 6838 restricted-name chars) excludes `;`,
/// `,`, and `"`: the characters that could inject a `data:` parameter or break out of the
/// `src` attribute. `image/svg+xml` is allowed: an SVG loaded via `<img>` is rendered
/// non-scripted (the browser disables scripting for image-referenced SVG), and the
/// document CSP carries no `script-src` regardless.
#[must_use]
pub fn image_media_type(media_type: &str) -> Option<String> {
    let lower = media_type.trim().to_ascii_lowercase();
    let subtype = lower.strip_prefix("image/")?;
    let safe = !subtype.is_empty()
        && subtype
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-' | b'_'));
    safe.then_some(lower)
}

/// Appends the standard (RFC 4648) base64 encoding of `bytes` to `out`. Hand-rolled to keep
/// the crate dependency-light; encoding is a fixed 3-byte→4-char mapping with `=` padding.
fn base64_encode_into(bytes: &[u8], out: &mut String) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    out.reserve(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(b1.unwrap_or(0)) << 8)
            | u32::from(b2.unwrap_or(0));
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if b1.is_some() {
            ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if b2.is_some() {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
}

/// Reverses [`inline_cid_images`] for the **outbound** (reply/forward) path: rewrites each
/// `data:` image in `html` that came from one of `parts` back to a `cid:` reference using
/// the part's **original** `Content-ID`, and returns the parts that were matched so the
/// caller can re-attach them as inline MIME parts.
///
/// When a reply/forward is sent, the quoted body the editor round-tripped carries the
/// reading view's resolved `data:` images. Sending them as `data:` works in most clients but
/// Outlook's reader blocks `data:` images, so the interoperable form (what Outlook and
/// Thunderbird produce) is a `multipart/related` message with `cid:` parts. The match is
/// exact because the same `Engine::message_inline_parts` fetch produces byte-identical parts
/// at read and send time, so the `data:` URI [`inline_cid_images`] wrote can be reconstructed
/// here and found. The original `Content-ID` is preserved (it can let a spam filter reuse a
/// hash it already scored earlier in the thread, and matches Outlook for Mac exactly).
#[must_use]
pub fn restore_cid_images<'a>(
    html: &str,
    parts: &[&'a InlinePart],
) -> (String, Vec<&'a InlinePart>) {
    let mut out = html.to_owned();
    let mut matched = Vec::new();
    for &part in parts {
        // Reconstruct exactly the `src="data:…"` attribute inline_cid_images would have written;
        // an image whose media type it would have rejected was never inlined, so it can't be here.
        // The match is anchored on the whole `src="…"` value (not the bare `data:` substring) so a
        // part whose base64 is a prefix of another's can't rewrite the middle of that other src.
        let Some(media) = image_media_type(part.media_type()) else {
            continue;
        };
        let mut needle = format!("src=\"data:{media};base64,");
        base64_encode_into(part.bytes(), &mut needle);
        needle.push('"');
        if !out.contains(&needle) {
            continue;
        }
        let mut replacement = String::from("src=\"cid:");
        escape_attr_value(part.content_id(), &mut replacement);
        replacement.push('"');
        out = out.replace(&needle, &replacement);
        matched.push(part);
    }
    (out, matched)
}

/// Escapes the characters that are significant inside a double-quoted HTML attribute value,
/// so a `Content-ID` placed in a `src="cid:…"` can never break out of the attribute. Normal
/// `Content-ID`s (addr-spec tokens) contain none of these, so this is defence in depth.
fn escape_attr_value(value: &str, out: &mut String) {
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
}

#[cfg(test)]
#[path = "cid_tests.rs"]
mod tests;
