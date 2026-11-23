//! A typed rule written in the dialect, as Roundcube's managesieve plugin writes one.
//!
//! `# rule:[name]` at the start of a line, then `if <test>`, a line with `{`, one action per
//! line indented by a tab, and a line with `}`, every line ending in CRLF; a disabled rule as
//! `if false # <test>`, which holds a test on one line only. The fixtures in
//! `tests/fixtures/roundcube/` are the plugin's own bytes, and the writer's output for the same
//! rules is compared with them byte for byte.
//!
//! The actions are written marks first (`addflag`, `addheader`), then the rest in the rule's
//! order: a flag set after a `fileinto` does not reach the copy already filed (RFC 5232 §3),
//! so the model's action order says nothing about when a keyword is added, and the writer adds
//! it before the message is delivered anywhere.
//!
//! A role is filed by `:specialuse` with its attribute's name as the fallback folder
//! (`\Trash` falls back to `Trash`), the conventional name a server without the role most
//! likely has; the inbox needs no role and is `INBOX`. An id is filed by `:mailboxid` with
//! `INBOX` as the fallback ([`ID_FALLBACK`]), which is where the message would have gone
//! without the rule.

use std::collections::BTreeSet;

use engine_core::{
    mail::{Keyword, MailboxRole},
    rules::{
        Action, AddressField, AddressPart, Condition, HeaderField, MailRule, Match, MatchMode,
        RuleAddress, RuleBody, RuleMailbox, SizeBound, TextOp, TextTest,
    },
};

use crate::string;

/// Where a rule filing by id falls back to, where no folder has the id (RFC 9042 §3).
pub(super) const ID_FALLBACK: &str = "INBOX";

/// Why a rule could not be written: the plan should have kept it from the writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Unrenderable;

type Rendered = Result<String, Unrenderable>;

/// The extensions a rendered rule requires.
pub(super) type Extensions = BTreeSet<&'static str>;

/// `rule` in the dialect, with `kept` (comments that led the rule before, without its name
/// line) before its name, and the extensions it requires.
pub(super) fn rule(rule: &MailRule, kept: &str, needs: &mut Extensions) -> Rendered {
    let RuleBody::Typed(typed) = &rule.body else {
        return Err(Unrenderable);
    };
    // The name line starts its line: an indent that led the rule would hide it from a reader.
    let mut out = String::from(kept.trim_end_matches([' ', '\t']));
    if let Some(name) = &rule.name {
        out.push_str("# rule:[");
        out.push_str(name.as_str());
        out.push_str("]\r\n");
    }
    let test = test(&typed.when, needs)?;
    if rule.enabled {
        out.push_str("if ");
        out.push_str(&test);
    } else if test.contains(['\r', '\n']) {
        // A comment ends at its line's end, so a test that spans lines cannot live in one. The
        // reader takes `if allof (false, …)` as disabled, but Roundcube reads that as a rule that
        // runs, and a save there could re-arm it, so the writer does not write it: the plan
        // keeps no such rule (`RuleFeature::DisabledMultiLine`).
        return Err(Unrenderable);
    } else {
        out.push_str("if false # ");
        out.push_str(&test);
    }
    out.push_str("\r\n{\r\n");
    let marks =
        |action: &&Action| matches!(action, Action::AddKeywords(_) | Action::MarkImportance(_));
    let ordered = typed
        .actions
        .iter()
        .filter(marks)
        .chain(typed.actions.iter().filter(|action| !marks(action)));
    for action in ordered {
        for command in action_commands(action, needs)? {
            out.push('\t');
            out.push_str(&command);
            out.push_str(";\r\n");
        }
    }
    if typed.stop {
        out.push_str("\tstop;\r\n");
    }
    out.push_str("}\r\n");
    Ok(out)
}

fn test(when: &Match, needs: &mut Extensions) -> Rendered {
    let conditions = when
        .conditions
        .iter()
        .map(|condition| self::condition(condition, needs))
        .collect::<Result<Vec<_>, _>>()?;
    let except = when
        .except
        .iter()
        .map(|condition| Ok(format!("not {}", self::condition(condition, needs)?)))
        .collect::<Result<Vec<_>, _>>()?;
    let matched = match (conditions.len(), when.mode) {
        (0, _) => None,
        (1, _) => Some(conditions[0].clone()),
        (_, MatchMode::All) => Some(format!("allof ({})", conditions.join(", "))),
        (_, MatchMode::Any) => Some(format!("anyof ({})", conditions.join(", "))),
    };
    Ok(match (matched, except.as_slice()) {
        (None, []) => "true".to_owned(),
        (None, [one]) => one.clone(),
        (None, _) => format!("allof ({})", except.join(", ")),
        (Some(matched), []) => matched,
        (Some(_), _) if when.mode == MatchMode::All || conditions.len() == 1 => {
            let mut items = conditions;
            items.extend(except);
            format!("allof ({})", items.join(", "))
        }
        (Some(any), _) => format!("allof ({any}, {})", except.join(", ")),
    })
}

