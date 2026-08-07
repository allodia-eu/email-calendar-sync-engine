//! The escape hatch: one DAV request, outside the adapter, printed verbatim.
//!
//! Everything else in this tool goes through [`CalDavProvider`](provider_caldav::CalDavProvider)
//! on purpose. This does not, and says so — because some questions cannot be asked through a
//! typed calendar API at all, and they are exactly the questions an unfamiliar server raises:
//!
//! - *Where does `.well-known/caldav` actually redirect to?* (Stalwart: `/dav/cal`, which cost
//!   three round trips to discover the first time.)
//! - *Is there anything in the scheduling inbox?* (RFC 6638 §3.2; the provider does not expose it,
//!   so the live suite reads it over raw DAV too.)
//! - *What does this server return for a property nothing models yet?*
//!
//! Its output is the bytes, unparsed. That is the point: when the adapter and the server
//! disagree, the thing you need is what actually came back, not a projection of it.

use std::time::Duration;

use engine_tls::TlsClientConfig;

use crate::profile::Profile;

/// Sends one request and prints the status, the interesting headers, and the body.
///
/// Redirects are **not** followed: on a DAV server a `301`/`307` is usually the answer you
/// were looking for (that is how `.well-known` discovery works), and quietly following it
/// hides the one fact worth printing.
pub(crate) async fn send(
    profile: &Profile,
    method: &str,
    href: &str,
    depth: &str,
    body: String,
) -> Result<(), String> {
    let url = absolute(&profile.url, href);
    // The engine's own trust policy, not reqwest's defaults: an escape hatch that trusted
    // something the adapter would refuse would answer a question nobody asked.
    let client = TlsClientConfig::bundled()
        .reqwest_builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|err| format!("cannot build an HTTP client: {err}"))?;

    let verb = reqwest::Method::from_bytes(method.to_uppercase().as_bytes())
        .map_err(|_| format!("{method} is not a usable HTTP method"))?;

    let mut request = client
        .request(verb, &url)
        .basic_auth(&profile.user, Some(&profile.pass))
        .header("User-Agent", "allodia-dav-cli")
        .header("Depth", depth);
    if !body.is_empty() {
        request = request
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body);
    }

    let response = request
        .send()
        .await
        .map_err(|err| format!("{method} {url} failed: {err}"))?;

    println!("{} {}", response.status().as_u16(), url);
    for name in ["location", "dav", "etag", "content-type", "schedule-tag"] {
        if let Some(value) = response.headers().get(name) {
            println!("  {name}: {}", value.to_str().unwrap_or("<binary>"));
        }
    }
    let text = response
        .text()
        .await
        .map_err(|err| format!("cannot read the body: {err}"))?;
    if text.trim().is_empty() {
        println!("\n(no body)");
    } else {
        println!("\n{text}");
    }
    Ok(())
}

/// An href against the profile's base: absolute URLs pass through, paths join the origin.
///
/// Joining onto the *origin* rather than the base path is what a DAV server means by an
/// href — every one it returns is site-absolute.
fn absolute(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_owned();
    }
    let trimmed = base.trim_end_matches('/');
    let origin = trimmed
        .find("://")
        .and_then(|scheme_end| {
            trimmed[scheme_end + 3..]
                .find('/')
                .map(|path_start| &trimmed[..scheme_end + 3 + path_start])
        })
        .unwrap_or(trimmed);
    if href.starts_with('/') {
        format!("{origin}{href}")
    } else {
        format!("{origin}/{href}")
    }
}

/// The `PROPFIND` body that asks for everything a server will admit to — the usual first
/// question about an unfamiliar collection.
pub(crate) const ALLPROP: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?><d:propfind xmlns:d=\"DAV:\"><d:allprop/></d:propfind>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_href_passes_through() {
        assert_eq!(
            absolute("https://dav.example.net/base", "https://other.test/x"),
            "https://other.test/x"
        );
    }

    #[test]
    fn a_site_absolute_href_joins_the_origin_not_the_base_path() {
        // The trap: joining onto the base path yields `/dav/cal/dav/cal/...`, which 404s in a
        // way that looks like the resource is missing rather than the URL being wrong.
        assert_eq!(
            absolute("http://127.0.0.1:18080/dav/cal", "/dav/cal/carol/default/"),
            "http://127.0.0.1:18080/dav/cal/carol/default/"
        );
    }

    #[test]
    fn a_relative_href_is_joined_to_the_origin() {
        assert_eq!(
            absolute("https://dav.example.net/base/", "calendars/"),
            "https://dav.example.net/calendars/"
        );
    }

    #[test]
    fn a_base_with_no_path_still_works() {
        assert_eq!(
            absolute("https://caldav.example.net", "/principals/x/"),
            "https://caldav.example.net/principals/x/"
        );
    }
}
