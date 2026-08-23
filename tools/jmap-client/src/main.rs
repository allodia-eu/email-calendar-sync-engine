//! `jmap-client` — a tiny local helper to drive a real JMAP server: capture responses as
//! offline fixtures for `provider-jmap`, and measure what the server actually does.
//!
//! The sibling of `tools/graph-oauth` and `tools/google-oauth`, and deliberately simpler than
//! either: JMAP specifies no authentication mechanism of its own (RFC 8620 §8.2), so there is
//! no OAuth dance here. A bearer token (Fastmail's API token) or basic credentials are handed
//! over directly.
//!
//! Standalone on purpose — the engine stays credential-agnostic, hosts own onboarding
//! (`north-star.md`) — and nothing provider-specific is hardcoded.
//!
//! ## Commands
//!
//! - `save --url <base> [--token T | --user U --password P]` — persist credentials to
//!   `.local/account.json` so later runs need no flags.
//! `--trust-advertised` takes the session's URLs literally; by default they are rebased onto
//! the origin actually dialled, which is the engine's default and what a proxied server (the
//! Stalwart harness) needs.
//!
//! - `session [outfile]` — resolve `/.well-known/jmap` and print the session document: the
//!   capabilities, the limits (`maxConcurrentRequests` among them) and the URL templates.
//! - `call <Method> <args-json|@file|-> [outfile]` — one method call against `apiUrl`.
//! - `get <url-or-path> [outfile]` — a raw authenticated GET, for a `downloadUrl` blob or
//!   anything else the session advertises.
//! - `bench [--messages N] [--widths 1,2,4,8]` — download N message bodies at each width and
//!   report the rate. A JMAP page of metadata is one `Email/get` for the whole page, but a
//!   body is one blob GET per message, so this is the only part of a sync with a round trip
//!   per message to overlap.
//!
//! Run from the repo root, e.g.:
//!   cargo run --manifest-path tools/jmap-client/Cargo.toml -- session

use std::{
    error::Error,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use serde_json::{Value, json};

type Res<T> = Result<T, Box<dyn Error>>;

const WELL_KNOWN: &str = "/.well-known/jmap";

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("save") => cmd_save(&args[1..]),
        Some("session") => cmd_session(args.get(1).map(String::as_str)),
        Some("call") => cmd_call(&args[1..]),
        Some("get") => cmd_get(&args[1..]),
        Some("bench") => cmd_bench(&args[1..]),
        _ => {
            eprintln!(
                "usage:\n  \
                 jmap-client save --url <base> [--token <t> | --user <u> --password <p>]\n  \
                 jmap-client session [outfile.json]\n  \
                 jmap-client call <Method> <args-json|@file|-> [outfile.json]\n  \
                 jmap-client get <url-or-path> [outfile]\n  \
                 jmap-client bench [--messages <n>] [--widths 1,2,4,8]\n\n\
                 --trust-advertised keeps the session's own URLs; the default rebases \
                 them onto the origin dialled\n\
                 credentials come from the flags, then $JMAP_URL / $JMAP_TOKEN / \
                 $JMAP_USER / $JMAP_PASSWORD, then .local/account.json"
            );
            std::process::exit(2);
        }
    }
}

// ------------------------------------------------------------------------------------------
// Credentials
// ------------------------------------------------------------------------------------------

/// How to reach one JMAP account.
#[derive(Clone)]
struct Account {
    base: String,
    token: Option<String>,
    user: Option<String>,
    password: Option<String>,
    /// Take the session's advertised URLs literally instead of rebasing them.
    trust_advertised: bool,
}

fn account_path() -> String {
    std::env::var("JMAP_ACCOUNT")
        .unwrap_or_else(|_| format!("{}/.local/account.json", env!("CARGO_MANIFEST_DIR")))
}

/// Reads a `--name value` flag out of `args`.
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

