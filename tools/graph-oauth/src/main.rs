//! `graph-oauth` — a tiny local helper to obtain Microsoft Graph OAuth tokens for
//! a *throwaway test account* and to capture real Graph JSON responses as offline
//! test fixtures for the `provider-graph` adapter.
//!
//! It is deliberately a standalone dev tool, not part of the engine: the engine
//! stays OAuth-agnostic (hosts own account onboarding — `north-star.md`). Nothing
//! product-specific is hardcoded; the client id / authority / scopes are config.
//!
//! ## Flow
//!
//! Authorization Code + PKCE (S256) for a **public client** (no client secret),
//! with an `http://localhost` loopback redirect — the pattern the Microsoft
//! identity platform documents for native/desktop apps (RFC 8252).
//!
//! ## Commands
//!
//! - `login`   — open the sign-in URL, catch the loopback redirect, exchange the
//!               code, and save `access_token` + `refresh_token` to the tokens file.
//! - `refresh` — mint a fresh access token from the saved refresh token.
//! - `get <graph-url> [outfile]` — refresh if needed, GET the Graph URL with the
//!               bearer token, and pretty-print (and optionally save) the JSON. Use
//!               this to capture real responses as fixtures.
//!
//! Run from the repo root, e.g.:
//!   cargo run --manifest-path tools/graph-oauth/Cargo.toml -- login --client-id <APP_ID>

use std::error::Error;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

type Res<T> = Result<T, Box<dyn Error>>;

/// Default authority: the multi-tenant + personal-accounts endpoint, so a personal
/// throwaway Microsoft account works.
const DEFAULT_AUTHORITY: &str = "https://login.microsoftonline.com/common";
/// Default delegated scopes — the broad read+write+send set, so a single consent
/// covers the later submission/calendar-write slices too. `offline_access` is what
/// yields a refresh token.
///
/// The `*.Shared` variants grant delegate access to *other users'* mailboxes and
/// calendars (shared mailboxes). They are an Exchange Online / work-school feature:
/// a **personal** Microsoft account usually cannot consent to them, so if `login`
/// fails with an AADSTS scope/consent error, re-run with `--scopes` limited to the
/// non-shared set.
const DEFAULT_SCOPES: &str = "offline_access openid profile User.Read \
    Mail.ReadWrite Mail.ReadWrite.Shared Mail.Send Mail.Send.Shared \
    Calendars.ReadWrite Calendars.ReadWrite.Shared";
