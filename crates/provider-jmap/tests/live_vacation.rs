//! Gated live integration: the out-of-office reply (`Vacation` for `JmapProvider`) against the
//! Stalwart harness, which also settles what `docs/design/filters-and-out-of-office.md` left to
//! a live test for slice 1 (RFC 9661 §4).
//!
//! The offline suite asserts what is sent and reads captured answers. Only the server says it
//! takes `VacationResponse/set` with `ifInState` and every property, refuses a stale state and
//! takes the current one, and what the reply does to the account's Sieve scripts:
//!
//! - setting a reply creates a `SieveScript` of the server's own, which `SieveScript/set` may not
//!   change (`forbidden`, as RFC 9661 §4 requires);
//! - `VacationResponse` and `SieveScript` share one state, so a write of either refuses a write of
//!   the other guarded by a state read before it;
//! - turning the reply on deactivates the user's own active script, and the receipt says so, while
//!   a reply turned on with no script of the user's running says it touched nothing;
//! - turning it off again leaves the user's script off, and the receipt says that too, while a
//!   reply turned off on an account with no script of the user's says it touched nothing.
//!
//! It runs as the scratch account `carol@`, never Alice, and restores Carol's reply and scripts
//! whether it passes or not. About twenty requests a run.
//!
//! ```sh
//! STALWART_HTTP_ADDR=127.0.0.1:18080 \
//!   cargo test -p provider-jmap --test live_vacation -- --nocapture
//! ```

use std::panic::{AssertUnwindSafe, resume_unwind};

use engine_core::{
    error::FailureClass,
    ids::AccountId,
    vacation::{AutoReply, RulesEffect, VacationResponse, VacationWindow, WindowSupport},
};
use engine_provider::{Provider, Vacation, VacationPrecondition::IfUnchanged, WriteGuard};
use futures_util::FutureExt;
use provider_jmap::{
    Credentials, JmapAccountId, JmapClient, JmapConfig, JmapProvider, Request, capability,
};
use serde_json::{Value, json};
use stalwart_harness::Harness;

/// The name of the script this suite plants as the user's own rules.
const OWN: &str = "live-vacation-own";

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn away(subject: &str) -> VacationResponse {
    VacationResponse::default()
        .turned_on()
        .with_window(
            VacationWindow::default()
                .with_start("2030-01-02T09:00:00Z".parse().unwrap())
                .with_end("2030-01-09T17:00:00Z".parse().unwrap()),
        )
        .with_reply(
            AutoReply::default()
                .with_subject(subject)
                .with_text("I am away until the ninth.")
                .with_html("<p>I am away until the <b>ninth</b>.</p>"),
        )
}

/// The parts a write sets, without what only a read carries.
fn parts(reply: &VacationResponse) -> (bool, VacationWindow, AutoReply) {
    (reply.enabled, reply.window, reply.reply.clone())
}

/// Carol's scripts: `(id, name, isActive)`, and the type's state.
async fn scripts(client: &JmapClient) -> (Vec<(String, String, bool)>, String) {
    let account = sieve(client);
    let mut request = Request::new([capability::CORE, capability::SIEVE]);
    let get = request.invoke(
        "SieveScript/get",
        json!({ "accountId": account, "ids": null }),
    );
    let response = client.execute(&request).await.expect("list the scripts");
    let result = response.result(&get).expect("SieveScript/get");
    let list = result["list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|script| {
            (
                script["id"].as_str().unwrap().to_owned(),
                script["name"].as_str().unwrap().to_owned(),
                script["isActive"].as_bool().unwrap(),
            )
        })
        .collect();
    (list, result["state"].as_str().unwrap().to_owned())
}

fn sieve(client: &JmapClient) -> JmapAccountId {
    client
        .session()
        .account_for(capability::SIEVE)
        .expect("the harness speaks JMAP Sieve")
        .clone()
}

/// One `SieveScript/set` (and a `Blob/upload` before it when `blob` is given), answering the
/// result of the set.
async fn sieve_set(client: &JmapClient, arguments: Value, blob: Option<&str>) -> Value {
    let account = sieve(client);
    let mut request = Request::new([capability::CORE, capability::SIEVE, capability::BLOB]);
    if let Some(script) = blob {
        request.invoke(
            "Blob/upload",
            json!({ "accountId": account, "create": { "b1": {
                "data": [{ "data:asText": script }], "type": "application/sieve"
            } } }),
        );
    }
    let mut arguments = arguments;
    arguments["accountId"] = json!(account);
    let set = request.invoke("SieveScript/set", arguments);
    let response = client.execute(&request).await.expect("SieveScript/set");
    response
        .result(&set)
        .expect("SieveScript/set answered")
        .clone()
}

