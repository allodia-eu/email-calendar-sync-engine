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

/// The data an extended `LIST` asks to have returned beside each row (RFC 5258 §6). Each is
/// set only where the session advertised the extension that defines it: a return option a
/// server does not know is a `BAD`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ListReturn {
    /// `SPECIAL-USE` (RFC 6154): the role attributes on each `* LIST` line.
    pub(crate) special_use: bool,
    /// `STATUS (UNSEEN)` (RFC 5819): a `* STATUS` line with each folder's unread count.
    pub(crate) unseen: bool,
    /// `MYRIGHTS` (RFC 8440): a `* MYRIGHTS` line with the caller's rights on each folder.
    pub(crate) myrights: bool,
}

/// `LIST "" <pattern>` with the `returning` options. `pattern` arrives quoted and encoded
/// for this session's wire (`Connection::quoted_name`) — `"*"` for every folder, or one
/// store's subtree (`crate::store`).
///
/// An **extended** `LIST` returns exactly the extended data its return options name
/// (RFC 5258 §3), so an option left out is data not returned — including data the same
/// server volunteers on a plain `LIST`.
pub(crate) fn list_command(pattern: &str, returning: ListReturn) -> String {
    let mut options: Vec<&str> = Vec::new();
    if returning.special_use {
        options.push("SPECIAL-USE");
    }
    if returning.unseen {
        options.push("STATUS (UNSEEN)");
    }
    if returning.myrights {
        options.push("MYRIGHTS");
    }
    if options.is_empty() {
        return format!(r#"LIST "" {pattern}"#);
    }
    format!(r#"LIST "" {pattern} RETURN ({})"#, options.join(" "))
}

/// Formats a calendar date as the IMAP `d-Mon-yyyy` form `UID SEARCH SINCE` expects
/// (RFC 9051 §6.4.4), e.g. 2026-03-18 → `18-Mar-2026`. The month is a fixed English
/// abbreviation and the rest is digits, so the result is a safe, unquoted search atom.
pub(crate) fn format_imap_date(date: time::Date) -> String {
    let month = MONTHS[usize::from(u8::from(date.month())) - 1];
    format!("{}-{month}-{}", date.day(), date.year())
}

/// The calendar day an IMAP date names: the leading `d-Mon-yyyy` of a search date or an
/// `INTERNALDATE` (whose day may be space-padded), ignoring any time and zone after it, which
/// is how `SINCE` compares (RFC 9051 §6.4.4).
pub(crate) fn imap_day(text: &str) -> Option<time::Date> {
    let date = text.trim_start().split(' ').next()?;
    let mut parts = date.split('-');
    let day = parts.next()?.parse().ok()?;
    let month = parts.next()?;
    let month = MONTHS
        .iter()
        .position(|name| name.eq_ignore_ascii_case(month))?;
    let year = parts.next()?.parse().ok()?;
    let month = time::Month::try_from(u8::try_from(month + 1).ok()?).ok()?;
    time::Date::from_calendar_date(year, month, day).ok()
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

#[cfg(test)]
mod tests {
    use super::*;

    const ROLES: ListReturn = ListReturn {
        special_use: true,
        unseen: false,
        myrights: false,
    };

    #[test]
    fn a_plain_list_carries_no_return_clause() {
        // A `RETURN (…)` the server never advertised support for is a `BAD`.
        assert_eq!(
            list_command(r#""*""#, ListReturn::default()),
            r#"LIST "" "*""#
        );
    }

    #[test]
    fn each_advertised_extension_adds_its_own_option() {
        assert_eq!(
            list_command(r#""*""#, ROLES),
            r#"LIST "" "*" RETURN (SPECIAL-USE)"#
        );
        let unseen = ListReturn {
            unseen: true,
            ..ListReturn::default()
        };
        assert_eq!(
            list_command(r#""*""#, unseen),
            r#"LIST "" "*" RETURN (STATUS (UNSEEN))"#
        );
        let myrights = ListReturn {
            myrights: true,
            ..ListReturn::default()
        };
        assert_eq!(
            list_command(r#""*""#, myrights),
            r#"LIST "" "*" RETURN (MYRIGHTS)"#
        );
        // All three, in one round trip: an extended `LIST` returns only what it is asked
        // for, so the counts and the rights must not cost the roles.
        let all = ListReturn {
            special_use: true,
            unseen: true,
            myrights: true,
        };
        assert_eq!(
            list_command(r#""*""#, all),
            r#"LIST "" "*" RETURN (SPECIAL-USE STATUS (UNSEEN) MYRIGHTS)"#
        );
    }

    #[test]
    fn a_stores_subtree_is_listed_by_its_own_pattern() {
        // A shared store is asked for alone, and keeps the options the session can use.
        assert_eq!(
            list_command(r#""Shared Folders/bob@test.local/*""#, ROLES),
            r#"LIST "" "Shared Folders/bob@test.local/*" RETURN (SPECIAL-USE)"#
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
