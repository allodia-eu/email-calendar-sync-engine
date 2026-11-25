use std::sync::Mutex;

use engine_provider::IgnoreConnectSteps;

use super::*;
use crate::{
    test_support::{Replay, ok},
    transport::HttpResponse,
};

/// A `307` to `location`.
fn redirect_to(location: &str) -> HttpResponse {
    HttpResponse {
        status: 307,
        body: String::new(),
        location: Some(location.to_owned()),
        etag: None,
        dav: None,
        retry_after: None,
    }
}

/// Records the hops an observer sees, as `from -> to` pairs.
#[derive(Default)]
struct Hops(Mutex<Vec<String>>);

impl ConnectObserver for Hops {
    fn step(&self, step: &ConnectStep<'_>) {
        if let ConnectStep::Redirected { from, to, .. } = step {
            self.0.lock().unwrap().push(format!("{from} -> {to}"));
        }
    }
}

#[tokio::test]
async fn every_hop_of_a_redirect_chain_is_reported_in_order() {
    // Two hops before the principal answers, so the steps are a *sequence*.
    let exec = Replay::new(vec![
        redirect_to("/dav"),
        redirect_to("/dav/cal"),
        ok(include_str!("../tests/fixtures/principal.xml")),
    ]);
    let hops = Hops::default();
    discover_home(&exec, "/.well-known/caldav", &hops)
        .await
        .unwrap();
    assert_eq!(
        *hops.0.lock().unwrap(),
        ["/.well-known/caldav -> /dav", "/dav -> /dav/cal"]
    );
}

#[tokio::test]
async fn a_relative_redirect_after_an_origin_change_stays_on_the_new_origin() {
    // RFC 6764 discovery starts on the account's domain and a hosted provider may
    // send it to the host actually serving DAV. The hop after that answers with a
    // bare path, which belongs to the *new* origin: left as a path it would be
    // resolved onto the connection base, which is a different server.
    let exec = Replay::new(vec![
        redirect_to("https://dav.example.net/principals/u/"),
        redirect_to("/dav/calendars/u/"),
        ok(include_str!("../tests/fixtures/principal.xml")),
    ]);
    discover_home(&exec, "/.well-known/caldav", &IgnoreConnectSteps)
        .await
        .unwrap();
    let seen = exec.seen();
    assert_eq!(seen[0].1, "/.well-known/caldav");
    assert_eq!(seen[1].1, "https://dav.example.net/principals/u/");
    assert_eq!(seen[2].1, "https://dav.example.net/dav/calendars/u/");
}

/// A `Replay` whose connection refuses to move — what the live client answers when
/// the hop names a plaintext origin (`DavExecutor::trust_origin`).
struct RefusesToMove(Replay);

#[async_trait::async_trait]
impl DavExecutor for RefusesToMove {
    async fn send(
        &self,
        method: DavMethod,
        href: &str,
        depth: &str,
        body: String,
    ) -> Result<HttpResponse, CalDavError> {
        self.0.send(method, href, depth, body).await
    }

    async fn send_write(
        &self,
        request: crate::transport::WriteRequest,
    ) -> Result<HttpResponse, CalDavError> {
        self.0.send_write(request).await
    }

