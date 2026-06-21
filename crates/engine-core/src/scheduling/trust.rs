//! The iMIP trust decision: an inbound scheduling message may be auto-applied
//! only when its authenticated sender matches the body identity it claims to act
//! as (RFC 6047 §2.2.1/§2.3).

use super::ScheduleMethod;

/// The reason a scheduling message is not trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ImipUntrusted {
    /// The message carried no authenticated sender (e.g. unsigned), so nothing
    /// can be verified.
    #[error("scheduling message has no authenticated sender")]
    Unauthenticated,
    /// The relevant body identity (organizer or attendee) was absent.
    #[error("scheduling message is missing the identity to verify against")]
    MissingIdentity,
    /// The authenticated sender did not match the expected body identity.
    #[error("authenticated sender does not match the {expected} in the message body")]
    SenderMismatch {
        /// Which body identity was expected (`organizer` or `attendee`).
        expected: &'static str,
    },
}

/// The trust verdict for an inbound scheduling message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImipTrust {
    /// Safe to apply: the authenticated sender matches the body identity.
    Trusted,
    /// Must not be auto-applied; carries the reason.
    Untrusted(ImipUntrusted),
}

/// Normalizes a calendar address for comparison: lowercased, with a leading
/// `mailto:` scheme removed.
pub(super) fn normalize_address(address: &str) -> String {
    let lowered = address.trim().to_ascii_lowercase();
    lowered
        .strip_prefix("mailto:")
        .unwrap_or(&lowered)
        .to_owned()
}

/// Returns `true` if two calendar addresses are equal after normalization
/// (case-insensitive, scheme-insensitive).
pub(super) fn addresses_match(a: &str, b: &str) -> bool {
    normalize_address(a) == normalize_address(b)
}

/// Decides whether an inbound iMIP message may be auto-applied.
///
/// Trust is verified against the **body** identity (`ORGANIZER`/`ATTENDEE`),
/// never the email envelope `From` (RFC 6047 §2.3): an organizer-originated
/// method must match the `organizer`, an attendee-originated one the
/// `replying_attendee`. A message with no `authenticated_sender` is always
/// untrusted (unsigned messages may not be trusted, RFC 6047 §2.2.1).
#[must_use]
pub fn evaluate_imip_trust(
    method: &ScheduleMethod,
    organizer: Option<&str>,
    replying_attendee: Option<&str>,
    authenticated_sender: Option<&str>,
) -> ImipTrust {
    let Some(sender) = authenticated_sender else {
        return ImipTrust::Untrusted(ImipUntrusted::Unauthenticated);
    };
    let (expected, label) = if method.is_organizer_originated() {
        (organizer, "organizer")
    } else {
        (replying_attendee, "attendee")
    };
    match expected {
        None => ImipTrust::Untrusted(ImipUntrusted::MissingIdentity),
        Some(identity) if addresses_match(identity, sender) => ImipTrust::Trusted,
        Some(_) => ImipTrust::Untrusted(ImipUntrusted::SenderMismatch { expected: label }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_trusts_only_matching_organizer() {
        let trust = evaluate_imip_trust(
            &ScheduleMethod::Request,
            Some("mailto:boss@example.com"),
            None,
            Some("BOSS@example.com"), // case- and scheme-insensitive match
        );
        assert_eq!(trust, ImipTrust::Trusted);

        let spoofed = evaluate_imip_trust(
            &ScheduleMethod::Cancel,
            Some("boss@example.com"),
            None,
            Some("attacker@evil.example"),
        );
        assert_eq!(
            spoofed,
            ImipTrust::Untrusted(ImipUntrusted::SenderMismatch {
                expected: "organizer"
            })
        );
    }

    #[test]
    fn reply_trusts_only_matching_attendee() {
        let trust = evaluate_imip_trust(
            &ScheduleMethod::Reply,
            Some("boss@example.com"),
            Some("guest@example.com"),
            Some("guest@example.com"),
        );
        assert_eq!(trust, ImipTrust::Trusted);

        // A reply that authenticates as the organizer (not the attendee) is not
        // trusted to set the attendee's status.
        let mismatch = evaluate_imip_trust(
            &ScheduleMethod::Reply,
            Some("boss@example.com"),
            Some("guest@example.com"),
            Some("boss@example.com"),
        );
        assert_eq!(
            mismatch,
            ImipTrust::Untrusted(ImipUntrusted::SenderMismatch {
                expected: "attendee"
            })
        );
    }

    #[test]
    fn unauthenticated_message_is_never_trusted() {
        let trust = evaluate_imip_trust(
            &ScheduleMethod::Request,
            Some("boss@example.com"),
            None,
            None,
        );
        assert_eq!(trust, ImipTrust::Untrusted(ImipUntrusted::Unauthenticated));
    }

    #[test]
    fn missing_body_identity_is_untrusted() {
        // A REQUEST with no ORGANIZER cannot be verified against anything.
        let trust = evaluate_imip_trust(
            &ScheduleMethod::Request,
            None,
            None,
            Some("boss@example.com"),
        );
        assert_eq!(trust, ImipTrust::Untrusted(ImipUntrusted::MissingIdentity));
    }
}
