# yahoo-oauth

A tiny standalone dev tool to obtain **Yahoo** (or AOL) OAuth 2.0 tokens for a test
mailbox, so the gated IMAP/SMTP OAuth live tests have a real access token to present
(`crates/provider-imap/tests/live_imap_oauth.rs`, issue #191). It mirrors
`tools/google-oauth` and `tools/graph-oauth`.

It is **not** part of the engine workspace (its own `[workspace]` table detaches it), so
it never affects the engine's fmt/clippy/coverage gates. The engine stays OAuth-agnostic
— hosts own account onboarding (`north-star.md`); this only exists to drive the
interactive flow locally.

## ⚠️ One-time setup: developer access, then an app

Unlike Google and Microsoft, **Yahoo does not self-serve the mail scope.**

1. **Request developer access** at
   <https://senders.yahooinc.com/developer/developer-access/>. Yahoo reviews and
   approves third parties before granting `mail-r`/`mail-w`; the console will not offer
   those scopes until it has.
2. **Create an app** at <https://developer.yahoo.com/apps/create/>. Note the **Client ID
   (Consumer Key)** and **Client Secret (Consumer Secret)** — Yahoo apps are
   *confidential* clients, so unlike a Google "Desktop app" the secret is required.
3. **Redirect URI.** Yahoo does not register loopback (`http://127.0.0.1`) URIs, so this
   tool uses the documented out-of-band flow by default (`redirect_uri=oob`): Yahoo shows
   the authorization code and you paste it at the prompt. If your app registered an
   `https` callback instead, pass `--redirect-uri <URI>` and paste the `code` parameter
   out of the address bar.

Until step 1 is approved, `login` fails at the authorization step with an invalid-scope
error. That is a Yahoo account state, not a fault in this tool or in `provider-imap`.

## Flow

Authorization Code, no PKCE (Yahoo documents no `code_challenge` support), with the
client authenticated at the token endpoint by HTTP Basic — the form Yahoo's guide
prescribes.

## Commands

```sh
# 1. Sign in (opens the browser; paste back the code it shows).
cargo run --manifest-path tools/yahoo-oauth/Cargo.toml -- \
  login --client-id <CONSUMER_KEY> --client-secret <CONSUMER_SECRET>

# 2. Refresh the access token any time.
cargo run --manifest-path tools/yahoo-oauth/Cargo.toml -- refresh

# 3. Check what the token can actually reach (start here when IMAP says NO).
cargo run --manifest-path tools/yahoo-oauth/Cargo.toml -- check

# 4. Print a fresh access token, for the gated live tests.
export IMAP_OAUTH_HOST=imap.mail.yahoo.com
export IMAP_OAUTH_USER=you@yahoo.example
export IMAP_OAUTH_TOKEN="$(cargo run -q --manifest-path tools/yahoo-oauth/Cargo.toml -- token)"
cargo test -p provider-imap --test live_imap_oauth -- --nocapture
```

`--client-id`/`--client-secret`/`--scopes`/`--redirect-uri` also read from
`YAHOO_CLIENT_ID` / `YAHOO_CLIENT_SECRET` / `YAHOO_SCOPES` / `YAHOO_REDIRECT_URI`.
Tokens are stored owner-only under `.local/tokens.json` (gitignored) — the refresh token
and client secret are long-lived credentials for a real mailbox, so don't commit them.

## What Yahoo's IMAP/SMTP expects

| | Host | Port | SASL mechanisms advertised |
|---|---|---|---|
| IMAP | `imap.mail.yahoo.com` | 993 (implicit TLS) | `XOAUTH2` **and** `OAUTHBEARER` |
| SMTP | `smtp.mail.yahoo.com` | 465 (implicit TLS) | `XOAUTH2` and `OAUTHBEARER` |

The IMAP row is what the server's pre-auth `CAPABILITY` actually returns, captured from
`imap.mail.yahoo.com:993`. Yahoo's own documentation presents `OAUTHBEARER` as the
mechanism for its IMAP and does not mention `XOAUTH2` there — the server offers both.
Yahoo's IMAP also advertises **no** `CONDSTORE`/`QRESYNC` (it has a proprietary
`XYMHIGHESTMODSEQ`), so a Yahoo mailbox syncs with the new-arrivals delta plus periodic
snapshot rather than the incremental path.

`provider-imap` negotiates the mechanism from the server's advertised `AUTH=` set, so
nothing above the adapter configures this — see
`docs/agent-guidance/imap-smtp.md` → "Authentication".
