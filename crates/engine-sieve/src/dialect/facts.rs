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

/// Tags of base Sieve and of the commands above, which require nothing more than their
/// command does. Read against each extension's RFC.
const NEUTRAL_TAGS: &[&str] = &[
    // RFC 5228: match types, address parts, size, comparator (whose name is checked below).
    "is",
    "contains",
    "matches",
    "all",
    "localpart",
    "domain",
    "over",
    "under",
    "comparator",
    // RFC 6609 include.
    "personal",
    "global",
    "optional",
    "once",
    // RFC 5173 body transforms.
    "text",
    "raw",
    "content",
    // RFC 5230 vacation; RFC 5435 notify.
    "subject",
    "from",
    "addresses",
    "handle",
    "days",
    "importance",
    "message",
    "options",
    // RFC 7352 duplicate.
    "header",
    "uniqueid",
    // RFC 5703 foreverypart and RFC 5703 extracttext.
    "name",
    "first",
];

/// Tags that need an extension of their own, whichever command carries them.
const TAGS: &[(&str, &str)] = &[
    ("copy", "copy"),
    ("regex", "regex"),
    ("specialuse", "special-use"),
    ("mailboxid", "mailboxid"),
    ("create", "mailbox"),
    ("value", "relational"),
    ("count", "relational"),
    ("index", "index"),
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
    ("percent", "spamtestplus"),
    ("lower", "variables"),
    ("upper", "variables"),
    ("lowerfirst", "variables"),
    ("upperfirst", "variables"),
    ("quotewildcard", "variables"),
    ("length", "variables"),
];

/// What a token known to the tables needs.
enum Need {
    /// Nothing more than its command needs.
    Nothing,
    /// This extension.
    Extension(&'static str),
}

/// A tag whose extension depends on the command or test it belongs to: `:seconds` is
/// `vacation-seconds` on `vacation` (RFC 6131) and `duplicate`'s own; `:last` is `index` on a
/// test (RFC 5260) and `editheader`'s or `duplicate`'s own on theirs; `:mime` is `mime` on a test
/// (RFC 5703) and `vacation`'s own on `vacation`. `None`: not one of these tags.
fn contextual(tag: &str, command: &str) -> Option<Need> {
    Some(match (tag, command) {
        ("seconds", "vacation") => Need::Extension("vacation-seconds"),
        ("seconds", "duplicate")
        | ("last", "addheader" | "deleteheader" | "duplicate")
        | ("mime", "vacation") => Need::Nothing,
        ("last", _) => Need::Extension("index"),
        ("mime", _) => Need::Extension("mime"),
        _ => return None,
    })
}

/// Whether `tokens` name a folder by its id (`fileinto :mailboxid`, RFC 9042), an id of their own
/// account's.
pub(super) fn names_folders_by_id(tokens: &[Token<'_>]) -> bool {
    tokens.iter().any(|token| token.is_tag("mailboxid"))
}

/// Whether `tokens` run another script of their account (`include`, RFC 6609).
pub(super) fn includes_scripts(tokens: &[Token<'_>]) -> bool {
    tokens.iter().any(|token| token.is_identifier("include"))
}

/// The extensions `tokens` use. Where a token is one the tables above do not know, everything
/// `script_requires` names (the source script's `require`s), so nothing a command needs is ever
/// left out; `None` where there is no source script to fall back to.
pub(super) fn requirements(
    tokens: &[Token<'_>],
    script_requires: Option<&BTreeSet<String>>,
) -> Option<BTreeSet<String>> {
    let mut needs = BTreeSet::new();
    let mut command = String::new();
    for token in tokens {
        let found = match token.kind() {
            TokenKind::Identifier => {
                command = token.text().to_ascii_lowercase();
                if BASE.contains(&command.as_str()) {
                    continue;
                }
                IDENTIFIERS
                    .iter()
                    .find(|(known, _)| *known == command)
                    .map(|(_, extension)| Need::Extension(extension))
            }
            TokenKind::Tag => {
                let name = token.text()[1..].to_ascii_lowercase();
                if NEUTRAL_TAGS.contains(&name.as_str()) {
                    continue;
                }
                contextual(&name, &command).or_else(|| {
                    TAGS.iter()
                        .find(|(known, _)| *known == name)
                        .map(|(_, extension)| Need::Extension(extension))
                })
            }
            _ => continue,
        };
        match found {
            Some(Need::Extension(extension)) => {
                needs.insert(extension.to_owned());
            }
            Some(Need::Nothing) => {}
            None => return script_requires.cloned(),
        }
    }
    // A comparator other than the two every interpreter has is an extension of its own
    // (RFC 5228 §2.7.3).
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
    Some(needs)
}

/// Whether `tokens` may send mail out of the account: unless every command and test in them is
/// one known to send none. A `redirect`, a `notify` (enotify can send mail), an `include` (the
/// script it runs may redirect), and anything this table does not know (`pipe`, `execute`, a
/// vendor's report command) may. A `vacation` or a `reject` answers the sender alone and carries
/// the message to nobody else, so neither counts.
pub(super) fn sends_mail(tokens: &[Token<'_>]) -> bool {
    tokens
        .iter()
        .filter(|token| token.kind() == TokenKind::Identifier)
        .any(|token| {
            let name = token.text().to_ascii_lowercase();
            let known = BASE.contains(&name.as_str())
                || IDENTIFIERS.iter().any(|(known, _)| *known == name);
            !known || matches!(name.as_str(), "redirect" | "notify" | "include")
        })
}

/// Whether a string in `tokens` holds `${`, which a script requiring `variables` or
/// `encoded-character` expands rather than reads as written.
pub(super) fn holds_expansion(tokens: &[Token<'_>]) -> bool {
    tokens
        .iter()
        .filter_map(crate::string::value)
        .any(|value| value.contains("${"))
}

/// A fingerprint of a script's text (64-bit FNV-1a): the same on every platform and release,
/// unlike `std`'s hasher, so a rule's origin names the script it came from.
pub(super) fn fingerprint(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}
