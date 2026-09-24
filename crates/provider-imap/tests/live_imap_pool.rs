//! Gated live integration: an account's folders share one bounded set of sockets.
//!
//! The claim is an upper bound — "this account never logs in more than five times however many
//! folders sync at once" — and a bound is an absence, so the test proves it can fail first. The
//! control arm connects one account *per folder*, the way hosts did before `ImapAccount`, and
//! must see a login for every one; only then is the pooled arm's lower count evidence of the
//! pool rather than of an observer that stopped counting. Logins are counted through the
//! config's connect observer, which every pool dial reports through.
//!
//! Skips with no `STALWART_IMAP_ADDR`, so the offline `cargo test --workspace` stays green.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use engine_core::ids::{AccountId, MailboxId};
use engine_provider::{ConnectObserver, ConnectStep, Provider};
use futures_util::future::join_all;
use provider_imap::{DEFAULT_IDLE_KEEPALIVE, ImapAccount, ImapConfig};
use stalwart_harness::Harness;

/// The account's budget (`provider_imap`'s `DEFAULT_MAX_CONNECTIONS`), restated because the
/// test asserts it from outside the crate.
const BUDGET: usize = 5;
/// Comfortably over the budget, so a pool that dialled per caller would show it.
const FOLDERS: usize = 12;

/// Counts successful logins — one per socket the account opened.
#[derive(Default)]
struct Logins(AtomicUsize);

impl ConnectObserver for Logins {
    fn step(&self, step: &ConnectStep<'_>) {
        if matches!(step, ConnectStep::Authenticated) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn config(harness: &Harness, logins: &Arc<Logins>) -> ImapConfig {
    let host = harness
        .imap_addr
        .rsplit_once(':')
        .map_or("localhost", |(host, _)| host);
    ImapConfig::new(
        harness.imap_addr.as_str(),
        host,
        harness.account.as_str(),
        harness.password.as_str(),
    )
    .with_connect_observer(Arc::clone(logins) as Arc<dyn ConnectObserver>)
}

fn connector() -> tokio_rustls::TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

#[tokio::test]
async fn live_imap_an_accounts_folders_sync_at_once_within_its_budget() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_imap_pool: STALWART_IMAP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("harness ready");
    let account = AccountId::try_from("imap-live-pool").unwrap();
    let inbox = MailboxId::try_from("INBOX").unwrap();

    // Control: an account per folder, each dialling its own socket. The observer must count
    // every one, or the pooled arm below proves nothing.
    let unpooled_logins = Arc::new(Logins::default());
    let unpooled = join_all((0..FOLDERS).map(|_| {
        let config = config(&harness, &unpooled_logins);
        async move { ImapAccount::connect(&config, connector()).await }
    }))
    .await;
    assert!(unpooled.iter().all(Result::is_ok), "every control connect");
    assert_eq!(unpooled_logins.0.load(Ordering::SeqCst), FOLDERS);
    drop(unpooled);

    // Pooled: one account, a watch holding one connection, and every folder syncing at once.
    let logins = Arc::new(Logins::default());
    let imap = ImapAccount::connect(&config(&harness, &logins), connector())
        .await
        .expect("connect the account");
    let _watch = imap
        .watch(inbox.clone(), DEFAULT_IDLE_KEEPALIVE)
        .await
        .expect("Stalwart advertises IDLE");
    let providers: Vec<_> = (0..FOLDERS).map(|_| imap.provider(inbox.clone())).collect();

    let listed = join_all(
        providers
            .iter()
            .map(|provider| provider.sync_mailboxes(&account, None)),
    )
    .await;

    assert!(
        listed.iter().all(Result::is_ok),
        "a caller over budget waits for a connection rather than failing: {listed:?}",
    );
    let opened = logins.0.load(Ordering::SeqCst);
    assert!(
        opened <= BUDGET,
        "{FOLDERS} folders syncing at once opened {opened} sockets on a budget of {BUDGET}",
    );
    assert!(
        opened > 1,
        "only the first connection was ever used — the syncs ran one at a time, not at once",
    );
}
