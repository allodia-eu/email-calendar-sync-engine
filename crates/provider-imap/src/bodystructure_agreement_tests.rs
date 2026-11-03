//! The list-time flag and the opened message agree.
//!
//! Each shape under `tests/fixtures/attachment-shapes/` is a message (`.eml`) and the
//! `BODYSTRUCTURE` a real server built for it (`.bodystructure`, captured from the Stalwart
//! harness by appending the `.eml`). The flag read from the structure must say what
//! `engine_mime::extract_attachments` finds in the bytes, and both must say what the table
//! below expects, so the two rules cannot drift apart or go wrong together.

use std::{fs, path::Path};

use engine_core::raw::RawMime;

use super::has_downloadable_part;
use crate::tokenize::items_of;

/// Every shape and whether it carries a download.
const EXPECTED: &[(&str, bool)] = &[
    ("01-named-pdf-attachment", true),
    ("02-related-cid-image", false),
    ("03-photo-between-paragraphs", true),
    ("04-unnamed-inline-image", true),
    ("05-named-html-alternative", false),
    ("06-imip-body-part", false),
    ("07-imip-plus-attached-copy", true),
    ("08-unnamed-forwarded-message", true),
    ("09-named-inline-text-after-body", true),
    ("10-named-text-as-first-part", false),
    ("11-single-text", false),
    ("12-single-unnamed-pdf", true),
    ("13-cid-image-marked-attachment", true),
    ("14-unnamed-octet-stream", true),
    ("15-outlook-related-plus-pdf", true),
    ("16-alternative-with-related-html", false),
    ("17-html-only", false),
];

fn shapes() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/attachment-shapes")
}

#[test]
fn every_shape_on_disk_has_an_expectation() {
    let mut on_disk: Vec<String> = fs::read_dir(shapes())
        .expect("fixture dir")
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            name.strip_suffix(".eml").map(str::to_owned)
        })
        .collect();
    on_disk.sort();
    let expected: Vec<&str> = EXPECTED.iter().map(|(name, _)| *name).collect();
    assert_eq!(on_disk, expected);
}

#[test]
fn the_structure_and_the_bytes_agree_on_every_shape() {
    for (name, expected) in EXPECTED {
        let eml = fs::read(shapes().join(format!("{name}.eml"))).expect("eml");
        let structure = fs::read(shapes().join(format!("{name}.bodystructure"))).expect("bs");
        let item = items_of(structure.trim_ascii_end())
            .expect("tokenize")
            .into_iter()
            .next()
            .expect("one item");
        let listed = engine_mime::extract_attachments(&RawMime::new(eml));
        assert_eq!(
            !listed.is_empty(),
            *expected,
            "{name}: extract_attachments {listed:?}"
        );
        assert_eq!(
            has_downloadable_part(&item),
            *expected,
            "{name}: BODYSTRUCTURE flag"
        );
    }
}
