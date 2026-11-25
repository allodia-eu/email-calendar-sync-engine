//! The transform one piece at a time: each piece moves exactly as the whole document moves it.

use super::{ColourRole, MessageScheme, adapt};
use crate::sanitize;

/// What a dark render of `raw` writes, through the real sanitiser.
fn dark(raw: &str) -> String {
    adapt(&sanitize(raw).into_html(), MessageScheme::Dark).into_owned()
}

#[test]
fn a_colour_moves_by_what_it_paints() {
    let dark = MessageScheme::Dark;
    assert_eq!(
        dark.adapt_colour("#000000", ColourRole::Ink).as_deref(),
        Some("#e3e3e3")
    );
    assert_eq!(
        dark.adapt_colour("#ffffff", ColourRole::Fill).as_deref(),
        Some("#1e1e1e")
    );
    // A pale divider stays subtle: it lands near the canvas, not near the ink.
    let line = dark
        .adapt_colour("rgb(181, 196, 223)", ColourRole::Line)
        .unwrap();
    assert!(line.starts_with('#') && line != "#b5c4df", "{line}");
}

#[test]
fn a_colour_reads_every_form_a_page_writes() {
    let dark = MessageScheme::Dark;
    for written in [
        "black",
        "#000",
        "#000000",
        "rgb(0, 0, 0)",
        "hsl(0 0% 0%)",
        " BLACK ",
    ] {
        assert_eq!(
            dark.adapt_colour(written, ColourRole::Ink).as_deref(),
            Some("#e3e3e3"),
            "{written}"
        );
    }
}

#[test]
fn a_colour_already_right_or_unreadable_stays() {
    let dark = MessageScheme::Dark;
    assert_eq!(dark.adapt_colour("#ffffff", ColourRole::Ink), None);
    assert_eq!(dark.adapt_colour("#1e1e1e", ColourRole::Fill), None);
    for unreadable in [
        "var(--ink)",
        "currentColor",
        "#12",
        "red blue",
        "",
        "linear-gradient(red, blue)",
    ] {
        assert_eq!(
            dark.adapt_colour(unreadable, ColourRole::Ink),
            None,
            "{unreadable}"
        );
    }
}

#[test]
fn nothing_moves_on_a_light_page() {
    let light = MessageScheme::Light;
    assert_eq!(light.adapt_colour("#000000", ColourRole::Ink), None);
    assert_eq!(light.adapt_attribute("p", "style", "color:#000"), None);
    assert_eq!(light.adapt_attribute("td", "bgcolor", "#ffffff"), None);
}

#[test]
fn an_attribute_moves_as_the_document_moves_it() {
    let cases = [
        (
            "p",
            "style",
            "color:#000000 !important;font-family:'Segoe UI';background:#F8F9FA",
        ),
        ("td", "bgcolor", "F8F9FA"),
        ("table", "BGCOLOR", "white"),
        ("font", "color", "#333333"),
        (
            "div",
            "style",
            "border-left:2px solid #cccccc;padding-left:1ex",
        ),
    ];
    for (element, attribute, value) in cases {
        let moved = MessageScheme::Dark
            .adapt_attribute(element, attribute, value)
            .unwrap_or_else(|| panic!("{element} {attribute}"));
        let tag = format!("<{element} {attribute}=\"{value}\">x</{element}>");
        // A cell outside a table is no cell to the parser.
        let markup = if element == "td" {
            format!("<table><tr>{tag}</tr></table>")
        } else {
            tag
        };
        let rendered = dark(&markup);
        assert!(
            rendered.contains(&format!("=\"{}\"", moved.replace('"', "&quot;"))),
            "{rendered} lacks {moved}"
        );
    }
}

#[test]
fn an_attribute_the_document_leaves_stays() {
    let dark = MessageScheme::Dark;
    // `color` paints only on `<font>`; anywhere else it is no attribute HTML reads.
    assert_eq!(dark.adapt_attribute("p", "color", "#000000"), None);
    assert_eq!(
        dark.adapt_attribute("a", "href", "https://example.com/"),
        None
    );
    // A block whose background is a picture keeps every colour it sets.
    assert_eq!(
        dark.adapt_attribute("td", "style", "background:url(cid:x);color:#000"),
        None
    );
    assert_eq!(dark.adapt_attribute("p", "style", "font-weight:bold"), None);
}

#[test]
fn a_style_sheet_moves_its_colours_on_a_dark_page_only() {
    let sheet = ".note{color:#000000;background:#ffffff}";
    let moved = MessageScheme::Dark.adapt_style_sheet(sheet).unwrap();
    assert_eq!(moved, ".note{color:#e3e3e3;background:#1e1e1e}");
    assert_eq!(MessageScheme::Light.adapt_style_sheet(sheet), None);
}

#[test]
fn a_style_sheet_has_its_scheme_queries_pinned_on_either_page() {
    let sheet = "@media (prefers-color-scheme: dark){.note{color:#fff}}";
    let light = MessageScheme::Light.adapt_style_sheet(sheet).unwrap();
    assert!(light.contains("(max-width:-1px)"), "{light}");
    let dark = MessageScheme::Dark.adapt_style_sheet(sheet).unwrap();
    // The sender's own dark rules apply as written.
    assert_eq!(dark, "@media (min-width:0px){.note{color:#fff}}");
}
