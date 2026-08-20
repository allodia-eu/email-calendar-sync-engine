//! Gated live check that a **work/school** account can read a colleague's profile photo
//! through `User.ReadBasic.All` alone.
//!
//! This exists to settle one question with real consequences: `ProfilePhoto.Read.All`
//! grants the same read but requires **admin consent**, so if it were required, tenant
//! directory avatars would be gated behind an administrator for every customer. The
//! adapter is built on `User.ReadBasic.All` being sufficient, and that is a claim about
//! Microsoft's behaviour — so it is asserted against Microsoft, not against a doc page.
//!
//! Needs a token from a work/school account; a personal Microsoft account has no
//! directory at all (`"This API is not supported for MSA accounts"`) and skips.
//!
//! ```sh
//! GRAPH_TOKENS=tools/graph-oauth/.local/tokens-work.json \
//!   cargo run --manifest-path tools/graph-oauth/Cargo.toml -- login --client-id <APP_ID>
//! GRAPH_ACCESS_TOKEN="$(python3 -c "import json;print(json.load(open('tools/graph-oauth/.local/tokens-work.json'))['access_token'])")" \
//!   cargo test -p provider-graph --test live_directory_photos -- --nocapture
//! ```

use engine_core::{contact::ContactCard, ids::AccountId, sync::SyncUpdate};
use engine_provider::{ContactSourceSync, ContactsProvider};
use provider_graph::{GraphClient, GraphContactProvider};

fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// The `scp` claim of a Graph access token: the scopes actually granted, space-separated.
///
/// Read because it is the only thing that makes the assertion below mean anything. The
/// app registration carries admin consent for `ProfilePhoto.Read.All` in its home tenant,
/// so a token *could* arrive already carrying it — and then a successful photo read would
/// prove nothing about `User.ReadBasic.All`. No signature check: this is our own token,
/// read for a control, not trusted for authorization.
///
/// `None` for a **personal** Microsoft account, whose Graph access token is an opaque
/// single-segment string rather than a JWT. That is also exactly the case with no
/// directory to read, so it is a skip rather than a failure.
fn granted_scopes(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let (mut bits, mut count, mut out) = (0_u32, 0_u8, Vec::new());
    for byte in payload.bytes() {
        let digit = alphabet.iter().position(|candidate| *candidate == byte)?;
        bits = (bits << 6) | u32::try_from(digit).ok()?;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push(u8::try_from((bits >> count) & 0xFF).ok()?);
        }
    }
    let claims: serde_json::Value = serde_json::from_slice(&out).ok()?;
    claims.get("scp")?.as_str().map(str::to_owned)
}

#[tokio::test]
async fn live_a_colleagues_photo_reads_without_the_admin_consented_scope() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_a_colleagues_photo_reads_without_the_admin_consented_scope: \
             GRAPH_ACCESS_TOKEN unset"
        );
        return;
    };
    // The control. Without it, an admin-consented tenant could be supplying the very
    // permission this test claims is unnecessary, and the result would be backwards.
    let Some(scopes) = granted_scopes(&token) else {
        eprintln!(
            "!! NOT VERIFIED: this access token is opaque, not a JWT — the mark of a \
             personal Microsoft account, which has no directory. Sign in with a \
             work/school account (GRAPH_TOKENS=…/tokens-work.json)."
        );
        return;
    };
    assert!(
        !scopes.contains("ProfilePhoto.Read"),
        "this token already grants ProfilePhoto.Read.All, so it cannot show whether \
         User.ReadBasic.All alone suffices — re-mint it without that scope. Granted: {scopes}"
    );
    assert!(
        scopes.contains("User.ReadBasic.All"),
        "the directory read needs User.ReadBasic.All. Granted: {scopes}"
    );

    let account = AccountId::try_from("live-directory").unwrap();
    let provider = GraphContactProvider::directory(
        GraphClient::connect(token, &engine_tls::TlsClientConfig::bundled()).expect("client"),
    );
    let users = match provider.sync_contacts(&account, None).await {
        Ok(ContactSourceSync::Available { sync, .. }) => match sync.update {
            SyncUpdate::Snapshot { objects, .. } => objects,
            SyncUpdate::Delta { changed, .. } => changed,
        },
        Ok(ContactSourceSync::Unavailable(reason)) => {
            eprintln!("!! NOT VERIFIED: the directory source is unavailable ({reason:?})");
            return;
        }
        Err(error) => {
            eprintln!(
                "!! NOT VERIFIED: no directory on this account — a personal Microsoft \
                 account has none. Sign in with a work/school account. ({error:?})"
            );
            return;
        }
    };
    assert!(
        !users.is_empty(),
        "a tenant directory has at least the signed-in user"
    );

    // Every directory card advertises a photo endpoint; only asking says whether an
    // image is there. Walk until one answers, so the test does not depend on *which*
    // colleague has a picture — only on some colleague having one.
    let mut asked = 0_usize;
    let mut found: Option<(ContactCard, usize)> = None;
    for card in &users {
        let Some(media) = card
            .media
            .values()
            .map(|resource| &resource.value)
            .find(|resource| resource.kind.as_deref() == Some("photo"))
        else {
            continue;
        };
        asked += 1;
        let photo = provider
            .fetch_contact_photo(&account, card, media)
            .await
            .expect("a directory photo read must not fail");
        if let Some(photo) = photo {
            found = Some((card.clone(), photo.as_bytes().len()));
            break;
        }
    }

    match found {
        Some((_, len)) => {
            assert!(len > 0, "a photo that exists has bytes");
            eprintln!(
                "verified: a directory photo read with User.ReadBasic.All and no \
                 ProfilePhoto.Read.All ({len} bytes, after asking {asked} user(s))"
            );
        }
        None => eprintln!(
            "!! NOT VERIFIED: asked {asked} directory user(s) and none has a profile photo. \
             Set one on any account in the tenant and re-run — a clean run of absences \
             cannot tell 'the scope is insufficient' from 'nobody has a picture'."
        ),
    }
}