impl Account {
    /// Flags win, then the environment, then the saved file.
    fn resolve(args: &[String]) -> Res<Self> {
        let saved: Value = std::fs::read(account_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_else(|| json!({}));
        let pick = |name: &str, env: &str| -> Option<String> {
            flag(args, &format!("--{name}"))
                .or_else(|| std::env::var(env).ok().filter(|v| !v.is_empty()))
                .or_else(|| {
                    saved
                        .get(name)
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
        };
        let base = pick("url", "JMAP_URL").ok_or(
            "no server: pass --url, set $JMAP_URL, or run `jmap-client save --url …` first",
        )?;
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            token: pick("token", "JMAP_TOKEN"),
            user: pick("user", "JMAP_USER"),
            password: pick("password", "JMAP_PASSWORD"),
            trust_advertised: args.iter().any(|a| a == "--trust-advertised"),
        })
    }

    fn client(&self) -> Res<reqwest::blocking::Client> {
        Ok(reqwest::blocking::Client::builder()
            .user_agent("jmap-client/0.1")
            .build()?)
    }

    /// Applies whichever credential was supplied. Bearer wins: a server that accepts both
    /// should be exercised the way a host would reach it.
    fn authed(&self, builder: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        match (&self.token, &self.user) {
            (Some(token), _) => builder.bearer_auth(token),
            (None, Some(user)) => builder.basic_auth(user, self.password.clone()),
            (None, None) => builder,
        }
    }
}

fn cmd_save(args: &[String]) -> Res<()> {
    let account = Account::resolve(args)?;
    let path = account_path();
    if let Some(dir) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut doc = json!({ "url": account.base });
    for (key, value) in [
        ("token", &account.token),
        ("user", &account.user),
        ("password", &account.password),
    ] {
        if let Some(value) = value {
            doc[key] = json!(value);
        }
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&doc)?)?;
    // A JMAP API token is a password, and this file is the only place it is kept.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    println!("saved to {path} (mode 600)");
    Ok(())
}

// ------------------------------------------------------------------------------------------
// Session
// ------------------------------------------------------------------------------------------

/// Fetches and parses the session document, rebasing the URLs it advertises onto the origin
/// actually dialled.
///
/// This is `SessionUrlPolicy::RebaseToConnection`, the engine's default, and it is the
/// default here for the same reason: a server behind a proxy — the Stalwart harness among
/// them — advertises the origin it believes it is served from, which is not one the caller
/// can reach. `--trust-advertised` takes the document literally (RFC 8620's own reading),
/// which is correct for a provider that genuinely serves its API from another origin.
fn session(account: &Account, client: &reqwest::blocking::Client) -> Res<Value> {
    let url = format!("{}{WELL_KNOWN}", account.base);
    let response = account.authed(client.get(&url)).send()?;
    let status = response.status();
    let body = response.text()?;
    if !status.is_success() {
        return Err(format!("session {status}: {body}").into());
    }
    let mut doc: Value = serde_json::from_str(&body)?;
    if !account.trust_advertised {
        for field in ["apiUrl", "downloadUrl", "uploadUrl", "eventSourceUrl"] {
            if let Some(rebased) = doc[field].as_str().map(|u| rebase(u, &account.base)) {
                doc[field] = json!(rebased);
            }
        }
    }
    Ok(doc)
}

/// Replaces a URL's scheme and authority with `base`'s, keeping its path — braces and all,
/// since `downloadUrl` is a template rather than a URL and parsing it would encode them.
fn rebase(url: &str, base: &str) -> String {
    let Some(rest) = url.split_once("://").map(|(_, rest)| rest) else {
        return url.to_owned();
    };
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    format!("{base}{path}")
}

fn emit(value: &Value, outfile: Option<&str>) -> Res<()> {
    let text = serde_json::to_string_pretty(value)?;
    match outfile {
        Some(path) => {
            std::fs::write(path, &text)?;
            println!("wrote {path} ({} bytes)", text.len());
        }
        None => println!("{text}"),
    }
    Ok(())
}

