//! Gated live: the ManageSieve client against both harness servers, Stalwart (`127.0.0.1:14190`,
//! as the scratch account `carol@`) and Dovecot (`127.0.0.1:14191`).
//!
//! Every command the client sends goes to a real server here: a sign-in, refused and accepted,
//! `LISTSCRIPTS`, `PUTSCRIPT` and `GETSCRIPT` of a script of the run's own, `CHECKSCRIPT` of a
//! script needing an extension the server does not list, and `DELETESCRIPT`. The script is named
//! for the run and deleted afterwards, so it disturbs nothing the account already holds. Skips
//! per server when its address variable is unset.

use std::time::{SystemTime, UNIX_EPOCH};

use engine_provider::Credential;

use super::{dial, name::ScriptName, response::ResponseCode};
use crate::ImapConfig;

/// A harness server's ManageSieve: its label, address variable, account, password, and a
/// Sieve extension it does not list.
pub(super) const SERVERS: [(&str, &str, &str, &str, &str); 2] = [
    (
        "stalwart",
        "STALWART_MANAGESIEVE_ADDR",
        "carol@test.local",
        "harness-carol-pw",
        // Stalwart takes any name in `require` it does not know (STALWART-8), but refuses one it
        // knows and does not offer.
        "imapsieve",
    ),
    (
        "dovecot",
        "DOVECOT_MANAGESIEVE_ADDR",
        "alice@test.local",
        "dovecot-alice-pw",
        "editheader",
    ),
];

/// Held by every live ManageSieve test for its whole run: they share each server's account, and the
/// rules' version is a hash of the account's script list, so a script another test stores for a
/// moment would make a write look stale. The test binary runs its tests on several threads.
pub(super) static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) fn config(addr: &str, user: &str, password: &str) -> ImapConfig {
    // The IMAP half is never dialled here.
    ImapConfig::new(
        "127.0.0.1:1",
        "localhost",
        Credential::password(user, password),
    )
    .with_managesieve(addr, "localhost")
}

#[tokio::test]
async fn the_client_s_commands_are_answered_by_both_servers() {
    let _alone = ONE_AT_A_TIME.lock().await;
    let connector = engine_tls::TlsClientConfig::dangerous_accept_any().connector();
    for (label, var, user, password, lacking) in SERVERS {
        let Ok(addr) = std::env::var(var) else {
            eprintln!("skipping ManageSieve on {label}: {var} unset");
            continue;
        };
        let wrong = config(&addr, user, "not-the-password");
        let endpoint = wrong.managesieve.clone().unwrap();
        let err = dial::connect(&wrong, &endpoint, &connector)
            .await
            .expect_err("a wrong password is refused");
        assert!(err.is_sign_in_refusal(), "{label}: {err:?}");

        let config = config(&addr, user, password);
        let mut session = dial::connect(&config, &endpoint, &connector)
            .await
            .unwrap_or_else(|err| panic!("{label}: sign in: {err}"));
        let run = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = ScriptName::new(&format!("zlive-client-{run}")).unwrap();
        let text = format!(
            "require [\"fileinto\"];\r\n# rule:[Live {run}]\r\nif header :contains \"subject\" \
             \"zlive-client-{run}\"\r\n{{\r\n\tfileinto \"Archive\";\r\n}}\r\n"
        );
        session.put_script(&name, &text).await.unwrap();
        let listed = session.list_scripts().await.unwrap();
        assert!(
            listed
                .iter()
                .any(|script| script.name == name && !script.active),
            "{label}: the stored script is listed, inactive"
        );
        assert_eq!(session.get_script(&name).await.unwrap(), text, "{label}");

        // Both directions: a script the server takes is checked fine, and one needing what it
        // does not list is refused, by the server, with no code.
        session.check_script(&text).await.unwrap();
        let refused = session
            .check_script(&format!("require [\"{lacking}\"];\r\nkeep;\r\n"))
            .await
            .unwrap_err();
        assert_eq!(refused.code(), None, "{label}: {refused:?}");
        assert!(!refused.to_string().contains(lacking));

        session.delete_script(&name).await.unwrap();
        let refused = session.get_script(&name).await.unwrap_err();
        assert_eq!(refused.code(), Some(ResponseCode::Nonexistent), "{label}");
        session.logout().await;
    }
}
