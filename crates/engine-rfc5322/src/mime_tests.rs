//! Tests for rich MIME assembly (HTML alternative + inline/regular attachments),
//! driven through the full `assemble_message`.

use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
use engine_provider::{ContentIdHeader, Draft, DraftAttachment};
use time::macros::datetime;

use super::*;

fn base_draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("rich@host").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Rich body",
        "plain fallback",
    )
}

fn assemble(draft: &Draft) -> String {
    let bytes = assemble_message(draft, datetime!(2026-06-20 12:00:00 UTC)).unwrap();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn html_body_uses_multipart_alternative() {
    let draft = base_draft().with_html_body("<p><strong>Hello</strong></p>");
    let message = assemble(&draft);

    assert!(message.contains("Content-Type: multipart/alternative; boundary=\""));
    assert!(message.contains("Content-Type: text/plain; charset=utf-8\r\n\r\nplain fallback\r\n"));
    assert!(
        message.contains(
            "Content-Type: text/html; charset=utf-8\r\n\r\n<p><strong>Hello</strong></p>\r\n"
        ),
        "{message}"
    );
}

#[test]
fn inline_and_regular_attachments_use_related_inside_mixed() {
    let draft = base_draft()
        .with_html_body("<p><img src=\"cid:chart.1@test.local\"></p>")
        .with_attachment(DraftAttachment::inline(
            "chart.png",
            "image/png",
            ContentIdHeader::new("chart.1@test.local").unwrap(),
            vec![0, 1, 2, 3, 4, 5],
        ))
        .with_attachment(DraftAttachment::attachment(
            "report.pdf",
            "application/pdf",
            b"PDF bytes".to_vec(),
        ));

    let message = assemble(&draft);

    assert!(message.contains("Content-Type: multipart/mixed; boundary=\""));
    assert!(message.contains("Content-Type: multipart/related; boundary=\""));
    assert!(message.contains("Content-Type: multipart/alternative; boundary=\""));
    assert!(message.contains("Content-Type: image/png; name=\"chart.png\"\r\n"));
    assert!(message.contains("Content-ID: <chart.1@test.local>\r\n"));
    assert!(message.contains("Content-Disposition: inline; filename=\"chart.png\"\r\n"));
    assert!(message.contains("Content-Type: application/pdf; name=\"report.pdf\"\r\n"));
    assert!(message.contains("Content-Disposition: attachment; filename=\"report.pdf\"\r\n"));
    assert!(message.contains("Content-Transfer-Encoding: base64\r\n"));
    assert!(message.contains("AAECAwQF\r\n"));
}

#[test]
fn a_non_ascii_attachment_filename_uses_rfc_5987_percent_encoding() {
    let draft = base_draft().with_attachment(DraftAttachment::attachment(
        "réçu ☕.pdf",
        "application/pdf",
        b"bytes".to_vec(),
    ));
    let message = assemble(&draft);
    // A non-ASCII filename is emitted as RFC 5987 `name*`/`filename*` percent-encoding,
    // never as raw 8-bit header bytes — the headers stay 7-bit clean.
    assert!(
        message.is_ascii(),
        "headers must stay 7-bit clean: {message}"
    );
    assert!(message.contains("name*=utf-8''"), "{message}");
    assert!(message.contains("filename*=utf-8''"), "{message}");
    // The space and the ☕ (U+2615) are percent-encoded; the `.pdf` survives verbatim.
    assert!(
        message.contains("%20") && message.contains("%E2%98%95"),
        "{message}"
    );
    assert!(message.contains(".pdf"), "{message}");
}

#[test]
fn an_unsafe_attachment_media_type_is_rejected_not_interpolated() {
    use engine_core::error::FailureClass;
    // A media type carrying a header-breaking char (space + `;`) must be refused, not
    // written into the `Content-Type` header where it could inject a parameter.
    let draft = base_draft().with_attachment(DraftAttachment::attachment(
        "ok.bin",
        "application/x-evil; boundary=inject",
        b"x".to_vec(),
    ));
    let err = assemble_message(&draft, datetime!(2026-06-20 12:00:00 UTC)).unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
}
