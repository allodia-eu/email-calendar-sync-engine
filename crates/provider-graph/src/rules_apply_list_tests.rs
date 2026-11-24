//! The writer against a fake that keeps the rules as a real list (`crate::rules_fake`): the list
//! a write leaves, not only the requests it sends, for every kind of reorder, insert and removal,
//! a randomised sweep, and the two steps of a rule written anew failing in turn.

use engine_core::{
    error::FailureClass,
    rules::{
        Action, Condition, FailedRule, HeaderField, Kept, Loss, LossReason, MailRule, Match,
        RuleBody, RuleName, RulePart, RuleSet, RuleWriteFailure, RulesReceipt, TextOp, TextTest,
    },
};

use super::replace;
use crate::{rules::Listing, rules_fake};

/// What a written rule is: one read, by name, or a new one of that name.
#[derive(Clone, Copy)]
enum Spec<'a> {
    Read(&'a str),
    New(&'a str),
}

fn new_rule(name: &str) -> MailRule {
    MailRule::typed(
        Match::all(vec![Condition::Header {
            field: HeaderField::Subject,
            test: TextTest::new(TextOp::Contains, vec![format!("marker-{name}")]).unwrap(),
        }]),
        vec![Action::Discard],
    )
    .named(RuleName::new(name).unwrap())
}

fn by_name<'a>(base: &'a RuleSet, name: &str) -> &'a MailRule {
    base.rules
        .iter()
        .find(|rule| rule.name.as_ref().map(RuleName::as_str) == Some(name))
        .unwrap()
}

/// Writes `specs` over a list of `initial` rules; the receipt and the names the list holds after.
async fn write(initial: &[&str], specs: &[Spec<'_>]) -> (RulesReceipt, Vec<String>) {
    let (client, state) = rules_fake::client(&rules_fake::named(initial));
    let base = Listing::read(&client).await.unwrap().rule_set();
    let rules: Vec<MailRule> = specs
        .iter()
        .map(|spec| match spec {
            Spec::Read(name) => by_name(&base, name).clone(),
            Spec::New(name) => new_rule(name),
        })
        .collect();
    let receipt = replace(&client, &base, &rules).await.unwrap();
    let names = state.lock().unwrap().names();
    (receipt, names)
}

fn names(specs: &[Spec<'_>]) -> Vec<String> {
    specs
        .iter()
        .map(|spec| match spec {
            Spec::Read(name) | Spec::New(name) => (*name).to_owned(),
        })
        .collect()
}

#[tokio::test]
async fn every_kind_of_reorder_leaves_the_list_as_asked() {
    use Spec::{New, Read};
    let initial = ["a", "b", "c", "d", "e"];
    let cases: Vec<Vec<Spec>> = vec![
        vec![
            New("n"),
            Read("a"),
            Read("b"),
            Read("c"),
            Read("d"),
            Read("e"),
        ],
        vec![
            Read("a"),
            Read("b"),
            New("n"),
            Read("c"),
            Read("d"),
            Read("e"),
        ],
        vec![Read("b"), Read("c"), Read("d"), Read("e"), Read("a")],
        vec![Read("e"), Read("d"), Read("c"), Read("b"), Read("a")],
        // A rotation of three among the rest.
        vec![Read("a"), Read("c"), Read("d"), Read("b"), Read("e")],
        vec![Read("a"), Read("e"), New("n"), Read("c")],
        vec![New("m"), New("n")],
    ];
    for specs in cases {
        let (receipt, after) = write(&initial, &specs).await;
        assert_eq!(receipt.failures, Vec::new());
        assert_eq!(after, names(&specs));
    }
}

/// A small xorshift: the sweep is the same on every run.
struct Random(u64);

impl Random {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % u64::try_from(n).unwrap()).unwrap()
    }
}

#[tokio::test]
async fn a_randomised_sweep_of_edits_leaves_the_list_as_asked() {
    const NAMES: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];
    const NEW: [&str; 3] = ["x", "y", "z"];
    let mut random = Random(0x5eed);
    for _ in 0..200 {
        let initial = &NAMES[..=random.below(NAMES.len())];
        let mut specs: Vec<Spec> = initial
            .iter()
            .filter(|_| random.below(4) != 0)
            .map(|name| Spec::Read(name))
            .collect();
        for i in (1..specs.len()).rev() {
            specs.swap(i, random.below(i + 1));
        }
        for name in NEW.iter().take(random.below(NEW.len() + 1)) {
            let at = random.below(specs.len() + 1);
            specs.insert(at, Spec::New(name));
        }
        let (receipt, after) = write(initial, &specs).await;
        assert_eq!(receipt.failures, Vec::new());
        assert_eq!(after, names(&specs), "from {initial:?}");
    }
}

