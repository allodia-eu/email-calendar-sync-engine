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

/// The message's representations plus their `bodyValues` (the content the attachments wrap
/// around): the plain text alone, or a `multipart/alternative` as soon as the draft carries
/// an HTML alternative or an iTIP scheduling object.
///
/// Ordered least-to-most faithful (RFC 2046 §5.1.4), matching what `engine-rfc5322`
/// assembles for the transports that submit raw MIME — the same message, expressed in the
/// two ways the two transports accept it.
fn main_body(draft: &Draft) -> (Value, Value) {
    let mut parts = vec![json!({ "partId": "text", "type": "text/plain" })];
    let mut values = Map::new();
    values.insert("text".to_owned(), json!({ "value": draft.text_body }));

    if let Some(html) = &draft.html_body {
        parts.push(json!({ "partId": "html", "type": "text/html" }));
        values.insert("html".to_owned(), json!({ "value": html }));
    }
    let structure = if parts.len() == 1 {
        parts.remove(0)
    } else {
        json!({ "type": "multipart/alternative", "subParts": parts })
    };
    (structure, Value::Object(values))
}

/// Refuses a draft carrying an iTIP object, because JMAP cannot express one.
///
/// **Why this is a refusal and not a body part.** RFC 6047 §2.4 requires the `method=`
/// parameter on the part's `Content-Type`, and a part with no `method=` is explicitly *not*
/// an iMIP body part (§2.4 note 2) — a receiving client files it as a calendar document and
/// never processes the answer. An `EmailBodyPart`'s `type` property is the media type
/// **without parameters**, so it cannot carry it.
///
/// RFC 8621 §4.1.3 looks like a way round: a raw `header:Content-Type` on the part, which
/// §4.6 permits on a body part (and forbids on the `Email`). Driven against Stalwart, all
/// three possible shapes produce a message the organizer's client will not process, and all
/// three **send successfully** — the silent-failure shape this whole capability exists to
/// prevent:
///
/// | shape | what the server emits |
/// |---|---|
/// | `header:Content-Type` alone | **two** `Content-Type` fields: ours, then a generated `text/plain`. A parser taking the last sees plain text. |
/// | `type` + `header:Content-Type` | two again — ours, then a generated `text/calendar` with no `method=`. Also breaks §4.6's "no two properties for one header field". |
/// | `type` alone | one clean field, and no `method=` at all. |
///
/// Pinned live in `tests/live_imip.rs`, both because a refusal that cannot be justified
/// gets re-litigated and because that test is what will notice if a server stops behaving
/// this way. It is a **server** limitation, not necessarily a protocol one — but Stalwart
/// is the only JMAP mail server this repo can drive (`jmap.md`), and shipping a path that
/// is malformed on the only server we can verify is worse than refusing.
///
/// So the adapter advertises
/// [`Capabilities::scheduling_submission`](engine_provider::Capabilities::scheduling_submission)
/// as `false` and refuses here, exactly as `RsvpControls` refuses a control it cannot
/// honour rather than dropping it.
///
/// # Errors
///
/// Returns an [`InvalidState`](engine_core::error::FailureClass::InvalidState)
/// [`ProviderError`](engine_provider::ProviderError). A host that read
/// `Capabilities::scheduling_submission` never reaches it.
pub(crate) fn reject_unsendable_calendar(
    draft: &Draft,
) -> Result<(), engine_provider::ProviderError> {
    if draft.calendar.is_some() {
        return Err(engine_provider::ProviderError::invalid_state(
            "JMAP cannot put the RFC 6047 `method=` parameter on a body part, so this \
             transport cannot send an iMIP scheduling message; read \
             Capabilities::scheduling_submission before composing one",
        ));
    }
    Ok(())
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
    fn an_itip_object_is_refused_rather_than_sent_without_its_method_parameter() {
        use engine_core::scheduling::ScheduleMethod;
        use engine_provider::DraftCalendar;

        // The rule this file's `reject_unsendable_calendar` docs justify: JMAP cannot put
        // `method=` on a body part, and a part without it is not a scheduling message at
        // all (RFC 6047 §2.4 note 2). Sending anyway would succeed and reach the organizer
        // as a calendar file — the silent success `Capabilities::scheduling_submission`
        // exists to let a host avoid.
        let plain = draft();
        assert!(reject_unsendable_calendar(&plain).is_ok());

        let scheduling = plain.with_calendar(DraftCalendar::new(
            ScheduleMethod::Reply,
            "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nEND:VCALENDAR\r\n",
        ));
        let err = reject_unsendable_calendar(&scheduling)
            .expect_err("a draft this transport cannot faithfully encode must be refused");
        assert_eq!(err.class(), engine_core::error::FailureClass::InvalidState);
        assert!(
            err.to_string().contains("scheduling_submission"),
            "the refusal must name the capability a host should have read: {err}"
        );
    }

    #[test]
    fn a_refused_itip_draft_never_reaches_the_body_structure() {
        use engine_core::scheduling::ScheduleMethod;
        use engine_provider::DraftCalendar;

        // Belt and braces: even if the refusal were bypassed, the body builder must not
        // quietly drop the object into an unmarked part. A draft carrying only text still
        // produces exactly the plain body it always did.
        let draft = draft().with_calendar(DraftCalendar::new(
            ScheduleMethod::Reply,
            "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nEND:VCALENDAR\r\n",
        ));
        let (structure, values) = body(&draft, &[]);
        assert_eq!(structure["type"], "text/plain");
        assert!(
            values.get("calendar").is_none(),
            "no half-encoded calendar part may reach the wire: {values}"
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
