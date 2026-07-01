//! IMAP BODYSTRUCTURE helpers used for list-level attachment metadata.
//!
//! The parser keeps this separate from the general FETCH reader because BODYSTRUCTURE is a recursive
//! MIME shape with its own positional grammar. The goal here is intentionally narrow: determine
//! whether the message has a user-visible downloadable part without materializing the body.

use crate::tokenize::Item;

/// Returns true when an IMAP BODYSTRUCTURE item describes at least one user-visible,
/// downloadable attachment. Multipart structures are recursive; single parts are considered
/// downloadable when the disposition is `attachment`, or when filename/name parameters carry
/// a file name and the part is not a CID inline resource.
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
    } else if kind.eq_ignore_ascii_case("message") && subtype.eq_ignore_ascii_case("rfc822") {
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
        if !(name.eq_ignore_ascii_case("name") || name.eq_ignore_ascii_case("filename")) {
            return false;
        }
        pair.get(1)
            .and_then(Item::as_nstring)
            .is_some_and(|value| !value.trim().is_empty())
    })
}
