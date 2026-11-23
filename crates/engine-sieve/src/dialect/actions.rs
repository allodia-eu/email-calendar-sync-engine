//! A rule's block, read into its actions and its stop.
//!
//! The commands the dialect writes, and reads back: `fileinto` (with `:copy`, `:specialuse`,
//! `:mailboxid`), `redirect` (with `:copy`), `discard`, `addflag`, `addheader "Importance"`, and a
//! last `stop`. Consecutive `redirect`s with the same `:copy` read as one action, and consecutive
//! `addflag`s as one, as they are written. Any other command (`keep`, `setflag`, `reject`,
//! `vacation`, …) leaves the rule one the engine carries whole.

use std::collections::BTreeSet;

use engine_core::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole},
    rules::{Action, MailboxPath, RuleAddress, RuleMailbox},
};

use super::cursor::Cursor;
use crate::lex::TokenKind;

/// Reads a `{ … }` block: its actions, and whether it ends in `stop`.
pub(super) fn read(cursor: &mut Cursor<'_, '_>) -> Option<(Vec<Action>, bool)> {
    cursor.eat(TokenKind::LeftBrace)?;
    let mut actions: Vec<Action> = Vec::new();
    let mut stop = false;
    while cursor.eat(TokenKind::RightBrace).is_none() {
        if stop {
            // Nothing the dialect writes follows a stop.
            return None;
        }
        let name = cursor.next()?;
        if name.kind() != TokenKind::Identifier {
            return None;
        }
        match name.text().to_ascii_lowercase().as_str() {
            "fileinto" => actions.push(fileinto(cursor)?),
            "redirect" => {
                let copy = flags(cursor, &["copy"])?.contains("copy");
                let to = RuleAddress::new(cursor.string()?);
                match actions.last_mut() {
                    Some(Action::Redirect {
                        to: list,
                        copy: was,
                    }) if *was == copy => {
                        list.push(to);
                    }
                    _ => actions.push(Action::Redirect { to: vec![to], copy }),
                }
            }
            "discard" => actions.push(Action::Discard),
            "addflag" => {
                let keywords = cursor
                    .strings()?
                    .iter()
                    .map(|flag| Keyword::from_flag(flag))
                    .collect::<Option<BTreeSet<Keyword>>>()?;
                match actions.last_mut() {
                    Some(Action::AddKeywords(set)) => set.extend(keywords),
                    _ => actions.push(Action::AddKeywords(keywords)),
                }
            }
            "addheader" => {
                let (name, value) = (cursor.string()?, cursor.string()?);
                if !name.eq_ignore_ascii_case("importance") {
                    return None;
                }
                let importance = [Importance::Low, Importance::Normal, Importance::High]
                    .into_iter()
                    .find(|level| level.as_str().eq_ignore_ascii_case(&value))?;
                actions.push(Action::MarkImportance(importance));
            }
            "stop" => stop = true,
            _ => return None,
        }
        cursor.eat(TokenKind::Semicolon)?;
    }
    Some((actions, stop))
}

/// The tags before a command's positional arguments, each one of `allowed`; `None` for any
/// other.
fn flags(cursor: &mut Cursor<'_, '_>, allowed: &[&str]) -> Option<BTreeSet<String>> {
    let mut found = BTreeSet::new();
    while cursor.peek()?.kind() == TokenKind::Tag {
        let tag = cursor.tag()?;
        if !allowed.contains(&tag.as_str()) {
            return None;
        }
        found.insert(tag);
    }
    Some(found)
}

fn fileinto(cursor: &mut Cursor<'_, '_>) -> Option<Action> {
    let (mut copy, mut role, mut id) = (false, None, None);
    while let Some(tag) = cursor.tag() {
        match tag.as_str() {
            "copy" => copy = true,
            "specialuse" => role = Some(cursor.string()?),
            "mailboxid" => id = Some(cursor.string()?),
            _ => return None,
        }
    }
    let path = cursor.string()?;
    let mailbox = match (role, id) {
        (Some(attribute), None) => match MailboxRole::from_imap_special_use(&attribute) {
            MailboxRole::Other(_) => return None,
            role => RuleMailbox::Role(role),
        },
        (None, Some(id)) => RuleMailbox::Id(MailboxId::try_from(id.as_str()).ok()?),
        // RFC 5228 §4.1: `INBOX` names the inbox, in any case.
        (None, None) if path.eq_ignore_ascii_case("inbox") => RuleMailbox::Role(MailboxRole::Inbox),
        (None, None) => RuleMailbox::Path(MailboxPath::new(path)),
        (Some(_), Some(_)) => return None,
    };
    Some(Action::FileInto { mailbox, copy })
}
