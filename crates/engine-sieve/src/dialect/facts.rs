//! What a command carried whole needs and does, read from its tokens: the extensions it
//! requires, whether it may send mail out of the account, and a fingerprint of the script it
//! came from.

use std::collections::BTreeSet;

use crate::lex::{Token, TokenKind};

/// Commands and tests of base Sieve (RFC 5228), which require nothing.
const BASE: &[&str] = &[
    "if", "elsif", "else", "require", "stop", "keep", "discard", "redirect", "true", "false",
    "not", "anyof", "allof", "header", "address", "exists", "size",
];

/// Commands and tests, and the extension each needs.
const IDENTIFIERS: &[(&str, &str)] = &[
    ("fileinto", "fileinto"),
    ("reject", "reject"),
    ("ereject", "ereject"),
    ("envelope", "envelope"),
    ("vacation", "vacation"),
    ("setflag", "imap4flags"),
    ("addflag", "imap4flags"),
    ("removeflag", "imap4flags"),
    ("hasflag", "imap4flags"),
    ("body", "body"),
    ("date", "date"),
    ("currentdate", "date"),
    ("include", "include"),
    ("return", "include"),
    ("global", "include"),
    ("set", "variables"),
    ("string", "variables"),
    ("notify", "enotify"),
    ("valid_notify_method", "enotify"),
    ("notify_method_capability", "enotify"),
    ("addheader", "editheader"),
    ("deleteheader", "editheader"),
    ("duplicate", "duplicate"),
    ("mailboxexists", "mailbox"),
    ("metadata", "mboxmetadata"),
    ("metadataexists", "mboxmetadata"),
    ("servermetadata", "servermetadata"),
    ("servermetadataexists", "servermetadata"),
    ("environment", "environment"),
    ("ihave", "ihave"),
    ("error", "ihave"),
    ("spamtest", "spamtest"),
    ("virustest", "virustest"),
    ("foreverypart", "foreverypart"),
    ("break", "foreverypart"),
    ("replace", "replace"),
    ("enclose", "enclose"),
    ("extracttext", "extracttext"),
    ("convert", "convert"),
];

/// Tags of base Sieve and of the commands above, which require nothing more.
const NEUTRAL_TAGS: &[&str] = &[
    "is",
    "contains",
    "matches",
    "all",
    "localpart",
    "domain",
    "over",
    "under",
    "comparator",
    "personal",
    "global",
    "optional",
    "once",
    "text",
    "raw",
    "content",
    "subject",
    "from",
    "addresses",
    "handle",
    "days",
    "importance",
    "message",
    "options",
    "upperlimit",
    "percent",
    "name",
    "first",
    "headers",
];

/// Tags that need an extension of their own.
const TAGS: &[(&str, &str)] = &[
    ("copy", "copy"),
    ("regex", "regex"),
    ("specialuse", "special-use"),
    ("mailboxid", "mailboxid"),
    ("create", "mailbox"),
    ("value", "relational"),
    ("count", "relational"),
    ("seconds", "vacation-seconds"),
    ("index", "index"),
    ("last", "index"),
    // Also `vacation :mime`, which needs no more than `vacation`: requiring `mime` too only
    // asks more of the target than it must, never less.
    ("mime", "mime"),
    ("anychild", "mime"),
    ("type", "mime"),
    ("subtype", "mime"),
    ("contenttype", "mime"),
    ("param", "mime"),
    ("zone", "date"),
    ("originalzone", "date"),
    ("user", "subaddress"),
    ("detail", "subaddress"),
    ("list", "extlists"),
    ("fcc", "fcc"),
    ("lower", "variables"),
    ("upper", "variables"),
    ("lowerfirst", "variables"),
    ("upperfirst", "variables"),
    ("quotewildcard", "variables"),
    ("length", "variables"),
];

/// The extensions `tokens` use, or `script_requires` (all the source script named) where a
/// token is one the tables above do not know, so nothing a command needs is ever left out.
pub(super) fn requirements(
    tokens: &[Token<'_>],
    script_requires: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut needs = BTreeSet::new();
    for token in tokens {
        let found = match token.kind() {
            TokenKind::Identifier => {
                let name = token.text().to_ascii_lowercase();
                if BASE.contains(&name.as_str()) {
                    continue;
                }
                IDENTIFIERS.iter().find(|(known, _)| *known == name)
            }
            TokenKind::Tag => {
                let name = token.text()[1..].to_ascii_lowercase();
                if NEUTRAL_TAGS.contains(&name.as_str()) {
                    continue;
                }
                TAGS.iter().find(|(known, _)| *known == name)
            }
            _ => continue,
        };
        match found {
            Some((_, extension)) => {
                needs.insert((*extension).to_owned());
            }
            None => return script_requires.clone(),
        }
    }
    // A comparator other than the two every interpreter has is an extension of its own.
    for pair in tokens
        .iter()
        .filter(|token| !token.kind().is_trivia())
        .collect::<Vec<_>>()
        .windows(2)
    {
        if pair[0].is_tag("comparator")
            && let Some(name) = crate::string::value(pair[1])
            && !matches!(name.as_str(), "i;octet" | "i;ascii-casemap")
        {
            needs.insert(format!("comparator-{name}"));
        }
    }
    needs
}

/// Whether `tokens` may send mail out of the account: a `redirect`, or a `notify` (enotify can
/// send mail).
pub(super) fn sends_mail(tokens: &[Token<'_>]) -> bool {
    tokens
        .iter()
        .any(|token| token.is_identifier("redirect") || token.is_identifier("notify"))
}

/// A fingerprint of a script's text (64-bit FNV-1a): the same on every platform and release,
/// unlike `std`'s hasher, so a rule's origin names the script it came from.
pub(super) fn fingerprint(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}
