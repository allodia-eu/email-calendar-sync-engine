# google-oauth

A tiny standalone dev tool to obtain Google OAuth tokens for a **throwaway test
account** and to capture real Gmail / Google Calendar JSON responses as offline
test fixtures for the `provider-google` adapter. It mirrors `tools/graph-oauth`.

It is **not** part of the engine workspace (its own `[workspace]` table detaches
it), so it never affects the engine's fmt/clippy/coverage gates. The engine stays
OAuth-agnostic — hosts own account onboarding (`north-star.md`); this only exists to
drive the interactive flow locally.

## One-time setup: a Google OAuth client

1. In the [Google Cloud Console](https://console.cloud.google.com/), create (or
   reuse) a project and enable the **Gmail API** and **Google Calendar API**.
2. Configure the OAuth consent screen (External, Testing mode is fine) and add the
   test account as a **Test user**. Add the scopes `https://mail.google.com/` and
   `https://www.googleapis.com/auth/calendar`.
3. Create an **OAuth client ID** of type **Desktop app**. Note the **client ID**
   and **client secret** (for a Desktop app the secret is embedded in the app, not
   confidential).

## Flow

Authorization Code + PKCE (S256) with an `http://127.0.0.1` loopback redirect (RFC
8252). `access_type=offline` + `prompt=consent` mint a refresh token.

## Commands

```sh
# 1. Sign in (opens the browser; catches the loopback redirect).
cargo run --manifest-path tools/google-oauth/Cargo.toml -- \
  login --client-id <CLIENT_ID> --client-secret <CLIENT_SECRET>

# 2. Refresh the access token any time.
cargo run --manifest-path tools/google-oauth/Cargo.toml -- refresh

# 3. Print a fresh access token (for the gated live tests).
GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
  cargo test -p provider-google --test live_provider -- --nocapture

# 4. Capture a real response as a fixture.
cargo run --manifest-path tools/google-oauth/Cargo.toml -- \
  get "/gmail/v1/users/me/labels" crates/provider-google/tests/fixtures/mail/labels.json
```

`--client-id`/`--client-secret` also read from `GOOGLE_CLIENT_ID` /
`GOOGLE_CLIENT_SECRET`. Tokens are stored owner-only under `.local/tokens.json`
(gitignored).
