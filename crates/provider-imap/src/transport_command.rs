//! Rendering the command strings [`crate::transport`] sends.
//!
//! Separate from the connection itself because *what* a command has to say is a protocol
//! question the session's negotiated capabilities answer, while sending it is not: the
//! same `LIST` is four different strings depending on which extensions the server
//! advertised, and getting that wrong costs data the server would happily have returned.

/// Wraps a value as an IMAP quoted string, escaping `\` and `"`.
pub(crate) fn quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// `LIST "" "*"` with the return options this session can use: `SPECIAL-USE` (RFC 6154)
/// where the server advertised it, and `STATUS (UNSEEN)` (RFC 5819) where the caller wants
/// the unread counts in the same round trip.
///
/// An **extended** `LIST` returns exactly the extended data its return options name
/// (RFC 5258 §3), so an option left out is data not returned — including data the same
/// server volunteers on a plain `LIST`.
pub(crate) fn list_command(special_use: bool, status_unseen: bool) -> String {
    let mut options: Vec<&str> = Vec::new();
    if special_use {
        options.push("SPECIAL-USE");
    }
    if status_unseen {
        options.push("STATUS (UNSEEN)");
    }
    if options.is_empty() {
        return r#"LIST "" "*""#.to_owned();
    }
    format!(r#"LIST "" "*" RETURN ({})"#, options.join(" "))
}

/// Formats a calendar date as the IMAP `d-Mon-yyyy` form `UID SEARCH SINCE` expects
/// (RFC 9051 §6.4.4), e.g. 2026-03-18 → `18-Mar-2026`. The month is a fixed English
/// abbreviation and the rest is digits, so the result is a safe, unquoted search atom.
pub(crate) fn format_imap_date(date: time::Date) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS[usize::from(u8::from(date.month())) - 1];
    format!("{}-{month}-{}", date.day(), date.year())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_list_carries_no_return_clause() {
        // A `RETURN (…)` the server never advertised support for is a `BAD`.
        assert_eq!(list_command(false, false), r#"LIST "" "*""#);
    }

    #[test]
    fn each_advertised_extension_adds_its_own_option() {
        assert_eq!(
            list_command(true, false),
            r#"LIST "" "*" RETURN (SPECIAL-USE)"#
        );
        assert_eq!(
            list_command(false, true),
            r#"LIST "" "*" RETURN (STATUS (UNSEEN))"#
        );
        // Both, in one round trip: an extended `LIST` returns only what it is asked for,
        // so the counts must not cost the roles.
        assert_eq!(
            list_command(true, true),
            r#"LIST "" "*" RETURN (SPECIAL-USE STATUS (UNSEEN))"#
        );
    }

    #[test]
    fn a_date_renders_as_the_unquoted_search_atom_imap_expects() {
        // RFC 9051 §6.4.4's `d-Mon-yyyy`: a fixed English month abbreviation and digits,
        // never a locale-formatted date and never a value needing quoting.
        let date = |y, m, d| time::Date::from_calendar_date(y, m, d).unwrap();
        assert_eq!(
            format_imap_date(date(2026, time::Month::March, 18)),
            "18-Mar-2026"
        );
        // A single-digit day is not zero-padded — the grammar allows either.
        assert_eq!(
            format_imap_date(date(2026, time::Month::January, 1)),
            "1-Jan-2026"
        );
        assert_eq!(
            format_imap_date(date(2025, time::Month::December, 31)),
            "31-Dec-2025"
        );
    }

    #[test]
    fn quoting_escapes_the_two_characters_that_would_end_the_string() {
        assert_eq!(quote("Sent"), r#""Sent""#);
        assert_eq!(quote(r#"od"d"#), r#""od\"d""#);
        assert_eq!(quote(r"back\slash"), r#""back\\slash""#);
    }
}
