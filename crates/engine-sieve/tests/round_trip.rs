//! The segmenter's promise, over many scripts: the segments' texts, in order, are the script,
//! byte for byte; there is one segment per top-level command; and each segment, split on its
//! own, is that one command again. The first holds by construction (the segments are adjacent
//! spans), so it proves little alone.
//!
//! Only the generated scripts carry an oracle, the commands they were built from, so only they
//! catch a lexer that misreads a string, a comment or a bracket consistently (seen red with
//! backslash escapes ignored). The arbitrary ones have none: they catch a splitter that panics
//! or cuts a command off at the wrong place (seen red with the rest of a line always taken).
//!
//! The scripts are generated from a seeded generator, so a failure reproduces, out of every
//! construct that can hide a `;` or a `}` from a careless splitter: quoted strings with escapes,
//! `text:` blocks with dot-stuffing, both comment forms, nested blocks, test lists, `elsif` and
//! `else` chains, and CRLF and LF line ends. The fuzz target (`fuzz/fuzz_targets/sieve_segment.rs`)
//! asserts the same over arbitrary bytes.

use std::fmt::Write as _;

use engine_sieve::{
    Segment, SegmentKind,
    dialect::{read_rules, write_rules},
    segment,
};

/// Each command segment, split on its own, is exactly one command segment with the same text:
/// a splitter that cut a command in two, or ran two together, fails here.
fn each_segment_is_one_command(segments: &[Segment<'_>]) {
    for part in segments
        .iter()
        .filter(|segment| segment.kind() != SegmentKind::Trailing)
    {
        let alone = segment(part.text()).expect("a segment splits on its own");
        assert_eq!(alone.len(), 1, "{:?}", part.text());
        assert_eq!(
            (alone[0].text(), alone[0].kind()),
            (part.text(), part.kind())
        );
    }
}

/// xorshift64: small, seeded, and the same on every platform.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

const TRIVIA: [&str; 8] = [
    "",
    " ",
    "\t",
    "\r\n",
    "\n",
    "# a comment; with } in it\r\n",
    "/* ; } { */",
    "\r\n\r\n",
];
const STRINGS: [&str; 6] = [
    "\"plain\"",
    "\"a \\\" quote; and } brace\"",
    "\"back\\\\slash\"",
    "\"two\r\nlines;}\"",
    "text:\r\nline; }\r\n..stuffed\r\n.\r\n",
    "text: # why\n{\n.\n",
];
const SIMPLE: [&str; 3] = ["keep", "stop", "discard"];

fn trivia(rng: &mut Rng) -> String {
    rng.pick(&TRIVIA).to_owned()
}

fn test(rng: &mut Rng, depth: usize) -> String {
    match rng.below(if depth > 1 { 3 } else { 5 }) {
        0 => "true".to_owned(),
        1 => format!("header :contains \"subject\" {}", rng.pick(&STRINGS)),
        2 => "size :over 10K".to_owned(),
        3 => format!(
            "not{}{}",
            if rng.below(2) == 0 { " " } else { "\r\n" },
            test(rng, depth + 1)
        ),
        _ => format!(
            "{} ({},{}{})",
            rng.pick(&["anyof", "allof"]),
            test(rng, depth + 1),
            trivia(rng),
            test(rng, depth + 1)
        ),
    }
}

fn block(rng: &mut Rng, depth: usize) -> String {
    let mut out = format!("{{{}", trivia(rng));
    for _ in 0..rng.below(3) {
        out.push_str(&command(rng, depth + 1).0);
        out.push_str(&trivia(rng));
    }
    out.push('}');
    out
}

/// A command, and its name.
fn command(rng: &mut Rng, depth: usize) -> (String, &'static str) {
    match rng.below(if depth > 1 { 3 } else { 5 }) {
        0 => {
            let name = SIMPLE[rng.below(SIMPLE.len())];
            (format!("{name};"), name)
        }
        1 => (
            format!("fileinto :copy {};", rng.pick(&STRINGS)),
            "fileinto",
        ),
        2 => (
            format!(
                "require [{},{}{}];",
                rng.pick(&STRINGS),
                trivia(rng),
                rng.pick(&STRINGS)
            ),
            "require",
        ),
        3 => (format!("set \"v\" {};", rng.pick(&STRINGS)), "set"),
        _ => {
            let mut out = format!(
                "if {}{}{}",
                test(rng, depth),
                trivia(rng),
                block(rng, depth)
            );
            for _ in 0..rng.below(3) {
                out.push_str(&trivia(rng));
                write!(out, "elsif {} {}", test(rng, depth), block(rng, depth)).unwrap();
            }
            if rng.below(2) == 0 {
                write!(out, "{}else {}", trivia(rng), block(rng, depth)).unwrap();
            }
            (out, "if")
        }
    }
}

#[test]
fn every_generated_script_splits_into_its_commands_and_joins_back() {
    let mut rng = Rng(0x5eed_5eed_5eed_5eed);
    for round in 0..2_000 {
        let mut script = trivia(&mut rng);
        let mut names = Vec::new();
        for _ in 0..rng.below(6) {
            let (text, name) = command(&mut rng, 0);
            script.push_str(&text);
            // A separator after each command, never empty, so `if … {}` + `else` stays an if
            // chain only when the generator meant it to.
            script.push_str(rng.pick(&[" ", "\r\n", "\n", " # c\r\n"]));
            names.push(name);
        }
        script.push_str(&trivia(&mut rng));
        let segments =
            segment(&script).unwrap_or_else(|err| panic!("round {round}: {err}\n{script:?}"));
        let joined: String = segments.iter().map(Segment::text).collect();
        assert_eq!(joined, script, "round {round}");
        let commands: Vec<&str> = segments
            .iter()
            .filter(|segment| segment.kind() != SegmentKind::Trailing)
            .map(|segment| segment.identifier().unwrap())
            .collect();
        assert_eq!(commands, names, "round {round}: {script:?}");
        each_segment_is_one_command(&segments);
        // Written over with its own rules, a script whose requires come first is unchanged.
        let requires_first = segments
            .iter()
            .skip_while(|segment| segment.kind() == SegmentKind::Require)
            .all(|segment| segment.kind() != SegmentKind::Require);
        if requires_first {
            let rules = read_rules(&script);
            assert_eq!(
                write_rules(&script, &rules).unwrap(),
                script,
                "round {round}"
            );
        }
    }
}

#[test]
fn arbitrary_text_never_panics_and_each_segment_is_one_command() {
    // Whole commands and the pieces that hide a `;` or a `}`, so that many scripts split, and
    // many hold several commands on one line.
    let alphabet = [
        "keep;",
        "stop ;",
        "if true",
        "if",
        "else",
        "elsif",
        " ",
        "\r\n",
        "\n",
        "{",
        "}",
        "[",
        "]",
        "(",
        ")",
        ";",
        ",",
        "\"",
        "\\",
        "\"a;}\"",
        "# c ; }\r\n",
        "/*",
        "*/",
        "/* ; */",
        "text:\r\n",
        ".\r\n",
        "keep",
        ":is",
        "10K",
        "é",
        "@",
        "anyof (true, false)",
    ];
    let mut rng = Rng(0xfeed_f00d_dead_beef);
    let mut split = 0;
    for _ in 0..20_000 {
        let script: String = (0..rng.below(24)).map(|_| rng.pick(&alphabet)).collect();
        if let Ok(segments) = segment(&script) {
            split += 1;
            let joined: String = segments.iter().map(Segment::text).collect();
            assert_eq!(joined, script);
            each_segment_is_one_command(&segments);
        }
    }
    assert!(
        split > 100,
        "the alphabet produces some scripts that split: {split}"
    );
}