#[tokio::test]
async fn live_the_reply_is_written_guarded_and_says_what_it_did_to_the_user_s_rules() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_vacation: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("harness ready");
    let (user, password) = harness.scratch[1].auth();
    let config = || {
        JmapConfig::new(
            format!("http://{}", harness.http_addr),
            Credentials::basic(user, password),
            engine_http::test_server::user_agent(),
        )
    };
    let provider = JmapProvider::connect(config()).await.expect("connect");
    let client = JmapClient::connect(config())
        .await
        .expect("connect a client");
    let vacation = provider
        .vacation()
        .expect("an account listing vacationresponse has a reply");
    assert_eq!(vacation.vacation_write_guard(), WriteGuard::Enforced);
    let vocabulary = vacation.vocabulary(&account()).await.expect("vocabulary");
    assert_eq!(vocabulary.window(), WindowSupport::EitherEnd);

    let prior = vacation.read_vacation(&account()).await.expect("read");
    let (prior_scripts, _) = scripts(&client).await;
    eprintln!("prior: {} scripts", prior_scripts.len());

    let checked = AssertUnwindSafe(exercise(vacation, &client, &prior_scripts))
        .catch_unwind()
        .await;
    let restored = restore(vacation, &client, &prior, &prior_scripts).await;
    if let Err(failed) = checked {
        if let Err(problem) = restored {
            eprintln!("and restoring Carol failed too: {problem}");
        }
        resume_unwind(failed);
    }
    restored.expect("Carol is left as she was found");
}

async fn exercise(vacation: &dyn Vacation, client: &JmapClient, prior: &[(String, String, bool)]) {
    assert!(
        prior.iter().all(|(_, _, active)| !active),
        "the suite starts with none of Carol's scripts running: {prior:?}"
    );

    // On, with no script of the user's running: the server adds one of its own, and the
    // receipt says the user's rules were not touched.
    let read = vacation.read_vacation(&account()).await.unwrap();
    let (_, sieve_state) = scripts(client).await;
    let receipt = vacation
        .set_vacation(&account(), IfUnchanged(&read), &away("Away"))
        .await
        .expect("a reply guarded by the version just read");
    assert_eq!(receipt.rules, RulesEffect::Untouched);
    let stored = vacation.read_vacation(&account()).await.unwrap();
    assert_eq!(
        parts(&stored),
        parts(&away("Away")),
        "every part kept as sent"
    );
    assert_eq!(
        stored.version, receipt.version,
        "the receipt's version is the reply's"
    );
    let (after, moved) = scripts(client).await;
    let created: Vec<_> = after
        .iter()
        .filter(|(id, ..)| prior.iter().all(|(was, ..)| was != id))
        .collect();
    eprintln!("RFC 9661 §4: setting a reply created {created:?}");
    assert_eq!(
        created.len(),
        1,
        "the reply is a script of the server's own"
    );
    let (reply_script, _, active) = created[0].clone();
    assert!(active, "and it is the active one");
    assert_ne!(
        moved, sieve_state,
        "writing the reply moved the SieveScript state"
    );

    // The server's script is the reply's alone: SieveScript/set may not change it.
    let refused = sieve_set(
        client,
        json!({ "update": { reply_script.as_str(): { "blobId": "#b1" } } }),
        Some("keep;\r\n"),
    )
    .await;
    assert_eq!(
        refused["notUpdated"][reply_script.as_str()]["type"],
        "forbidden"
    );

    // A write built on the first read is stale now, and refused, and does not land.
    let err = vacation
        .set_vacation(&account(), IfUnchanged(&read), &away("Stale"))
        .await
        .expect_err("a stale version is refused");
    assert_eq!(err.class(), FailureClass::Conflict);
    let unchanged = vacation.read_vacation(&account()).await.unwrap();
    assert_eq!(
        parts(&unchanged),
        parts(&away("Away")),
        "the refused write did not land"
    );

    // Over that fully populated reply, one with no subject, only a start and no HTML: every part
    // left out is cleared on the server, not kept from before.
    let start = "2030-02-01T08:00:00Z".parse().unwrap();
    let cleared = VacationResponse::default()
        .turned_on()
        .with_window(VacationWindow::default().with_start(start))
        .with_reply(AutoReply::default().with_text("Away for a while."));
    let receipt = vacation
        .set_vacation(&account(), IfUnchanged(&unchanged), &cleared)
        .await
        .expect("a reply with parts left out");
    let stored = vacation.read_vacation(&account()).await.unwrap();
    assert_eq!(stored.version, receipt.version);
    assert_eq!(stored.window.start, Some(start));
    assert_eq!(stored.window.end, None, "the end is cleared");
    assert_eq!(stored.reply.subject, None, "the subject is cleared");
    assert_eq!(stored.reply.html, None, "the HTML body is cleared");
    assert_eq!(stored.reply.text.as_deref(), Some("Away for a while."));

    // The user's own script, planted and activated: the reply's version read before it is
    // stale too, because the two types share one state.
    let (before_own, _) = scripts(client).await;
    let read = vacation.read_vacation(&account()).await.unwrap();
    let off = vacation
        .set_vacation(&account(), IfUnchanged(&read), &away("Away").turned_off())
        .await
        .expect("off");
    assert_eq!(
        off.rules,
        RulesEffect::Untouched,
        "off, with no script of the user's to leave off: {before_own:?}"
    );
    let read = vacation.read_vacation(&account()).await.unwrap();
    let planted = sieve_set(
        client,
        json!({ "create": { "s1": { "name": OWN, "blobId": "#b1" } },
                "onSuccessActivateScript": "#s1" }),
        Some("require [\"fileinto\"];\r\nif header :contains \"subject\" \"probe\" {\r\n    fileinto \"INBOX\";\r\n}\r\n"),
    )
    .await;
    let own = planted["created"]["s1"]["id"]
        .as_str()
        .expect("the user's script is stored")
        .to_owned();
    let err = vacation
        .set_vacation(&account(), IfUnchanged(&read), &away("Away"))
        .await
        .expect_err("a script write moves the reply's state");
    assert_eq!(err.class(), FailureClass::Conflict);

    // On again, over the user's running script: the server switches it off, and says nothing;
    // the receipt does.
    let read = vacation.read_vacation(&account()).await.unwrap();
    let receipt = vacation
        .set_vacation(&account(), IfUnchanged(&read), &away("Away"))
        .await
        .expect("on over the user's script");
    assert_eq!(receipt.rules, RulesEffect::Deactivated);
    let (after, _) = scripts(client).await;
    let state = |id: &str| after.iter().find(|(was, ..)| was == id).map(|s| s.2);
    assert_eq!(
        state(&own),
        Some(false),
        "the user's script was switched off"
    );
    assert_eq!(state(&reply_script), Some(true));

    // Off again: the user's script stays off, and the receipt says so; nothing switches it
    // back on for them.
    let read = vacation.read_vacation(&account()).await.unwrap();
    let off = vacation
        .set_vacation(&account(), IfUnchanged(&read), &away("Away").turned_off())
        .await
        .expect("off");
    assert_eq!(off.rules, RulesEffect::StillDeactivated);
    let (after, _) = scripts(client).await;
    let own_now = after.iter().find(|(id, ..)| *id == own).map(|s| s.2);
    eprintln!("after the reply is off, the user's script is active: {own_now:?}");
    assert_eq!(
        own_now,
        Some(false),
        "turning the reply off does not switch it back"
    );
}

