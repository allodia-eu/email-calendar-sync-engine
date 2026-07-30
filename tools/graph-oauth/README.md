# graph-oauth

A tiny **local dev tool** to get Microsoft Graph OAuth tokens for a *throwaway
test account* and to capture real Graph JSON responses as offline test fixtures
for the `provider-graph` adapter.

It is intentionally **not** part of the engine workspace (it has its own
`[workspace]` table), so it never affects the engine's fmt/clippy/coverage gates.
The engine itself stays OAuth-agnostic — hosts own account onboarding
(`docs/agent-guidance/north-star.md`). Nothing product-specific is hardcoded.

## One-time Entra app registration

In <https://entra.microsoft.com> → **App registrations** → **New registration**:

1. **Supported account types:** *Accounts in any organizational directory and
   personal Microsoft accounts* (the `common` authority).
2. **Authentication → Add a platform → Mobile and desktop applications**, redirect
   URI **`http://localhost`** (a *public client*; no secret — the port is ignored
   for loopback matching).
3. **API permissions → Microsoft Graph → Delegated**: `offline_access`, `openid`,
   `profile`, `User.Read`, `Mail.ReadWrite`, `Mail.Send`, `Calendars.ReadWrite`,
   and (for shared-mailbox delegate access) `Mail.ReadWrite.Shared`,
   `Mail.Send.Shared`, `Calendars.ReadWrite.Shared`. For contacts, add
   `Contacts.ReadWrite` and `Contacts.ReadWrite.Shared`. Add
   `MailboxSettings.ReadWrite` for `userPurpose` (which mailbox kind this is) and
   automatic replies — it has **no `.Shared` variant**. For the independently
   optional tenant sources add `OrgContact.Read.All`, `User.ReadBasic.All`, and
   `ProfilePhoto.Read.All`; those require administrator consent.
4. Copy the **Application (client) ID**.

> The `*.Shared` scopes are an Exchange Online (work/school) feature. A **personal**
> Microsoft account usually cannot consent to them — if `login` errors on consent,
> re-run with the non-shared set:
> `--scopes "offline_access openid profile User.Read Mail.ReadWrite Mail.Send Calendars.ReadWrite Contacts.ReadWrite"`

## Usage

Run from the repo root:

```sh
# 1. Sign in (opens the browser, catches the localhost redirect, saves tokens).
cargo run --manifest-path tools/graph-oauth/Cargo.toml -- login --client-id <APP_ID>

# 2. (any time) mint a fresh access token from the saved refresh token.
cargo run --manifest-path tools/graph-oauth/Cargo.toml -- refresh

# 3. Print a valid access token (refreshes if stale) — what a live test consumes.
cargo run --manifest-path tools/graph-oauth/Cargo.toml -- token

# 4. Capture real Graph responses as fixtures (refreshes automatically).
cargo run --manifest-path tools/graph-oauth/Cargo.toml -- get /me
cargo run --manifest-path tools/graph-oauth/Cargo.toml -- get /me/mailFolders mailfolders.json
cargo run --manifest-path tools/graph-oauth/Cargo.toml -- get /me/contacts contacts.json
cargo run --manifest-path tools/graph-oauth/Cargo.toml -- get /contacts org-contacts.json
```

Tokens are written to `tools/graph-oauth/.local/tokens.json` (gitignored). The
refresh token is sensitive even for a throwaway account — don't commit it.

Override defaults with `--authority`, `--scopes`, `--port`, or the env vars
`GRAPH_CLIENT_ID`, `GRAPH_AUTHORITY`, `GRAPH_SCOPES`, `GRAPH_TOKENS`.

## Profiles — several accounts signed in at once

A personal Microsoft account and a work/school one are **not interchangeable**:
only a work/school tenant has shared mailboxes and can consent to the `*.Shared`
scopes, while the personal account is the cheap throwaway for everything else.
Proving the adapter needs both, so `--profile <name>` (or `GRAPH_PROFILE`) gives
each its own tokens file and they stay signed in side by side:

```sh
# The unnamed profile keeps `.local/tokens.json` — an existing sign-in is untouched.
cargo run --manifest-path tools/graph-oauth/Cargo.toml -- login --client-id <APP_ID>

# A named profile writes `.local/tokens-work.json`.
cargo run --manifest-path tools/graph-oauth/Cargo.toml -- login --profile work --client-id <APP_ID>

# Every command takes it, anywhere in the arguments.
cargo run --manifest-path tools/graph-oauth/Cargo.toml -- get /me --profile work
cargo run --manifest-path tools/graph-oauth/Cargo.toml -- token --profile work
```

Feeding a live test, without switching anything:

```sh
GRAPH_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml -- token --profile work)" \
  cargo test -p provider-graph --test live_provider -- --nocapture
```