fn cmd_session(outfile: Option<&str>) -> Res<()> {
    let account = Account::resolve(&[])?;
    let doc = session(&account, &account.client()?)?;
    if outfile.is_none() {
        // The two numbers a caller almost always wants, before the wall of JSON.
        let core = &doc["capabilities"]["urn:ietf:params:jmap:core"];
        println!(
            "// maxConcurrentRequests: {}   maxObjectsInGet: {}",
            core["maxConcurrentRequests"], core["maxObjectsInGet"],
        );
    }
    emit(&doc, outfile)
}

// ------------------------------------------------------------------------------------------
// Method calls
// ------------------------------------------------------------------------------------------

/// Reads a JSON argument given inline, as `@file`, or as `-` for stdin.
fn read_json(spec: &str) -> Res<Value> {
    if spec == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        return Ok(serde_json::from_str(&buf)?);
    }
    if let Some(path) = spec.strip_prefix('@') {
        return Ok(serde_json::from_slice(&std::fs::read(path)?)?);
    }
    Ok(serde_json::from_str(spec)?)
}

/// Every capability URN the session advertises — sent as `using`, so a call is never
/// refused for a capability the server has and this tool did not name.
fn using(session: &Value) -> Vec<String> {
    session["capabilities"]
        .as_object()
        .map(|caps| caps.keys().cloned().collect())
        .unwrap_or_default()
}

/// Posts one method call and returns the whole response envelope.
fn call(
    account: &Account,
    client: &reqwest::blocking::Client,
    session: &Value,
    method: &str,
    args: Value,
) -> Res<Value> {
    let api_url = session["apiUrl"]
        .as_str()
        .ok_or("session advertised no apiUrl")?;
    let request = json!({
        "using": using(session),
        "methodCalls": [[method, args, "0"]],
    });
    let response = account
        .authed(client.post(api_url))
        .json(&request)
        .send()?;
    let status = response.status();
    let body = response.text()?;
    if !status.is_success() {
        return Err(format!("{method} {status}: {body}").into());
    }
    Ok(serde_json::from_str(&body)?)
}

fn cmd_call(args: &[String]) -> Res<()> {
    let method = args.first().ok_or("usage: call <Method> <args-json>")?;
    let payload = read_json(args.get(1).map_or("{}", String::as_str))?;
    let account = Account::resolve(args)?;
    let client = account.client()?;
    let session = session(&account, &client)?;
    let response = call(&account, &client, &session, method, payload)?;
    emit(&response, args.get(2).map(String::as_str))
}

