//! Whose a calendar is: its `DAV:owner` principal, read as that principal's address, for
//! [`Calendar::owner`](engine_core::calendar::Calendar::owner).
//!
//! The field is for a calendar someone shared with the user, so a calendar the user's own
//! principal owns has none. Observed: a calendar in another principal's home names that
//! principal (dave's, read through a proxy delegation or a share), while SabreDAV names an
//! in-home share's *sharee* — the user — as its owner, so `bob-readonly` stays ownerless
//! there; the server says nothing more without its sharing plugin.
//!
//! An owner principal costs one `PROPFIND` the first time it is met, then is remembered for
//! the provider's life.

use std::{collections::BTreeMap, sync::Mutex};

use crate::{
    discovery::follow,
    error::CalDavError,
    shared::{describe, principal_address, same_resource},
    transport::DavExecutor,
};

/// Each foreign owner principal met so far, by URL, with its address (`None`: it names none,
/// or the credential may not read it).
#[derive(Debug, Default)]
pub(crate) struct Owners(Mutex<BTreeMap<String, Option<String>>>);

impl Owners {
    /// The address of the calendar owner `owner`, or `None` when the calendar is the user's
    /// (`user`), names no owner, or its owner names no address.
    pub(crate) async fn address_of(
        &self,
        exec: &dyn DavExecutor,
        owner: Option<&str>,
        user: Option<&str>,
    ) -> Result<Option<String>, CalDavError> {
        let Some(owner) = owner else {
            return Ok(None);
        };
        // Compared as absolute URLs: the listing's href is relative to a home the connection
        // may hold as a path, while discovery kept the principal absolute.
        let owner = &exec.absolute(owner);
        if user.is_some_and(|user| same_resource(owner, &exec.absolute(user))) {
            return Ok(None);
        }
        if let Some(known) = self.0.lock().expect("owners lock").get(owner) {
            return Ok(known.clone());
        }
        // An owner on an origin the credential may not go to is not asked.
        let address = match follow(exec, owner, owner) {
            Ok(url) => describe(exec, &url)
                .await?
                .and_then(|props| principal_address(&props)),
            Err(_) => None,
        };
        self.0
            .lock()
            .expect("owners lock")
            .insert(owner.to_owned(), address.clone());
        Ok(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Replay, ok, status};

    const DAVE: &str = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
<d:response><d:href>/principals/dave/</d:href><d:propstat><d:prop>
<c:calendar-user-address-set><d:href>mailto:dave@example.test</d:href></c:calendar-user-address-set>
</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;

    #[tokio::test]
    async fn a_foreign_owner_is_read_once_and_remembered() {
        let exec = Replay::new(vec![ok(DAVE)]);
        let owners = Owners::default();
        let dave = Some("https://dav.example.test/principals/dave/");
        let me = Some("https://dav.example.test/principals/alice/");
        for _ in 0..2 {
            assert_eq!(
                owners.address_of(&exec, dave, me).await.unwrap().as_deref(),
                Some("dave@example.test")
            );
        }
        assert_eq!(exec.reads().len(), 1);
    }

    #[tokio::test]
    async fn the_users_own_calendar_and_an_unnamed_owner_have_none() {
        let exec = Replay::new(vec![]);
        let owners = Owners::default();
        let me = "https://dav.example.test/principals/alice%40example.test/";
        // However the server spells the `@`.
        let mine = Some("https://dav.example.test/principals/alice@example.test");
        assert_eq!(
            owners.address_of(&exec, mine, Some(me)).await.unwrap(),
            None
        );
        assert_eq!(
            owners.address_of(&exec, None, Some(me)).await.unwrap(),
            None
        );
        assert!(exec.reads().is_empty());
    }

    #[tokio::test]
    async fn an_owner_the_credential_may_not_read_has_none() {
        let exec = Replay::new(vec![status(403, "")]);
        let owners = Owners::default();
        let bob = Some("https://dav.example.test/principals/bob/");
        assert_eq!(owners.address_of(&exec, bob, None).await.unwrap(), None);
    }
}
