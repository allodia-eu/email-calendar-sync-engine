//! The `Debug` form of an identifier or cursor that can carry content.
//!
//! An id is opaque to the engine, but not empty of meaning: a Graph shared-mailbox handle or a
//! send-as identity is an address, a JMAP account id can be a login, an IMAP mailbox key holds
//! the folder's path, a `UID`, `Message-ID` or `Content-ID` often names the sender's host, and a
//! Graph cursor or page token is a URL whose path names the mailbox. So the type's own `Debug`
//! prints the value only where a failing test needs it, and its length otherwise.
//!
//! The rule lives here once, in two items:
//!
//! - [`SHOWS_VALUES`] says whether this build shows values: `true` when engine-core is compiled
//!   with debug assertions (Cargo's `dev` and `test` profiles), `false` without them (`release`).
//!   Cargo builds engine-core under the host's profile, so the **host's** `debug-assertions`
//!   setting decides: a host that ships a build with `debug-assertions = true`, or turns them on
//!   for engine-core alone, prints values.
//! - [`Content`] is a field's `Debug`: the quoted value as `#[derive(Debug)]` prints it, byte for
//!   byte (`"inbox"`), or `<redacted, 5 bytes>`. A type writes it inside its own `debug_tuple` or
//!   `debug_struct`, so the rest of its `Debug` reads as derived.
//!
//! What this covers is each such type's **own** `Debug`. A struct that holds an address or a
//! name in a plain `String` field (a mailbox's name, a participant's email), and an error whose
//! text quotes a value, still print it; those are a separate change. `Display` and `as_str()`
//! are unchanged: they are the deliberate way to reach the value, for the code that has to send
//! it. The raw payloads ([`crate::raw`]) and the rule and reply values redact in every build
//! instead, because no failing test needs their bytes either.

use core::fmt;

/// Whether `Debug` shows a value that can carry content: `true` where engine-core is compiled
/// with debug assertions, `false` without them.
///
/// The one place this is decided. [`Content`] reads it, and so does a test that states what a
/// type's `Debug` prints, rather than asking `cfg!(debug_assertions)` of its own crate, which
/// can differ (a doctest is compiled with debug assertions even under `cargo test --release`).
pub const SHOWS_VALUES: bool = cfg!(debug_assertions);

/// A string that can carry content, as a field of a type's `Debug`.
///
/// Prints the value quoted and escaped, exactly as `#[derive(Debug)]` would, where
/// [`SHOWS_VALUES`] holds; otherwise `<redacted, N bytes>`, `N` being its length in bytes.
/// It is named for what the value may be, content, while the module says what happens to it:
/// `redact::Content(..)` at the call site reads as both.
///
/// ```
/// use core::fmt;
///
/// use engine_core::redact;
///
/// struct Folder(String);
///
/// impl fmt::Debug for Folder {
///     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
///         f.debug_tuple("Folder")
///             .field(&redact::Content(&self.0))
///             .finish()
///     }
/// }
///
/// let shown = format!("{:?}", Folder("Archive/2026".to_owned()));
/// // Ask `SHOWS_VALUES`, not `cfg!(debug_assertions)`: rustdoc compiles this example with
/// // debug assertions even under `cargo test --release`, where engine-core has none.
/// if redact::SHOWS_VALUES {
///     assert_eq!(shown, r#"Folder("Archive/2026")"#);
/// } else {
///     assert_eq!(shown, "Folder(<redacted, 12 bytes>)");
/// }
/// ```
#[derive(Clone, Copy)]
pub struct Content<'a>(pub &'a str);

impl fmt::Debug for Content<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(f, SHOWS_VALUES)
    }
}

impl Content<'_> {
    /// The `Debug` with what to show given, so both forms run under test in every build.
    fn write(self, f: &mut fmt::Formatter<'_>, shows_value: bool) -> fmt::Result {
        if shows_value {
            fmt::Debug::fmt(self.0, f)
        } else {
            write!(f, "<redacted, {} bytes>", self.0.len())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A value written with what to show chosen by the test, so both forms run in every build.
    struct Forced(&'static str, bool);

    impl fmt::Debug for Forced {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            Content(self.0).write(f, self.1)
        }
    }

    /// A newtype and a struct around a value, as a type's own `Debug` writes them.
    struct Probe(Forced);

    impl fmt::Debug for Probe {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_tuple("Probe").field(&self.0).finish()
        }
    }

    struct Fields(Forced);

    impl fmt::Debug for Fields {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Fields").field("name", &self.0).finish()
        }
    }

    /// The derived `Debug`s a shown value must match byte for byte.
    #[derive(Debug)]
    #[expect(dead_code, reason = "only its derived Debug is read")]
    struct Derived(&'static str);

    #[derive(Debug)]
    #[expect(dead_code, reason = "only its derived Debug is read")]
    struct DerivedFields {
        name: &'static str,
    }

    #[test]
    fn a_shown_value_prints_as_the_derive_would() {
        for value in ["inbox", "a\"quoted\\ value\n", "zoë@example.com"] {
            let tuple = Probe(Forced(value, true));
            let derived = format!("{:?}", Derived(value)).replacen("Derived", "Probe", 1);
            assert_eq!(format!("{tuple:?}"), derived);
            let derived = format!("{:#?}", Derived(value)).replacen("Derived", "Probe", 1);
            assert_eq!(format!("{tuple:#?}"), derived);

            let fields = Fields(Forced(value, true));
            let derived = DerivedFields { name: value };
            let derived_line = format!("{derived:?}").replacen("DerivedFields", "Fields", 1);
            assert_eq!(format!("{fields:?}"), derived_line);
            let derived_pretty = format!("{derived:#?}").replacen("DerivedFields", "Fields", 1);
            assert_eq!(format!("{fields:#?}"), derived_pretty);
        }
    }

    #[test]
    fn a_redacted_value_prints_its_length_only() {
        let tuple = Probe(Forced("zoë@example.com", false));
        assert_eq!(format!("{tuple:?}"), "Probe(<redacted, 16 bytes>)");
        assert_eq!(
            format!("{tuple:#?}"),
            "Probe(\n    <redacted, 16 bytes>,\n)"
        );
        let fields = Fields(Forced("zoë@example.com", false));
        assert_eq!(
            format!("{fields:?}"),
            "Fields { name: <redacted, 16 bytes> }"
        );
    }

    #[test]
    fn debug_shows_what_the_build_says() {
        for value in ["inbox", "zoë@example.com"] {
            assert_eq!(
                format!("{:?}", Content(value)),
                format!("{:?}", Forced(value, SHOWS_VALUES))
            );
        }
    }
}