fn cmd_get(args: &[String]) -> Res<()> {
    let target = args.first().ok_or("usage: get <url-or-path>")?;
    let account = Account::resolve(args)?;
    let url = if target.starts_with("http") {
        target.clone()
    } else {
        format!("{}{target}", account.base)
    };
    let response = account.authed(account.client()?.get(&url)).send()?;
    let status = response.status();
    let bytes = response.bytes()?;
    println!("// HTTP {status}  {} bytes", bytes.len());
    match args.get(1) {
        Some(path) => {
            std::fs::write(path, &bytes)?;
            println!("wrote {path}");
        }
        None => println!("{}", String::from_utf8_lossy(&bytes)),
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// Bench
// ------------------------------------------------------------------------------------------

/// Fills a `downloadUrl` template (RFC 8620 §2).
fn download_url(template: &str, account_id: &str, blob_id: &str) -> String {
    template
        .replace("{accountId}", account_id)
        .replace("{blobId}", blob_id)
        .replace("{type}", "application%2Foctet-stream")
        .replace("{name}", "message")
}

/// The first `limit` messages' blob ids, newest first.
fn blob_ids(
    account: &Account,
    client: &reqwest::blocking::Client,
    session: &Value,
    mail_account: &str,
    limit: usize,
) -> Res<Vec<String>> {
    let query = call(
        account,
        client,
        session,
        "Email/query",
        json!({
            "accountId": mail_account,
            "sort": [{ "property": "receivedAt", "isAscending": false }],
            "limit": limit,
        }),
    )?;
    let ids = query["methodResponses"][0][1]["ids"].clone();
    let got = call(
        account,
        client,
        session,
        "Email/get",
        json!({ "accountId": mail_account, "ids": ids, "properties": ["id", "blobId"] }),
    )?;
    Ok(got["methodResponses"][0][1]["list"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|m| m["blobId"].as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

/// Downloads every blob with `width` threads pulling from one queue, returning the elapsed
/// seconds, the bytes moved, and how many did not answer `200` (with the first such status).
fn sweep(
    account: &Account,
    client: &reqwest::blocking::Client,
    urls: &[String],
    width: usize,
) -> (f64, usize, usize, Option<u16>) {
    let next = AtomicUsize::new(0);
    let bytes = AtomicUsize::new(0);
    let bad = AtomicUsize::new(0);
    let first_bad = std::sync::Mutex::new(None);
    let started = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..width {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::SeqCst);
                let Some(url) = urls.get(index) else { return };
                match account.authed(client.get(url)).send() {
                    Ok(response) => {
                        let status = response.status().as_u16();
                        let len = response.bytes().map(|b| b.len()).unwrap_or(0);
                        if status == 200 {
                            bytes.fetch_add(len, Ordering::Relaxed);
                        } else {
                            bad.fetch_add(1, Ordering::Relaxed);
                            let mut slot = first_bad.lock().unwrap();
                            slot.get_or_insert(status);
                        }
                    }
                    Err(_) => {
                        bad.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    let elapsed = started.elapsed().as_secs_f64();
    let status = *first_bad.lock().unwrap();
    (
        elapsed,
        bytes.load(Ordering::Relaxed),
        bad.load(Ordering::Relaxed),
        status,
    )
}

fn cmd_bench(args: &[String]) -> Res<()> {
    let messages: usize = flag(args, "--messages").map_or(Ok(50), |v| v.parse())?;
    let widths: Vec<usize> = flag(args, "--widths")
        .unwrap_or_else(|| "1,2,4,8,16,1".to_owned())
        .split(',')
        .map(str::parse)
        .collect::<Result<_, _>>()?;

    let account = Account::resolve(args)?;
    let client = account.client()?;
    let session = session(&account, &client)?;
    let mail_account = session["primaryAccounts"]["urn:ietf:params:jmap:mail"]
        .as_str()
        .ok_or("session names no primary mail account")?
        .to_owned();
    let template = session["downloadUrl"]
        .as_str()
        .ok_or("session advertised no downloadUrl")?
        .to_owned();
    let granted = session["capabilities"]["urn:ietf:params:jmap:core"]["maxConcurrentRequests"]
        .as_u64()
        .unwrap_or(0);

    let blobs = blob_ids(&account, &client, &session, &mail_account, messages)?;
    let urls: Vec<String> = blobs
        .iter()
        .map(|blob| download_url(&template, &mail_account, blob))
        .collect();
    println!(
        "{} message bodies; the session grants {granted} concurrent request(s)",
        urls.len(),
    );
    // The last width repeats the first on purpose: if the two disagree, the sweep measured a
    // warming cache rather than concurrency.
    for width in widths {
        let (seconds, bytes, bad, status) = sweep(&account, &client, &urls, width);
        let count = u32::try_from(urls.len()).unwrap_or(u32::MAX);
        println!(
            "  {width:>2} in flight: {seconds:6.2}s  {:6.1} bodies/s  {:7.2} MB{}",
            f64::from(count) / seconds,
            bytes as f64 / 1_048_576.0,
            match (bad, status) {
                (0, _) => String::new(),
                (n, Some(s)) => format!("   <-- {n} failed, first HTTP {s}"),
                (n, None) => format!("   <-- {n} failed"),
            },
        );
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    Ok(())
}
