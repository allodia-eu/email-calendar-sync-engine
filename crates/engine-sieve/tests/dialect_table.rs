//! The Sieve column of the mapping (`docs/design/filters-and-out-of-office.md`, §5), one row per
//! cell, against the two Sieve servers the harness runs: what `plan_rules` keeps under each
//! one's extensions, and, for every rule kept, that the writer writes it and the reader reads
//! it back as it was.
//!
//! The extension lists are the ones each server listed (Stalwart v1.0.2's `sieveExtensions` and
//! `maxNumberRedirects: 1`; Dovecot 2.4.4 Pigeonhole's ManageSieve greeting and its
//! `sieve_max_redirects = 4`), as the design note records them.

#[path = "dialect_table/cells.rs"]
mod cells;

use cells::table;
use engine_core::rules::{Kept, LossReason, PlanMode, RuleBody, RuleVocabulary, plan_rules};
use engine_sieve::dialect::{read_rules, vocabulary, write_rules};

const STALWART: &[&str] = &[
    "body",
    "comparator-elbonia",
    "comparator-i;ascii-casemap",
    "comparator-i;ascii-numeric",
    "comparator-i;octet",
    "convert",
    "copy",
    "date",
    "duplicate",
    "editheader",
    "enclose",
    "encoded-character",
    "enotify",
    "envelope",
    "envelope-deliverby",
    "envelope-dsn",
    "environment",
    "ereject",
    "extlists",
    "extracttext",
    "fcc",
    "fileinto",
    "foreverypart",
    "ihave",
    "imap4flags",
    "include",
    "index",
    "mailbox",
    "mailboxid",
    "mboxmetadata",
    "mime",
    "redirect-deliverby",
    "redirect-dsn",
    "regex",
    "reject",
    "relational",
    "replace",
    "servermetadata",
    "spamtest",
    "spamtestplus",
    "special-use",
    "subaddress",
    "vacation",
    "vacation-seconds",
    "variables",
    "virustest",
];

const DOVECOT: &[&str] = &[
    "fileinto",
    "reject",
    "envelope",
    "encoded-character",
    "vacation",
    "subaddress",
    "comparator-i;ascii-numeric",
    "relational",
    "regex",
    "imap4flags",
    "copy",
    "include",
    "body",
    "variables",
    "enotify",
    "environment",
    "mailbox",
    "date",
    "index",
    "ihave",
    "duplicate",
    "mime",
    "foreverypart",
    "extracttext",
];

fn stalwart() -> RuleVocabulary {
    vocabulary(STALWART.iter().copied(), Some(1))
}

fn dovecot() -> RuleVocabulary {
    vocabulary(DOVECOT.iter().copied(), Some(4))
}

#[test]
fn each_cell_of_the_sieve_column_plans_as_the_design_says() {
    for row in table() {
        for (server, vocabulary, expected) in [
            ("Stalwart", stalwart(), row.stalwart),
            ("Dovecot", dovecot(), row.dovecot),
        ] {
            let plan = plan_rules(std::slice::from_ref(&row.rule), &vocabulary, PlanMode::Edit);
            let outcome = &plan.outcomes[0];
            assert_eq!(
                outcome.kept, expected,
                "{}: {server}: {:?}",
                row.what, outcome.losses
            );
            let approximated = outcome
                .losses
                .iter()
                .any(|loss| loss.reason == LossReason::Approximated);
            if expected == Kept::Whole {
                assert_eq!(approximated, row.approximated, "{}: {server}", row.what);
            }
        }
    }
}

#[test]
fn every_rule_a_server_keeps_is_written_and_reads_back_as_it_was() {
    for row in table() {
        let plan = plan_rules(std::slice::from_ref(&row.rule), &stalwart(), PlanMode::Edit);
        if row.stalwart != Kept::Whole {
            continue;
        }
        let script =
            write_rules("", &plan.written()).unwrap_or_else(|err| panic!("{}: {err}", row.what));
        let read = read_rules(&script);
        assert_eq!(read.len(), 1, "{}: {script:?}", row.what);
        let mut back = read[0].clone();
        back.origin = None;
        assert!(
            matches!(back.body, RuleBody::Typed(_)),
            "{}: {script:?}",
            row.what
        );
        if row.approximated {
            // Written as what the server can say: a forward reads back as a redirect it keeps.
            continue;
        }
        assert_eq!(back, row.rule, "{}: {script:?}", row.what);
    }
}

#[test]
fn every_extension_the_writer_uses_is_one_the_server_lists() {
    for (name, extensions, vocabulary) in [
        ("Stalwart", STALWART, stalwart()),
        ("Dovecot", DOVECOT, dovecot()),
    ] {
        for row in table() {
            let plan = plan_rules(std::slice::from_ref(&row.rule), &vocabulary, PlanMode::Edit);
            let script = write_rules("", &plan.written()).unwrap();
            let required =
                engine_sieve::require::required(&engine_sieve::segment(&script).unwrap()).unwrap();
            for extension in required {
                assert!(
                    extensions.contains(&extension.as_str()),
                    "{}: {name}: the writer required {extension}",
                    row.what
                );
            }
        }
    }
}
