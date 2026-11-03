//! Tests for attachment extraction.

use super::*;

fn raw(bytes: &[u8]) -> RawMime {
    RawMime::new(bytes)
}

const MIXED_WITH_ATTACHMENTS: &[u8] = b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
    --m\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
    --m\r\nContent-Type: application/pdf; name=\"report.pdf\"\r\n\
    Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
    Content-Transfer-Encoding: base64\r\n\r\nUERG\r\n\
    --m\r\nContent-Type: text/calendar\r\n\
    Content-Disposition: attachment; filename=\"invite.ics\"\r\n\r\nBEGIN:VCALENDAR\r\n\
    --m--\r\n";

// --- G6: an iMIP body part is a card, not a file -----------------------------

/// Outlook / Sabre-CalDAV: the calendar payload is an alternative **body** part —
/// no disposition, no filename.
const IMIP_INLINE_ONLY: &[u8] = b"Content-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
    --a\r\nContent-Type: text/plain\r\n\r\nWhen: today\r\n\
    --a\r\nContent-Type: text/calendar; charset=\"iso-8859-1\"; method=REQUEST\r\n\r\n\
    BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n\
    --a--\r\n";

/// Gmail: the body part **and** a deliberately attached duplicate.
const IMIP_INLINE_PLUS_ATTACHED: &[u8] = b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
    --m\r\nContent-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
    --a\r\nContent-Type: text/plain\r\n\r\nnote\r\n\
    --a\r\nContent-Type: text/calendar; charset=UTF-8; method=REQUEST\r\n\r\n\
    BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n\
    --a--\r\n\
    --m\r\nContent-Type: application/ics; name=\"invite.ics\"\r\n\
    Content-Disposition: attachment; filename=\"invite.ics\"\r\n\r\n\
    BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n\
    --m--\r\n";

#[test]
fn an_imip_body_part_does_not_become_a_junk_attachment_row() {
    // Regression (G6). `text/calendar` is a text type that belongs to neither the text
    // nor the HTML body index, so mail-parser hands it back as an attachment and
    // `default_file_name` invented `attachment-2.ics`. Every invitation therefore showed
    // a paperclip nobody sent. Outlook and Gmail both hide this part.
    let attachments = extract_attachments(&raw(IMIP_INLINE_ONLY));
    assert!(
        attachments.is_empty(),
        "the invitation body part is a card, not a file: {attachments:?}"
    );
}

#[test]
fn a_deliberately_attached_ics_still_shows_beside_the_hidden_body_part() {
    // Gmail's belt-and-braces copy is a real file the sender chose to attach — Outlook
    // shows it too. Hiding the body part must not hide this.
    let attachments = extract_attachments(&raw(IMIP_INLINE_PLUS_ATTACHED));
    assert_eq!(attachments.len(), 1, "{attachments:?}");
    assert_eq!(attachments[0].file_name(), "invite.ics");
}

#[test]
fn a_published_ics_attachment_is_untouched() {
    // A `METHOD:PUBLISH` calendar file (a newsletter-style .ics) is dispositioned as an
    // attachment and produces no card, so its chip must survive.
    let published = b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
        --m\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n\
        --m\r\nContent-Type: text/calendar; charset=UTF-8\r\n\
        Content-Disposition: attachment; filename=\"agenda.ics\"\r\n\r\n\
        BEGIN:VCALENDAR\r\nMETHOD:PUBLISH\r\nEND:VCALENDAR\r\n\
        --m--\r\n";
    let attachments = extract_attachments(&raw(published));
    assert_eq!(attachments.len(), 1, "{attachments:?}");
    assert_eq!(attachments[0].file_name(), "agenda.ics");
}

#[test]
fn a_suppressed_imip_body_part_cannot_be_downloaded_by_id_either() {
    // The listing and the by-id fetch must agree: a part hidden from the list must not
    // be reachable through `extract_attachment`, or a host could still surface it.
    for id in 0..4 {
        assert!(
            extract_attachment(&raw(IMIP_INLINE_ONLY), AttachmentPartId::new(id)).is_none(),
            "id {id} should not resolve to a downloadable part"
        );
    }
}

