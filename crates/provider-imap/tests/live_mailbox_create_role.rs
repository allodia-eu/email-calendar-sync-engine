//! Gated live integration: a create that gives the new folder a role, `CREATE … (USE (…))`
//! (RFC 6154 §3), against every configured IMAP server.
//!
//! A client may send `USE` only to a server that advertises `CREATE-SPECIAL-USE`, so each
//! server is first asked for its capabilities on a raw session, and the adapter must report
//! exactly that as `mailbox_create_role`:
//!
//! - **Where it is advertised,** the adapter's create must make a folder the server then lists with
//!   the attribute, and a retry must meet it.
//! - **Where it is not,** the adapter must refuse the create and make nothing. What the server
//!   would have done is then sent raw: the `CREATE … (USE (…))` the adapter withholds, once with a
//!   role another folder holds (`\Archive`, which the Dovecot harness gives its `Archive`) and once
//!   with one no folder holds (`\Flagged`), each read back by a plain `LIST` and by one asking
//!   `RETURN (SPECIAL-USE)`, beside a control showing that listing does show a role (`Archive`'s).
//!   On every server a refused create must have made nothing and a taken one the folder. On Dovecot
//!   the observed behaviour is asserted, so DOVECOT-1 goes red the day it changes: it takes `USE`
//!   unadvertised and the folder has no role either way.
//!
//! Every name starts with `RoleCreate` and is unique to the run; each server is first cleared of
//! the folders an earlier, failed run left behind.
//!
//! On Stalwart it runs as the scratch account `carol@`; the Dovecot harness has only Alice, and
//! every folder made here is gone again at the end. Skips per server when its address variable
//! is unset.

#[path = "common/imap_live.rs"]
mod imap_live;

use std::time::{SystemTime, UNIX_EPOCH};

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId},
    mail::{Mailbox, MailboxRole},
};
use engine_provider::{MailboxEdit, MailboxWrites, Provider};
use imap_live::{DOVECOT_REV1, DOVECOT_REV2, LiveProvider, Server, connect_to, folders};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::{client::TlsStream, rustls::pki_types::ServerName};

/// Stalwart, as the scratch account nothing counts.
const STALWART_SCRATCH: Server = Server {
    label: "stalwart",
    addr_var: "STALWART_IMAP_ADDR",
    account: "carol@test.local",
    password: "harness-carol-pw",
};

const SERVERS: [Server; 3] = [STALWART_SCRATCH, DOVECOT_REV1, DOVECOT_REV2];

fn account() -> AccountId {
    AccountId::try_from("live-harness").unwrap()
}

fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}{nanos}")
}

async fn create(provider: &LiveProvider, edit: &MailboxEdit) -> Result<MailboxId, FailureClass> {
    provider
        .edit_mailbox(&account(), edit)
        .await
        .map(|receipt| receipt.mailbox.expect("a create names its folder"))
        .map_err(|error| error.class())
}

async fn delete(provider: &LiveProvider, target: &MailboxId, label: &str) {
    provider
        .edit_mailbox(
            &account(),
            &MailboxEdit::Delete {
                target: target.clone(),
            },
        )
        .await
        .unwrap_or_else(|err| panic!("delete {target} on {label}: {err}"));
}

fn listed(all: &[Mailbox], name: &str) -> Option<Mailbox> {
    all.iter().find(|m| m.name == name).cloned()
}

/// A raw, logged-in session, for what the adapter will not send.
struct Raw {
    io: BufReader<TlsStream<tokio::net::TcpStream>>,
    tag: u32,
}

impl Raw {
    async fn open(server: &Server, addr: &str) -> Self {
        let tcp = tokio::net::TcpStream::connect(addr)
            .await
            .unwrap_or_else(|err| panic!("dial {}: {err}", server.label));
        let tls = engine_tls::TlsClientConfig::dangerous_accept_any()
            .connector()
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .expect("TLS");
        let mut raw = Self {
            io: BufReader::new(tls),
            tag: 0,
        };
        raw.read_until("*").await;
        let login = format!("LOGIN \"{}\" \"{}\"", server.account, server.password);
        let answer = raw.command(&login).await;
        assert!(raw.ok(&answer), "{} login: {answer}", server.label);
        raw
    }

    /// Sends `line` under the next tag and returns every line up to its completion.
    async fn command(&mut self, line: &str) -> String {
        self.tag += 1;
        let tag = format!("r{} ", self.tag);
        self.io
            .get_mut()
            .write_all(format!("{tag}{line}\r\n").as_bytes())
            .await
            .unwrap();
        self.read_until(&tag).await
    }

    /// Deletes every `RoleCreate…` folder an earlier, failed run left behind.
    async fn remove_leftovers(&mut self, label: &str) {
        let listing = self.command("LIST \"\" \"RoleCreate*\"").await;
        for name in listed_names(&listing) {
            let answer = self.command(&format!("DELETE \"{name}\"")).await;
            eprintln!("{label}: removed the leftover {name}: {}", self.ok(&answer));
        }
    }