fn condition(condition: &Condition, needs: &mut Extensions) -> Rendered {
    Ok(match condition {
        Condition::Header { field, test } => {
            let name = match field {
                HeaderField::Named(name) if !field.is_subject() => name.as_str(),
                _ => "subject",
            };
            format!(
                "header {} {} {}",
                op(test.op(), needs)?,
                value(name)?,
                values(test)?
            )
        }
        Condition::Address { field, part, test } => {
            let part = match part {
                AddressPart::All => "",
                AddressPart::LocalPart => ":localpart ",
                AddressPart::Domain => ":domain ",
                _ => return Err(Unrenderable),
            };
            let fields = match field {
                AddressField::From => "\"from\"",
                AddressField::To => "\"to\"",
                AddressField::Cc => "\"cc\"",
                AddressField::ToOrCc => "[\"to\",\"cc\"]",
                _ => return Err(Unrenderable),
            };
            format!(
                "address {part}{} {fields} {}",
                op(test.op(), needs)?,
                values(test)?
            )
        }
        Condition::Body(test) => {
            needs.insert("body");
            format!("body :text {} {}", op(test.op(), needs)?, values(test)?)
        }
        Condition::Size(SizeBound::Over(bytes)) => format!("size :over {}", size(*bytes)),
        Condition::Size(SizeBound::Under(bytes)) => format!("size :under {}", size(*bytes)),
        Condition::HasAttachment => {
            needs.insert("mime");
            "header :mime :anychild :contains \"Content-Disposition\" \"attachment\"".to_owned()
        }
        _ => return Err(Unrenderable),
    })
}

fn op(op: TextOp, needs: &mut Extensions) -> Result<&'static str, Unrenderable> {
    Ok(match op {
        TextOp::Contains => ":contains",
        TextOp::Is => ":is",
        TextOp::Matches => ":matches",
        TextOp::Regex => {
            needs.insert("regex");
            ":regex"
        }
        _ => return Err(Unrenderable),
    })
}

/// A size in bytes, with the largest quantifier that keeps it exact (`5M`, as Roundcube writes
/// what a user typed).
fn size(bytes: u64) -> String {
    for (unit, suffix) in [(1u64 << 30, "G"), (1 << 20, "M"), (1 << 10, "K")] {
        if bytes != 0 && bytes.is_multiple_of(unit) {
            return format!("{}{suffix}", bytes / unit);
        }
    }
    bytes.to_string()
}

/// One value: a quoted string, or a `text:` block where it spans lines (as Roundcube writes it:
/// the block's value is the text with a line end added, which the reader takes off again).
fn value(text: &str) -> Rendered {
    if text.contains(['\r', '\n']) {
        return string::multi_line(&format!("{text}\r\n")).ok_or(Unrenderable);
    }
    string::quoted(text).ok_or(Unrenderable)
}

fn values(test: &TextTest) -> Rendered {
    list(test.values())
}

/// One value as itself, several as `["a","b"]`.
fn list<S: AsRef<str>>(items: &[S]) -> Rendered {
    match items {
        [] => Err(Unrenderable),
        [one] => value(one.as_ref()),
        many => {
            let rendered = many
                .iter()
                .map(|item| value(item.as_ref()))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("[{}]", rendered.join(",")))
        }
    }
}

/// The commands one action is written as: one `redirect` per address.
fn action_commands(action: &Action, needs: &mut Extensions) -> Result<Vec<String>, Unrenderable> {
    let copy = |copy: bool, needs: &mut Extensions| {
        if copy {
            needs.insert("copy");
            ":copy "
        } else {
            ""
        }
    };
    Ok(match action {
        Action::FileInto {
            mailbox,
            copy: keep,
        } => {
            needs.insert("fileinto");
            let copy = copy(*keep, needs);
            let target = match mailbox {
                RuleMailbox::Path(path) => value(path.as_str())?,
                RuleMailbox::Role(MailboxRole::Inbox) => "\"INBOX\"".to_owned(),
                RuleMailbox::Role(role) => {
                    let attribute = role.as_imap_special_use().ok_or(Unrenderable)?;
                    needs.insert("special-use");
                    // RFC 8579: the named folder is where it goes if none has the role.
                    format!(
                        ":specialuse {} {}",
                        value(attribute)?,
                        value(&attribute[1..])?
                    )
                }
                RuleMailbox::Id(id) => {
                    needs.insert("mailboxid");
                    format!(":mailboxid {} \"INBOX\"", value(id.as_str())?)
                }
                _ => return Err(Unrenderable),
            };
            vec![format!("fileinto {copy}{target}")]
        }
        Action::Discard => vec!["discard".to_owned()],
        Action::Redirect { to, copy: keep } => {
            let copy = copy(*keep, needs);
            redirects(to.iter().map(RuleAddress::as_str), copy)?
        }
        // Exchange's forward sends as the account; a Sieve redirect keeps the sender. The plan
        // states the difference.
        Action::Forward { to } => {
            let copy = copy(true, needs);
            redirects(to.iter().map(RuleAddress::as_str), copy)?
        }
        Action::AddKeywords(keywords) => {
            needs.insert("imap4flags");
            let flags: Vec<String> = keywords.iter().map(Keyword::to_flag).collect();
            vec![format!("addflag {}", list(&flags)?)]
        }
        Action::MarkImportance(importance) => {
            needs.insert("editheader");
            vec![format!(
                "addheader \"Importance\" {}",
                value(importance.as_str())?
            )]
        }
        _ => return Err(Unrenderable),
    })
}

fn redirects<'a>(
    to: impl Iterator<Item = &'a str>,
    copy: &str,
) -> Result<Vec<String>, Unrenderable> {
    let commands = to
        .map(|address| Ok(format!("redirect {copy}{}", value(address)?)))
        .collect::<Result<Vec<_>, _>>()?;
    if commands.is_empty() {
        return Err(Unrenderable);
    }
    Ok(commands)
}