/// Loopback port the redirect server listens on. The Microsoft identity platform
/// ignores the port when matching a registered `http://localhost` redirect, so the
/// app only needs `http://localhost` registered (RFC 8252 §7.3).
const DEFAULT_PORT: u16 = 8400;
/// Graph base for the `get` command's relative URLs.
const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

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
        Some("get") => cmd_get(&args[1..]),
        Some("req") => cmd_req(&args[1..]),
        _ => {
            eprintln!(
                "usage:\n  graph-oauth login --client-id <APP_ID> [--authority <URL>] [--port <N>] [--scopes \"<s1 s2 ...>\"]\n  graph-oauth refresh\n  graph-oauth get <graph-url-or-path> [outfile.json]\n  graph-oauth req <METHOD> <graph-url-or-path> [body-json|@file|-] [outfile.json]"
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
        .or_else(|| std::env::var("GRAPH_CLIENT_ID").ok())
        .ok_or("missing --client-id (or GRAPH_CLIENT_ID)")?;
    let authority = flag(args, "--authority")
        .or_else(|| std::env::var("GRAPH_AUTHORITY").ok())
        .unwrap_or_else(|| DEFAULT_AUTHORITY.to_owned());
    let scopes = flag(args, "--scopes")
        .or_else(|| std::env::var("GRAPH_SCOPES").ok())
        .unwrap_or_else(|| DEFAULT_SCOPES.to_owned());
    let port: u16 = flag(args, "--port")
        .map(|p| p.parse())
        .transpose()?
        .unwrap_or(DEFAULT_PORT);
    let redirect_uri = format!("http://localhost:{port}");

    // PKCE: a high-entropy verifier and its S256 challenge.
    let verifier = URL_SAFE_NO_PAD.encode(rand_bytes(32)?);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = URL_SAFE_NO_PAD.encode(rand_bytes(16)?);

    let mut auth_url = reqwest::Url::parse(&format!("{authority}/oauth2/v2.0/authorize"))?;
    auth_url
        .query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_mode", "query")
        .append_pair("scope", &scopes)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");

    println!("Open this URL in your browser and sign in:\n\n{auth_url}\n");
    let _ = open_browser(auth_url.as_str());

    // Catch the loopback redirect and verify the state.
    let (code, returned_state) = wait_for_redirect(port)?;
    if returned_state.as_deref() != Some(state.as_str()) {
        return Err("state mismatch on redirect (possible CSRF)".into());
    }

    let resp = post_token(
        &authority,
        &[
            ("client_id", client_id.as_str()),
            ("scope", scopes.as_str()),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier.as_str()),
        ],
    )?;

    let tokens = build_tokens(&resp, &client_id, &authority, &scopes)?;
    save_tokens(&tokens)?;
    println!(
        "\nSuccess. Tokens saved to {}\nScopes granted: {}",
        tokens_path(),
        resp.get("scope").and_then(Value::as_str).unwrap_or("(none)")
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// refresh
// ---------------------------------------------------------------------------

/// Refreshes and persists the access token, returning the live access token.
fn cmd_refresh() -> Res<String> {
    let saved = load_tokens()?;
    let (client_id, authority, scopes) = (
        str_field(&saved, "client_id")?,
        str_field(&saved, "authority")?,
        str_field(&saved, "scope")?,
    );
    let refresh_token = str_field(&saved, "refresh_token")?;

    let resp = post_token(
        &authority,
        &[
            ("client_id", client_id.as_str()),
            ("scope", scopes.as_str()),
            ("refresh_token", refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ],
    )?;
    let tokens = build_tokens(&resp, &client_id, &authority, &scopes)?;
    save_tokens(&tokens)?;
    Ok(str_field(&tokens, "access_token")?)
}

/// Returns a valid access token, refreshing if the saved one is near expiry.
fn fresh_access_token() -> Res<String> {
    let saved = load_tokens()?;
    let obtained = saved.get("obtained_at").and_then(Value::as_u64).unwrap_or(0);
    let expires_in = saved.get("expires_in").and_then(Value::as_u64).unwrap_or(0);
    // Refresh with a 5-minute safety margin.
    if now_epoch() + 300 >= obtained + expires_in {
        cmd_refresh()
    } else {
        str_field(&saved, "access_token")
    }
}

// ---------------------------------------------------------------------------
// get (fixture capture)
// ---------------------------------------------------------------------------

fn cmd_get(args: &[String]) -> Res<()> {
    let url = args.first().ok_or("usage: get <graph-url-or-path> [outfile]")?;
    cmd_req(&["GET".to_owned(), url.clone(), String::new(), args.get(1).cloned().unwrap_or_default()])
}

/// Generic authenticated Graph request — `req <METHOD> <url> [body] [outfile]`.
/// Drives capture of changed/removed delta and (later) write-slice E2E. `body` may
/// be inline JSON, `@path` to a JSON file, or empty/`-` for none.
fn cmd_req(args: &[String]) -> Res<()> {
    let method = args
        .first()
        .ok_or("usage: req <METHOD> <url> [body] [outfile]")?
        .to_uppercase();
    let url = args.get(1).ok_or("usage: req <METHOD> <url> [body] [outfile]")?;
    let full = graph_url(url);
    let token = fresh_access_token()?;
    let m = reqwest::Method::from_bytes(method.as_bytes())?;
    let mut rb = http_client()?
        .request(m, &full)
        // Immutable ids survive folder moves — the right ProviderKey for Graph mail.
        .header("Prefer", "IdType=\"ImmutableId\"")
        .bearer_auth(&token);
    if let Some(body) = args.get(2).filter(|b| !b.is_empty() && b.as_str() != "-") {
        let json: Value = match body.strip_prefix('@') {
            Some(path) => serde_json::from_slice(&std::fs::read(path)?)?,
            None => serde_json::from_str(body)?,
        };
        rb = rb.json(&json);
    }
    let resp = rb.send()?;
    let status = resp.status();
    let text = resp.text()?;
    // Pretty-print when the body is JSON; pass through otherwise (e.g. 204 empty).
    let out = serde_json::from_str::<Value>(&text)
        .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.clone()))
        .unwrap_or(text);
    match args.get(3).filter(|o| !o.is_empty()) {
        Some(outfile) => {
            std::fs::write(outfile, &out)?;
            println!("HTTP {status} -> wrote {} bytes to {outfile}", out.len());
        }
        None => println!("HTTP {status}\n{out}"),
    }
    Ok(())
}

/// Resolves a relative path against the Graph base; passes absolute URLs through.
fn graph_url(url: &str) -> String {
    if url.starts_with("http") {
        url.to_owned()
    } else if url.starts_with('/') {
        format!("{GRAPH_BASE}{url}")
    } else {
        format!("{GRAPH_BASE}/{url}")
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

fn http_client() -> Res<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder().build()?)
}

fn post_token(authority: &str, form: &[(&str, &str)]) -> Res<Value> {
    let url = format!("{authority}/oauth2/v2.0/token");
    let resp = http_client()?.post(&url).form(form).send()?;
    let status = resp.status();
    let body: Value = resp.json()?;
    if !status.is_success() {
        let desc = body
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or("(no description)");
        return Err(format!("token endpoint returned {status}: {desc}").into());
    }
    Ok(body)
}

/// Blocks on a single loopback connection and returns `(code, state)` from the
/// redirect query. Responds with a tiny page so the browser tab is friendly.
fn wait_for_redirect(port: u16) -> Res<(String, Option<String>)> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("cannot bind 127.0.0.1:{port} for the redirect: {e}"))?;
    println!("Waiting for the sign-in redirect on http://localhost:{port} ...");
    let (mut stream, _) = listener.accept()?;
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf)?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or("malformed redirect request")?;

    // Parse the query off the request target against a dummy base.
    let url = reqwest::Url::parse(&format!("http://localhost{target}"))?;
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_code = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error_code = Some(v.into_owned()),
            "error_description" => error = Some(v.into_owned()),
            _ => {}
        }
    }

    let page = "<html><body><h3>Sign-in complete.</h3>You can close this tab and return to the terminal.</body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    let _ = stream.write_all(response.as_bytes());

    if let Some(code) = code {
        Ok((code, state))
    } else {
        // Prefer the human-readable description, fall back to the OAuth `error`
        // code (e.g. `access_denied`), then to a generic message.
        let reason = error
            .or(error_code)
            .unwrap_or_else(|| "no code returned".into());
        Err(format!("authorization failed: {reason}").into())
    }
}

