//! Gated live integration: `TalkMeetings` against the Talk installed in the **Nextcloud** harness
//! (`docker/nextcloud/`). It makes a conversation a guest can open, deletes it so that Talk no
//! longer knows it, and on a server whose administrator forces passwords on public
//! conversations, makes one with the caller's key as its password. Skips unless
//! `NEXTCLOUD_HTTP_ADDR` is set.

use engine_core::calendar::{ConferenceOrigin, ConferenceService, ConferenceUri};
use engine_provider::{MeetingKey, MeetingRequest, MeetingService};
use provider_caldav::{Credentials, TalkMeetings};
use reqwest::{Client, Method, RequestBuilder};
use serde_json::Value;

const KEY: &str = "k5x2q7m4n9b3v8c1z6l0p2w4r7";

struct Harness {
    base: String,
    user: String,
    pass: String,
    http: Client,
}

impl Harness {
    fn from_env() -> Option<Self> {
        let addr = std::env::var("NEXTCLOUD_HTTP_ADDR").ok()?;
        Some(Self {
            base: format!("http://{addr}"),
            user: std::env::var("NEXTCLOUD_USER").unwrap_or_else(|_| "alice".to_owned()),
            pass: std::env::var("NEXTCLOUD_PASS")
                .unwrap_or_else(|_| "nextcloud-alice-pw".to_owned()),
            // Through the shared TLS config, which installs the crypto provider reqwest needs.
            http: engine_tls::TlsClientConfig::bundled()
                .reqwest_builder()
                .build()
                .expect("client"),
        })
    }

    fn talk(&self) -> TalkMeetings {
        TalkMeetings::new(
            &self.base,
            Credentials::Basic {
                username: self.user.clone(),
                password: self.pass.clone(),
            },
            &engine_tls::TlsClientConfig::bundled(),
            &engine_http::RetryConfig::default(),
            &engine_http::test_server::user_agent(),
        )
        .expect("talk")
    }

    /// An OCS request as the harness account, outside the adapter.
    fn ocs(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}/ocs/v2.php{path}", self.base))
            .basic_auth(&self.user, Some(&self.pass))
            .header("OCS-APIRequest", "true")
            .header("Accept", "application/json")
    }

    /// What Talk knows of the conversation `token`, or `None` once it is gone.
    async fn room(&self, token: &str) -> Option<Value> {
        let response = self
            .ocs(Method::GET, &format!("/apps/spreed/api/v4/room/{token}"))
            .send()
            .await
            .unwrap();
        if response.status() == 404 {
            return None;
        }
        assert!(response.status().is_success(), "{}", response.status());
        let body: Value = response.json().await.unwrap();
        Some(body["ocs"]["data"].clone())
    }

    /// Forces passwords on public conversations, or stops forcing them, as the administrator.
    async fn force_passwords(&self, on: bool) {
        let path = "/apps/provisioning_api/api/v1/config/apps/spreed/force_passwords";
        let request = if on {
            self.ocs(Method::POST, path)
                .json(&serde_json::json!({ "value": "yes" }))
        } else {
            self.ocs(Method::DELETE, path)
        };
        let status = request.send().await.unwrap().status();
        assert!(status.is_success(), "force_passwords {on}: {status}");
    }
}

fn request() -> MeetingRequest {
    MeetingRequest {
        key: MeetingKey::new(KEY).unwrap(),
        title: "PIM live Talk meeting".into(),
        start: "2026-12-14T09:00:00Z".parse().unwrap(),
        end: "2026-12-14T09:30:00Z".parse().unwrap(),
    }
}

/// The token at the end of a Talk link the adapter made.
fn token_of(link: &ConferenceUri) -> String {
    link.as_str().rsplit('/').next().unwrap().to_owned()
}

#[tokio::test]
async fn live_a_conversation_is_made_for_guests_and_deleted() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_a_conversation_is_made...: NEXTCLOUD_HTTP_ADDR unset");
        return;
    };
    let talk = harness.talk();
    harness.force_passwords(false).await;
    assert!(
        talk.offered().await.expect("capabilities"),
        "the harness has Talk"
    );

    let made = talk.create_meeting(&request()).await.expect("create");
    let join = made.join().expect("a join link").clone();
    assert_eq!(made.service, Some(ConferenceService::NextcloudTalk));
    assert_eq!(made.origin, ConferenceOrigin::Attached);
    assert_eq!(made.entry_points[0].access.password, None);
    let token = token_of(&join);
    let room = harness
        .room(&token)
        .await
        .expect("Talk knows the conversation");
    assert_eq!(room["type"], 3, "public: {room}");
    assert_eq!(room["name"], "PIM live Talk meeting");
    let guest = harness.http.get(join.as_str()).send().await.unwrap();
    assert_eq!(guest.status(), 200, "a guest opens the link");

    talk.delete_meeting(&made).await.expect("delete");
    assert!(
        harness.room(&token).await.is_none(),
        "the conversation is gone"
    );
    talk.delete_meeting(&made)
        .await
        .expect("deleting one already gone");

    // A server that forces a password: the key is the password, and the invitation carries it.
    harness.force_passwords(true).await;
    let forced = talk.create_meeting(&request()).await;
    harness.force_passwords(false).await;
    let forced = forced.expect("create with a forced password");
    assert_eq!(forced.entry_points[0].access.password.as_deref(), Some(KEY));
    let token = token_of(forced.join().unwrap());
    let room = harness
        .room(&token)
        .await
        .expect("Talk knows the conversation");
    assert_eq!(room["hasPassword"], true, "{room}");
    talk.delete_meeting(&forced).await.expect("delete");
    assert!(harness.room(&token).await.is_none());
}
