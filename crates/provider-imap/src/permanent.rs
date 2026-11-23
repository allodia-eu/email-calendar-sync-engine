//! Which keywords a mailbox keeps: the one rule every write that sets a keyword follows.
//!
//! An `APPEND` (a Sent copy, a draft, an import) and a `STORE` (a report verdict, the keywords
//! of a copy already placed) all ask the same question of the folder they write to, and a
//! server that cannot keep a flag answers `OK` and drops it, so the answer must come from what
//! the folder said when it was opened. Read once, here, so the paths cannot drift apart.

use std::collections::BTreeSet;

use engine_core::mail::Keyword;

use crate::mail::{flags_to_keywords, keyword_to_flag};

/// What a mailbox said, on being opened, about the keywords it keeps (RFC 9051 §7.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PermanentFlags {
    /// The keywords it keeps because it names them: the system flags and keywords its
    /// `PERMANENTFLAGS` lists, or, where it sent none, its `FLAGS`.
    listed: BTreeSet<Keyword>,
    /// Whether it keeps any keyword besides, `\*` in `PERMANENTFLAGS`: the client may create a
    /// new one by storing it. Never the four system flags, which a server keeps only by naming.
    new_keywords: bool,
}

impl PermanentFlags {
    /// A mailbox that keeps no keyword at all: what a write assumes of one it could not open.
    pub(crate) fn none() -> Self {
        Self::default()
    }

    /// Reads what a `SELECT`/`EXAMINE` answered.
    ///
    /// `PERMANENTFLAGS` lists the flags a client can change permanently, and `\*` in it says new
    /// keywords can be created. Where a server sends no `PERMANENTFLAGS`, RFC 9051 §6.3.2 says to
    /// assume every flag can be changed permanently: read as every flag its `FLAGS` response
    /// names. Creating a keyword it does not name is stated only by `\*`, so it is not assumed.
    pub(crate) fn from_select(lines: &[Vec<u8>]) -> Self {
        if let Some(tokens) = lines
            .iter()
            .find_map(|line| list_after(line, b"[PERMANENTFLAGS ("))
        {
            return Self {
                new_keywords: tokens.iter().any(|token| token == "\\*"),
                listed: flags_to_keywords(&tokens),
            };
        }
        let flags = lines
            .iter()
            .find_map(|line| list_after(line, b"* FLAGS ("))
            .unwrap_or_default();
        Self {
            listed: flags_to_keywords(&flags),
            new_keywords: false,
        }
    }

    /// Whether the mailbox keeps `keyword` once it is stored.
    pub(crate) fn keeps(&self, keyword: &Keyword) -> bool {
        self.listed.contains(keyword) || (self.new_keywords && !is_system_flag(keyword))
    }

    /// The keywords of `asked` the mailbox keeps.
    pub(crate) fn keepable(&self, asked: &BTreeSet<Keyword>) -> BTreeSet<Keyword> {
        asked
            .iter()
            .filter(|keyword| self.keeps(keyword))
            .cloned()
            .collect()
    }
}

/// Whether `keyword` travels as one of IMAP's backslash system flags (`\Seen`, …) rather than as
/// a keyword atom.
fn is_system_flag(keyword: &Keyword) -> bool {
    keyword_to_flag(keyword).starts_with('\\')
}

/// The flag tokens of the parenthesised list that follows `opening` on `line`, matched without
/// regard to case, or `None` when the line carries no such list.
///
/// Read on the raw bytes rather than through the item tokenizer: `\*` is a flag token, not an
/// atom, and a response code's brackets are not part of the list.
fn list_after(line: &[u8], opening: &[u8]) -> Option<Vec<String>> {
    let start = line
        .windows(opening.len())
        .position(|window| window.eq_ignore_ascii_case(opening))?
        + opening.len();
    let rest = &line[start..];
    let end = rest.iter().position(|&byte| byte == b')')?;
    Some(
        String::from_utf8_lossy(&rest[..end])
            .split_ascii_whitespace()
            .map(str::to_owned)
            .collect(),
    )
}

#[cfg(test)]
#[path = "permanent_tests.rs"]
mod tests;
