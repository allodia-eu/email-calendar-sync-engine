//! Resolving the URLs a JMAP session hands out: the `apiUrl`, the download and
//! upload templates, and a well-known redirect's `Location`.
//!
//! Beside [`session`](crate::session) rather than inside it because resolving a URL
//! against a policy is a separate job from parsing a session document, and that file
//! reached the line limit.

use reqwest::Url;

use crate::{error::JmapError, session::SessionUrlPolicy};

/// Resolves a `target` URL (absolute or a relative path) against the connection
/// `base` per the policy.
///
/// `base.join` already resolves a relative target against the base and lets an
/// absolute target win; [`SessionUrlPolicy::RebaseToConnection`] then forces the
/// origin back to the connection base, keeping only the path and query. Used for
/// both the session `apiUrl` and the well-known redirect `Location`.
pub(crate) fn resolve_against(
    base: &Url,
    target: &str,
    policy: SessionUrlPolicy,
) -> Result<String, JmapError> {
    let joined = base
        .join(target)
        .map_err(|e| JmapError::session(format!("bad URL {target:?}: {e}")))?;
    match policy {
        SessionUrlPolicy::TrustAdvertised => Ok(joined.into()),
        SessionUrlPolicy::RebaseToConnection => {
            let mut rebased = base.clone();
            rebased.set_path(joined.path());
            rebased.set_query(joined.query());
            Ok(rebased.into())
        }
    }
}

/// Rebases a URI *template*'s origin onto the connection `base` per `policy`,
/// preserving its path and query verbatim so RFC 6570 placeholders (`{accountId}`,
/// `{blobId}`, …) survive. Unlike [`resolve_against`], it never runs the template
/// through URL parsing — which would percent-encode the `{`/`}` braces and break
/// the later placeholder substitution.
///
/// Under [`SessionUrlPolicy::RebaseToConnection`] the rewrite is scoped to
/// `session_origin` — the origin the session advertised for **itself** (its `apiUrl`).
/// A template the server deliberately serves from a *different* origin is kept verbatim:
/// the mismatch the rebase corrects (a reverse-proxied or self-hosted server advertising
/// a public hostname it is not reached at) applies uniformly to that server's own origin
/// and cannot explain a second one, so rewriting it can only produce a URL the connection
/// host does not route. Fastmail is the live case — its `apiUrl` is on `api.fastmail.com`
/// while `downloadUrl` is on `www.fastmailusercontent.com`, a separate cookie-less origin
/// for untrusted user content — and rebasing it turned every message-source download into
/// a catch-all `302` to a marketing page.
pub(crate) fn rebase_template(
    base: &Url,
    advertised: &str,
    policy: SessionUrlPolicy,
    session_origin: Option<&str>,
) -> String {
    if policy == SessionUrlPolicy::TrustAdvertised {
        return advertised.to_owned();
    }
    let advertised_origin = origin_of(advertised);
    let same_origin = match (advertised_origin, session_origin) {
        // Origin-free: already relative, so it is the session's own origin by definition.
        (None, _) => true,
        (Some(origin), Some(session)) => origin.eq_ignore_ascii_case(session),
        (Some(_), None) => false,
    };
    if !same_origin {
        return advertised.to_owned();
    }
    let path_and_query = advertised_origin.map_or(advertised, |origin| &advertised[origin.len()..]);
    format!(
        "{}/{}",
        base.origin().ascii_serialization(),
        path_and_query.trim_start_matches('/')
    )
}

/// The `scheme://authority` prefix of an absolute URL, or `None` when it carries no
/// scheme (a relative reference). The origin never contains an RFC 6570 placeholder, so
/// splitting at the first `/` after `://` is safe on a URI template too.
pub(crate) fn origin_of(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.find('/').unwrap_or(rest.len());
    Some(&url[..url.len() - rest.len() + authority])
}
