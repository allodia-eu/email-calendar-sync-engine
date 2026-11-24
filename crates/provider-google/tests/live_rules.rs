//! Gated live **reads** of the filters and the vacation reply against a real Google account.
//! Reading is free: nothing here writes.
//!
//! - The filters are read twice and name one version, and every filter the account holds plans
//!   unchanged against Gmail's own vocabulary, so a write of the set as read sends nothing.
//! - The reply is read twice and names one version; the vocabulary asks the account's affiliation.
//! - The forwarding addresses the writer checks a forward against are listed.
//!
//! It prints counts and kinds only, never a filter's values or the reply's text.
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_rules -- --nocapture --test-threads=1
//! ```

mod gmail_settings_support;

use engine_core::rules::{Action, Condition, Kept, LossReason, PlanMode, RuleBody, plan_rules};
use engine_provider::Provider;
use gmail_settings_support::{account, failed, provider, send, token};

#[tokio::test]
async fn live_filters_read_as_one_version_and_plan_unchanged() {
    let Some(token) = token() else {
        eprintln!("skipping: GOOGLE_ACCESS_TOKEN not set");
        return;
    };
    let gmail = provider(&token);
    let rules = gmail.mail_rules().expect("Gmail offers its filters");
    let first = rules
        .read_rules(&account())
        .await
        .unwrap_or_else(|err| failed("read", &err));
    let second = rules
        .read_rules(&account())
        .await
        .unwrap_or_else(|err| failed("read again", &err));
    assert_eq!(first.version, second.version, "two reads, one version");
    let vocabulary = rules
        .rule_vocabulary(&account())
        .await
        .unwrap_or_else(|err| failed("vocabulary", &err));
    let plan = plan_rules(&first.rules, &vocabulary, PlanMode::Edit);
    assert!(plan.written() == first.rules, "a filter would be rewritten");
    let mut natives = (0, 0);
    for (rule, outcome) in first.rules.iter().zip(&plan.outcomes) {
        assert!(matches!(outcome.kept, Kept::Whole | Kept::Reduced));
        assert!(outcome.losses.iter().all(|loss| matches!(
            loss.reason,
            LossReason::Approximated | LossReason::NotExpressible
        )));
        if let RuleBody::Typed(typed) = &rule.body {
            let conditions = typed.when.conditions.iter().chain(&typed.when.except);
            natives.0 += conditions
                .filter(|c| matches!(c, Condition::Native(_)))
                .count();
            natives.1 += typed
                .actions
                .iter()
                .filter(|a| matches!(a, Action::Native(_)))
                .count();
        }
    }
    eprintln!(
        "filters: {}; native conditions: {}; native actions: {}",
        first.rules.len(),
        natives.0,
        natives.1
    );
}

#[tokio::test]
async fn live_reply_reads_as_one_version() {
    let Some(token) = token() else {
        eprintln!("skipping: GOOGLE_ACCESS_TOKEN not set");
        return;
    };
    let gmail = provider(&token);
    let vacation = gmail.vacation().expect("Gmail offers its reply");
    let first = vacation
        .read_vacation(&account())
        .await
        .unwrap_or_else(|err| failed("read", &err));
    let second = vacation
        .read_vacation(&account())
        .await
        .unwrap_or_else(|err| failed("read again", &err));
    assert_eq!(first.version, second.version, "two reads, one version");
    let vocabulary = vacation
        .vocabulary(&account())
        .await
        .unwrap_or_else(|err| failed("vocabulary", &err));
    eprintln!(
        "reply on: {}; window ends set: {}/{}; audiences offered: {}",
        first.enabled,
        first.window.start.is_some(),
        first.window.end.is_some(),
        vocabulary.audiences().len()
    );
}

#[tokio::test]
async fn live_forwarding_addresses_are_listed() {
    let Some(token) = token() else {
        eprintln!("skipping: GOOGLE_ACCESS_TOKEN not set");
        return;
    };
    let (status, listed) = send(
        &token,
        reqwest::Method::GET,
        "/settings/forwardingAddresses",
        None,
    )
    .await;
    // `204` with no body where there are none (observed).
    assert!(matches!(status, 200 | 204), "{status}");
    let addresses = listed["forwardingAddresses"].as_array().map_or(0, Vec::len);
    let accepted = listed["forwardingAddresses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["verificationStatus"] == "accepted")
        .count();
    eprintln!("forwarding addresses: {addresses}; verified: {accepted}");
}
