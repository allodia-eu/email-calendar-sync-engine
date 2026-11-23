//! The extensions a script requires (RFC 5228 §3.2), and one `require` that names them all.
//!
//! A script may require an extension in several `require` commands, before its first other
//! command. A writer that changes some rules may need an extension the script did not name
//! yet; it then writes one `require` naming everything the script named and everything the
//! changed rules need, in the layout Roundcube's managesieve plugin writes
//! (`require ["a","b"];` and CRLF, names sorted), and copies the script's other segments as
//! they were. It never drops a name: an unchanged segment the engine cannot read may be the
//! one that uses it.

use std::collections::BTreeSet;

use crate::{
    SieveError, SieveErrorKind,
    lex::{Token, TokenKind},
    segment::{Segment, SegmentKind},
    string,
};

/// The extensions one `require` segment names, in its order.
///
/// # Errors
///
/// [`SieveErrorKind::MalformedRequire`] where the argument is not one string or a list of
/// strings followed by `;`.
pub fn capabilities(segment: &Segment<'_>) -> Result<Vec<String>, SieveError> {
    let malformed = || SieveError::new(SieveErrorKind::MalformedRequire, segment.start());
    let significant: Vec<&Token<'_>> = segment
        .tokens()
        .iter()
        .filter(|token| !token.kind().is_trivia())
        .collect();
    let [name, arguments @ .., last] = significant.as_slice() else {
        return Err(malformed());
    };
    if !name.is_identifier("require") || last.kind() != TokenKind::Semicolon {
        return Err(malformed());
    }
    let strings = match arguments {
        [single] => std::slice::from_ref(single),
        [open, inner @ .., close]
            if open.kind() == TokenKind::LeftBracket && close.kind() == TokenKind::RightBracket =>
        {
            inner
        }
        _ => return Err(malformed()),
    };
    let mut names = Vec::new();
    for (i, token) in strings.iter().enumerate() {
        if i % 2 == 1 {
            if token.kind() != TokenKind::Comma {
                return Err(malformed());
            }
            continue;
        }
        names.push(string::value(token).ok_or_else(malformed)?);
    }
    if strings.len() % 2 == 0 {
        // Empty, or a trailing comma: no list the grammar allows.
        return Err(malformed());
    }
    Ok(names)
}

/// Every extension the script's `require` segments name.
///
/// # Errors
///
/// What [`capabilities`] answers for the first malformed `require`.
pub fn required(segments: &[Segment<'_>]) -> Result<BTreeSet<String>, SieveError> {
    let mut names = BTreeSet::new();
    for segment in segments
        .iter()
        .filter(|segment| segment.kind() == SegmentKind::Require)
    {
        names.extend(capabilities(segment)?);
    }
    Ok(names)
}

/// One `require` naming `names`, as Roundcube writes it: `require ["a","b"];` and CRLF, names
/// sorted. `None` where `names` is empty, or where a name cannot be a quoted string.
pub fn require_command(names: &BTreeSet<String>) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let quoted: Option<Vec<String>> = names.iter().map(|name| string::quoted(name)).collect();
    Some(format!("require [{}];\r\n", quoted?.join(",")))
}

/// The `require` a script needs once its changed rules need `needed`: `None` where its own
/// `require` segments already name all of it, else one naming their union ([`require_command`]).
///
/// # Errors
///
/// What [`required`] answers.
pub fn merged(
    segments: &[Segment<'_>],
    needed: &BTreeSet<String>,
) -> Result<Option<String>, SieveError> {
    let have = required(segments)?;
    if needed.is_subset(&have) {
        return Ok(None);
    }
    Ok(require_command(&have.union(needed).cloned().collect()))
}

#[cfg(test)]
#[path = "require_tests.rs"]
mod tests;
