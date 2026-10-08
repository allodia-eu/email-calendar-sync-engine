//! The text rendering of an HTML-only message.

use std::borrow::Cow;

/// The elements whose content is not text: [`mail_parser`]'s converter skips what they hold.
const NON_TEXT_TAGS: [&str; 4] = ["head", "style", "script", "template"];

/// Renders `html` as text, leaving out the content of [`NON_TEXT_TAGS`].
///
/// [`mail_parser`]'s converter recognises one of those opening tags only when `>` follows its
/// name directly, so `<style type="text/css">`, a common form in mail, is read as an unknown tag
/// and every rule inside it comes out as text: in the list snippet, in the plain-text reading view
/// and in the search index. Each such tag is rewritten to its bare form first, so the converter's
/// own handling applies.
pub(crate) fn html_to_text(html: &str) -> String {
    mail_parser::decoders::html::html_to_text(&bare_non_text_tags(html))
}

/// `html` with the attributes of every opening [`NON_TEXT_TAGS`] tag removed; borrowed when there
/// were none to remove. Every cut is at an ASCII byte, so hostile input cannot split a character.
fn bare_non_text_tags(html: &str) -> Cow<'_, str> {
    let bytes = html.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut pos = 0;
    while let Some(rel) = html[pos..].find('<') {
        let name_start = pos + rel + 1;
        pos = name_start;
        let Some(name) = NON_TEXT_TAGS.iter().find(|name| {
            bytes
                .get(name_start..name_start + name.len())
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name.as_bytes()))
        }) else {
            continue;
        };
        let name_end = name_start + name.len();
        // A bare `<style>` is already understood, and `<styles>` is a different tag.
        if !bytes
            .get(name_end)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b'/')
        {
            continue;
        }
        let Some(close) = html[name_end..].find('>') else {
            break;
        };
        let out = out.get_or_insert_with(|| String::with_capacity(html.len()));
        out.push_str(&html[copied..name_end]);
        out.push('>');
        copied = name_end + close + 1;
        pos = copied;
    }
    match out {
        Some(mut out) => {
            out.push_str(&html[copied..]);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(html),
    }
}

#[cfg(test)]
mod tests {
    use super::{bare_non_text_tags, html_to_text};

    #[test]
    fn a_style_block_with_attributes_is_not_text() {
        let text = html_to_text(
            "<style type=\"text/css\">body, p, div {font:12px Verdana;}</style><p>Beste Dennis,</p>",
        );
        assert!(!text.contains("Verdana"), "{text:?}");
        assert!(text.contains("Beste Dennis,"), "{text:?}");
    }

    #[test]
    fn every_non_text_element_is_left_out_whatever_its_attributes() {
        let text = html_to_text(concat!(
            "<HEAD lang=\"nl\"><meta charset=\"utf-8\"></HEAD>",
            "<Style media=\"screen\">.a{color:red}</Style>",
            "<script type=\"text/javascript\">track()</script>",
            "<template id=\"t\">hidden</template>",
            "<p>shown</p>",
        ));
        assert_eq!(text.trim(), "shown");
    }

    #[test]
    fn only_the_opening_tags_of_those_elements_are_rewritten() {
        let html = "<p class=\"x\">a</p><styles>b</styles><style>c</style><headline x=\"1\">";
        assert_eq!(bare_non_text_tags(html), html);
        assert_eq!(
            bare_non_text_tags("<p><style\ttype=\"text/css\">x</style><script/>"),
            "<p><style>x</style><script>"
        );
    }

    #[test]
    fn hostile_input_never_panics() {
        for html in [
            "<style",
            "<style ",
            "<style é",
            "é<style é>é",
            "<",
            "<<style x>>",
            "",
        ] {
            let _ = html_to_text(html);
        }
    }
}
