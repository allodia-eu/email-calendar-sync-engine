//! The OAuth wire: the token endpoint, the loopback redirect catcher, PKCE
//! randomness, and opening a browser.
//!
//! Split out of `main.rs` (which keeps the command dispatch) so each file stays under
//! the repo's 500-line limit.

use std::{
    io::{Read, Write},
    net::TcpListener,
};

use serde_json::Value;

use crate::Res;

pub(crate) fn http_client() -> Res<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder().build()?)
}

pub(crate) fn post_token(authority: &str, form: &[(&str, &str)]) -> Res<Value> {
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
pub(crate) fn wait_for_redirect(port: u16) -> Res<(String, Option<String>)> {
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

pub(crate) fn open_browser(url: &str) -> Res<()> {
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

/// Cryptographically-random bytes from the OS, no extra crate needed — the PKCE
/// verifier and the CSRF `state` are both built from these.
pub(crate) fn rand_bytes(n: usize) -> Res<Vec<u8>> {
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf)?;
    Ok(buf)
}