#[test]
fn extracts_attachment_metadata_without_bytes() {
    let attachments = extract_attachments(&raw(MIXED_WITH_ATTACHMENTS));
    assert_eq!(attachments.len(), 2, "{attachments:?}");
    // The MIME part number: the multipart is 0, the text body 1, the PDF 2.
    assert_eq!(attachments[0].id().as_u32(), 2);
    assert_eq!(attachments[0].file_name(), "report.pdf");
    assert_eq!(attachments[0].media_type(), "application/pdf");
    assert_eq!(attachments[0].size(), 3);
    assert!(!attachments[0].is_inline());
    assert_eq!(attachments[1].file_name(), "invite.ics");
    assert_eq!(attachments[1].media_type(), "text/calendar");
}

#[test]
fn extracts_selected_attachment_content() {
    let raw = raw(MIXED_WITH_ATTACHMENTS);
    let listed = extract_attachments(&raw);
    let content = extract_attachment(&raw, listed[0].id()).expect("attachment");
    assert_eq!(content.attachment(), &listed[0]);
    assert_eq!(content.bytes(), b"PDF");
    // A part that exists but is not a download (the text body), and one that does not.
    for id in [1, 9] {
        assert!(extract_attachment(&raw, AttachmentPartId::new(id)).is_none());
    }
}

/// An iPhone places a photo between two paragraphs of a `multipart/mixed`: inline, named,
/// no `Content-ID`. RFC 8621 shows it in both body lists and not among the attachments.
const PHOTO_BETWEEN_PARAGRAPHS: &[u8] = b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
    --m\r\nContent-Type: text/plain\r\n\r\nbefore\r\n\
    --m\r\nContent-Type: image/jpeg; name=\"IMG_1.jpg\"\r\n\
    Content-Disposition: inline; filename=\"IMG_1.jpg\"\r\n\
    Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n\
    --m\r\nContent-Type: text/plain\r\n\r\nafter\r\n--m--\r\n";

#[test]
fn a_photo_shown_inside_the_body_is_still_a_download() {
    // Regression (mail-parser 1.0 follows RFC 8621 here; 0.11 listed it). The engine
    // renders no media inline and the photo has no `cid:`, so the list is the only way
    // to reach it.
    let raw = raw(PHOTO_BETWEEN_PARAGRAPHS);
    let attachments = extract_attachments(&raw);
    assert_eq!(attachments.len(), 1, "{attachments:?}");
    assert_eq!(attachments[0].file_name(), "IMG_1.jpg");
    assert!(attachments[0].is_inline());
    let content = extract_attachment(&raw, attachments[0].id()).expect("photo");
    assert_eq!(content.bytes(), b"hello");
}

#[test]
fn the_parts_of_a_forwarded_message_are_not_listed_beside_it() {
    // The `.eml` is the download; its own attachment travels inside it.
    let raw = raw(b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
          --m\r\nContent-Type: text/plain\r\n\r\nfwd\r\n\
          --m\r\nContent-Type: message/rfc822\r\n\r\n\
          Content-Type: multipart/mixed; boundary=\"i\"\r\n\r\n\
          --i\r\nContent-Type: text/plain\r\n\r\ninner\r\n\
          --i\r\nContent-Type: application/pdf\r\n\
          Content-Disposition: attachment; filename=\"inner.pdf\"\r\n\r\nPDF\r\n\
          --i--\r\n--m--\r\n");
    let attachments = extract_attachments(&raw);
    assert_eq!(attachments.len(), 1, "{attachments:?}");
    assert_eq!(attachments[0].media_type(), "message/rfc822");
    assert_eq!(attachments[0].file_name(), "attachment-1.eml");
    // Its id is a part number, so the nested PDF has one too; it is still not a download.
    for id in 3..7 {
        assert!(extract_attachment(&raw, AttachmentPartId::new(id)).is_none());
    }
}

