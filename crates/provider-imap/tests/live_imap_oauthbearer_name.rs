//! Gated live proof of `OAUTHBEARER`'s GS2 header on the Stalwart harness, for an account name
//! holding `,` or `=` (RFC 7628 §3.1, RFC 5801 §4: `,` ends the field and `=` starts an escape,
//! so the adapter sends them as `=2C` and `=3D`).
//!
//! What the harness can show, and what it cannot:
//!
//! - **Stalwart takes the escaped header.** The adapter signs in with a token for the seeded
//!   account under a name holding both bytes, and the session opens.
//! - **Stalwart reads no authorization identity from the header,** so it cannot tell an escaped
//!   name from a broken one. Pinned against a control: a header naming the token's own account,
//!   another account, no account, the escaped name and the unescaped one all open the token's
//!   account, whose INBOX count differs from the other account's own. This is why the escaping
//!   itself (RFC conformant, checked offline in `sasl_tests.rs`) is not proven live: that needs a
//!   server that reads the field.
//!
//! Gated on `STALWART_HTTP_ADDR` (`stalwart_harness::Harness::from_env`). Every sign-in here
//! succeeds, so none counts towards Stalwart's ban on failed sign-ins. The tokens come from the
//! harness's own OAuth endpoints (`provider-jmap/tests/stalwart_oauth`).

// Shared with the JMAP and CalDAV suites; this one mints tokens and revokes none.
#[allow(dead_code)]
#[path = "../../provider-jmap/tests/stalwart_oauth/mod.rs"]
mod stalwart_oauth;

use engine_provider::Credential;
use provider_imap::{ImapAccount, ImapConfig};
use stalwart_harness::Harness;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::{TlsConnector, rustls::pki_types::ServerName};

/// The name the adapter is handed: legal in an account name, and both bytes need escaping.
const ESCAPED_NAME: &str = "al=i,ce@test.local";

fn connector() -> TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

/// A token for the seeded account, minted on the harness's own OAuth endpoints.
async fn token(harness: &Harness) -> String {
    let client = stalwart_oauth::Client::register(&harness.http_addr).await;
    client.mint_token(&harness.account, &harness.password).await
}

#[tokio::test]
async fn stalwart_takes_the_escaped_name_the_adapter_sends() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping: STALWART_HTTP_ADDR unset");
        return;
    };
    let token = token(&harness).await;
    let config = ImapConfig::new(
        harness.imap_addr.as_str(),
        "localhost",
        Credential::bearer_for(ESCAPED_NAME, token),
    );
    ImapAccount::connect(&config, connector())
        .await
        .expect("Stalwart takes OAUTHBEARER with an escaped account name");
}

/// The base64 of `bytes` (RFC 4648 §4), for a header built by hand.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let padded = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(padded[0]) << 16) | (u32::from(padded[1]) << 8) | u32::from(padded[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Reads lines until one opens with `tag` (or a continuation), and returns every line read.
async fn until_tagged<S: tokio::io::AsyncRead + Unpin>(io: &mut BufReader<S>, tag: &str) -> String {
    let mut read = String::new();
    loop {
        let mut line = String::new();
        if io.read_line(&mut line).await.expect("the server answers") == 0 {
            return read;
        }
        read.push_str(&line);
        if line.starts_with(tag) || line.starts_with('+') {
            return read;
        }
    }
}

/// Signs in with `first` (an `AUTHENTICATE` or a `LOGIN` line, tagged `a1`) and answers the
/// INBOX's message count of the account that opened, or `None` where the sign-in was refused.
async fn inbox_count(harness: &Harness, first: &str) -> Option<u64> {
    let tcp = tokio::net::TcpStream::connect(&harness.imap_addr)
        .await
        .expect("the harness listens");
    let tls = connector()
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .expect("TLS");
    let mut io = BufReader::new(tls);
    until_tagged(&mut io, "*").await;
    io.get_mut().write_all(first.as_bytes()).await.unwrap();
    if !until_tagged(&mut io, "a1 ").await.contains("a1 OK") {
        return None;
    }
    io.get_mut()
        .write_all(b"a2 STATUS INBOX (MESSAGES)\r\n")
        .await
        .unwrap();
    let status = until_tagged(&mut io, "a2 ").await;
    let count = status
        .split("MESSAGES ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .and_then(|n| n.parse().ok());
    let _ = io.get_mut().write_all(b"a3 LOGOUT\r\n").await;
    count
}

/// `OAUTHBEARER`'s initial response naming `authzid` in the GS2 header, verbatim.
fn oauthbearer(authzid: &str, token: &str) -> String {
    let blob = format!("n,a={authzid},\u{1}auth=Bearer {token}\u{1}\u{1}");
    format!(
        "a1 AUTHENTICATE OAUTHBEARER {}\r\n",
        base64(blob.as_bytes())
    )
}

#[tokio::test]
async fn stalwart_reads_no_authorization_identity_from_the_gs2_header() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping: STALWART_HTTP_ADDR unset");
        return;
    };
    let token = token(&harness).await;
    let other = &harness.scratch[0];
    let login =
        |account: &str, password: &str| format!("a1 LOGIN \"{account}\" \"{password}\"\r\n");
    let own = inbox_count(&harness, &login(&harness.account, &harness.password))
        .await
        .expect("the seeded account signs in");
    let others = inbox_count(&harness, &login(&other.address, &other.password))
        .await
        .expect("the other account signs in");
    assert_ne!(
        own, others,
        "the two accounts' INBOXes must differ for the check to tell them apart"
    );
    for authzid in [
        harness.account.as_str(),
        other.address.as_str(),
        "",
        "al=3Di=2Cce@test.local",
        "al=i,ce@test.local",
    ] {
        let opened = inbox_count(&harness, &oauthbearer(authzid, &token)).await;
        assert_eq!(
            opened,
            Some(own),
            "a header naming {authzid:?} opens the token's account"
        );
    }
}
