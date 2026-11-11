//! Shared-store discovery over the **captured** answers of both harness servers
//! (`tests/fixtures/shared-*.xml`), each from the request `shared.rs` sends.
//!
//! The two servers reach the same answer by different routes, which is why both are here:
//! Stalwart has no proxy sets and refuses bob's home `403`; SabreDAV names its delegations in
//! the proxy sets and answers bob's home `207` with nothing granted inside.

use std::sync::Arc;

use super::*;
use crate::{
    request::{CALENDAR_LIST_PROPFIND, DELEGATION_PROPFIND, PRINCIPAL_DESCRIPTION_PROPFIND},
    test_support::{Replay, ok, status},
};

const STALWART: &str = "http://127.0.0.1:18180";
const SABREDAV: &str = "http://127.0.0.1:18181";

fn fixture(name: &str) -> String {
    let dir = env!("CARGO_MANIFEST_DIR");
    std::fs::read_to_string(format!("{dir}/tests/fixtures/{name}")).expect("fixture")
}

fn summary(stores: &[SharedAccount]) -> Vec<(String, Option<String>)> {
    stores
        .iter()
        .map(|s| (s.handle.as_str().to_owned(), s.address.clone()))
        .collect()
}

#[tokio::test]
async fn stalwart_offers_the_principals_whose_homes_answer_and_skips_the_refused_one() {
    let exec = Arc::new(Replay::new(vec![
        ok(&fixture("shared-delegation-stalwart.xml")),
        ok(&fixture("shared-principals-stalwart.xml")),
        // Listed in principal order: bob (his INBOX only), dave, the support group.
        status(403, ""),
        ok(&fixture("shared-home-stalwart-dave.xml")),
        ok(&fixture("shared-home-stalwart-support.xml")),
    ]));
    let me = format!("{STALWART}/dav/pal/alice%40test.local/");
    let stores = list(exec.as_ref(), &me).await.unwrap();
    assert_eq!(
        summary(&stores),
        [
            (
                format!("{STALWART}/dav/pal/dave%40test.local/"),
                Some("dave@test.local".to_owned())
            ),
            (
                format!("{STALWART}/dav/pal/support%40test.local/"),
                Some("support@test.local".to_owned())
            ),
        ]
    );
    for store in &stores {
        assert_eq!(store.domains, StoreDomains::of(SearchDomain::Calendar));
    }
    assert_eq!(stores[0].name, "Dave Tester");

    // The requests, and the bodies they carried: the principal for its routes, the principal
    // collection at Depth 1, then each candidate's home. Alice's own is never asked.
    let asked: Vec<(String, String, String)> = exec
        .reads()
        .iter()
        .map(|(_, href, depth, body)| (href.clone(), depth.clone(), body.clone()))
        .collect();
    let expect = |href: String, depth: &str, body: &str| (href, depth.to_owned(), body.to_owned());
    assert_eq!(
        asked,
        [
            expect(me.clone(), "0", DELEGATION_PROPFIND),
            expect(
                format!("{STALWART}/dav/pal/"),
                "1",
                PRINCIPAL_DESCRIPTION_PROPFIND
            ),
            expect(
                format!("{STALWART}/dav/cal/bob%40test.local/"),
                "1",
                CALENDAR_LIST_PROPFIND
            ),
            expect(
                format!("{STALWART}/dav/cal/dave%40test.local/"),
                "1",
                CALENDAR_LIST_PROPFIND
            ),
            expect(
                format!("{STALWART}/dav/cal/support%40test.local/"),
                "1",
                CALENDAR_LIST_PROPFIND
            ),
        ]
    );
}

#[tokio::test]
async fn sabredav_offers_its_delegations_and_skips_a_home_that_grants_nothing() {
    let exec = Arc::new(Replay::new(vec![
        ok(&fixture("shared-delegation-sabredav.xml")),
        ok(&fixture("shared-principals-sabredav.xml")),
        // The proxy sets come first: dave (read), support (write). Admin and bob are listed
        // with their properties withheld, so neither names a home and neither is asked.
        ok(&fixture("shared-home-sabredav-dave.xml")),
        ok(&fixture("shared-home-sabredav-support.xml")),
    ]));
    let me = format!("{SABREDAV}/principals/alice@test.local/");
    let stores = list(exec.as_ref(), &me).await.unwrap();
    assert_eq!(
        summary(&stores),
        [
            (
                format!("{SABREDAV}/principals/dave@test.local/"),
                Some("dave@test.local".to_owned())
            ),
            (
                format!("{SABREDAV}/principals/support@test.local/"),
                Some("support@test.local".to_owned())
            ),
        ]
    );
    assert_eq!(exec.reads().len(), 4);
}

