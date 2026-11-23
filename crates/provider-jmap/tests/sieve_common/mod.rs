//! Shared setup for the gated mail-rule suites (`live_rules`, `live_rules_reply`) against the
//! Stalwart harness.
//!
//! Both run as the scratch account `carol@`, whose reply and Sieve scripts `live_vacation` also
//! writes. Cargo runs a crate's test binaries one after another, and each suite is one test
//! function, so none of the three ever touches Carol's scripts while another does. Each leaves
//! Carol as it found her: no script running, the reply off and empty, no script of its own left.

#![allow(
    dead_code,
    reason = "each live suite uses a different subset of these helpers"
)]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use engine_core::{
    ids::{AccountId, MessageIdHeader},
    mail::EmailAddress,
};
use engine_provider::{Draft, HandOver, Provider, Unrecorded};
use provider_jmap::{
    Credentials, JmapAccountId, JmapClient, JmapConfig, JmapProvider, Request, capability,
};
use serde_json::{Value, json};
use stalwart_harness::{Harness, ScratchAccount};

pub(crate) fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// A marker for this run's subjects: no other suite's, and no other run's, contains it.
pub(crate) fn marker(suite: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("zsieve{suite}{nanos}")
}

pub(crate) fn config(harness: &Harness, who: &ScratchAccount) -> JmapConfig {
    JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&who.address, &who.password),
        engine_http::test_server::user_agent(),
    )
}

pub(crate) async fn provider(harness: &Harness, who: &ScratchAccount) -> JmapProvider {
    JmapProvider::connect(config(harness, who))
        .await
        .expect("connect")
}

pub(crate) async fn client(harness: &Harness, who: &ScratchAccount) -> JmapClient {
    JmapClient::connect(config(harness, who))
        .await
        .expect("connect a client")
}

fn sieve(client: &JmapClient) -> JmapAccountId {
    client
        .session()
        .account_for(capability::SIEVE)
        .expect("the harness speaks JMAP Sieve")
        .clone()
}

/// One script as listed: `(id, name, isActive, blobId)`.
pub(crate) type Listed = (String, String, bool, String);

/// The scripts, and the type's state.
pub(crate) async fn scripts(client: &JmapClient) -> (Vec<Listed>, String) {
    let mut request = Request::new([capability::CORE, capability::SIEVE]);
    let get = request.invoke(
        "SieveScript/get",
        json!({ "accountId": sieve(client), "ids": null }),
    );
    let response = client.execute(&request).await.expect("list the scripts");
    let result = response.result(&get).expect("SieveScript/get");
    let list = result["list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|script| {
            let text = |name: &str| script[name].as_str().unwrap().to_owned();
            (
                text("id"),
                text("name"),
                script["isActive"].as_bool().unwrap(),
                text("blobId"),
            )
        })
        .collect();
    (list, result["state"].as_str().unwrap().to_owned())
}

/// A script's text.
pub(crate) async fn text(client: &JmapClient, blob: &str) -> String {
    let template = client.session().download_url().expect("a download URL");
    let blob = engine_core::ids::BlobId::try_from(blob).unwrap();
    let url = provider_jmap::blob::download_url(
        template,
        &sieve(client),
        &blob,
        "application/sieve",
        "s",
    );
    String::from_utf8(client.download(&url).await.expect("download a script")).unwrap()
}