    fn ok(&self, answer: &str) -> bool {
        answer.contains(&format!("r{} OK", self.tag))
    }

    async fn read_until(&mut self, tag: &str) -> String {
        let mut read = String::new();
        loop {
            let mut line = String::new();
            if self
                .io
                .read_line(&mut line)
                .await
                .expect("the server answers")
                == 0
            {
                return read;
            }
            read.push_str(&line);
            if line.starts_with(tag) {
                return read;
            }
        }
    }
}

/// The mailbox names of the `* LIST` lines in `listing`, quoted (Stalwart) or bare atoms
/// (Dovecot rev1); the names this test makes need no escaping.
fn listed_names(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter(|line| line.starts_with("* LIST "))
        .filter_map(|line| line.trim_end().rsplit(' ').next())
        .map(|name| name.trim_matches('"').to_owned())
        .collect()
}

/// The flags of the `* LIST` line naming `name` in `listing`, or `None` when none does.
fn list_flags(listing: &str, name: &str) -> Option<String> {
    listing
        .lines()
        .filter(|line| line.starts_with("* LIST ("))
        .find(|line| line.trim_end().trim_end_matches('"').ends_with(name))
        .and_then(|line| line.split_once('(')?.1.split_once(')'))
        .map(|(flags, _)| flags.to_owned())
}

#[tokio::test]
async fn a_role_on_create_is_offered_exactly_where_the_server_advertises_it() {
    let test = "a_role_on_create_is_offered_exactly_where_the_server_advertises_it";
    for server in &SERVERS {
        let Some(provider) = connect_to(server, "INBOX", test).await else {
            continue;
        };
        let label = server.label;
        let addr = std::env::var(server.addr_var).unwrap();
        let mut raw = Raw::open(server, &addr).await;
        raw.remove_leftovers(label).await;
        let capability = raw.command("CAPABILITY").await;
        let advertised = capability
            .split_ascii_whitespace()
            .any(|atom| atom.eq_ignore_ascii_case("CREATE-SPECIAL-USE"));
        eprintln!("{label}: CREATE-SPECIAL-USE advertised: {advertised}");
        assert_eq!(
            provider
                .connection_info()
                .capabilities
                .mailbox_create_role(),
            advertised,
            "{label}: the capability is what the server advertised"
        );
        let special_use = capability
            .split_ascii_whitespace()
            .any(|atom| atom.eq_ignore_ascii_case("SPECIAL-USE"));
        if advertised {
            made_with_a_role(&provider, &mut raw, label).await;
        } else {
            refused_and_proven_unsupported(&provider, &mut raw, label, special_use).await;
        }
        let _ = raw.command("LOGOUT").await;
    }
}

/// Where the server advertises it: the folder is made with the role, lists with it, and a
/// retry meets it; a role this test's own folder holds is refused for a second.
async fn made_with_a_role(provider: &LiveProvider, raw: &mut Raw, label: &str) {
    let held: Vec<MailboxRole> = folders(provider)
        .await
        .into_iter()
        .filter_map(|m| m.role)
        .collect();
    eprintln!("{label}: roles before: {held:?}");
    let role = [
        MailboxRole::Archive,
        MailboxRole::Important,
        MailboxRole::Flagged,
    ]
    .into_iter()
    .find(|role| !held.contains(role))
    .unwrap_or_else(|| panic!("{label}: a free role to give"));
    let name = unique("RoleCreate");
    let edit = MailboxEdit::create_with_role(name.as_str(), None, role.clone());
    let id = create(provider, &edit)
        .await
        .unwrap_or_else(|class| panic!("{label}: create with {role}: {class:?}"));

    let stored =
        listed(&folders(provider).await, &name).unwrap_or_else(|| panic!("{label} lists {name}"));
    assert_eq!(
        stored.role.as_ref(),
        Some(&role),
        "{label}: listed with the role"
    );
    assert_eq!(
        create(provider, &edit).await,
        Ok(id.clone()),
        "{label}: the retry"
    );

    // A second folder asking for the same role: RFC 6154 §3 lets a server refuse it, move the
    // role, or keep two. Printed, so the docs say which this server does.
    let second = unique("RoleCreateSecond");
    let again = MailboxEdit::create_with_role(second.as_str(), None, role.clone());
    let outcome = create(provider, &again).await;
    eprintln!("{label}: a second folder with {role}: {outcome:?}");
    let all = folders(provider).await;
    eprintln!(
        "{label}: roles after: first {:?}, second {:?}",
        listed(&all, &name).and_then(|m| m.role),
        listed(&all, &second).and_then(|m| m.role)
    );
    match outcome {
        Ok(made) => delete(provider, &made, label).await,
        Err(class) => assert_eq!(class, FailureClass::Conflict, "{label}"),
    }
    // The server's own answer to that second create, for the docs.
    let attribute = role
        .as_imap_special_use()
        .expect("a role with an attribute");
    let raw_name = unique("RoleCreateRaw");
    let answer = raw
        .command(&format!("CREATE \"{raw_name}\" (USE ({attribute}))"))
        .await;
    eprintln!("{label}: a raw second CREATE with {attribute} answered: {answer}");
    if raw.ok(&answer) {
        let _ = raw.command(&format!("DELETE \"{raw_name}\"")).await;
    }

    // A role with no attribute is refused before sending.
    let records = MailboxEdit::create_with_role(name.as_str(), None, MailboxRole::ServerRecords);
    assert_eq!(
        create(provider, &records).await,
        Err(FailureClass::InvalidState)
    );

    delete(provider, &id, label).await;
    assert!(listed(&folders(provider).await, &name).is_none(), "{label}");
}

