//! IMAP BODYSTRUCTURE helpers used for list-level attachment metadata.
//!
//! The parser keeps this separate from the general FETCH reader because BODYSTRUCTURE is a
//! recursive MIME shape with its own positional grammar. The goal here is intentionally narrow:
//! determine whether the message has a user-visible downloadable part without materializing the
//! body.

use crate::tokenize::Item;

/// Returns true when an IMAP BODYSTRUCTURE item describes at least one user-visible,
/// downloadable attachment: a part `engine_mime::extract_attachments` would list once the
/// message is opened, so the list-time flag and the opened message agree.
///
/// That rule is RFC 8621's classification plus the engine's own exceptions, read here from
/// the structure alone. A leaf part counts unless it is
///
/// - a `text/plain` or `text/html` part of the body: the first child of its multipart (or the
///   message itself), a version of a `multipart/alternative`, or an unnamed part elsewhere outside
///   a `multipart/related`;
/// - an undispositioned `text/calendar`, the iMIP payload rendered as an invitation card;
/// - a non-text, non-message part carrying a `Content-ID`, a `cid:` resource of the HTML.
///
/// `Content-Disposition: attachment` always counts. Every other leaf counts, named or not: an
/// image shown inside the body, an unnamed `application/pdf`, a forwarded `message/rfc822`.
/// A nested message is one part; its own parts are not read.
pub(crate) fn has_downloadable_part(item: &Item) -> bool {
    counts(item, None)
}

/// Where a part sits: the subtype of its multipart and its position among the children.
#[derive(Clone, Copy)]
struct Place<'a> {
    parent: &'a str,
    index: usize,
}

fn counts(item: &Item, place: Option<Place<'_>>) -> bool {
    let Some(list) = item.as_list() else {
        return false;
    };
    if list.is_empty() {
        return false;
    }
    if is_multipart_body(list) {
        let children = list
            .iter()
            .take_while(|part| part.as_list().is_some())
            .count();
        let subtype = list
            .get(children)
            .and_then(Item::as_nstring)
            .unwrap_or_default();
        return list[..children].iter().enumerate().any(|(index, child)| {
            counts(
                child,
                Some(Place {
                    parent: &subtype,
                    index,
                }),
            )
        });
    }
    single_part_is_downloadable(list, place)
}

fn is_multipart_body(list: &[Item]) -> bool {
    matches!(list.first(), Some(Item::List(_)))
}

fn single_part_is_downloadable(list: &[Item], place: Option<Place<'_>>) -> bool {
    let disposition = body_disposition(list);
    if disposition
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("attachment"))
    {
        return true;
    }
    if is_body_text(list) {
        return text_is_an_attachment(list, place);
    }
    let kind = list.first().and_then(Item::as_nstring).unwrap_or_default();
    let subtype = list.get(1).and_then(Item::as_nstring).unwrap_or_default();
    if kind.eq_ignore_ascii_case("text")
        && subtype.eq_ignore_ascii_case("calendar")
        && disposition.is_none()
    {
        return false;
    }
    let cid = list
        .get(3)
        .and_then(Item::as_nstring)
        .is_some_and(|value| !value.trim().is_empty());
    !(cid && !kind.eq_ignore_ascii_case("text") && !kind.eq_ignore_ascii_case("message"))
}

/// Whether an undispositioned (or inline) `text/plain`/`text/html` part is offered as a file
/// rather than read as the body: RFC 8621 `isInline` fails for it, and it is not a version of
/// a `multipart/alternative`, which is the body whatever it carries.
fn text_is_an_attachment(list: &[Item], place: Option<Place<'_>>) -> bool {
    let Some(place) = place else {
        return false;
    };
    if place.index == 0 || place.parent.eq_ignore_ascii_case("alternative") {
        return false;
    }
    place.parent.eq_ignore_ascii_case("related")
        || params_contain_name(list.get(2))
        || body_disposition_params(list).is_some_and(params_contain_name_in)
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
    fn an_undisposed_unnamed_file_is_still_a_download() {
        // No disposition, no name, no Content-ID: RFC 8621 offers it as an attachment and the
        // opened message lists it, so the flag counts it too.
        let part =
            structure(b"(\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"BASE64\" 100 NIL NIL NIL)");
        assert!(has_downloadable_part(&part));
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

#[cfg(test)]
#[path = "bodystructure_agreement_tests.rs"]
mod agreement_tests;
