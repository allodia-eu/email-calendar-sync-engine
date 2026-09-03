//! `yahoo-oauth` — a tiny local helper to obtain Yahoo (and AOL) OAuth 2.0 tokens for a
//! test mailbox, so the gated IMAP/SMTP OAuth live tests have a real access token to
//! present (`crates/provider-imap/tests/live_imap_oauth.rs`, issue #191).
//!
//! It is deliberately a standalone dev tool, not part of the engine: the engine stays
//! OAuth-agnostic — hosts own account onboarding (`north-star.md`). Nothing
//! product-specific is hardcoded; the client id/secret, endpoints and scopes are config.
//! It mirrors `tools/google-oauth` and `tools/graph-oauth`.
//!
//! ## Two ways it differs from its siblings, both forced by Yahoo
//!
//! **No loopback redirect.** Google and Microsoft accept an `http://127.0.0.1` redirect
//! (RFC 8252) and the sibling tools catch it with a one-shot listener. Yahoo does not
//! register loopback URIs, so the flow here is the documented out-of-band one: the code
//! is shown in the browser (or in the address bar of your registered `https` redirect)
//! and pasted back at the prompt. That is why this tool has no HTTP server.
//!
//! **No PKCE.** Yahoo issues a client *secret* and documents no `code_challenge`
//! support, so the exchange authenticates the client with HTTP Basic instead
//! (RFC 6749 §2.3.1) — the form Yahoo's guide prescribes.
//!
//! ## Before it can work: developer access to the mail scope
//!
//! Yahoo does **not** self-serve mail scopes. Creating an app at
//! <https://developer.yahoo.com/apps/create/> gets you a client id and secret, but
//! `mail-r`/`mail-w` are granted only after Yahoo approves a developer-access request
//! (<https://senders.yahooinc.com/developer/developer-access/>). Until then the
//! authorization step fails with an invalid-scope error — which is a Yahoo account
//! state, not a bug in this tool or in `provider-imap`.
//!
//! ## Commands
//!
//! - `login` — print (and open) the sign-in URL, take the pasted code, exchange it, and
//!   save `access_token` + `refresh_token`.
//! - `refresh` — mint a fresh access token from the saved refresh token.
//! - `token` — print a valid access token to stdout, refreshing if near expiry, so a live
//!   test can read it: `IMAP_OAUTH_TOKEN="$(… token)"`.
//! - `check` — verify the saved token against Yahoo's OpenID `userinfo` endpoint and print
//!   the scopes it actually carries. The first thing to run when IMAP answers
//!   `AUTHENTICATE` with `NO`: a token without the mail scope authenticates nowhere.
//!
//! Run from the repo root, e.g.:
//!   cargo run --manifest-path tools/yahoo-oauth/Cargo.toml -- \
//!     login --client-id <CONSUMER_KEY> --client-secret <CONSUMER_SECRET>

