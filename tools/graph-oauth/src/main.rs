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
//! - `login`   — open the sign-in URL, catch the loopback redirect, exchange the code, and save
//!   `access_token` + `refresh_token` to the tokens file.
//! - `refresh` — mint a fresh access token from the saved refresh token.
//! - `token`   — print a valid access token (refreshing if needed), so a live test can read it
//!   without parsing the tokens file.
//! - `get <graph-url> [outfile]` — refresh if needed, GET the Graph URL with the bearer token, and
//!   pretty-print (and optionally save) the JSON. Use this to capture real responses as fixtures.
//!
//! ## Profiles
//!
//! `--profile <name>` (or `GRAPH_PROFILE`) selects an independent tokens file, so
//! several accounts stay signed in **side by side** — a personal Microsoft account
//! and a work/school one differ in what they can do (only a work/school tenant has
//! shared mailboxes and can consent to the `*.Shared` scopes), and both are needed
//! to prove the adapter against each. Without the flag the file is the original
//! `tokens.json`, so an existing sign-in keeps working untouched.
//!
//! Run from the repo root, e.g.:
//!   cargo run --manifest-path tools/graph-oauth/Cargo.toml -- login --client-id <APP_ID>

mod oauth;
mod tokens;

use std::error::Error;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    oauth::{http_client, open_browser, post_token, rand_bytes, wait_for_redirect},
    tokens::{build_tokens, load_tokens, now_epoch, save_tokens, str_field, tokens_path},
};

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
///
/// `MailboxSettings.ReadWrite` has **no `.Shared` variant**, and the live probe settled
/// what that means: with the scope granted, `/me/mailboxSettings/…` answers `200`, while
/// **every** `/users/{other}/mailboxSettings` route answers `403 ErrorAccessDenied` —
/// whole object and each sub-path — even with Full Access to that mailbox. So it earns
/// its place here for the signed-in mailbox only (the foundation for automatic replies),
/// and contributes nothing to shared mailboxes: there is no delegated way to read
/// another mailbox's `userPurpose`, hence no mailbox *kind* in the engine's model
/// (`graph.md`).
const DEFAULT_SCOPES: &str = "offline_access openid profile User.Read \
    Mail.ReadWrite Mail.ReadWrite.Shared Mail.Send Mail.Send.Shared \
    Calendars.ReadWrite Calendars.ReadWrite.Shared \
    Contacts.ReadWrite Contacts.ReadWrite.Shared MailboxSettings.ReadWrite \
    OrgContact.Read.All User.ReadBasic.All ProfilePhoto.Read.All";
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
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Taken (not just read) so it never lands in a command's positional arguments.
    let profile = take_flag(&mut args, "--profile").or_else(|| std::env::var("GRAPH_PROFILE").ok());
    let profile = profile.as_deref();
    match args.first().map(String::as_str) {
        Some("login") => cmd_login(&args[1..], profile),
        Some("refresh") => {
            let _ = cmd_refresh(profile)?;
            println!("refreshed; saved to {}", tokens_path(profile));
            Ok(())
        }
        Some("token") => {
            println!("{}", fresh_access_token(profile)?);
            Ok(())
        }
        Some("get") => cmd_get(&args[1..], profile),
        Some("req") => cmd_req(&args[1..], profile),
        _ => {
            eprintln!(
                "usage (every command accepts --profile <name> for a side-by-side account):\n  graph-oauth login --client-id <APP_ID> [--authority <URL>] [--port <N>] [--scopes \"<s1 s2 ...>\"]\n  graph-oauth refresh\n  graph-oauth token\n  graph-oauth get <graph-url-or-path> [outfile.json]\n  graph-oauth req <METHOD> <graph-url-or-path> [body-json|@file|-] [outfile.json]"
            );
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

fn cmd_login(args: &[String], profile: Option<&str>) -> Res<()> {
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

    let tokens = build_tokens(&resp, &client_id, &authority, &scopes, profile)?;
    save_tokens(&tokens, profile)?;
    println!(
        "\nSuccess. Tokens saved to {}\nScopes granted: {}",
        tokens_path(profile),
        resp.get("scope")
            .and_then(Value::as_str)
            .unwrap_or("(none)")
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// refresh
// ---------------------------------------------------------------------------

/// Refreshes and persists the access token, returning the live access token.
fn cmd_refresh(profile: Option<&str>) -> Res<String> {
    let saved = load_tokens(profile)?;
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
    let tokens = build_tokens(&resp, &client_id, &authority, &scopes, profile)?;
    save_tokens(&tokens, profile)?;
    Ok(str_field(&tokens, "access_token")?)
}

/// Returns a valid access token, refreshing if the saved one is near expiry.
fn fresh_access_token(profile: Option<&str>) -> Res<String> {
    let saved = load_tokens(profile)?;
    let obtained = saved
        .get("obtained_at")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let expires_in = saved.get("expires_in").and_then(Value::as_u64).unwrap_or(0);
    // Refresh with a 5-minute safety margin.
    if now_epoch() + 300 >= obtained + expires_in {
        cmd_refresh(profile)
    } else {
        str_field(&saved, "access_token")
    }
}

// ---------------------------------------------------------------------------
// get (fixture capture)
// ---------------------------------------------------------------------------

fn cmd_get(args: &[String], profile: Option<&str>) -> Res<()> {
    let url = args
        .first()
        .ok_or("usage: get <graph-url-or-path> [outfile]")?;
    cmd_req(
        &[
            "GET".to_owned(),
            url.clone(),
            String::new(),
            args.get(1).cloned().unwrap_or_default(),
        ],
        profile,
    )
}

/// Generic authenticated Graph request — `req <METHOD> <url> [body] [outfile]`.
/// Drives capture of changed/removed delta and (later) write-slice E2E. `body` may
/// be inline JSON, `@path` to a JSON file, or empty/`-` for none.
fn cmd_req(args: &[String], profile: Option<&str>) -> Res<()> {
    let method = args
        .first()
        .ok_or("usage: req <METHOD> <url> [body] [outfile]")?
        .to_uppercase();
    let url = args
        .get(1)
        .ok_or("usage: req <METHOD> <url> [body] [outfile]")?;
    let full = graph_url(url);
    let token = fresh_access_token(profile)?;
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
// small utilities
// ---------------------------------------------------------------------------

/// Reads `--name value` out of `args`.
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Reads `--name value` out of `args` **and removes both**, so a global flag can
/// appear anywhere without being mistaken for a command's positional argument
/// (`get /me --profile work` must not treat `--profile` as the outfile).
fn take_flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    // Removed even when it carries no value, so a trailing `get /me --profile` does not
    // leave the flag behind to be read as the command's outfile.
    args.remove(i);
    (i < args.len()).then(|| args.remove(i))
}