fn open_browser(url: &str) -> Res<()> {
    // Best-effort; the URL is also printed so the user can open it manually.
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

// ---------------------------------------------------------------------------
// Token persistence
// ---------------------------------------------------------------------------

/// Builds the on-disk token record, preserving config so `refresh`/`get` need no
/// re-passing. The refresh token rotates on each refresh, so it is always re-saved.
fn build_tokens(resp: &Value, client_id: &str, authority: &str, scopes: &str) -> Res<Value> {
    let access = resp
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("token response had no access_token")?;
    // On refresh, Microsoft may omit a new refresh_token; fall back to the old one.
    let refresh = resp
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| load_tokens().ok().and_then(|t| str_field(&t, "refresh_token").ok()))
        .ok_or("no refresh_token in response or on disk")?;
    Ok(json!({
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": resp.get("expires_in").and_then(Value::as_u64).unwrap_or(3600),
        "obtained_at": now_epoch(),
        "scope": resp.get("scope").and_then(Value::as_str).unwrap_or(scopes),
        "client_id": client_id,
        "authority": authority,
    }))
}

fn tokens_path() -> String {
    std::env::var("GRAPH_TOKENS")
        .unwrap_or_else(|_| format!("{}/.local/tokens.json", env!("CARGO_MANIFEST_DIR")))
}

fn load_tokens() -> Res<Value> {
    let path = tokens_path();
    let bytes = std::fs::read(&path).map_err(|e| format!("no tokens at {path} ({e}); run `login` first"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn save_tokens(tokens: &Value) -> Res<()> {
    let path = tokens_path();
    if let Some(dir) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(tokens)?)?;
    // The refresh token is a long-lived credential; keep it owner-only.
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
