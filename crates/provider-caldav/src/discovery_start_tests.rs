//! Where a connect begins discovery: at the URL a person entered, unless it names only a server.

use crate::{CalDavConfig, CardDavConfig, Credentials};

fn credentials() -> Credentials {
    Credentials::Basic {
        username: "alice".into(),
        password: "secret".into(),
    }
}

fn caldav_start(base_url: &str) -> String {
    CalDavConfig::new(
        base_url,
        credentials(),
        engine_http::test_server::user_agent(),
    )
    .discovery_path
}

fn carddav_start(base_url: &str) -> String {
    CardDavConfig::new(
        base_url,
        credentials(),
        engine_http::test_server::user_agent(),
    )
    .discovery_path
}

#[test]
fn a_server_alone_is_bootstrapped_through_the_well_known() {
    for base in ["https://dav.example.com", "https://dav.example.com/"] {
        assert_eq!(caldav_start(base), "/.well-known/caldav", "{base}");
        assert_eq!(carddav_start(base), "/.well-known/carddav", "{base}");
    }
}

#[test]
fn a_url_with_a_path_is_where_discovery_starts() {
    // The well-known is for when nothing more than a server is known. Were it asked anyway, a
    // well-known that redirects somewhere unusable would leave no URL that connects.
    assert_eq!(
        caldav_start("https://cloud.example.org/remote.php/dav"),
        "/remote.php/dav"
    );
    assert_eq!(
        carddav_start("https://cloud.example.org/remote.php/dav/"),
        "/remote.php/dav/"
    );
}
