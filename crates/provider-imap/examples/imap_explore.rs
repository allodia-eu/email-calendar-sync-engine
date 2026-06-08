//! Explore a live IMAP account: list folders and the most recent messages.
//!
//! **Read-only by default** — it lists folders + recent mail. It validates the
//! `provider-imap` client against a *real* IMAP provider over a properly
//! **verifying** TLS connector (Mozilla roots via `webpki-roots`), in contrast to
//! the self-signed Stalwart fixture. Two opt-in writes exercise the rest: `IMAP_DRAFT`
//! saves a draft (IMAP `APPEND`), and `IMAP_SEND` submits a test mail to yourself
//! over SMTP (`AUTH PLAIN` + implicit TLS on port 465).
//!
//! Credentials come from the environment — never hard-code or paste a password:
//!
//! ```sh
//! export IMAP_HOST=imap.example.com IMAP_USER=you@example.com
//! read -rs IMAP_PASS; export IMAP_PASS   # type the password (no echo)
//! cargo run -p provider-imap --example imap_explore
//! # optional: IMAP_PORT=993 (default), IMAP_MAILBOX=INBOX (default)
//! # optional: IMAP_DRAFT=1 saves a test draft to your Drafts folder (IMAP APPEND).
//! # optional: IMAP_SEND=1 sends a test mail to yourself over SMTP AUTH+TLS.
//! #           SMTP host defaults to your IMAP host with imap.→smtp.; override with
//! #           IMAP_SMTP_HOST / IMAP_SMTP_PORT (default 465).
//! ```

use std::env;
use std::sync::Arc;

use engine_core::ids::{AccountId, MailboxId, MessageIdHeader};
use engine_core::mail::EmailAddress;
use engine_core::sync::SyncUpdate;
use engine_provider::{Draft, Provider};
use provider_imap::{ImapConfig, ImapProvider};
use tokio_rustls::TlsConnector;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (Ok(host), Ok(user), Ok(pass)) = (
        env::var("IMAP_HOST"),
        env::var("IMAP_USER"),
        env::var("IMAP_PASS"),
    ) else {
        eprintln!("Set IMAP_HOST, IMAP_USER, IMAP_PASS to run. For example:");
        eprintln!("  export IMAP_HOST=imap.example.com IMAP_USER=you@example.com");
        eprintln!("  read -rs IMAP_PASS; export IMAP_PASS   # type the password, no echo");
        eprintln!("  cargo run -p provider-imap --example imap_explore");
        return Ok(());
    };
    let port: u16 = env::var("IMAP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(993);
    let mailbox = env::var("IMAP_MAILBOX").unwrap_or_else(|_| "INBOX".to_owned());

    println!("Connecting to {host}:{port} as {user} (implicit TLS)…");
    let address = user.clone();
    let mut config = ImapConfig::new(format!("{host}:{port}"), host.clone(), user, pass);

    // Opt-in SMTP submission over implicit TLS + AUTH (a real provider; port 465).
    let sending = env::var("IMAP_SEND").is_ok();
    if sending {
        let smtp_host = env::var("IMAP_SMTP_HOST").unwrap_or_else(|_| host.replace("imap", "smtp"));
        let smtp_port: u16 = env::var("IMAP_SMTP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(465);
        config = config.with_smtp_tls(format!("{smtp_host}:{smtp_port}"), smtp_host);
    }

    let provider = ImapProvider::connect(
        &config,
        verifying_connector(),
        MailboxId::try_from(mailbox.as_str())?,
    )
    .await?;
    let account = AccountId::try_from("explore")?;

    // Folder list.
    let folders = provider.sync_mailboxes(&account, None).await?;
    if let SyncUpdate::Snapshot { objects, .. } = &folders.update {
        println!("\n{} folders:", objects.len());
        for m in objects {
            let role = m
                .role
                .as_ref()
                .map_or_else(String::new, |r| format!("  [{r}]"));
            println!("  {}{role}", m.name);
        }
    }

    // The newest page of the bound mailbox (read-only metadata fetch).
    let page = provider.sync_email_page(&account, None, None, 20).await?;
    println!(
        "\n{mailbox}: {} most recent messages (newest first):",
        page.changed.len()
    );
    for msg in &page.changed {
        let unread = if msg.is_unread() { "●" } else { " " };
        let date = msg
            .received_at
            .map_or_else(|| "?".to_owned(), |d| d.to_string());
        let from = msg
            .envelope
            .from
            .first()
            .map_or("(unknown)", |a| a.email.as_str());
        let subject = msg.envelope.subject.as_deref().unwrap_or("(no subject)");
        println!("  {unread} {date}  {from:<28.28}  {subject}");
    }
    // Opt-in write: save a test draft to Drafts via IMAP APPEND (no SMTP).
    let drafting = env::var("IMAP_DRAFT").is_ok();
    if drafting {
        let draft = Draft::new(
            MessageIdHeader::new("imap-explore-draft@example.invalid")?,
            EmailAddress::new(address.clone()),
            vec![EmailAddress::new(address.clone())],
            "Test draft from provider-imap",
            "Created by the imap_explore example — safe to delete.",
        );
        let key = provider.save_draft(&draft).await?;
        println!(
            "\nSaved a draft to Drafts (key: {key}). Check your Drafts folder — safe to delete."
        );
    }

    // Opt-in send: submit a test mail to yourself over SMTP (AUTH + implicit TLS).
    if sending {
        let mail = Draft::new(
            MessageIdHeader::new("imap-explore-send@example.invalid")?,
            EmailAddress::new(address.clone()),
            vec![EmailAddress::new(address)],
            "Test send from provider-imap",
            "Sent by the imap_explore example over SMTP AUTH + implicit TLS.",
        );
        let receipt = provider.submit_email(&account, &mail).await?;
        println!(
            "\nSent a test mail to yourself (key: {}). Check your inbox + Sent.",
            receipt.email_key
        );
    }

    if !drafting && !sending {
        println!(
            "\nDone — read-only. IMAP_DRAFT=1 saves a draft; IMAP_SEND=1 sends a test mail \
             (needs IMAP_SMTP_HOST or an imap.→smtp. host)."
        );
    }
    Ok(())
}

/// A TLS connector that verifies the server certificate against the Mozilla root
/// store (`webpki-roots`) — what a real provider needs, vs. the fixture's
/// no-verify verifier. The library stays root-store-agnostic; the host supplies
/// this.
fn verifying_connector() -> TlsConnector {
    use tokio_rustls::rustls;
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}
