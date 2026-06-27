//! Offline tests for rich SMTP MIME assembly.

use super::*;
use engine_core::ids::MessageIdHeader;
use engine_core::mail::EmailAddress;
use engine_provider::{ContentIdHeader, Draft, DraftAttachment};
use time::macros::datetime;

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