use std::error::Error;
use std::io::{BufRead, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

type Res<T> = Result<T, Box<dyn Error>>;

/// Yahoo's OAuth 2.0 authorization endpoint.
const AUTH_ENDPOINT: &str = "https://api.login.yahoo.com/oauth2/request_auth";
/// Yahoo's OAuth 2.0 token endpoint (issue and refresh).
const TOKEN_ENDPOINT: &str = "https://api.login.yahoo.com/oauth2/get_token";
/// The OpenID Connect user-info endpoint `check` reads the granted scopes from.
const USERINFO_ENDPOINT: &str = "https://api.login.yahoo.com/openid/v1/userinfo";
/// Default scopes. `mail-w` is read **and** write, which is what IMAP needs: a client
/// sets `\Seen`, `APPEND`s a sent copy, and expunges. `openid` is what makes `check`
/// able to answer "which scopes did this token actually get".
const DEFAULT_SCOPES: &str = "mail-w openid";
/// The documented out-of-band redirect: the authorization code is displayed rather than
/// redirected to a URI (Yahoo's guide, "if your application cannot use a browser").
const OOB_REDIRECT: &str = "oob";

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("login") => cmd_login(&args[1..]),
        Some("refresh") => {
            let _ = cmd_refresh()?;
            println!("refreshed; saved to {}", tokens_path());
            Ok(())
        }
        Some("token") => {
            // Print only the access token, so a live test can capture it directly.
            println!("{}", fresh_access_token()?);
            Ok(())
        }
        Some("check") => cmd_check(),
        _ => {
            eprintln!(
                "usage:\n  yahoo-oauth login --client-id <KEY> --client-secret <SECRET> [--scopes \"<s1 s2 ...>\"] [--redirect-uri <URI>]\n  yahoo-oauth refresh\n  yahoo-oauth token\n  yahoo-oauth check"
            );
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

fn cmd_login(args: &[String]) -> Res<()> {
    let client_id = flag(args, "--client-id")
        .or_else(|| std::env::var("YAHOO_CLIENT_ID").ok())
        .ok_or("missing --client-id (or YAHOO_CLIENT_ID)")?;
    // Yahoo apps are confidential clients: the secret is required, not optional as it
    // is for a Google "Desktop app".
    let client_secret = flag(args, "--client-secret")
        .or_else(|| std::env::var("YAHOO_CLIENT_SECRET").ok())
        .ok_or("missing --client-secret (or YAHOO_CLIENT_SECRET)")?;
    let scopes = flag(args, "--scopes")
        .or_else(|| std::env::var("YAHOO_SCOPES").ok())
        .unwrap_or_else(|| DEFAULT_SCOPES.to_owned());
    // `oob` unless the app registered a real redirect; either way the code is pasted
    // back here, because neither form can reach a listener on this machine.
    let redirect_uri = flag(args, "--redirect-uri")
        .or_else(|| std::env::var("YAHOO_REDIRECT_URI").ok())
        .unwrap_or_else(|| OOB_REDIRECT.to_owned());

    let state = hex(&rand_bytes(16)?);
    let nonce = hex(&rand_bytes(16)?);

    let mut auth_url = reqwest::Url::parse(AUTH_ENDPOINT)?;
    auth_url
        .query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", &scopes)
        .append_pair("state", &state)
        // Optional for a plain code flow, but Yahoo's OpenID guide expects it whenever
        // `openid` is among the scopes, and sending it always costs nothing.
        .append_pair("nonce", &nonce);

    println!("Open this URL in your browser and sign in:\n\n{auth_url}\n");
    let _ = open_browser(auth_url.as_str());
    if redirect_uri == OOB_REDIRECT {
        println!("Yahoo will display an authorization code once you approve.");
    } else {
        println!(
            "Yahoo will redirect to {redirect_uri}; copy the `code` parameter out of the \
             address bar (and check `state` reads {state})."
        );
    }
    let code = prompt("Paste the authorization code: ")?;
    if code.is_empty() {
        return Err("no authorization code entered".into());
    }

    let resp = post_token(
        &client_id,
        &client_secret,
        &[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
        ],
    )?;

    let tokens = build_tokens(&resp, &client_id, &client_secret, &scopes, &redirect_uri)?;
    save_tokens(&tokens)?;
    println!(
        "\nSuccess. Tokens saved to {}\nScopes granted: {}",
        tokens_path(),
        resp.get("scope").and_then(Value::as_str).unwrap_or("(none)")
    );
    println!("Use it with:  export IMAP_OAUTH_TOKEN=\"$(… -- token)\"");
    Ok(())
}

// ---------------------------------------------------------------------------
// refresh
// ---------------------------------------------------------------------------

/// Refreshes and persists the access token, returning the live access token.
fn cmd_refresh() -> Res<String> {
    let saved = load_tokens()?;
    let client_id = str_field(&saved, "client_id")?;
    let client_secret = str_field(&saved, "client_secret")?;
    let scopes = str_field(&saved, "scope")?;
    let redirect_uri = str_field(&saved, "redirect_uri")?;
    let refresh_token = str_field(&saved, "refresh_token")?;

    let resp = post_token(
        &client_id,
        &client_secret,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
        ],
    )?;
    let tokens = build_tokens(&resp, &client_id, &client_secret, &scopes, &redirect_uri)?;
    save_tokens(&tokens)?;
    str_field(&tokens, "access_token")
}

/// Returns a valid access token, refreshing if the saved one is near expiry.
fn fresh_access_token() -> Res<String> {
    let saved = load_tokens()?;
    let obtained = saved.get("obtained_at").and_then(Value::as_u64).unwrap_or(0);
    let expires_in = saved.get("expires_in").and_then(Value::as_u64).unwrap_or(0);
    // Refresh with a 5-minute safety margin. Yahoo's access tokens last an hour, so a
    // long live-test run would otherwise expire mid-suite.
    if now_epoch() + 300 >= obtained + expires_in {
        cmd_refresh()
    } else {
        str_field(&saved, "access_token")
    }
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// Verifies the saved token and prints what it can reach.
///
/// Worth its own command because Yahoo's failure mode is quiet: a token minted without
/// approved mail scope looks perfectly valid and still cannot open IMAP, so the useful
/// question is not "is the token good" but "which scopes did it get".
fn cmd_check() -> Res<()> {
    let token = fresh_access_token()?;
    let saved = load_tokens()?;
    println!(
        "requested scopes: {}\ngranted scopes:   {}",
        str_field(&saved, "requested_scope").unwrap_or_else(|_| "(unknown)".to_owned()),
        str_field(&saved, "scope").unwrap_or_else(|_| "(unknown)".to_owned())
    );
    let resp = http_client()?
        .get(USERINFO_ENDPOINT)
        .bearer_auth(&token)
        .send()?;
    let status = resp.status();
    let text = resp.text()?;
    let body = serde_json::from_str::<Value>(&text)
        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.clone()))
        .unwrap_or(text);
    println!("HTTP {status}\n{body}");
    if !status.is_success() {
        return Err("the saved token was refused by Yahoo".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

fn http_client() -> Res<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder().build()?)
}

/// Posts to the token endpoint, authenticating the client with HTTP Basic — the form
/// Yahoo's guide prescribes (`Authorization: Basic base64(client_id:client_secret)`).
fn post_token(client_id: &str, client_secret: &str, form: &[(&str, &str)]) -> Res<Value> {
    let resp = http_client()?
        .post(TOKEN_ENDPOINT)
        .basic_auth(client_id, Some(client_secret))
        .form(form)
        .send()?;
    let status = resp.status();
    let body: Value = resp.json()?;
    if !status.is_success() {
        let desc = body
            .get("error_description")
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("(no description)");
        return Err(format!("token endpoint returned {status}: {desc}").into());
    }
    Ok(body)
}

fn open_browser(url: &str) -> Res<()> {
    // Best-effort; the URL is also printed so it can be opened by hand.
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener).arg(url).spawn()?;
    Ok(())
}

