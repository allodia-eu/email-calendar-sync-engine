//! IMAP BODYSTRUCTURE helpers used for list-level attachment metadata.
//!
//! The parser keeps this separate from the general FETCH reader because BODYSTRUCTURE is a recursive
//! MIME shape with its own positional grammar. The goal here is intentionally narrow: determine
//! whether the message has a user-visible downloadable part without materializing the body.

use crate::tokenize::Item;

/// Returns true when an IMAP BODYSTRUCTURE item describes at least one user-visible,
/// downloadable attachment. Multipart structures are recursive; single parts are considered
/// downloadable when the disposition is `attachment`, or when filename/name parameters carry
/// a file name and the part is not a CID inline resource. A `text/plain`/`text/html` body part
/// is never counted on a bare `name` parameter alone — that is the message body, not a download.
pub(crate) fn has_downloadable_part(item: &Item) -> bool {
    let Some(list) = item.as_list() else {
        return false;
    };
    if list.is_empty() {
        return false;
    }
    if is_multipart_body(list) {
        return list
            .iter()
            .take_while(|part| part.as_list().is_some())
            .any(has_downloadable_part);
    }
    single_part_is_downloadable(list)
}

fn is_multipart_body(list: &[Item]) -> bool {
    matches!(list.first(), Some(Item::List(_)))
}

fn single_part_is_downloadable(list: &[Item]) -> bool {
    let disposition = body_disposition(list);
    if disposition
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("attachment"))
    {
        return true;
    }
    // A `text/plain`/`text/html` part with no `attachment` disposition is the message body
    // (or its inline text rendering), even if it carries a stray `name=` parameter. mail-parser
    // classifies it as body, so this flag must not mislabel a bodied message as carrying a
    // download.
    if is_body_text(list) {
        return false;
    }
    let has_filename = params_contain_name(list.get(2))
        || body_disposition_params(list).is_some_and(params_contain_name_in);
    if !has_filename {
        return false;
    }
    let cid = list
        .get(3)
        .and_then(Item::as_nstring)
        .is_some_and(|value| !value.trim().is_empty());
    !cid
}

fn is_body_text(list: &[Item]) -> bool {
    let Some(kind) = list.first().and_then(Item::as_nstring) else {
        return false;
    };
    if !kind.eq_ignore_ascii_case("text") {
        return false;
    }
    list.get(1)
        .and_then(Item::as_nstring)
        .is_some_and(|subtype| {
            subtype.eq_ignore_ascii_case("plain") || subtype.eq_ignore_ascii_case("html")
        })
}

fn body_disposition(list: &[Item]) -> Option<String> {
    body_disposition_item(list)?
        .as_list()?
        .first()
        .and_then(Item::as_nstring)
}

fn body_disposition_params(list: &[Item]) -> Option<&[Item]> {
    body_disposition_item(list)?
        .as_list()?
        .get(1)
        .and_then(Item::as_list)
}

fn body_disposition_item(list: &[Item]) -> Option<&Item> {
    let kind = list.first().and_then(Item::as_nstring)?;
    let subtype = list.get(1).and_then(Item::as_nstring)?;
    let base_fields = if kind.eq_ignore_ascii_case("text") {
        8
    } else if kind.eq_ignore_ascii_case("message")
        && (subtype.eq_ignore_ascii_case("rfc822") || subtype.eq_ignore_ascii_case("global"))
    {
        // `message/global` (RFC 6532) carries the same envelope+body+lines shape as rfc822.
        10
    } else {
        7
    };
    list.get(base_fields + 1)
}

fn params_contain_name(item: Option<&Item>) -> bool {
    let Some(params) = item.and_then(Item::as_list) else {
        return false;
    };
    params_contain_name_in(params)
}

fn params_contain_name_in(params: &[Item]) -> bool {
    params.chunks(2).any(|pair| {
        let Some(name) = pair.first().and_then(Item::as_nstring) else {
            return false;
        };
        // RFC 2231 splits/encodes long parameters as `name*`, `filename*0*`, `filename*=…`;
        // compare the base key before the first `*`.
        let base = name.split('*').next().unwrap_or(name.as_str());
        if !(base.eq_ignore_ascii_case("name") || base.eq_ignore_ascii_case("filename")) {
            return false;
        }
        pair.get(1)
            .and_then(Item::as_nstring)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenize::items_of;

    /// Tokenizes a bare BODYSTRUCTURE value (a single parenthesized list) into its `Item`.
    fn structure(bytes: &[u8]) -> Item {
        items_of(bytes)
            .expect("tokenize")
            .into_iter()
            .next()
            .expect("one item")
    }

    #[test]
    fn non_list_and_empty_structures_are_not_downloadable() {
        // Malformed BODYSTRUCTURE values yield `false`, never a panic.
        assert!(!has_downloadable_part(&Item::Nil));
        assert!(!has_downloadable_part(&structure(b"()")));
    }

    #[test]
    fn a_name_param_after_a_non_name_param_is_still_found() {
        // The real `name` sits behind an unrelated `charset` param; the scan must reach it.
        let part = structure(
            b"(\"APPLICATION\" \"OCTET-STREAM\" (\"CHARSET\" \"UTF-8\" \"NAME\" \"file.bin\") \
              NIL NIL \"BASE64\" 100 NIL NIL NIL)",
        );
        assert!(has_downloadable_part(&part));
    }

    #[test]
    fn an_undisposed_unnamed_part_is_not_downloadable() {
        // No disposition, no name, no Content-ID — nothing marks it as a download.
        let part =
            structure(b"(\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"BASE64\" 100 NIL NIL NIL)");
        assert!(!has_downloadable_part(&part));
    }

    #[test]
    fn helpers_are_total_on_non_string_shapes() {
        // Defensive: a media-type or param key that is not a string must yield `false`, never
        // panic, if a helper is ever reached with a malformed shape.
        assert!(!is_body_text(&[Item::Nil]));
        assert!(!params_contain_name_in(&[
            Item::Nil,
            Item::Quoted("x".into())
        ]));
    }
}
