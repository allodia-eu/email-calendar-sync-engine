//! What a Sieve account can express, from the extensions its server lists.

use engine_core::rules::{RuleFeature, RuleSyntax, RuleVocabulary, Support, TextOp};

/// What the dialect can write for an account whose server lists `extensions` (JMAP Sieve's
/// `sieveExtensions`, RFC 9661 §1.3; the ManageSieve `SIEVE` capability, RFC 5804 §1.7) and
/// lets one message be sent on `max_redirects` times (`maxNumberRedirects`).
///
/// It is exactly what the writer renders, so a rule the plan keeps is a rule the writer can
/// write: base Sieve (RFC 5228) tests headers and addresses by `:is`, `:contains` and
/// `:matches`, combines them, excepts, sizes, redirects, discards and stops; `regex` adds
/// `:regex`, `body` the body, `fileinto` filing, `copy` filing and redirecting a copy (and
/// Exchange's forward, approximated as a `redirect :copy`), `special-use` and `mailboxid` filing
/// by role and by id, `imap4flags` keywords, `editheader` an importance (approximated as an
/// `Importance` header), and `mime` an attachment (approximated by a `Content-Disposition`
/// test). A rule's name, a disabled rule and the order are the dialect's own and read back
/// exactly. Whether the account's own addresses, a message's importance, or another provider's
/// own parts can be tested is not expressed: the writer does not render them.
pub fn vocabulary<'a>(
    extensions: impl IntoIterator<Item = &'a str>,
    max_redirects: Option<usize>,
) -> RuleVocabulary {
    let listed: Vec<&str> = extensions.into_iter().collect();
    let has = |name: &str| {
        listed
            .iter()
            .any(|listed| listed.eq_ignore_ascii_case(name))
    };
    let mut ops = vec![TextOp::Contains, TextOp::Is, TextOp::Matches];
    if has("regex") {
        ops.push(TextOp::Regex);
    }
    let mut vocabulary = RuleVocabulary::new()
        .with_exact([
            RuleFeature::MatchAny,
            RuleFeature::Except,
            RuleFeature::AddressLocalPart,
            RuleFeature::AddressDomain,
            RuleFeature::SizeOver,
            RuleFeature::SizeUnder,
            RuleFeature::Discard,
            RuleFeature::Redirect,
            RuleFeature::Stop,
            RuleFeature::Disabled,
            RuleFeature::Name,
            RuleFeature::Order,
        ])
        .with_opaque(
            RuleSyntax::Sieve,
            listed.iter().map(|name| (*name).to_owned()),
        );
    for op in &ops {
        vocabulary = vocabulary.with_exact([
            RuleFeature::SubjectText(*op),
            RuleFeature::HeaderText(*op),
            RuleFeature::AddressText(*op),
        ]);
        if has("body") {
            vocabulary = vocabulary.with_exact([RuleFeature::Body(*op)]);
        }
    }
    if has("fileinto") {
        vocabulary = vocabulary.with_exact([RuleFeature::FileInto]);
        if has("copy") {
            vocabulary = vocabulary.with_exact([RuleFeature::FileIntoCopy]);
        }
        if has("special-use") {
            vocabulary = vocabulary.with_exact([RuleFeature::FileIntoRole]);
        }
        if has("mailboxid") {
            vocabulary = vocabulary.with_exact([RuleFeature::FileIntoId]);
        }
    }
    if has("copy") {
        vocabulary = vocabulary
            .with_exact([RuleFeature::RedirectCopy])
            .with(RuleFeature::Forward, Support::Approximated);
    }
    if has("imap4flags") {
        vocabulary = vocabulary.with_exact([
            RuleFeature::MarkRead,
            RuleFeature::Flag,
            RuleFeature::NamedKeyword,
        ]);
    }
    if has("editheader") {
        vocabulary = vocabulary.with(RuleFeature::MarkImportance, Support::Approximated);
    }
    if has("mime") {
        vocabulary = vocabulary.with(RuleFeature::HasAttachment, Support::Approximated);
    }
    match max_redirects {
        Some(max) => vocabulary.with_max_redirects(max),
        None => vocabulary,
    }
}