/// Puts Carol's reply back as it was, removes every script the suite added (the server's reply
/// script included, which Stalwart lets a client destroy once inactive: `spec-deviations.md`,
/// STALWART-3), and reactivates the script that ran before, if one did. Then reads both back and
/// answers what still differs.
async fn restore(
    vacation: &dyn Vacation,
    client: &JmapClient,
    prior: &VacationResponse,
    prior_scripts: &[(String, String, bool)],
) -> Result<(), String> {
    let account = account();
    let mut back = VacationResponse::default()
        .with_window(prior.window)
        .with_reply(prior.reply.clone());
    back.enabled = prior.enabled;
    let now = vacation
        .read_vacation(&account)
        .await
        .map_err(|err| format!("reading the reply: {err}"))?;
    let _ = vacation
        .set_vacation(&account, IfUnchanged(&now), &back)
        .await
        .map_err(|err| format!("restoring the reply: {err}"))?;
    let (now, _) = scripts(client).await;
    let added: Vec<&str> = now
        .iter()
        .filter(|(id, ..)| prior_scripts.iter().all(|(was, ..)| was != id))
        .map(|(id, ..)| id.as_str())
        .collect();
    let active = prior_scripts.iter().find(|(.., active)| *active);
    let mut arguments = json!({ "destroy": added });
    if let Some((id, ..)) = active {
        arguments["onSuccessActivateScript"] = json!(id);
    }
    sieve_set(client, json!({ "onSuccessDeactivateScript": true }), None).await;
    let answer = sieve_set(client, arguments, None).await;
    if answer.get("notDestroyed").is_some() {
        return Err(format!("removing the suite's scripts: {answer}"));
    }

    let reply = vacation
        .read_vacation(&account)
        .await
        .map_err(|err| format!("reading the restored reply: {err}"))?;
    if parts(&reply) != parts(prior) {
        return Err("the reply differs from what it was".to_owned());
    }
    let (scripts_now, _) = scripts(client).await;
    if scripts_now != prior_scripts {
        return Err(format!(
            "the scripts differ: {scripts_now:?}, not {prior_scripts:?}"
        ));
    }
    Ok(())
}
