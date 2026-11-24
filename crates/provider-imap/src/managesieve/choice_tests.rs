//! Which script a ManageSieve read takes for the rules', which scripts it reads to choose, and what
//! it never writes over.

use engine_core::{error::FailureClass, ids::AccountId, rules::RuleSet};
use engine_provider::MailRules;

use crate::managesieve::test_support::{
    STALWART_REPLY, capability, listing, script, scripted, sent,
};

const LOGOUT: &[u8] = b"OK\r\n";

fn account() -> AccountId {
    AccountId::try_from("sieve").unwrap()
}

/// A script as SOGo writes one: its own filters, and a reply no `# rule:[…]` name marks.
const SOGO: &str = "require [\"fileinto\",\"vacation\"];\r\nif header :contains \"subject\" \"x\"\r\n\
                    {\r\n\tfileinto \"X\";\r\n}\r\nvacation :days 7 \"Away.\";\r\n";

/// A user's own script that only replies: not the shape a server writes, so also the user's.
const PLAIN_REPLY: &str = "require \"vacation\";\r\nvacation :days 7 \"Away\";\r\n";

#[tokio::test]
async fn another_editor_s_script_with_its_own_reply_is_refused_never_rerouted_or_switched() {
    for theirs in [SOGO, PLAIN_REPLY] {
        refused_untouched(theirs).await;
    }
}

async fn refused_untouched(theirs: &str) {
    let (sieve, _) = scripted(&[&listing(&[("sogo", true)]), &script(theirs), LOGOUT]);
    let base = sieve.read_rules(&account()).await.unwrap();
    let (sieve, sessions) = scripted(&[
        &capability("fileinto vacation include"),
        &listing(&[("sogo", true)]),
        &script(theirs),
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
    // Neither active, neither `rules`: two scripts of the user's, so the engine starts its own
    // rather than pick. The one it can address is read, to tell a server's reply; the other never.
    let (sieve, sessions) = scripted(&[
        "\"a\u{2028}b\"\r\n\"mine\"\r\nOK\r\n".as_bytes(),
        &script("keep;\r\n"),
        LOGOUT,
    ]);
    let set: RuleSet = sieve.read_rules(&account()).await.unwrap();
    assert_eq!(set.rules.len(), 0);
    assert_eq!(
        sent(&sessions),
        "LISTSCRIPTS\r\nGETSCRIPT \"mine\"\r\nLOGOUT\r\n"
    );
}

#[tokio::test]
async fn beside_a_server_s_inactive_reply_the_one_script_of_the_user_s_holds_the_rules() {
    let rule = "# rule:[Mine]\r\nif header :contains \"subject\" \"x\"\r\n{\r\n\tkeep;\r\n}\r\n";
    let (sieve, sessions) = scripted(&[
        &listing(&[("myrules", false), ("vacation", false)]),
        &script(rule),
        &script(STALWART_REPLY),
        LOGOUT,
    ]);
    let set: RuleSet = sieve.read_rules(&account()).await.unwrap();
    assert_eq!(set.rules.len(), 1, "the rules of myrules");
    assert_eq!(
        sent(&sessions),
        "LISTSCRIPTS\r\nGETSCRIPT \"myrules\"\r\nGETSCRIPT \"vacation\"\r\nLOGOUT\r\n",
        "both read, to tell the server's reply from the user's script"
    );
}
