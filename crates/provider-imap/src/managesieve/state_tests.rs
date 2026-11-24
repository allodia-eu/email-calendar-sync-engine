//! Which script holds the rules, and the version a write is guarded by.

use super::*;

fn listed(scripts: &[(&str, bool)]) -> Vec<ListedScript> {
    scripts
        .iter()
        .map(|(name, active)| ListedScript {
            name: ScriptName::new(name).unwrap(),
            active: *active,
            usable: true,
        })
        .collect()
}

fn state(scripts: &[(&str, bool)], text: &str) -> State {
    let listed = listed(scripts);
    State {
        script: listed
            .iter()
            .find(|script| script.active)
            .or(listed.first().filter(|_| listed.len() == 1))
            .cloned(),
        listed,
        text: text.to_owned(),
        server_reply: None,
    }
}

#[test]
fn the_version_moves_with_the_text_and_with_which_script_runs_and_with_nothing_else() {
    let base = state(&[("a", true), ("b", false)], "keep;\r\n").version();
    assert_eq!(base.len(), 64);
    assert_eq!(
        state(&[("b", false), ("a", true)], "keep;\r\n").version(),
        base,
        "order"
    );
    assert_ne!(
        state(&[("a", true), ("b", false)], "discard;\r\n").version(),
        base
    );
    assert_ne!(
        state(&[("a", false), ("b", true)], "keep;\r\n").version(),
        base
    );
    assert_ne!(
        state(&[("a", true)], "keep;\r\n").version(),
        base,
        "a script went"
    );
    // Where one name ends and the text begins is part of what is hashed.
    assert_ne!(
        state(&[("a", true)], "b").version(),
        state(&[("ab", true)], "").version()
    );
}

#[test]
fn a_write_goes_to_the_script_holding_the_rules_or_creates_rules() {
    let existing = state(&[("mine", false)], "");
    assert_eq!(existing.target().as_str(), "mine");
    assert!(!existing.running());
    let none = state(&[], "");
    assert_eq!(none.target().as_str(), RULES_SCRIPT);
    assert!(none.running(), "a first write creates it running");
}
