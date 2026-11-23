//! The rows of the Sieve column: one rule per cell, and what each server keeps of it.

use std::collections::BTreeSet;

use engine_core::{
    ids::MailboxId,
    mail::{Importance, Keyword, MailboxRole, SystemKeyword},
    rules::{
        Action, AddressField, AddressPart, Condition, HeaderField, Kept, MailRule, MailboxPath,
        Match, MeField, NativeAction, NativeCondition, NativeKind, RuleAddress, RuleMailbox,
        RuleName, SizeBound, TextOp, TextTest,
    },
};

fn text(op: TextOp) -> TextTest {
    TextTest::new(op, vec!["value".to_owned()])
}

fn when(condition: Condition) -> MailRule {
    MailRule::typed(Match::all(vec![condition]), vec![Action::Discard])
}

fn doing(action: Action) -> MailRule {
    MailRule::typed(Match::default(), vec![action])
}

fn to(count: usize) -> Vec<RuleAddress> {
    (0..count)
        .map(|i| RuleAddress::new(format!("to{i}@example.com")))
        .collect()
}

fn keywords(list: &[Keyword]) -> BTreeSet<Keyword> {
    list.iter().cloned().collect()
}

/// One cell: the rule, and what each server keeps of it, with whether a loss is approximated.
pub(crate) struct Cell {
    pub(crate) what: &'static str,
    pub(crate) rule: MailRule,
    pub(crate) stalwart: Kept,
    pub(crate) dovecot: Kept,
    pub(crate) approximated: bool,
}

fn cell(what: &'static str, rule: MailRule, stalwart: Kept, dovecot: Kept) -> Cell {
    Cell {
        what,
        rule,
        stalwart,
        dovecot,
        approximated: false,
    }
}

fn approximated(what: &'static str, rule: MailRule, stalwart: Kept, dovecot: Kept) -> Cell {
    Cell {
        approximated: true,
        ..cell(what, rule, stalwart, dovecot)
    }
}