    fn trust_origin(&self, url: &str) -> Result<(), engine_provider::CredentialFailure> {
        // A bare path names no host, so the live client accepts it too. A connection on TLS
        // refuses plaintext the server led it to as the live client does (`LedToInsecure`), and
        // here every other host as a host's policy would (`NotPermitted`).
        if url.starts_with("http://") {
            Err(engine_provider::CredentialFailure::led_to_insecure(
                "plaintext",
            ))
        } else if url.contains("://") {
            Err(engine_provider::CredentialFailure::not_permitted("refused"))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn a_refused_origin_change_fails_the_walk_rather_than_following_it() {
    // Discovery starts at a bare well-known path, which names no scheme, so only the
    // connection can tell that the hop gives up TLS. A refusal must stop the walk:
    // every request it would go on to make carries the account's credentials.
    let exec = RefusesToMove(Replay::new(vec![
        redirect_to("http://dav.example.net/principals/u/"),
        ok(include_str!("../tests/fixtures/principal.xml")),
    ]));
    let err = discover_home(&exec, "/.well-known/caldav", &IgnoreConnectSteps)
        .await
        .unwrap_err();
    assert_eq!(
        engine_provider::ProviderError::from(err).credential_failure(),
        Some(engine_provider::CredentialFailureKind::LedToInsecure)
    );
    // And nothing was sent to the plaintext host.
    assert_eq!(exec.0.seen().len(), 1);
}

#[tokio::test]
async fn a_plaintext_redirect_to_the_same_host_is_followed_over_tls() {
    // Nextcloud behind a TLS-terminating proxy: its `.well-known` rewrite answers with an
    // `http://` URL on the host that just answered over TLS.
    let exec = Replay::new(vec![
        redirect_to("http://dav.example.com/remote.php/dav/"),
        ok(include_str!("../tests/fixtures/principal.xml")),
    ]);
    discover_home(
        &exec,
        "https://dav.example.com/.well-known/caldav",
        &IgnoreConnectSteps,
    )
    .await
    .unwrap();
    assert_eq!(exec.seen()[1].1, "https://dav.example.com/remote.php/dav/");
}

#[tokio::test]
async fn a_plaintext_redirect_to_another_host_says_why_it_is_not_followed() {
    let exec = Replay::new(vec![redirect_to("http://cal.example.net/dav/")]);
    let err = discover_home(
        &exec,
        "https://dav.example.com/.well-known/caldav",
        &IgnoreConnectSteps,
    )
    .await
    .unwrap_err();
    // The server led the credential off TLS: not the account's address, the server's.
    assert_eq!(
        engine_provider::ProviderError::from(err).credential_failure(),
        Some(engine_provider::CredentialFailureKind::LedToInsecure)
    );
    assert_eq!(exec.seen().len(), 1);
}

#[tokio::test]
async fn a_redirect_target_carrying_credentials_is_scrubbed() {
    // A `Location` is server-controlled and may carry userinfo; these steps are
    // built to be logged (`north-star.md`).
    let exec = Replay::new(vec![
        redirect_to("https://alice:hunter2@dav.example.com/cal"),
        ok(include_str!("../tests/fixtures/principal.xml")),
    ]);
    let hops = Hops::default();
    discover_home(&exec, "/.well-known/caldav", &hops)
        .await
        .unwrap();
    assert_eq!(
        *hops.0.lock().unwrap(),
        ["/.well-known/caldav -> https://dav.example.com/cal"]
    );
}

#[tokio::test]
async fn the_principal_second_step_is_not_a_redirect() {
    // The RFC 6764 §6 two-step flow is two PROPFINDs of *different* resources, not
    // one resource moving — so it emits no hop.
    let root = ok(
        "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:href>/</D:href><D:propstat><D:prop><D:current-user-principal><D:href>/principals/users/dennis/</D:href></D:current-user-principal></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",
    );
    let principal = ok(
        "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:href>/principals/users/dennis/</D:href><D:propstat><D:prop><C:calendar-home-set><D:href>/calendars/dennis/</D:href></C:calendar-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",
    );
    let hops = Hops::default();
    let exec = Replay::new(vec![root, principal]);
    discover_home(&exec, "/.well-known/caldav", &hops)
        .await
        .unwrap();
    assert!(hops.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn follows_a_redirect_then_reads_the_home() {
    let redirect = HttpResponse {
        status: 307,
        body: String::new(),
        location: Some("/dav/cal".to_owned()),
        etag: None,
        dav: None,
        retry_after: None,
    };
    let exec = Replay::new(vec![
        redirect,
        ok(include_str!("../tests/fixtures/principal.xml")),
    ]);
    let home = discover_home(&exec, "/.well-known/caldav", &IgnoreConnectSteps)
        .await
        .unwrap();
    assert_eq!(home.href, "/dav/cal/alice%40test.local/");
    // Two requests: the well-known, then the redirect target.
    let seen = exec.seen();
    assert_eq!(seen[0].1, "/.well-known/caldav");
    assert_eq!(seen[1].1, "/dav/cal");
}

#[tokio::test]
async fn discovers_the_home_in_two_steps_via_the_principal() {
    // The RFC-correct shape (e.g. Soverin): the start URL returns only the
    // current-user-principal (the home-set comes back 404 there), so discovery
    // must PROPFIND the principal for the calendar-home-set.
    let root = ok(
        "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:href>/</D:href><D:propstat><D:prop><D:current-user-principal><D:href>/principals/users/dennis/</D:href></D:current-user-principal></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat><D:propstat><D:prop><C:calendar-home-set/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat></D:response></D:multistatus>",
    );
    let principal = ok(
        "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:href>/principals/users/dennis/</D:href><D:propstat><D:prop><C:calendar-home-set><D:href>/calendars/dennis/</D:href></C:calendar-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",
    );
    let exec = Replay::new(vec![root, principal]);

    let home = discover_home(&exec, "/.well-known/caldav", &IgnoreConnectSteps)
        .await
        .unwrap();
    assert_eq!(home.href, "/calendars/dennis/");
    let seen = exec.seen();
    assert_eq!(seen[0].1, "/.well-known/caldav"); // step 1: the well-known
    assert_eq!(seen[1].1, "/principals/users/dennis/"); // step 2: the principal
}

#[tokio::test]
async fn lists_only_calendar_collections() {
    let exec = Replay::new(vec![ok(include_str!(
        "../tests/fixtures/calendar-home.xml"
    ))]);
    let calendars = list_calendars(&exec, "/dav/cal/alice%40test.local/")
        .await
        .unwrap();
    // The home itself is a plain collection and is filtered out.
    assert_eq!(calendars.len(), 1);
    assert_eq!(
        calendars[0].0.id.as_str(),
        "/dav/cal/alice%40test.local/default/"
    );
}

/// A home the way iCloud lists one: a calendar of events, a reminders list that holds tasks
/// alone, and a calendar that does not say which components it holds; and one whose component
/// set is present but empty.
const HOME_WITH_REMINDERS: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:response><D:href>/1/calendars/home/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:calendar/></D:resourcetype><C:supported-calendar-component-set><C:comp name="VEVENT"/></C:supported-calendar-component-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>/1/calendars/tasks/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:calendar/></D:resourcetype><C:supported-calendar-component-set><C:comp name="VTODO"/></C:supported-calendar-component-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>/1/calendars/silent/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:calendar/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat><D:propstat><D:prop><C:supported-calendar-component-set/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat></D:response><D:response><D:href>/1/calendars/empty/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:calendar/></D:resourcetype><C:supported-calendar-component-set/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;

#[tokio::test]
async fn a_collection_of_tasks_alone_is_not_listed_as_a_calendar() {
    let exec = Replay::new(vec![ok(HOME_WITH_REMINDERS)]);
    let calendars = list_calendars(&exec, "/1/calendars/").await.unwrap();
    let hrefs: Vec<&str> = calendars.iter().map(|(c, ..)| c.id.as_str()).collect();
    // One that names no components, absent or empty, holds any of them (RFC 4791 §5.2.3),
    // events included.
    assert_eq!(
        hrefs,
        [
            "/1/calendars/home/",
            "/1/calendars/silent/",
            "/1/calendars/empty/"
        ]
    );
}

#[tokio::test]
async fn a_response_without_a_home_set_is_an_error() {
    let exec = Replay::new(vec![ok(
        "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/x</D:href><D:propstat><D:prop/><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",
    )]);
    assert!(
        discover_home(&exec, "/x", &IgnoreConnectSteps)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_home_on_a_host_the_connection_may_not_move_to_fails_the_walk() {
    // Listing it anyway would send the request without the credential and read the refusal as
    // the server's answer about the account.
    let exec = RefusesToMove(Replay::new(vec![ok(
        r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/u/</d:href><d:propstat><d:prop><c:calendar-home-set><d:href>http://dav.example.net/u/calendars/</d:href></c:calendar-home-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#,
    )]));
    let err = discover_home(&exec, "/.well-known/caldav", &IgnoreConnectSteps)
        .await
        .unwrap_err();
    assert_eq!(
        engine_provider::ProviderError::from(err).credential_failure(),
        Some(engine_provider::CredentialFailureKind::LedToInsecure)
    );
}

/// A home on an `https` host the policy refuses is the policy's refusal, not an insecure origin.
#[tokio::test]
async fn a_home_on_a_secure_host_the_policy_refuses_is_not_permitted() {
    let exec = RefusesToMove(Replay::new(vec![ok(
        r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/u/</d:href><d:propstat><d:prop><c:calendar-home-set><d:href>https://p1-dav.example.net/u/calendars/</d:href></c:calendar-home-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#,
    )]));
    let err = discover_home(&exec, "/.well-known/caldav", &IgnoreConnectSteps)
        .await
        .unwrap_err();
    assert_eq!(
        engine_provider::ProviderError::from(err).credential_failure(),
        Some(engine_provider::CredentialFailureKind::NotPermitted)
    );
}
