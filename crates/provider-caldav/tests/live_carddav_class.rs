//! Gated check that a real server's address-book home advertises CardDAV, and that the connect
//! trace says so.
//!
//! Stalwart lists `addressbook` in the `DAV:` header of an `OPTIONS` on the home (RFC 6352
//! §6.1). The offline suite pins what the adapter does with either answer; this pins that the
//! question reaches the right resource on a server that answers it. Skips with no
//! `STALWART_HTTP_ADDR`.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use engine_provider::{ConnectObserver, ConnectStep, Provider};
use provider_caldav::{CardDavConfig, CardDavProvider, Credentials};
use stalwart_harness::Harness;

/// Records the classes a connect reported.
#[derive(Default)]
struct Negotiated(Mutex<Vec<String>>);

impl ConnectObserver for Negotiated {
    fn step(&self, step: &ConnectStep<'_>) {
        if let ConnectStep::Negotiated { features, .. } = step {
            self.0
                .lock()
                .unwrap()
                .extend(features.iter().map(|feature| (*feature).to_owned()));
        }
    }
}

#[tokio::test]
async fn stalwart_advertises_address_books_on_the_home() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping the CardDAV class check: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("harness ready");
    let observer = Arc::new(Negotiated::default());
    let provider = CardDavProvider::connect(
        CardDavConfig::new(
            format!("http://{}", harness.http_addr),
            Credentials::Basic {
                username: harness.account.clone(),
                password: harness.password.clone(),
            },
        )
        .with_connect_observer(Arc::clone(&observer) as Arc<dyn ConnectObserver>),
    )
    .await
    .expect("CardDAV connect");

    assert!(provider.connection_info().capabilities.contacts());
    let classes = observer.0.lock().unwrap().clone();
    assert!(
        classes.iter().any(|class| class == "addressbook"),
        "the home advertised {classes:?}"
    );
}
