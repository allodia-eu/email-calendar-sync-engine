//! How a [`GoogleClient`] roots its URLs at each API's host, and what its `Debug` leaves out.

use super::*;

#[test]
fn client_roots_urls_at_the_base_and_redacts_debug() {
    let client = GoogleClient::connect(
        "super-secret-token",
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap();
    assert_eq!(
        client.url("/gmail/v1/users/me/labels"),
        format!("{GOOGLE_BASE}/gmail/v1/users/me/labels")
    );
    // A custom base roots every path there (a replay server / proxy).
    let custom = GoogleClient::with_base(
        "t",
        "http://127.0.0.1:9",
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap();
    assert_eq!(
        custom.url("/calendar/v3/users/me/calendarList"),
        "http://127.0.0.1:9/calendar/v3/users/me/calendarList"
    );
    // The Debug rendering must not leak the bearer token.
    assert!(!format!("{client:?}").contains("super-secret-token"));
}

/// Gmail bills a user's calls against 6,000 units a minute, and the account's gate is the
/// one place every provider and every core of that account spends through, so the client
/// states its pace there as it connects.
#[test]
fn connecting_holds_the_account_to_gmails_quota() {
    let retry = crate::test_support::retry();
    GoogleClient::connect(
        "t",
        crate::test_support::tls(),
        &retry,
        &crate::test_support::user_agent(),
    )
    .unwrap();
    assert_eq!(
        retry.gate().pace(),
        Some((crate::quota::UNITS_PER_MINUTE, crate::quota::WINDOW))
    );
}

/// People is the one API not served from [`GOOGLE_BASE`]: a contact path rooted
/// there answers an HTML `404`, so `people_url` must retarget it — while a custom
/// base (replay server / proxy) still wins.
#[test]
fn people_paths_root_at_the_people_host_unless_a_custom_base_is_set() {
    let client = GoogleClient::connect(
        "t",
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap();
    assert_eq!(
        client.people_url("/v1/people/me/connections"),
        format!("{PEOPLE_BASE}/v1/people/me/connections")
    );
    assert_eq!(
        client.meet_url("/v2/spaces"),
        format!("{MEET_BASE}/v2/spaces")
    );
    // Gmail and Calendar are unaffected — they stay on the universal host.
    assert_eq!(
        client.url("/gmail/v1/users/me/labels"),
        format!("{GOOGLE_BASE}/gmail/v1/users/me/labels")
    );
    // A custom base wins, so the offline fixture-replay tests are unchanged.
    let custom = GoogleClient::with_base(
        "t",
        "http://127.0.0.1:9",
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap();
    assert_eq!(
        custom.people_url("/v1/people/me/connections"),
        "http://127.0.0.1:9/v1/people/me/connections"
    );
    assert_eq!(
        custom.meet_url("/v2/spaces"),
        "http://127.0.0.1:9/v2/spaces"
    );
}