/// One `SieveScript/set`, with a `Blob/upload` of `script` before it as `#b1` where given:
/// the answer of the set, or of the method error.
pub(crate) async fn sieve_set(
    client: &JmapClient,
    arguments: Value,
    script: Option<&str>,
) -> Value {
    let account = sieve(client);
    let mut request = Request::new([capability::CORE, capability::SIEVE, capability::BLOB]);
    if let Some(script) = script {
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
    match response.result(&set) {
        Ok(result) => result.clone(),
        Err(err) => json!({ "methodError": err.to_string() }),
    }
}

/// `SieveScript/validate` of `script`, uploaded in a request of its own first: Stalwart's
/// validate cannot see a blob uploaded in the same request (`docs/spec-deviations.md`).
pub(crate) async fn validate(client: &JmapClient, script: &str) -> Value {
    let account = sieve(client);
    let mut request = Request::new([capability::CORE, capability::BLOB]);
    let upload = request.invoke(
        "Blob/upload",
        json!({ "accountId": account, "create": { "b1": {
            "data": [{ "data:asText": script }], "type": "application/sieve"
        } } }),
    );
    let response = client.execute(&request).await.expect("upload a script");
    let blob = response.result(&upload).unwrap()["created"]["b1"]["id"].clone();
    let mut request = Request::new([capability::CORE, capability::SIEVE]);
    let check = request.invoke(
        "SieveScript/validate",
        json!({ "accountId": account, "blobId": blob }),
    );
    let response = client.execute(&request).await.expect("validate");
    response
        .result(&check)
        .expect("SieveScript/validate")
        .clone()
}

/// `from` sends `subject` to `to`, through the server's own delivery, where Sieve runs.
pub(crate) async fn send(
    from: &JmapProvider,
    sender: &ScratchAccount,
    to: &ScratchAccount,
    subject: &str,
) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let draft = Draft::new(
        MessageIdHeader::new(format!("live-sieve-{nanos}@test.local")).unwrap(),
        EmailAddress::new(&sender.address),
        vec![EmailAddress::new(&to.address)],
        subject,
        "A message for the rules.",
    );
    from.submit_email(&account(), &draft, &HandOver::new(&Unrecorded))
        .await
        .expect("send");
}

/// One message found: its subject, keywords, id and mailbox ids.
pub(crate) type Found = (String, Value, String, Value);

