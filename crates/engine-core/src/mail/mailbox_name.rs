//! The engine's portable policy for a folder's own name: what a host offers its user.
//!
//! A name that passes it can be held by every mail transport and means the same on each:
//! it is not empty, carries no control character, has no `/` (Gmail and most IMAP servers read
//! it as a level of the tree) and no surrounding whitespace (which no two servers treat
//! alike). It is a policy, not the transports' limit: JMAP leaves the name to the server
//! (RFC 8621 §2), an IMAP server whose delimiter is `.` holds a `/` as one level, and an IMAP
//! quoted string carries a tab.
//!
//! So the adapters do not apply it. Each refuses exactly the names its own transport cannot
//! carry, because a host that mirrors folders from one account to another must be able to
//! write a name the target accepts, and to move or trash a folder whose current name another
//! client chose. The policy is applied where a host asks for a name: by the host itself, and by
//! the outbox before it queues a change (`engine_sync::edit_mailbox`).

/// Why a folder name falls outside the portable policy.
///
/// The message says which rule the name breaks and never repeats the name, which is the
/// user's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MailboxNameError {
    /// The name is empty, or only whitespace.
    #[error("a folder name cannot be empty")]
    Empty,
    /// The name starts or ends with whitespace, which no two servers treat alike.
    #[error("a folder name cannot start or end with a space")]
    Surrounded,
    /// The name carries a control character.
    #[error("a folder name cannot contain a control character")]
    Control,
    /// The name carries `/`, which Gmail and most IMAP servers read as a level of the tree.
    #[error("a folder name cannot contain '/'")]
    Separator,
}

/// Checks a folder's own name against the engine's portable policy (see the module docs).
///
/// A host calls it while the user is still typing. Passing it is not every rule a server has:
/// an IMAP server whose hierarchy separator is `.` refuses a name containing one, and its
/// adapter says so when asked. Failing it does not mean a transport would refuse the name.
///
/// # Errors
///
/// Returns the [`MailboxNameError`] the name breaks.
pub fn validate_mailbox_name(name: &str) -> Result<(), MailboxNameError> {
    if name.trim().is_empty() {
        return Err(MailboxNameError::Empty);
    }
    if name.trim() != name {
        return Err(MailboxNameError::Surrounded);
    }
    if name.chars().any(char::is_control) {
        return Err(MailboxNameError::Control);
    }
    if name.contains('/') {
        return Err(MailboxNameError::Separator);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MailboxNameError, validate_mailbox_name};

    #[test]
    fn an_ordinary_name_passes_in_any_script() {
        assert_eq!(validate_mailbox_name("Receipts"), Ok(()));
        assert_eq!(validate_mailbox_name("Reçus 2024"), Ok(()));
        assert_eq!(validate_mailbox_name("Work.2024"), Ok(()));
    }

    #[test]
    fn names_outside_the_policy_are_caught() {
        assert_eq!(validate_mailbox_name(""), Err(MailboxNameError::Empty));
        assert_eq!(validate_mailbox_name("   "), Err(MailboxNameError::Empty));
        assert_eq!(
            validate_mailbox_name(" Bills"),
            Err(MailboxNameError::Surrounded)
        );
        assert_eq!(
            validate_mailbox_name("Bills\u{a0}"),
            Err(MailboxNameError::Surrounded)
        );
        assert_eq!(
            validate_mailbox_name("Bi\tlls"),
            Err(MailboxNameError::Control)
        );
        assert_eq!(
            validate_mailbox_name("Bi\r\nlls"),
            Err(MailboxNameError::Control)
        );
        assert_eq!(
            validate_mailbox_name("Bi\u{0}lls"),
            Err(MailboxNameError::Control)
        );
        assert_eq!(
            validate_mailbox_name("Work/2024"),
            Err(MailboxNameError::Separator)
        );
    }

    #[test]
    fn an_error_names_the_rule_and_never_the_name() {
        for name in ["", " Bills", "Bi\tlls", "Work/2024"] {
            let error = validate_mailbox_name(name).unwrap_err().to_string();
            assert!(error.starts_with("a folder name cannot"), "{error}");
            assert!(
                !error.contains("Bills") && !error.contains("Work"),
                "{error}"
            );
        }
    }
}
