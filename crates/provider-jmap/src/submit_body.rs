//! Assembling a draft's JMAP body structure + `bodyValues` (RFC 8621 §4.1.4).
//!
//! Pure `serde_json` shaping, split out of [`crate::submit`] so the submission
//! orchestration and the (independently unit-tested) MIME-shape assembly stay under
//! one responsibility each. The text/HTML body wraps around any attachments: inline
//! (`cid`-referenced) parts relate to the body in a `multipart/related`, and regular
//! downloadable attachments wrap that in a `multipart/mixed`.

use engine_provider::{Draft, DraftAttachment};
use serde_json::{Map, Value, json};

/// Builds the JMAP `bodyStructure` and `bodyValues` for `draft`, referencing the
/// uploaded attachment `blob_ids` (one per `draft.attachments`, in order). An
/// attachment-free draft is just its text-or-alternative body.
pub(crate) fn body(draft: &Draft, blob_ids: &[String]) -> (Value, Value) {
    let (main, body_values) = main_body(draft);
    if draft.attachments.is_empty() {
        return (main, body_values);
    }

    let mut inline = Vec::new();
    let mut files = Vec::new();
    for (attachment, blob_id) in draft.attachments.iter().zip(blob_ids) {
        let part = attachment_part(attachment, blob_id);
        if attachment.is_inline() {
            inline.push(part);
        } else {
            files.push(part);
        }
    }

    let content = if inline.is_empty() {
        main
    } else {
        let mut sub = vec![main];
        sub.extend(inline);
        json!({ "type": "multipart/related", "subParts": sub })
    };
    let structure = if files.is_empty() {
        content
    } else {
        let mut sub = vec![content];
        sub.extend(files);
        json!({ "type": "multipart/mixed", "subParts": sub })
    };
    (structure, body_values)
}

/// The message's text-or-alternative body part plus its `bodyValues` (the content the
/// attachments wrap around). Plain-text alone, or a `multipart/alternative` when the
/// draft carries an HTML alternative.
fn main_body(draft: &Draft) -> (Value, Value) {
    match &draft.html_body {
        Some(html) => (
            json!({
                "type": "multipart/alternative",
                "subParts": [
                    { "partId": "text", "type": "text/plain" },
                    { "partId": "html", "type": "text/html" },
                ],
            }),
            json!({
                "text": { "value": draft.text_body },
                "html": { "value": html },
            }),
        ),
        None => (
            json!({ "partId": "text", "type": "text/plain" }),
            json!({ "text": { "value": draft.text_body } }),
        ),
    }
}

/// An `EmailBodyPart` for one attachment (RFC 8621 §4.1.4): its uploaded `blobId`,
/// media type, file name, and disposition — `inline` with a `cid` for a
/// body-referenced related part, else `attachment` for a downloadable file.
fn attachment_part(attachment: &DraftAttachment, blob_id: &str) -> Value {
    let mut part = Map::new();
    part.insert("blobId".to_owned(), json!(blob_id));
    part.insert("type".to_owned(), json!(attachment.media_type));
    part.insert("name".to_owned(), json!(attachment.file_name));
    match attachment.content_id() {
        Some(cid) => {
            part.insert("disposition".to_owned(), json!("inline"));
            part.insert("cid".to_owned(), json!(cid.as_str()));
        }
        None => {
            part.insert("disposition".to_owned(), json!("attachment"));
        }
    }
    Value::Object(part)
}

#[cfg(test)]
mod tests {
    use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
    use engine_provider::{ContentIdHeader, DraftAttachment};

    use super::*;

    fn draft() -> Draft {
        Draft::new(
            MessageIdHeader::new("probe@test.local").unwrap(),
            EmailAddress::new("alice@test.local"),
            vec![EmailAddress::new("bob@test.local")],
            "Subject",
            "Body",
        )
    }

    #[test]
    fn a_plain_draft_has_no_wrapping() {
        let (structure, values) = body(&draft(), &[]);
        assert_eq!(structure["partId"], "text");
        assert_eq!(structure["type"], "text/plain");
        assert_eq!(values["text"]["value"], "Body");
    }

    #[test]
    fn a_file_attachment_wraps_the_body_in_multipart_mixed() {
        let draft = draft().with_attachment(DraftAttachment::attachment(
            "report.pdf",
            "application/pdf",
            vec![4, 5, 6],
        ));
        // Blob id resolved by a prior upload (here a stand-in).
        let (structure, _values) = body(&draft, &["blob-1".to_owned()]);
        assert_eq!(structure["type"], "multipart/mixed");
        // First sub-part is the text body; the attachment follows.
        assert_eq!(structure["subParts"][0]["partId"], "text");
        let file = &structure["subParts"][1];
        assert_eq!(file["blobId"], "blob-1");
        assert_eq!(file["type"], "application/pdf");
        assert_eq!(file["name"], "report.pdf");
        assert_eq!(file["disposition"], "attachment");
        assert!(file.get("cid").is_none());
    }

    #[test]
    fn an_inline_attachment_relates_to_the_html_body() {
        let draft = draft()
            .with_html_body("<img src=\"cid:chart.1@test.local\">")
            .with_attachment(DraftAttachment::inline(
                "chart.png",
                "image/png",
                ContentIdHeader::new("chart.1@test.local").unwrap(),
                vec![1, 2, 3],
            ));
        let (structure, values) = body(&draft, &["blob-img".to_owned()]);
        // Inline parts relate to the alternative body under multipart/related.
        assert_eq!(structure["type"], "multipart/related");
        assert_eq!(structure["subParts"][0]["type"], "multipart/alternative");
        let img = &structure["subParts"][1];
        assert_eq!(img["blobId"], "blob-img");
        assert_eq!(img["disposition"], "inline");
        assert_eq!(img["cid"], "chart.1@test.local");
        // The text/HTML values still travel in bodyValues (the blob part does not).
        assert_eq!(
            values["html"]["value"],
            "<img src=\"cid:chart.1@test.local\">"
        );
    }

    #[test]
    fn inline_and_file_attachments_nest_related_inside_mixed() {
        let draft = draft()
            .with_html_body("<img src=\"cid:c@h\">")
            .with_attachment(DraftAttachment::inline(
                "c.png",
                "image/png",
                ContentIdHeader::new("c@h").unwrap(),
                vec![1],
            ))
            .with_attachment(DraftAttachment::attachment(
                "r.pdf",
                "application/pdf",
                vec![2],
            ));
        let (structure, _values) = body(&draft, &["blob-i".to_owned(), "blob-f".to_owned()]);
        // Outer multipart/mixed = [ multipart/related(body + inline), file ].
        assert_eq!(structure["type"], "multipart/mixed");
        assert_eq!(structure["subParts"][0]["type"], "multipart/related");
        assert_eq!(structure["subParts"][1]["blobId"], "blob-f");
        assert_eq!(structure["subParts"][1]["disposition"], "attachment");
    }
}