#[tokio::test]
async fn nextcloud_offers_its_delegation_though_it_will_not_list_principals() {
    // Captured from the Nextcloud fixture with a second user, bob, who made alice his read
    // proxy. Nextcloud names four principal collections and refuses to list each `405`.
    let exec = Replay::new(vec![
        ok(&fixture("shared-delegation-nextcloud.xml")),
        status(405, &fixture("shared-principals-nextcloud-405.xml")),
        status(405, ""),
        status(405, ""),
        status(405, ""),
        ok(&fixture("shared-principal-nextcloud-bob.xml")),
        ok(&fixture("shared-home-nextcloud-bob.xml")),
    ]);
    let origin = "http://127.0.0.1:18182";
    let me = format!("{origin}/remote.php/dav/principals/users/alice/");
    let stores = list(&exec, &me).await.unwrap();
    assert_eq!(
        summary(&stores),
        [(
            format!("{origin}/remote.php/dav/principals/users/bob/"),
            Some("bob@test.local".to_owned())
        )]
    );
    assert_eq!(stores[0].name, "Bob Tester");
}

#[tokio::test]
async fn a_home_listed_but_closed_is_not_offered() {
    // SabreDAV's answer for a principal who delegated nothing: `207`, the calendar inside
    // listed with every property `403`.
    let exec = Replay::new(vec![ok(&fixture("shared-home-sabredav-bob.xml"))]);
    assert!(
        !reads_a_calendar(&exec, &format!("{SABREDAV}/calendars/bob@test.local/"))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_delegation_without_a_principal_collection_is_described_on_its_own() {
    let delegation = r#"<d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">
<d:response><d:href>/principals/alice/</d:href><d:propstat><d:prop>
<cs:calendar-proxy-read-for><d:href>/principals/dave/</d:href></cs:calendar-proxy-read-for>
</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
    let dave = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
<d:response><d:href>/principals/dave/</d:href><d:propstat><d:prop>
<c:calendar-home-set><d:href>/calendars/dave/</d:href></c:calendar-home-set>
<c:calendar-user-address-set><d:href>mailto:dave@example.test</d:href></c:calendar-user-address-set>
</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
    let exec = Replay::new(vec![
        ok(delegation),
        ok(dave),
        ok(&fixture("shared-home-sabredav-dave.xml")),
    ]);
    let stores = list(&exec, "https://dav.example.test/principals/alice/")
        .await
        .unwrap();
    assert_eq!(stores.len(), 1);
    // No display name: the address labels it.
    assert_eq!(stores[0].name, "dave@example.test");
    assert_eq!(
        exec.reads()[1],
        (
            DavMethod::Propfind,
            "https://dav.example.test/principals/dave/".to_owned(),
            "0".to_owned(),
            PRINCIPAL_DESCRIPTION_PROPFIND.to_owned()
        )
    );
}

#[tokio::test]
async fn a_server_fault_fails_the_listing_rather_than_shortening_it() {
    let exec = Replay::new(vec![
        ok(&fixture("shared-delegation-stalwart.xml")),
        ok(&fixture("shared-principals-stalwart.xml")),
        status(500, "boom"),
    ]);
    let me = format!("{STALWART}/dav/pal/alice%40test.local/");
    list(&exec, &me).await.unwrap_err();
}

#[tokio::test]
async fn a_principal_that_answers_no_routes_has_nothing_to_offer() {
    let exec = Replay::new(vec![status(404, "")]);
    assert_eq!(
        list(&exec, "https://dav.example.test/principals/alice/")
            .await
            .unwrap(),
        []
    );
}

#[test]
fn a_principal_is_the_same_resource_however_its_at_sign_is_written() {
    assert!(same_resource(
        "http://h/dav/pal/alice%40test.local/",
        "http://h/dav/pal/alice@test.local"
    ));
    assert!(!same_resource(
        "http://h/dav/pal/alice@test.local/",
        "http://h/dav/pal/bob@test.local/"
    ));
}