/// Where the server does not advertise it: the adapter refuses and makes nothing. Then the
/// `USE` the adapter withholds is sent raw, for a held and an unheld role, and what the server
/// does with it is recorded; on Dovecot it is asserted (DOVECOT-1).
async fn refused_and_proven_unsupported(
    provider: &LiveProvider,
    raw: &mut Raw,
    label: &str,
    special_use: bool,
) {
    let name = unique("RoleCreate");
    let edit = MailboxEdit::create_with_role(name.as_str(), None, MailboxRole::Archive);
    assert_eq!(
        create(provider, &edit).await,
        Err(FailureClass::InvalidState),
        "{label}: refused before sending"
    );
    assert!(
        listed(&folders(provider).await, &name).is_none(),
        "{label}: nothing was made"
    );

    // The control: this session's listing does show a role where a folder has one.
    let dovecot = label.starts_with("dovecot");
    // Dovecot advertises SPECIAL-USE, so the control and the asked-for listing below cannot
    // skip themselves there.
    assert!(!dovecot || special_use, "{label}: SPECIAL-USE advertised");
    if special_use {
        let control = raw
            .command("LIST \"\" \"Archive\" RETURN (SPECIAL-USE)")
            .await;
        let flags = list_flags(&control, "Archive");
        eprintln!("{label}: the control, Archive, lists with {flags:?}");
        if dovecot {
            assert!(
                flags.as_deref().is_some_and(|f| f.contains("\\Archive")),
                "{label}: the harness gives Archive its role: {control}"
            );
        }
    }

    // `\Archive` is held (Dovecot's `Archive`); `\Flagged` is not, on either harness server.
    for attribute in ["\\Archive", "\\Flagged"] {
        let probe = unique("RoleCreateRaw");
        let with_use = raw
            .command(&format!("CREATE \"{probe}\" (USE ({attribute}))"))
            .await;
        let took_use = raw.ok(&with_use);
        eprintln!(
            "{label}: CREATE with USE ({attribute}), unadvertised: took it {took_use}: {with_use}"
        );
        let plain_list = raw.command(&format!("LIST \"\" \"{probe}\"")).await;
        let plain_flags = list_flags(&plain_list, &probe);
        // The return option only where advertised: anywhere else it is a `BAD` (RFC 9051 §6.3.9).
        let asked_flags = if special_use {
            let asked = raw
                .command(&format!("LIST \"\" \"{probe}\" RETURN (SPECIAL-USE)"))
                .await;
            list_flags(&asked, &probe)
        } else {
            None
        };
        let via_adapter = listed(&folders(provider).await, &probe).map(|m| m.role);
        eprintln!(
            "{label}: {attribute}: listed plainly with {plain_flags:?}, asking SPECIAL-USE with \
             {asked_flags:?}; the adapter gives it the role {via_adapter:?}"
        );
        // True whichever way the server went: a refused create made nothing, a taken one a folder.
        assert_eq!(plain_flags.is_some(), took_use, "{label}: {plain_list}");
        if took_use {
            let removed = raw.command(&format!("DELETE \"{probe}\"")).await;
            assert!(raw.ok(&removed), "{label}: {removed}");
        }
        if dovecot {
            // DOVECOT-1, pinned: it takes `USE` it does not advertise, and gives no role.
            assert!(
                took_use,
                "{label}: DOVECOT-1 changed, {attribute}: {with_use}"
            );
            let carries =
                |flags: &Option<String>| flags.as_deref().is_some_and(|f| f.contains(attribute));
            assert!(
                !carries(&plain_flags) && !carries(&asked_flags),
                "{label}: DOVECOT-1 changed, {attribute} now sticks"
            );
            assert_eq!(via_adapter, Some(None), "{label}: no role, {attribute}");
        }
    }

    // The plain `CREATE` the adapter would send for an ordinary folder.
    let plain = raw.command(&format!("CREATE \"{name}\"")).await;
    assert!(raw.ok(&plain), "{label}: a plain CREATE: {plain}");
    let removed = raw.command(&format!("DELETE \"{name}\"")).await;
    assert!(raw.ok(&removed), "{label}: {removed}");
}