#[test]
fn skips_cid_inline_images_but_keeps_inline_files_without_cid() {
    let raw = raw(b"Content-Type: multipart/related; boundary=\"b\"\r\n\r\n\
          --b\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:logo@x\">\r\n\
          --b\r\nContent-Type: image/png\r\nContent-ID: <logo@x>\r\n\
          Content-Disposition: inline; filename=\"logo.png\"\r\n\
          Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n\
          --b\r\nContent-Type: application/pdf\r\n\
          Content-Disposition: inline; filename=\"preview.pdf\"\r\n\r\nPDF\r\n\
          --b--\r\n");
    let attachments = extract_attachments(&raw);
    assert_eq!(attachments.len(), 1, "{attachments:?}");
    assert_eq!(attachments[0].file_name(), "preview.pdf");
    assert!(attachments[0].is_inline());
}

// A `multipart/related` embedded image referenced by `cid:` but carrying **no**
// `Content-Disposition`. mail-parser types a non-first related child as `Binary` (not
// `InlineBinary`), so this is the exact shape that used to leak into the attachment list.
const RELATED_UNDISPOSED_CID: &[u8] = b"Content-Type: multipart/related; boundary=\"b\"\r\n\r\n\
    --b\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:logo@x\">\r\n\
    --b\r\nContent-Type: image/png\r\nContent-ID: <logo@x>\r\n\
    Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n\
    --b--\r\n";

#[test]
fn undisposed_related_cid_image_is_inline_not_a_downloadable_attachment() {
    // Regression: an embedded body image (cid:, no disposition) must not appear as a
    // downloadable file — and must not be double-listed with the inline parts.
    let attachments = extract_attachments(&raw(RELATED_UNDISPOSED_CID));
    assert!(attachments.is_empty(), "{attachments:?}");
    // It is still resolvable as an inline part for the HTML body.
    let inline = crate::extract_inline_parts(&raw(RELATED_UNDISPOSED_CID));
    assert_eq!(inline.len(), 1);
    assert_eq!(inline[0].content_id(), "logo@x");
}

#[test]
fn attachment_disposition_stays_downloadable_even_with_a_content_id() {
    // The sender explicitly marked this image `attachment`; a `Content-ID` on it must not
    // hide it from the download list.
    let raw = raw(b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
          --m\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
          --m\r\nContent-Type: image/png\r\nContent-ID: <pic@x>\r\n\
          Content-Disposition: attachment; filename=\"pic.png\"\r\n\
          Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--m--\r\n");
    let attachments = extract_attachments(&raw);
    assert_eq!(attachments.len(), 1, "{attachments:?}");
    assert_eq!(attachments[0].file_name(), "pic.png");
    assert_eq!(attachments[0].content_id(), Some("pic@x"));
}

#[test]
fn safe_file_name_replaces_bidi_and_zero_width_controls() {
    // U+202E RIGHT-TO-LEFT OVERRIDE is the classic extension-spoof; it must be neutralized,
    // along with the other bidi overrides, isolates, and zero-width/format controls.
    let safe = safe_file_name("invoice\u{202E}cod.exe");
    assert!(!safe.contains('\u{202E}'), "{safe:?}");
    assert_eq!(safe, "invoice_cod.exe");
    assert_eq!(safe_file_name("a\u{200B}b\u{FEFF}c"), "a_b_c");
    // A bidi isolate (U+2066) and the Arabic letter mark (U+061C) are equally spoofable.
    assert_eq!(safe_file_name("x\u{2066}y\u{061C}z"), "x_y_z");
}

#[test]
fn safe_extension_falls_back_when_nothing_alphanumeric_remains() {
    assert_eq!(safe_extension("###"), "bin");
    assert_eq!(safe_extension("PdF"), "PdF");
}

#[test]
fn safe_file_name_falls_back_on_dot_only_and_empty_names() {
    assert_eq!(safe_file_name(".."), "attachment.bin");
    assert_eq!(safe_file_name("."), "attachment.bin");
    assert_eq!(safe_file_name("   "), "attachment.bin");
    // A legitimate dotfile keeps its name.
    assert_eq!(safe_file_name(".gitignore"), ".gitignore");
}