/// `client`'s messages whose subject contains `text`, waiting up to `wait` for at least one.
pub(crate) async fn messages(client: &JmapClient, text: &str, wait: Duration) -> Vec<Found> {
    let mail = client
        .session()
        .account_for(capability::MAIL)
        .unwrap()
        .clone();
    let deadline = Instant::now() + wait;
    loop {
        let mut request = Request::new([capability::CORE, capability::MAIL]);
        let query = request.invoke(
            "Email/query",
            json!({ "accountId": mail, "filter": { "subject": text } }),
        );
        let get = request.invoke(
            "Email/get",
            json!({ "accountId": mail,
                    "#ids": { "resultOf": query.as_str(), "name": "Email/query", "path": "/ids" },
                    "properties": ["subject", "keywords", "mailboxIds"] }),
        );
        let response = client.execute(&request).await.expect("query mail");
        let found: Vec<Found> = response.result(&get).unwrap()["list"]
            .as_array()
            .unwrap()
            .iter()
            .map(|email| {
                (
                    email["subject"].as_str().unwrap_or_default().to_owned(),
                    email["keywords"].clone(),
                    email["id"].as_str().unwrap().to_owned(),
                    email["mailboxIds"].clone(),
                )
            })
            .collect();
        if !found.is_empty() || Instant::now() > deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Removes `client`'s messages whose subject contains `text`, which only this run's carry.
pub(crate) async fn remove_messages(client: &JmapClient, text: &str) {
    let found = messages(client, text, Duration::ZERO).await;
    if found.is_empty() {
        return;
    }
    let mail = client
        .session()
        .account_for(capability::MAIL)
        .unwrap()
        .clone();
    let ids: Vec<&str> = found.iter().map(|(_, _, id, _)| id.as_str()).collect();
    let mut request = Request::new([capability::CORE, capability::MAIL]);
    request.invoke("Email/set", json!({ "accountId": mail, "destroy": ids }));
    client
        .execute(&request)
        .await
        .expect("remove this run's messages");
}

/// Puts Carol's reply back as `prior_reply` (the object read before the suite), removes every
/// script not in `prior` (deactivating one only where it is the suite's), and checks the scripts
/// are as they were: what each suite ends with, pass or fail. It touches nothing the suite did not
/// change: each suite starts only on an account with no scripts at all.
pub(crate) async fn restore(
    client: &JmapClient,
    prior: &[Listed],
    prior_reply: &Value,
) -> Result<(), String> {
    let account = client
        .session()
        .account_for(capability::VACATION_RESPONSE)
        .unwrap()
        .clone();
    let mut back = prior_reply.clone();
    if let Some(object) = back.as_object_mut() {
        object.remove("id");
    }
    let mut request = Request::new([capability::CORE, capability::VACATION_RESPONSE]);
    request.invoke(
        "VacationResponse/set",
        json!({ "accountId": account, "update": { "singleton": back } }),
    );
    client
        .execute(&request)
        .await
        .map_err(|err| format!("the reply: {err}"))?;
    let (now, _) = scripts(client).await;
    let added: Vec<&Listed> = now
        .iter()
        .filter(|(id, ..)| prior.iter().all(|(was, ..)| was != id))
        .collect();
    if added.iter().any(|(.., active, _)| *active) {
        sieve_set(client, json!({ "onSuccessDeactivateScript": true }), None).await;
    }
    let ids: Vec<&str> = added.iter().map(|(id, ..)| id.as_str()).collect();
    let answer = sieve_set(client, json!({ "destroy": ids }), None).await;
    if answer.get("notDestroyed").is_some() {
        return Err(format!("removing the suite's scripts: {answer}"));
    }
    let (after, _) = scripts(client).await;
    let shape = |list: &[Listed]| {
        list.iter()
            .map(|(id, name, active, _)| (id.clone(), name.clone(), *active))
            .collect::<Vec<_>>()
    };
    if shape(&after) != shape(prior) {
        return Err(format!("the scripts differ: {:?}", shape(&after)));
    }
    if reply_object(client).await != *prior_reply {
        return Err("the reply differs from what it was".to_owned());
    }
    Ok(())
}

/// Runs `step`, a part of a suite's cleanup, so that one that panics leaves the others to run:
/// its panic is reported, and the suite fails after every step has had its turn.
pub(crate) async fn cleanup(
    what: &str,
    step: impl std::future::Future<Output = ()>,
) -> Result<(), String> {
    use futures_util::FutureExt;
    std::panic::AssertUnwindSafe(step)
        .catch_unwind()
        .await
        .map_err(|_| format!("{what} panicked"))
}

/// The harness, ready, or `None` where no harness is configured.
pub(crate) fn harness(suite: &str) -> Option<Harness> {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping {suite}: STALWART_HTTP_ADDR unset");
        return None;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("harness ready");
    Some(harness)
}

/// The provider's rules, which the harness account has.
pub(crate) fn rules(provider: &JmapProvider) -> &dyn engine_provider::MailRules {
    provider
        .mail_rules()
        .expect("an account with JMAP Sieve has rules")
}

/// Creates a top-level folder named `name` in `client`'s mail account, answering its id.
pub(crate) async fn create_folder(client: &JmapClient, name: &str) -> String {
    let mail = client
        .session()
        .account_for(capability::MAIL)
        .unwrap()
        .clone();
    let mut request = Request::new([capability::CORE, capability::MAIL]);
    let set = request.invoke(
        "Mailbox/set",
        json!({ "accountId": mail, "create": { "f": { "name": name, "parentId": null } } }),
    );
    let response = client.execute(&request).await.expect("create a folder");
    response.result(&set).unwrap()["created"]["f"]["id"]
        .as_str()
        .expect("the folder is created")
        .to_owned()
}

/// Removes the folder `id`, with what it holds, which only this run put there.
pub(crate) async fn remove_folder(client: &JmapClient, id: &str) {
    let mail = client
        .session()
        .account_for(capability::MAIL)
        .unwrap()
        .clone();
    let mut request = Request::new([capability::CORE, capability::MAIL]);
    request.invoke(
        "Mailbox/set",
        json!({ "accountId": mail, "destroy": [id], "onDestroyRemoveEmails": true }),
    );
    client.execute(&request).await.expect("remove the folder");
}

/// The `VacationResponse` object as the server holds it.
pub(crate) async fn reply_object(client: &JmapClient) -> Value {
    let account = client
        .session()
        .account_for(capability::VACATION_RESPONSE)
        .unwrap()
        .clone();
    let mut request = Request::new([capability::CORE, capability::VACATION_RESPONSE]);
    let get = request.invoke("VacationResponse/get", json!({ "accountId": account }));
    let response = client.execute(&request).await.expect("read the reply");
    response.result(&get).unwrap()["list"][0].clone()
}