#[tokio::test]
async fn a_failed_move_is_reported_and_no_other_rule_is_lost() {
    let (client, state) = rules_fake::client(&rules_fake::named(&["a", "b", "c"]));
    state
        .lock()
        .unwrap()
        .failing
        .insert(("PATCH", "id-c".to_owned()), 503);
    let base = Listing::read(&client).await.unwrap().rule_set();
    let rules = vec![
        by_name(&base, "c").clone(),
        by_name(&base, "a").clone(),
        by_name(&base, "b").clone(),
    ];
    let receipt = replace(&client, &base, &rules).await.unwrap();
    assert_eq!(
        receipt.failures,
        vec![RuleWriteFailure::new(
            FailedRule::Written(0),
            FailureClass::Retryable
        )]
    );
    assert_eq!(receipt.outcomes[0].kept, Kept::Reduced);
    assert_eq!(state.lock().unwrap().names(), ["a", "b", "c"]);
}

/// The first rule with its subject test dropped and a discard in its place: a rule written anew.
fn dropping(base: &RuleSet) -> Vec<MailRule> {
    let mut rules = base.rules.clone();
    if let RuleBody::Typed(typed) = &mut rules[0].body {
        typed.when.conditions.clear();
        typed.actions = vec![Action::Discard];
    }
    rules
}

#[tokio::test]
async fn a_rule_written_anew_takes_the_old_ones_place() {
    let (client, state) = rules_fake::client(&rules_fake::named(&["a", "b"]));
    let base = Listing::read(&client).await.unwrap().rule_set();
    let receipt = replace(&client, &base, &dropping(&base)).await.unwrap();
    assert_eq!(receipt.failures, Vec::new());
    let state = state.lock().unwrap();
    assert_eq!(state.names(), ["a", "b"]);
    assert_eq!(
        state.writes,
        [("POST", "a".to_owned()), ("DELETE", "id-a".to_owned())]
    );
}

#[tokio::test]
async fn a_failed_create_leaves_the_old_rule_and_sends_no_delete() {
    let (client, state) = rules_fake::client(&rules_fake::named(&["a", "b"]));
    state
        .lock()
        .unwrap()
        .failing
        .insert(("POST", "a".to_owned()), 400);
    let base = Listing::read(&client).await.unwrap().rule_set();
    let receipt = replace(&client, &base, &dropping(&base)).await.unwrap();
    assert_eq!(
        receipt.failures,
        vec![RuleWriteFailure::new(
            FailedRule::Written(0),
            FailureClass::Permanent
        )]
    );
    assert_eq!(receipt.outcomes[0].kept, Kept::NotKept);
    let state = state.lock().unwrap();
    assert_eq!(state.names(), ["a", "b"]);
    assert_eq!(state.writes.len(), 0);
}

#[tokio::test]
async fn a_failed_delete_after_the_create_shows_the_duplicate_on_the_rules_outcome() {
    let (client, state) = rules_fake::client(&rules_fake::named(&["a", "b"]));
    state
        .lock()
        .unwrap()
        .failing
        .insert(("DELETE", "id-a".to_owned()), 503);
    let base = Listing::read(&client).await.unwrap().rule_set();
    let receipt = replace(&client, &base, &dropping(&base)).await.unwrap();
    assert_eq!(
        receipt.failures,
        vec![RuleWriteFailure::new(
            FailedRule::Removed(0),
            FailureClass::Retryable
        )]
    );
    assert_eq!(receipt.outcomes[0].kept, Kept::Reduced);
    assert_eq!(
        receipt.outcomes[0].losses,
        vec![Loss::new(RulePart::Rule, LossReason::LeftBeside)]
    );
    // The new rule first, the old one still beside it.
    assert_eq!(state.lock().unwrap().names(), ["a", "a", "b"]);
}