#[test]
fn unnamed_attachments_get_default_names_by_media_type() {
    // Parts with no filename anywhere fall back to a synthesized `attachment-N.ext` derived
    // from the media type (covers the pdf and image arms and the extension sanitizer).
    let raw = raw(b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
          --m\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
          --m\r\nContent-Type: application/pdf\r\n\
          Content-Disposition: attachment\r\nContent-Transfer-Encoding: base64\r\n\r\nUERG\r\n\
          --m\r\nContent-Type: image/png\r\n\
          Content-Disposition: attachment\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n\
          --m--\r\n");
    let attachments = extract_attachments(&raw);
    assert_eq!(attachments.len(), 2, "{attachments:?}");
    assert_eq!(attachments[0].file_name(), "attachment-1.pdf");
    assert_eq!(attachments[1].file_name(), "attachment-2.png");
}

#[test]
fn attachment_with_a_bare_media_type_falls_back_to_octet_defaults() {
    // A Content-Type with a type but no subtype, and no filename: media type keeps the bare
    // type and the default name uses the `.bin` fallback extension.
    let raw = raw(b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
          --m\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
          --m\r\nContent-Type: application\r\nContent-Disposition: attachment\r\n\r\nX\r\n\
          --m--\r\n");
    let attachments = extract_attachments(&raw);
    assert_eq!(attachments.len(), 1, "{attachments:?}");
    assert_eq!(attachments[0].media_type(), "application");
    assert_eq!(attachments[0].file_name(), "attachment-1.bin");
}

#[test]
fn sanitizes_suggested_file_names() {
    let raw = raw(b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
          --m\r\nContent-Type: application/octet-stream\r\n\
          Content-Disposition: attachment; filename=\"..\\secret/report?.bin\"\r\n\r\nbytes\r\n\
          --m--\r\n");
    let attachments = extract_attachments(&raw);
    assert_eq!(attachments[0].file_name(), "..secret_report_.bin");
}

#[test]
fn a_parameter_value_folded_after_its_equals_sign_is_kept() {
    // mail-parser 1.0.0 read the value on the folded line as a new parameter name (its #165),
    // losing the `boundary` (so the message lost its parts) and the file name.
    let raw = raw(
        b"Content-Type: multipart/mixed; boundary=\r\n \"m\"\r\n\r\n\
          --m\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
          --m\r\nContent-Type: application/pdf; name=\r\n \"report.pdf\"\r\n\
          Content-Disposition: attachment; filename=\r\n \"report.pdf\"\r\n\
          Content-Transfer-Encoding: base64\r\n\r\nUERG\r\n--m--\r\n",
    );
    let attachments = extract_attachments(&raw);
    assert_eq!(attachments.len(), 1, "{attachments:?}");
    assert_eq!(attachments[0].file_name(), "report.pdf");
}

#[test]
fn the_flag_answers_what_the_list_holds() {
    for (shape, expected) in [
        (MIXED_WITH_ATTACHMENTS, true),
        (IMIP_INLINE_ONLY, false),
        (IMIP_INLINE_PLUS_ATTACHED, true),
        (RELATED_UNDISPOSED_CID, false),
        (&b"Content-Type: text/plain\r\n\r\njust text"[..], false),
    ] {
        let raw = raw(shape);
        assert_eq!(has_downloadable_attachment(&raw), Some(expected));
        assert_eq!(!extract_attachments(&raw).is_empty(), expected);
    }
}

#[test]
fn bytes_that_are_no_message_answer_nothing() {
    // Not "no attachment": a broken source says nothing, so it must not clear a flag.
    assert_eq!(has_downloadable_attachment(&raw(b"")), None);
    assert_eq!(
        has_downloadable_attachment(&raw(b"no header, no blank line")),
        None
    );
}

#[test]
fn hostile_input_never_panics() {
    assert_eq!(extract_attachments(&raw(b"")), []);
    assert!(extract_attachment(&raw(b"\xff\x00not mime"), AttachmentPartId::new(0)).is_none());
}
