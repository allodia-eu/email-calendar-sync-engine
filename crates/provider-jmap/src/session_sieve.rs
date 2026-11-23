//! What the session says about an account's Sieve scripts (RFC 9661 §1.3).

use serde_json::Value;

use crate::request::capability;

/// The `urn:ietf:params:jmap:sieve` account capability: the extensions the account's
/// interpreter lists, and its limits. Every limit is `None` where the server states none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SieveCapability {
    /// `sieveExtensions`: what a script may require.
    pub(crate) extensions: Vec<String>,
    /// `maxSizeScript`, in octets.
    pub(crate) max_size_script: Option<usize>,
    /// `maxNumberRedirects`: how many times one message may be redirected.
    pub(crate) max_redirects: Option<usize>,
}

/// The Sieve capability of `account`, as `session` states it, or `None` where the account
/// lists none.
pub(crate) fn sieve_capability(session: &Value, account: Option<&str>) -> Option<SieveCapability> {
    let caps = session
        .get("accounts")?
        .get(account?)?
        .get("accountCapabilities")?
        .get(capability::SIEVE)?;
    let limit = |name: &str| {
        caps.get(name)
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
    };
    Some(SieveCapability {
        extensions: caps
            .get("sieveExtensions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        max_size_script: limit("maxSizeScript"),
        max_redirects: limit("maxNumberRedirects"),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_capability_is_read_from_the_addressed_account() {
        let session = json!({ "accounts": { "a": { "accountCapabilities": {
            "urn:ietf:params:jmap:sieve": {
                "sieveExtensions": ["fileinto", 7, "regex"],
                "maxSizeScript": 102_400, "maxNumberRedirects": 1, "maxNumberScripts": 100
            }
        } }, "b": { "accountCapabilities": {} } } });
        assert_eq!(
            sieve_capability(&session, Some("a")),
            Some(SieveCapability {
                extensions: vec!["fileinto".to_owned(), "regex".to_owned()],
                max_size_script: Some(102_400),
                max_redirects: Some(1),
            })
        );
        assert_eq!(sieve_capability(&session, Some("b")), None);
        assert_eq!(sieve_capability(&session, None), None);
        let bare = json!({ "accounts": { "a": { "accountCapabilities": {
            "urn:ietf:params:jmap:sieve": {}
        } } } });
        assert_eq!(
            sieve_capability(&bare, Some("a")),
            Some(SieveCapability::default())
        );
    }
}