#[tokio::test]
async fn a_name_the_edit_removes_is_left_as_it_was_and_reported() {
    let (client, state) = rules_fake::client(&rules_fake::named(&["a", "b"]));
    let base = Listing::read(&client).await.unwrap().rule_set();
    let mut rules = base.rules.clone();
    rules[0].name = None;
    let receipt = replace(&client, &base, &rules).await.unwrap();
    assert_eq!(receipt.failures, Vec::new());
    assert_eq!(receipt.outcomes[0].kept, Kept::Reduced);
    assert_eq!(
        receipt.outcomes[0].losses,
        vec![Loss::new(RulePart::Name, LossReason::NotWritten)]
    );
    let state = state.lock().unwrap();
    // Nothing else changed: nothing was sent.
    assert_eq!(state.writes.len(), 0);
    assert_eq!(state.names(), ["a", "b"]);
}

#[tokio::test]
async fn a_rule_whose_id_holds_a_slash_is_never_written_to() {
    let mut rules = rules_fake::named(&["a", "b"]);
    rules[0]["id"] = serde_json::json!("id/a");
    let (client, state) = rules_fake::client(&rules);
    let base = Listing::read(&client).await.unwrap().rule_set();
    // Removing it is refused, not counted as done.
    let receipt = replace(&client, &base, &base.rules[1..]).await.unwrap();
    assert_eq!(
        receipt.failures,
        vec![RuleWriteFailure::new(
            FailedRule::Removed(0),
            FailureClass::Permanent
        )]
    );
    // Changing it is refused too.
    let mut renamed = base.rules.clone();
    renamed[0].name = Some(RuleName::new("renamed").unwrap());
    let receipt = replace(&client, &base, &renamed).await.unwrap();
    assert_eq!(
        receipt.failures,
        vec![RuleWriteFailure::new(
            FailedRule::Written(0),
            FailureClass::Permanent
        )]
    );
    let state = state.lock().unwrap();
    assert_eq!(state.writes.len(), 0);
    assert_eq!(state.names(), ["a", "b"]);
}

#[tokio::test]
async fn a_randomised_sweep_with_read_only_rules_changes_and_failures_keeps_its_promises() {
    // Whatever happens: no request reaches a read-only rule, and a write that reports no failure
    // leaves the list exactly as asked.
    const NAMES: [&str; 6] = ["a", "b", "c", "d", "e", "f"];
    let mut random = Random(0xfa11);
    for round in 0..200 {
        let initial = &NAMES[..=random.below(NAMES.len())];
        let mut listed = rules_fake::named(initial);
        let read_only: Vec<String> = listed
            .iter_mut()
            .filter(|_| random.below(5) == 0)
            .map(|rule| {
                rule["isReadOnly"] = serde_json::json!(true);
                rule["id"].as_str().unwrap().to_owned()
            })
            .collect();
        let (client, state) = rules_fake::client(&listed);
        let base = Listing::read(&client).await.unwrap().rule_set();
        let mut rules: Vec<MailRule> = base
            .rules
            .iter()
            .filter(|_| random.below(4) != 0)
            .cloned()
            .collect();
        for i in (1..rules.len()).rev() {
            rules.swap(i, random.below(i + 1));
        }
        for rule in &mut rules {
            if random.below(4) == 0 {
                let name = format!("{}2", rule.name.as_ref().unwrap().as_str());
                rule.name = Some(RuleName::new(name).unwrap());
            }
        }
        if random.below(2) == 0 {
            let at = random.below(rules.len() + 1);
            rules.insert(at, new_rule("x"));
        }
        if random.below(3) == 0 {
            let method = ["POST", "PATCH", "DELETE"][random.below(3)];
            let target = if method == "POST" {
                "x".to_owned()
            } else {
                format!("id-{}", initial[random.below(initial.len())])
            };
            state.lock().unwrap().failing.insert((method, target), 503);
        }
        let receipt = replace(&client, &base, &rules).await.unwrap();
        let state = state.lock().unwrap();
        assert!(
            !state.writes.iter().any(|(_, id)| read_only.contains(id)),
            "round {round}: a read-only rule was sent"
        );
        if receipt.failures.is_empty() {
            let asked: Vec<String> = rules
                .iter()
                .map(|rule| rule.name.as_ref().unwrap().as_str().to_owned())
                .collect();
            assert_eq!(state.names(), asked, "round {round}");
        }
    }
}
