//! The schemes any link in a message may carry.
//!
//! One allowlist for both directions: what a composer lets a message link to, what the reading
//! view's sanitiser keeps on an `<a href>`, and what a clicked link may be handed to the OS for.
//! A link the sanitiser strips can never be clicked, and a link a host sends should be one its own
//! reader would open, so the three cannot be allowed to disagree.

mod tel;

pub use tel::without_number_spaces;

/// The URL schemes a link may carry. Deliberately strict: never `javascript:`, `data:`, `file:`
/// or a custom application scheme. `tel` is here because dialling a number is something a message
/// legitimately asks for: the URI is handed to the OS as written, exactly as a `mailto:` is, and
/// the OS decides what its dialler makes of it.
pub const LINK_SCHEMES: [&str; 4] = ["http", "https", "mailto", "tel"];

/// Whether `url` starts with one of [`LINK_SCHEMES`].
#[must_use]
pub fn has_link_scheme(url: &str) -> bool {
    scheme_of(url).is_some_and(|scheme| LINK_SCHEMES.contains(&scheme.as_str()))
}

/// Whether `url` is a link a host sends, keeps and opens: one of [`LINK_SCHEMES`], and for
/// `tel:` a number RFC 3966 can write with no `*` or `#`.
#[must_use]
pub fn is_link_target(url: &str) -> bool {
    let url = url.trim();
    match scheme_of(url).as_deref() {
        Some("tel") => url
            .split_once(':')
            .is_some_and(|(_, subscriber)| tel::is_telephone_subscriber(subscriber)),
        Some(scheme) => LINK_SCHEMES.contains(&scheme),
        None => false,
    }
}

/// Whether `url` is a link a person may be handed to the OS for: [`is_link_target`] once a
/// `tel:` number's spaces are dropped, since a number is written with them and every dialler
/// ignores them. The one policy behind a clicked link in a message and a meeting's Join and
/// dial-in, so the two cannot disagree.
#[must_use]
pub fn is_openable_link(url: &str) -> bool {
    is_link_target(&without_number_spaces(url))
}

/// `url` in the form to hand the OS, when [`is_openable_link`] admits it: a `tel:` number without
/// its spaces, since a URI holding one is one some platforms will not parse; anything else as
/// written.
#[must_use]
pub fn openable_link(url: &str) -> Option<String> {
    let url = without_number_spaces(url);
    is_link_target(&url).then(|| url.into_owned())
}

/// The lowercased URL scheme (the part before the first `:`), if `url` starts with a
/// syntactically valid RFC 3986 scheme; `None` for a relative URL or anything not in `scheme:`
/// form. Byte-safe on hostile input: `split_once` and char iteration never slice mid-codepoint.
fn scheme_of(url: &str) -> Option<String> {
    let (scheme, _) = url.trim().split_once(':')?;
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then(|| scheme.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::{has_link_scheme, is_link_target, is_openable_link, openable_link};

    #[test]
    fn accepts_web_mail_and_telephone_links() {
        for good in [
            "https://example.com",
            "http://example.com/a?b=c&d=e#f",
            "HTTPS://Example.com",
            "mailto:someone@example.com",
            "tel:+15032223333",
            "tel:+44-20-7946-0958;ext=1234",
            "TEL:+15032223333",
            "  https://example.com  ",
        ] {
            assert!(is_link_target(good), "expected {good:?} to be kept");
        }
    }

    #[test]
    fn refuses_every_other_scheme_and_malformed_value() {
        for bad in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "data:text/html,<script>x</script>",
            "file:///etc/passwd",
            "myapp://open",
            "sms:+15032223333",
            "callto:+15032223333",
            "sip:ada@example.test",
            "whatsapp://send?phone=15032223333",
            "example.com",
            "/relative",
            "tel:",
            "tel:+1 503 222 3333",
            "tel:*21*+31612345678#",
            "tel:112",
            "",
        ] {
            assert!(!is_link_target(bad), "expected {bad:?} to be refused");
        }
    }

    #[test]
    fn the_scheme_check_is_case_insensitive_and_byte_safe() {
        assert!(has_link_scheme("MAILTO:a@b.example"));
        assert!(!has_link_scheme("ht tp://x"));
        assert!(!has_link_scheme("é:x"));
        assert!(!has_link_scheme("no scheme"));
    }

    #[test]
    fn a_number_written_with_spaces_opens_without_them() {
        assert!(is_openable_link("tel:+1 503 222 3333"));
        assert_eq!(
            openable_link("tel:+1 503 222 3333").as_deref(),
            Some("tel:+15032223333")
        );
        assert_eq!(
            openable_link("https://example.com/a").as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(openable_link("tel:*21#"), None);
        assert!(!is_openable_link("javascript:alert(1)"));
    }
}
