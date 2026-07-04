//! Explore a live IMAP account: list folders and the most recent messages.
//!
//! **Read-only by default** — it lists folders + recent mail. It validates the
//! `provider-imap` client against a *real* IMAP provider over a properly
//! **verifying** TLS connector (Mozilla roots via `webpki-roots`), in contrast to
//! the self-signed Stalwart fixture. Two read-only opt-ins prove more of the client:
//! `IMAP_QRESYNC` runs the CONDSTORE/QRESYNC incremental delta (snapshot, then an
//! immediate delta that reconciles without re-snapshotting), and `IMAP_IDLE` opens an
//! IMAP IDLE push watch on the mailbox so new mail appears instantly. Two opt-in writes
//! exercise the rest: `IMAP_DRAFT` saves a draft (IMAP `APPEND`), and `IMAP_SEND`
//! submits a test mail to yourself over SMTP (`AUTH PLAIN` + implicit TLS on port 465).
//!
//! Credentials come from the environment — never hard-code or paste a password:
//!
//! ```sh
//! export IMAP_HOST=imap.example.com IMAP_USER=you@example.com
//! read -rs IMAP_PASS; export IMAP_PASS   # type the password (no echo)
//! cargo run -p provider-imap --example imap_explore
//! # optional: IMAP_PORT=993 (default), IMAP_MAILBOX=INBOX (default)
//! # optional: IMAP_QRESYNC=1 verifies the CONDSTORE/QRESYNC delta (read-only).
//! # optional: IMAP_IDLE=1 watches the mailbox via IMAP IDLE push (read-only);
//! #           IMAP_IDLE_SECS sets the window (default 40) — send yourself mail to see it.
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
use provider_imap::{ImapConfig, ImapProvider, ImapWatcher};
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
    print_recent(&provider, &account, &mailbox).await?;

    // Opt-in read-only QRESYNC check (no content printed, nothing mutated).
    if env::var("IMAP_QRESYNC").is_ok() {
        qresync_check(&provider, &account).await?;
    }

    // Opt-in read-only push: watch the mailbox via IMAP IDLE and print events.
    if env::var("IMAP_IDLE").is_ok() {
        idle_watch(&config, &mailbox).await?;
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
            "\nDone — read-only. IMAP_QRESYNC=1 verifies the CONDSTORE/QRESYNC delta; \
             IMAP_DRAFT=1 saves a draft; IMAP_SEND=1 sends a test mail \
             (needs IMAP_SMTP_HOST or an imap.→smtp. host)."
        );
    }
    Ok(())
}

/// Prints the newest page of the bound mailbox (read-only metadata fetch): unread
/// marker, delivery date, sender, and subject, newest first.
async fn print_recent<P: Provider>(
    provider: &P,
    account: &AccountId,
    mailbox: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let page = provider.sync_email_page(account, None, None, 20).await?;
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
    Ok(())
}

/// Read-only proof that the CONDSTORE/QRESYNC incremental delta runs end to end
/// against a real provider: snapshot the mailbox, then immediately re-sync from that
/// cursor. Nothing changed in between, so a QRESYNC delta must come back as a
/// near-empty `Delta` and — crucially — must NOT re-list or tombstone the whole mailbox
/// the way a snapshot would. Metadata only: no message content is printed, and nothing
/// is mutated (no flags set, nothing expunged).
async fn qresync_check<P: Provider>(
    provider: &P,
    account: &AccountId,
) -> Result<(), Box<dyn std::error::Error>> {
    let snap = provider.sync_email_page(account, None, None, 5).await?;
    let has_modseq = snap.next_cursor.as_str().contains(";m");
    println!("\n[QRESYNC] snapshot SELECT recorded a HIGHESTMODSEQ in the cursor: {has_modseq}");
    let delta = provider
        .sync_email_page(account, Some(&snap.next_cursor), None, 5)
        .await?;
    let pass = has_modseq
        && matches!(delta.kind, engine_provider::SyncKind::Delta)
        && delta.removed.is_empty();
    println!(
        "[QRESYNC] immediate re-sync → kind={:?}, changed={}, removed={} (expected Delta, ~0/0)",
        delta.kind,
        delta.changed.len(),
        delta.removed.len()
    );
    println!(
        "[QRESYNC] {}",
        if pass {
            "PASS — CONDSTORE/QRESYNC negotiated; the delta reconciles incrementally \
             without re-snapshotting the mailbox"
        } else {
            "server did not negotiate QRESYNC — using the new-arrivals fallback"
        }
    );
    Ok(())
}

/// Read-only push demo: open an IMAP IDLE watcher on the bound mailbox and print each
/// change / keep-alive event for a short window (`IMAP_IDLE_SECS`, default 40). Send
/// yourself an email while it runs to watch the `Changed` notification arrive instantly
/// — nothing is mutated. A 20-second keep-alive makes the periodic heartbeat visible in
/// the window (a real desktop watch uses the 28-minute default).
async fn idle_watch(config: &ImapConfig, mailbox: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;

    let window: u64 = env::var("IMAP_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    println!(
        "\n[IDLE] watching {mailbox} for {window}s — send yourself an email to see it arrive \
         instantly (nothing is mutated)..."
    );
    let mut watcher = ImapWatcher::connect(
        config,
        verifying_connector(),
        MailboxId::try_from(mailbox)?,
        Duration::from_secs(20),
    )
    .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(window);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, watcher.next_event()).await {
            Ok(Ok(event)) => println!("[IDLE] {event:?} — a host would sync the mailbox now"),
            Ok(Err(e)) => {
                println!("[IDLE] watch error (a host would reconnect): {e}");
                break;
            }
            Err(_) => break, // the demo window elapsed
        }
    }
    watcher.stop().await.ok();
    println!("[IDLE] done watching.");
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
