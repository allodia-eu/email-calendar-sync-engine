//! Which script a ManageSieve read takes for the rules', which scripts it reads to choose, and what
//! it never writes over.

use engine_core::{error::FailureClass, ids::AccountId, rules::RuleSet};
use engine_provider::MailRules;

use crate::managesieve::test_support::{capability, listing, script, scripted, sent};

const LOGOUT: &[u8] = b"OK\r\n";

fn account() -> AccountId {
    AccountId::try_from("sieve").unwrap()
}

/// A script as SOGo writes one: its own filters, and a reply no `# rule:[…]` name marks.
const SOGO: &str = "require [\"fileinto\",\"vacation\"];\r\nif header :contains \"subject\" \"x\"\r\n\
                    {\r\n\tfileinto \"X\";\r\n}\r\nvacation :days 7 \"Away.\";\r\n";

#[tokio::test]
async fn another_editor_s_script_with_its_own_reply_is_refused_never_rerouted_or_switched() {
    let (sieve, _) = scripted(&[&listing(&[("sogo", true)]), &script(SOGO), LOGOUT]);
    let base = sieve.read_rules(&account()).await.unwrap();
    let (sieve, sessions) = scripted(&[
        &capability("fileinto vacation include"),
        &listing(&[("sogo", true)]),
        &script(SOGO),
        LOGOUT,
    ]);
    let err = sieve
        .replace_rules(&account(), &base, &base.rules)
        .await
        .unwrap_err();
    assert!(err.is_item_unavailable(), "{err:?}");
    let sent = sent(&sessions);
    assert!(
        !sent.contains("PUTSCRIPT") && !sent.contains("SETACTIVE"),
        "{sent}"
    );
}

#[tokio::test]
async fn a_script_the_choice_does_not_need_is_never_read() {
    // `broken` would fail to read; the active script decides, so it is not asked for.
    let (sieve, sessions) = scripted(&[
        &listing(&[("broken", false), ("mine", true)]),
        &script("keep;\r\n"),
        LOGOUT,
    ]);
    let _ = sieve.read_rules(&account()).await.unwrap();
    assert_eq!(
        sent(&sessions),
        "LISTSCRIPTS\r\nGETSCRIPT \"mine\"\r\nLOGOUT\r\n"
    );
}

#[tokio::test]
async fn a_script_removed_between_its_listing_and_its_read_is_a_conflict() {
    let (sieve, _) = scripted(&[
        &listing(&[("mine", true)]),
        b"NO (NONEXISTENT) \"gone\"\r\n",
        LOGOUT,
    ]);
    let err = sieve.read_rules(&account()).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict, "{err:?}");
}

#[tokio::test]
async fn a_script_with_a_forbidden_name_still_counts_so_two_never_read_as_one() {
    // Neither active, neither `rules`: two scripts, so the engine starts its own rather than pick.
    let (sieve, sessions) = scripted(&["\"a\u{2028}b\"\r\n\"mine\"\r\nOK\r\n".as_bytes(), LOGOUT]);
    let set: RuleSet = sieve.read_rules(&account()).await.unwrap();
    assert_eq!(set.rules.len(), 0);
    assert_eq!(sent(&sessions), "LISTSCRIPTS\r\nLOGOUT\r\n", "nothing read");
}
