//! Message headers and the parsed envelope.

use serde::{Deserialize, Serialize};

use super::EmailAddress;
use crate::ids::MessageIdHeader;

/// A single header in its raw form, preserving original capitalization and
/// source order (JMAP `EmailHeader`, RFC 8621 §4.1.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EmailHeader {
    /// The field name before the `:` separator, e.g. `To`.
    pub name: String,
    /// The field value after the `:` separator, in raw form.
    pub value: String,
}

impl EmailHeader {
    /// Creates a header from a name and raw value.
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// The parsed envelope: the standard addressing and threading headers projected
/// into typed form (IMAP `ENVELOPE`, RFC 9051 §7.5.2; JMAP convenience
/// properties, RFC 8621 §4.1.3).
///
/// Address lists are flattened to [`EmailAddress`]; group structure, if any, is
/// recoverable from the preserved raw message. The `message_id`, `in_reply_to`,
/// and `references` lists carry RFC 5322 `Message-ID` values **without** angle
/// brackets; they are threading and reconciliation hints, never identity, so a
/// missing (empty) or duplicate value is valid.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// The `Message-ID` header values (usually one; may be empty).
    pub message_id: Vec<MessageIdHeader>,
    /// The `In-Reply-To` header values.
    pub in_reply_to: Vec<MessageIdHeader>,
    /// The `References` header values, in order.
    pub references: Vec<MessageIdHeader>,
    /// The `Subject`, if present.
    pub subject: Option<String>,
    /// The `From` addresses.
    pub from: Vec<EmailAddress>,
    /// The `Sender` addresses (the actual sending mailbox; may differ from
    /// `from` in delegation/sharing scenarios).
    pub sender: Vec<EmailAddress>,
    /// The `Reply-To` addresses.
    pub reply_to: Vec<EmailAddress>,
    /// The `To` recipients.
    pub to: Vec<EmailAddress>,
    /// The `Cc` recipients.
    pub cc: Vec<EmailAddress>,
    /// The `Bcc` recipients.
    pub bcc: Vec<EmailAddress>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_preserves_name_and_value() {
        let header = EmailHeader::new("X-Custom", "  spacey value ");
        assert_eq!(header.name, "X-Custom");
        assert_eq!(header.value, "  spacey value ");
    }

    #[test]
    fn envelope_defaults_are_empty() {
        let env = Envelope::default();
        assert!(env.message_id.is_empty());
        assert!(env.from.is_empty());
        assert!(env.subject.is_none());
    }

    #[test]
    fn envelope_roundtrips_through_json() {
        let env = Envelope {
            message_id: vec![MessageIdHeader::new("abc@example.com").unwrap()],
            subject: Some("Hello".into()),
            from: vec![EmailAddress::named("Alice", "a@example.com")],
            to: vec![EmailAddress::new("b@example.com")],
            ..Envelope::default()
        };
        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(serde_json::from_str::<Envelope>(&json).unwrap(), env);
    }
}
