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
    fn quoting_escapes_the_two_characters_that_would_end_the_string() {
        assert_eq!(quote("Sent"), r#""Sent""#);
        assert_eq!(quote(r#"od"d"#), r#""od\"d""#);
        assert_eq!(quote(r"back\slash"), r#""back\\slash""#);
    }
}
