//! Nextcloud Talk meetings, for an event whose calendar's server makes none of its own
//! (`calendar-semantics.md`, "Meeting services").
//!
//! A meeting is a public conversation (`roomType` 3), which a guest joins at
//! `<server>/index.php/call/<token>`. Talk keeps no key of the caller's, so a repeated
//! request makes a second conversation. A server whose administrator forces a password on
//! public conversations refuses one without (`force-passwords`), so there the caller's key is
//! the password and the invitation carries it. A conversation is found again by the token in
//! its link, and Talk answers 404 for one that is gone.

use async_trait::async_trait;
use engine_core::calendar::{
    Conference, ConferenceKey, ConferenceOrigin, ConferenceService, ConferenceUri, EntryPoint,
};
use engine_http::{Exchange, RetryConfig, send_retrying};
use engine_provider::{MeetingRequest, MeetingService, ProviderError, ProviderResult};
use engine_tls::TlsClientConfig;
use reqwest::{Client, Method, RequestBuilder, redirect::Policy};
use serde_json::{Value, json};

use crate::{error::CalDavError, transport::Credentials};

/// What a Talk meeting attached to an event is called there.
const LABEL: &str = "Nextcloud Talk";
/// A public conversation, which a guest with the link can join.
const PUBLIC: u8 = 3;
/// The longest conversation name Talk accepts.
const NAME_LIMIT: usize = 255;
const ROOMS: &str = "/ocs/v2.php/apps/spreed/api/v4/room";
const CAPABILITIES: &str = "/ocs/v2.php/cloud/capabilities";

/// Talk meetings on one Nextcloud server, made as the account its credentials sign in as.
pub struct TalkMeetings {
    client: Client,
    /// The server's address, without a trailing `/`; a path is kept, for a server beneath one.
    server: String,
    credentials: Credentials,
    retry: RetryConfig,
}

impl core::fmt::Debug for TalkMeetings {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TalkMeetings")
            .field("server", &self.server)
            .field("credentials", &self.credentials)
            .finish_non_exhaustive()
    }
}

/// What the server says about making a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rules {
    can_create: bool,
    force_passwords: bool,
}

impl TalkMeetings {
    /// Meetings on the Nextcloud server at `server`, such as `https://cloud.example.org`.
    /// Requests follow no redirect, so the credential goes to that origin and no other.
    ///
    /// # Errors
    ///
    /// [`ProviderError::invalid_state`] when `server` is no `http` or `https` address, or has
    /// a user name, a query or a fragment, none of which a conversation's link appended to it
    /// could carry; a transport error when the HTTP client cannot be built.
    pub fn new(
        server: &str,
        credentials: Credentials,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> ProviderResult<Self> {
        let url = reqwest::Url::parse(server)
            .ok()
            .filter(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
            })
            .ok_or_else(|| {
                ProviderError::invalid_state(
                    "a Talk server is an http or https address without a user name, query or fragment",
                )
            })?;
        let client = engine_http::client(tls, user_agent)
            .redirect(Policy::none())
            .build()
            .map_err(|err| unanswered(err.into()))?;
        Ok(Self {
            client,
            server: url.as_str().trim_end_matches('/').to_owned(),
            credentials,
            retry: retry.clone().labelled("talk"),
        })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let builder = self
            .client
            .request(method, format!("{}{path}", self.server))
            .header("OCS-APIRequest", "true")
            .header(reqwest::header::ACCEPT, "application/json");
        match &self.credentials {
            Credentials::Basic { username, password } => {
                builder.basic_auth(username, Some(password))
            }
            Credentials::Bearer(token) => builder.bearer_auth(token),
        }
    }

    /// Sends `request`, answering the reply's status and its OCS `data`, or `None` for 404.
    async fn send(&self, request: RequestBuilder) -> ProviderResult<Option<Value>> {
        let sent = send_retrying(request, &self.retry, Exchange::Ordinary)
            .await
            .map_err(unanswered)?;
        let status = sent.status().as_u16();
        let wait = sent.stated_wait();
        let body = sent.text().await.map_err(unanswered)?;
        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(refused(status, body, wait));
        }
        let value: Value = serde_json::from_str(&body)
            .map_err(|_| ProviderError::permanent("Talk answered without JSON"))?;
        Ok(Some(
            value.pointer("/ocs/data").cloned().unwrap_or(Value::Null),
        ))
    }

    async fn rules(&self) -> ProviderResult<Rules> {
        self.capabilities()
            .await?
            .ok_or_else(|| ProviderError::permanent("the server has no Talk"))
    }

    /// What the server's capabilities say about making a conversation; `None` without Talk.
    async fn capabilities(&self) -> ProviderResult<Option<Rules>> {
        let data = self
            .send(self.request(Method::GET, CAPABILITIES))
            .await?
            .unwrap_or(Value::Null);
        Ok(rules(&data))
    }

    /// Whether this account can make a meeting here: the server has Talk, and lets the account
    /// start a conversation. Asked of the server's capabilities, as a creation asks them first.
    ///
    /// # Errors
    ///
    /// The server's refusal or a transport failure. A server without Talk is `Ok(false)`.
    pub async fn offered(&self) -> ProviderResult<bool> {
        Ok(self
            .capabilities()
            .await?
            .is_some_and(|rules| rules.can_create))
    }

    /// The link of the conversation whose token is `token`.
    fn link(&self, token: &str) -> String {
        format!("{}/index.php/call/{token}", self.server)
    }

    /// The token of the conversation `join` opens, when it is one on this server.
    fn token<'a>(&self, join: &'a ConferenceUri) -> Option<&'a str> {
        token_in(&self.server, join.as_str())
    }
}

