//! Filling [`Message::preview`] for IMAP list rows.
//!
//! Microsoft Graph and JMAP hand the engine a server-computed snippet, so their messages
//! arrive with `preview` set and a host's list row can show a line of body under the subject.
//! IMAP has no such field, so a freshly synced IMAP message has `preview == None` and the row
//! has nothing to show. IMAP only lets us derive one by reading the body, so we do — but
//! **bounded**: only the newest [`PREVIEW_HYDRATE_CAP`] messages of a page that still lack a
//! preview are read, so a first sync is not dominated by body downloads (older rows fill in as
//! they page in, and a steady-state delta only reads its few new arrivals). Each read is a
//! `BODY.PEEK[]` — previewing a body must never mark it `\Seen`; an expunged or unreadable
//! message is skipped, never fatal.

use engine_core::{mail::Message, raw::RawMime};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{mail::parse_message_key, transport::Connection};

/// How many preview-less messages of one page to hydrate. The page is newest-first, so this
/// covers the top of the folder — what a user sees first.
pub(crate) const PREVIEW_HYDRATE_CAP: usize = 30;

/// The longest snippet we keep. `Message::preview` is documented as ≤256 chars; a list row
/// shows only ~2 lines, so a little under that is plenty and keeps the store lean.
const PREVIEW_MAX_CHARS: usize = 200;

/// Fills a plain-text preview for up to [`PREVIEW_HYDRATE_CAP`] of `messages` that arrived
/// without one, reading each body over the already-selected mailbox. Best-effort: a fetch
/// error or a body with no text leaves that message's preview unset (the row degrades to
/// sender-only rather than showing noise).
pub(crate) async fn hydrate_previews<S>(conn: &mut Connection<S>, messages: &mut [Message])
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut budget = PREVIEW_HYDRATE_CAP;
    for message in messages.iter_mut() {
        if budget == 0 {
            break;
        }
        if message
            .preview
            .as_deref()
            .is_some_and(|p| !p.trim().is_empty())
        {
            continue;
        }
        let Some((_mailbox, _uid_validity, uid)) = parse_message_key(message.id.key().as_str())
        else {
            continue;
        };
        budget -= 1;
        // `BODY.PEEK[]` — reading a body to preview it must never mark the message `\Seen`.
        // An expunged/unreadable message (no `BODY[]`, or a transport error) is skipped.
        let Ok(Some(bytes)) = conn.uid_fetch_body(uid).await else {
            continue;
        };
        if let Some(preview) = preview_from_source(bytes) {
            message.preview = Some(preview);
        }
    }
}

/// Derives a compact, whitespace-collapsed snippet from a raw RFC 5322 message: its decoded
/// text body (the shared parser converts an HTML-only body to text), trimmed to
/// [`PREVIEW_MAX_CHARS`]. `None` when the message carries no usable text.
fn preview_from_source(raw: Vec<u8>) -> Option<String> {
    let body = engine_mime::extract_body(&RawMime::new(raw));
    let collapsed = body
        .plain()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(truncate_chars(&collapsed, PREVIEW_MAX_CHARS))
}

/// Truncates `s` to at most `max` characters on a char boundary, dropping any trailing space.
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => s[..idx].trim_end().to_owned(),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_body_becomes_a_collapsed_snippet() {
        let raw = b"From: a@example.com\r\nSubject: Hi\r\n\
            Content-Type: text/plain; charset=utf-8\r\n\r\n\
            Hello   there,\r\n\r\nthis is  the body.\r\n"
            .to_vec();
        assert_eq!(
            preview_from_source(raw).as_deref(),
            Some("Hello there, this is the body."),
        );
    }

    #[test]
    fn html_only_body_is_reduced_to_text() {
        let raw = b"From: a@example.com\r\nSubject: Hi\r\n\
            Content-Type: text/html; charset=utf-8\r\n\r\n\
            <html><body><h1>Sale</h1><p>Big <b>news</b> today</p></body></html>\r\n"
            .to_vec();
        let preview = preview_from_source(raw).expect("html reduces to text");
        assert!(preview.contains("Sale"), "got: {preview}");
        assert!(preview.contains("news"), "got: {preview}");
        assert!(!preview.contains('<'), "tags leaked: {preview}");
    }

    #[test]
    fn long_body_is_truncated_on_a_char_boundary() {
        let long = "é".repeat(500);
        let raw = format!("Content-Type: text/plain; charset=utf-8\r\n\r\n{long}\r\n").into_bytes();
        let preview = preview_from_source(raw).expect("has text");
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS);
    }

    #[test]
    fn a_body_with_no_text_yields_no_preview() {
        let raw = b"From: a@example.com\r\nSubject: Hi\r\n\
            Content-Type: text/plain\r\n\r\n   \r\n"
            .to_vec();
        assert_eq!(preview_from_source(raw), None);
    }
}