pub(crate) fn table() -> Vec<Cell> {
    use Kept::{NotKept, Whole};
    let header = |op| Condition::Header {
        field: HeaderField::Subject,
        test: text(op),
    };
    let address = |field, part, op| Condition::Address {
        field,
        part,
        test: text(op),
    };
    vec![
        cell(
            "all of",
            MailRule::typed(
                Match::all(vec![
                    header(TextOp::Contains),
                    Condition::Size(SizeBound::Over(1)),
                ]),
                vec![Action::Discard],
            ),
            Whole,
            Whole,
        ),
        cell(
            "any of",
            MailRule::typed(
                Match::any(vec![header(TextOp::Contains), header(TextOp::Is)]),
                vec![Action::Discard],
            ),
            Whole,
            Whole,
        ),
        cell(
            "except",
            MailRule::typed(
                Match::all(vec![header(TextOp::Contains)]).except(vec![address(
                    AddressField::From,
                    AddressPart::Domain,
                    TextOp::Is,
                )]),
                vec![Action::Discard],
            ),
            Whole,
            Whole,
        ),
        cell(
            "any of, except",
            MailRule::typed(
                Match::any(vec![header(TextOp::Contains), header(TextOp::Matches)]).except(vec![
                    header(TextOp::Is),
                    Condition::Size(SizeBound::Under(9)),
                ]),
                vec![Action::Discard],
            ),
            Whole,
            Whole,
        ),
        cell(
            "subject contains",
            when(header(TextOp::Contains)),
            Whole,
            Whole,
        ),
        cell("subject is", when(header(TextOp::Is)), Whole, Whole),
        cell("subject glob", when(header(TextOp::Matches)), Whole, Whole),
        cell("subject regex", when(header(TextOp::Regex)), Whole, Whole),
        cell(
            "named header contains",
            when(Condition::Header {
                field: HeaderField::Named("X-Mailer".to_owned()),
                test: text(TextOp::Contains),
            }),
            Whole,
            Whole,
        ),
        cell(
            "from is",
            when(address(AddressField::From, AddressPart::All, TextOp::Is)),
            Whole,
            Whole,
        ),
        cell(
            "from contains",
            when(address(
                AddressField::From,
                AddressPart::All,
                TextOp::Contains,
            )),
            Whole,
            Whole,
        ),
        cell(
            "from domain",
            when(address(AddressField::From, AddressPart::Domain, TextOp::Is)),
            Whole,
            Whole,
        ),
        cell(
            "to local part",
            when(address(
                AddressField::To,
                AddressPart::LocalPart,
                TextOp::Is,
            )),
            Whole,
            Whole,
        ),
        cell(
            "cc",
            when(address(AddressField::Cc, AddressPart::All, TextOp::Is)),
            Whole,
            Whole,
        ),
        cell(
            "to or cc",
            when(address(
                AddressField::ToOrCc,
                AddressPart::All,
                TextOp::Matches,
            )),
            Whole,
            Whole,
        ),
        cell(
            "sent to me",
            when(Condition::SentToMe(MeField::To)),
            NotKept,
            NotKept,
        ),
        cell(
            "body contains",
            when(Condition::Body(text(TextOp::Contains))),
            Whole,
            Whole,
        ),
        cell(
            "body regex",
            when(Condition::Body(text(TextOp::Regex))),
            Whole,
            Whole,
        ),
        cell(
            "size over",
            when(Condition::Size(SizeBound::Over(5 << 20))),
            Whole,
            Whole,
        ),
        cell(
            "size under",
            when(Condition::Size(SizeBound::Under(1000))),
            Whole,
            Whole,
        ),
        approximated(
            "has attachment",
            when(Condition::HasAttachment),
            Whole,
            Whole,
        ),
        cell(
            "importance",
            when(Condition::Importance(Importance::High)),
            NotKept,
            NotKept,
        ),
        cell(
            "native condition",
            when(Condition::Native(NativeCondition::new(
                NativeKind::Graph,
                "{}",
            ))),
            NotKept,
            NotKept,
        ),
        cell(
            "file into a path",
            doing(Action::FileInto {
                mailbox: RuleMailbox::Path(MailboxPath::new("Board")),
                copy: false,
            }),
            Whole,
            Whole,
        ),
        cell(
            "file a copy",
            doing(Action::FileInto {
                mailbox: RuleMailbox::Path(MailboxPath::new("Board")),
                copy: true,
            }),
            Whole,
            Whole,
        ),
        cell(
            "file into the inbox",
            doing(Action::FileInto {
                mailbox: RuleMailbox::Role(MailboxRole::Inbox),
                copy: false,
            }),
            Whole,
            Whole,
        ),
        cell(
            "file into Trash by role (special-use)",
            doing(Action::FileInto {
                mailbox: RuleMailbox::Role(MailboxRole::Trash),
                copy: false,
            }),
            Whole,
            NotKept,
        ),
        cell(
            "file by id (mailboxid)",
            doing(Action::FileInto {
                mailbox: RuleMailbox::Id(MailboxId::try_from("a1b2").unwrap()),
                copy: false,
            }),
            Whole,
            NotKept,
        ),
        cell("discard", doing(Action::Discard), Whole, Whole),
        cell(
            "redirect",
            doing(Action::Redirect {
                to: to(1),
                copy: false,
            }),
            Whole,
            Whole,
        ),
        cell(
            "redirect a copy",
            doing(Action::Redirect {
                to: to(1),
                copy: true,
            }),
            Whole,
            Whole,
        ),
        cell(
            "redirect to two (maxNumberRedirects 1 on Stalwart)",
            doing(Action::Redirect {
                to: to(2),
                copy: false,
            }),
            NotKept,
            Whole,
        ),
        approximated(
            "forward",
            doing(Action::Forward { to: to(1) }),
            Whole,
            Whole,
        ),
        cell(
            "mark read and flag",
            doing(Action::AddKeywords(keywords(&[
                Keyword::system(SystemKeyword::Seen),
                Keyword::system(SystemKeyword::Flagged),
            ]))),
            Whole,
            Whole,
        ),
        cell(
            "a named keyword",
            doing(Action::AddKeywords(keywords(&[
                Keyword::new("$label1").unwrap()
            ]))),
            Whole,
            Whole,
        ),
        approximated(
            "mark importance (editheader)",
            doing(Action::MarkImportance(Importance::High)),
            Whole,
            NotKept,
        ),
        cell(
            "native action",
            doing(Action::Native(NativeAction::new(NativeKind::Graph, "{}"))),
            NotKept,
            NotKept,
        ),
        cell("stop", doing(Action::Discard).stopping(), Whole, Whole),
        cell("disabled", doing(Action::Discard).disabled(), Whole, Whole),
        cell(
            "named",
            doing(Action::Discard).named(RuleName::new("A name")),
            Whole,
            Whole,
        ),
    ]
}
