//! The mail↔calendar bridge's detection step: finding the iMIP `text/calendar`
//! part in a message's MIME tree.

use crate::mail::EmailBodyPart;

/// The iCalendar media type an iMIP scheduling message carries (RFC 6047 §2.4).
const CALENDAR_MEDIA_TYPE: &str = "text/calendar";

/// Finds the iMIP `text/calendar` part in a message's MIME tree, if any.
///
/// An iMIP message carries its iTIP object as a `text/calendar` body part
/// (RFC 6047 §2.4); detecting it is the mail sync path's hand-off to the calendar
/// layer. The returned part's `blob_id` holds the iCalendar bytes to fetch and
/// parse. The search is depth-first, so a `text/calendar` nested inside a
/// `multipart/mixed` (the common "invite + note + attachment" shape) is found.
/// The authoritative `METHOD` lives in the body itself, not the MIME `method=`
/// content-type parameter (RFC 6047 §2.4), so only the media type is matched here.
#[must_use]
pub fn find_calendar_part(root: &EmailBodyPart) -> Option<&EmailBodyPart> {
    if root.media_type.eq_ignore_ascii_case(CALENDAR_MEDIA_TYPE) {
        return Some(root);
    }
    root.sub_parts.iter().find_map(find_calendar_part)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{BlobId, PartId};

    fn leaf(part: &str, media_type: &str) -> EmailBodyPart {
        EmailBodyPart::leaf(
            PartId::try_from(part).unwrap(),
            BlobId::try_from(format!("blob-{part}").as_str()).unwrap(),
            media_type,
            10,
        )
    }

    #[test]
    fn finds_a_nested_calendar_part() {
        // The common invite shape: multipart/mixed of a note and the invitation.
        let tree = EmailBodyPart::multipart(
            "multipart/mixed",
            vec![
                leaf("1", "text/plain"),
                EmailBodyPart::multipart(
                    "multipart/alternative",
                    vec![leaf("2", "text/html"), leaf("3", "text/calendar")],
                ),
            ],
        );
        let part = find_calendar_part(&tree).unwrap();
        assert_eq!(part.media_type, "text/calendar");
        assert_eq!(part.blob_id, Some(BlobId::try_from("blob-3").unwrap()));
    }

    #[test]
    fn matches_media_type_case_insensitively() {
        let part = leaf("1", "TEXT/Calendar");
        assert!(find_calendar_part(&part).is_some());
    }

    #[test]
    fn a_message_without_a_calendar_part_yields_none() {
        let tree = EmailBodyPart::multipart(
            "multipart/alternative",
            vec![leaf("1", "text/plain"), leaf("2", "text/html")],
        );
        assert!(find_calendar_part(&tree).is_none());
    }
}