/// Prints `label` and reads one line from stdin.
fn prompt(label: &str) -> Res<String> {
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

// ---------------------------------------------------------------------------
// Token persistence
// ---------------------------------------------------------------------------

/// Builds the on-disk token record, preserving the configuration so `refresh`/`token`
/// need nothing re-passed.
fn build_tokens(
    resp: &Value,
    client_id: &str,
    client_secret: &str,
    scopes: &str,
    redirect_uri: &str,
) -> Res<Value> {
    let access = resp
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("token response had no access_token")?;
    // Yahoo returns a fresh refresh token on every exchange, but fall back to the saved
    // one rather than losing the grant if a response ever omits it.
    let refresh = resp
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            load_tokens()
                .ok()
                .and_then(|t| str_field(&t, "refresh_token").ok())
        })
        .ok_or("no refresh_token in response or on disk")?;
    Ok(json!({
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": resp.get("expires_in").and_then(Value::as_u64).unwrap_or(3600),
        "obtained_at": now_epoch(),
        // What the token actually carries, and what was asked for: `check` prints both,
        // because Yahoo silently grants a subset when the mail scope is not approved.
        "scope": resp.get("scope").and_then(Value::as_str).unwrap_or(scopes),
        "requested_scope": scopes,
        "client_id": client_id,
        "client_secret": client_secret,
        "redirect_uri": redirect_uri,
    }))
}

fn tokens_path() -> String {
    std::env::var("YAHOO_TOKENS")
        .unwrap_or_else(|_| format!("{}/.local/tokens.json", env!("CARGO_MANIFEST_DIR")))
}

fn load_tokens() -> Res<Value> {
    let path = tokens_path();
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("no tokens at {path} ({e}); run `login` first"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn save_tokens(tokens: &Value) -> Res<()> {
    let path = tokens_path();
    if let Some(dir) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(tokens)?)?;
    // The refresh token and the client secret are long-lived credentials for a real
    // mailbox; keep the file owner-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// small utilities
// ---------------------------------------------------------------------------

/// Reads `--name value` out of `args`.
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn str_field(v: &Value, key: &str) -> Res<String> {
    Ok(v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("tokens file missing `{key}`"))?
        .to_owned())
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Cryptographically-random bytes from the OS, no extra crate needed.
fn rand_bytes(n: usize) -> Res<Vec<u8>> {
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Hex, for the `state`/`nonce` values — URL-safe with no encoding to think about.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