#[async_trait]
impl MeetingService for TalkMeetings {
    fn service(&self) -> ConferenceService {
        ConferenceService::NextcloudTalk
    }

    async fn create_meeting(&self, request: &MeetingRequest) -> ProviderResult<Conference> {
        let rules = self.rules().await?;
        if !rules.can_create {
            return Err(ProviderError::permanent(
                "this account may not start a Talk conversation",
            ));
        }
        let password = rules.force_passwords.then(|| request.key.as_str());
        let mut body = json!({ "roomType": PUBLIC, "roomName": name(&request.title) });
        if let Some(password) = password {
            body["password"] = json!(password);
        }
        let data = self
            .send(self.request(Method::POST, ROOMS).json(&body))
            .await?
            .ok_or_else(|| ProviderError::permanent("the server has no Talk"))?;
        let token = created(&data)
            .ok_or_else(|| ProviderError::permanent("Talk answered without a conversation"))?;
        conference(&self.link(token), password)
    }

    async fn delete_meeting(&self, meeting: &Conference) -> ProviderResult<()> {
        let token = meeting
            .join()
            .and_then(|join| self.token(join))
            .ok_or_else(|| {
                ProviderError::invalid_state("the meeting is no Talk conversation on this server")
            })?;
        self.send(self.request(Method::DELETE, &format!("{ROOMS}/{token}")))
            .await?;
        Ok(())
    }
}

/// What the capabilities `data` says about making a conversation; `None` without Talk.
fn rules(data: &Value) -> Option<Rules> {
    let conversations = data.pointer("/capabilities/spreed/config/conversations")?;
    let flag = |name: &str| conversations.get(name).and_then(Value::as_bool);
    Some(Rules {
        can_create: flag("can-create").unwrap_or(true),
        force_passwords: flag("force-passwords").unwrap_or(false),
    })
}

/// The token of the conversation a creation's `data` describes.
fn created(data: &Value) -> Option<&str> {
    data.get("token")
        .and_then(Value::as_str)
        .filter(|token| is_token(token))
}

/// `title` cut to what Talk accepts as a name, at a character boundary.
fn name(title: &str) -> String {
    title.trim().chars().take(NAME_LIMIT).collect()
}

/// Whether `token` has a Talk token's shape, so it can go in a path as it is.
fn is_token(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The token in `link` when it opens a conversation on `server`, with or without
/// `index.php`, a trailing `/`, a query or a fragment.
fn token_in<'a>(server: &str, link: &'a str) -> Option<&'a str> {
    let rest = link.strip_prefix(server)?;
    let rest = rest.strip_prefix("/index.php").unwrap_or(rest);
    let rest = rest.strip_prefix("/call/")?;
    let token = rest.split(['/', '?', '#']).next()?;
    is_token(token).then_some(token)
}

/// The conversation at `link` as an event holds one made elsewhere, with its password when
/// the server forces one.
fn conference(link: &str, password: Option<&str>) -> ProviderResult<Conference> {
    let uri = ConferenceUri::parse(link)
        .map_err(|_| ProviderError::permanent("a Talk link that cannot be opened"))?;
    let mut entry = EntryPoint::at(uri.clone());
    entry.access.password = password.map(str::to_owned);
    Ok(Conference {
        key: ConferenceKey::new(uri.as_str()),
        service: Some(ConferenceService::NextcloudTalk),
        origin: ConferenceOrigin::Attached,
        entry_points: vec![entry],
        label: Some(LABEL.to_owned()),
        meeting_id: None,
        notes: None,
    })
}

/// Talk's refusal of a request, classified as a CalDAV status is.
fn refused(status: u16, body: String, wait: Option<core::time::Duration>) -> ProviderError {
    let detail = format!("Talk HTTP {status}: {body}");
    CalDavError::status(status, body)
        .with_retry_after(wait)
        .into_provider_error(detail)
}

/// A request that never got Talk's answer.
fn unanswered(cause: engine_http::SendError) -> ProviderError {
    let detail = format!("Talk transport error: {cause}");
    CalDavError::Transport(cause).into_provider_error(detail)
}

#[cfg(test)]
#[path = "talk_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "talk_http_tests.rs"]
mod http_tests;
