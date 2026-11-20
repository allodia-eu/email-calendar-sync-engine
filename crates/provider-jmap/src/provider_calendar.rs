//! The calendar halves of the JMAP adapter: [`CalendarSync`], fetching the calendars and
//! their events, and [`CalendarWrites`], the four verbs, each one `CalendarEvent/set` call,
//! and the addresses those writes schedule as.
//!
//! Beside `provider.rs` rather than in it, because these belong with the request builders
//! they delegate to (`crate::fetch_scope`, `crate::calendar_write`, `crate::calendar_rsvp`)
//! rather than with the mail spine.

use async_trait::async_trait;
use engine_core::{
    calendar::Event,
    ids::{AccountId, CalendarId},
    sync::SyncState,
    time::Horizon,
};
use engine_provider::{
    CalendarListing, CalendarSync, CalendarUserAddresses, CalendarWrites, ProviderResult, ScopeSync,
};

use crate::{
    JmapProvider,
    calendar::{calendar_from_json, event_from_json},
    fetch::MemberFetch,
    fetch_scope,
    request::capability,
};

#[async_trait]
impl CalendarSync for JmapProvider {
    async fn sync_calendars(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<CalendarListing> {
        let account = self.calendar_account()?;
        Ok(fetch_scope::container_sync(
            self.executor.as_ref(),
            &account,
            &[capability::CORE, capability::CALENDARS],
            "Calendar",
            cursor,
            calendar_from_json,
        )
        .await?
        .into())
    }

    /// Every calendar's events under the account-wide `CalendarEvent` state, whichever asks.
    async fn sync_events(
        &self,
        _account: &AccountId,
        _calendar: &CalendarId,
        cursor: Option<&SyncState>,
        _window: Horizon,
    ) -> ProviderResult<ScopeSync<Event>> {
        let account = self.calendar_account()?;
        let fetch = MemberFetch {
            executor: self.executor.as_ref(),
            account: &account,
            using: &[capability::CORE, capability::CALENDARS],
            type_name: "CalendarEvent",
            properties: None,
        };
        Ok(fetch_scope::member_sync(&fetch, cursor, event_from_json).await?)
    }
}

#[async_trait]
impl CalendarWrites for JmapProvider {
    /// One `CalendarEvent/set` `create`. The **server** assigns the id, so the receipt is
    /// the only place the caller learns it (`crate::calendar_write`).
    async fn create_event(
        &self,
        _account: &AccountId,
        draft: &engine_provider::EventDraft,
    ) -> ProviderResult<engine_provider::EventWriteReceipt> {
        engine_provider::refuse_native_conference(draft.conference.as_ref())?;
        let account = self.calendar_account()?;
        Ok(crate::calendar_write::create_event(self.executor.as_ref(), &account, draft).await?)
    }

    /// One `CalendarEvent/set` `update`, whose PatchObject the **server** merges — so there
    /// is no document surgery on this transport, and no JSCalendar serializer to keep in
    /// step with the parser (`crate::calendar_write`).
    async fn patch_event(
        &self,
        _account: &AccountId,
        base: &Event,
        edit: &engine_provider::EventEdit,
    ) -> ProviderResult<engine_provider::EventWriteReceipt> {
        edit.refuse_conference_on_occurrence()?;
        engine_provider::refuse_native_conference(
            edit.patch
                .conference_edit()
                .and_then(engine_provider::ConferenceEdit::adds),
        )?;
        let account = self.calendar_account()?;
        Ok(
            crate::calendar_write::patch_event(self.executor.as_ref(), &account, base, edit)
                .await?,
        )
    }

    /// One `CalendarEvent/set` `update` of *my* participant's `participationStatus`, which
    /// is what makes the server schedule the iTIP `REPLY` (`crate::calendar_write`).
    async fn rsvp_event(
        &self,
        _account: &AccountId,
        base: &Event,
        rsvp: &engine_provider::EventRsvp,
    ) -> ProviderResult<engine_provider::EventWriteReceipt> {
        crate::calendar_rsvp::JMAP_RSVP.accept(rsvp)?;
        let account = self.calendar_account()?;
        Ok(crate::calendar_rsvp::rsvp_event(self.executor.as_ref(), &account, base, rsvp).await?)
    }

    /// One `CalendarEvent/set` `destroy`, or — for one occurrence — an `update` marking it
    /// excluded. An already-gone event is a success (`crate::calendar_write`).
    async fn delete_event(
        &self,
        _account: &AccountId,
        base: Option<&Event>,
        deletion: &engine_provider::EventDeletion,
    ) -> ProviderResult<()> {
        let (executor, account) = (self.executor.as_ref(), self.calendar_account()?);
        Ok(crate::calendar_write::delete_event(executor, &account, base, deletion).await?)
    }

    /// One `ParticipantIdentity/get` on the calendars account (`crate::participant_identity`),
    /// narrowed to the owner's identities when the client is bound to a shared account.
    async fn calendar_user_addresses(
        &self,
        _account: &AccountId,
    ) -> ProviderResult<CalendarUserAddresses> {
        let (executor, account) = (self.executor.as_ref(), self.calendar_account()?);
        let answer =
            crate::participant_identity::calendar_user_addresses(executor, &account).await?;
        Ok(match executor.session().bound_account() {
            None => answer,
            Some(store) => crate::participant_identity::for_store(answer, store),
        })
    }
}

#[cfg(test)]
mod tests {
    use engine_provider::{CalendarUserAddresses, CalendarWrites};
    use serde_json::json;

    use crate::provider::provider_test_support::{account, fixture, recording};

    #[tokio::test]
    async fn the_participant_identities_are_the_calendar_user_addresses() {
        let (provider, exec) = recording(vec![fixture("participant_identity_get_response.json")]);
        assert_eq!(
            provider.calendar_user_addresses(&account()).await.unwrap(),
            CalendarUserAddresses::Known(vec!["alice@test.local".to_owned()])
        );
        let (using, method, arguments) = exec.sole_call();
        assert_eq!(method, "ParticipantIdentity/get");
        assert!(using.contains(&"urn:ietf:params:jmap:calendars".to_owned()));
        assert_eq!(arguments["accountId"], "c");
    }

    #[tokio::test]
    async fn a_server_without_the_method_has_no_answer_rather_than_an_error() {
        let (provider, _) = recording(vec![json!({
            "methodResponses": [["error", { "type": "unknownMethod" }, "0"]]
        })]);
        assert_eq!(
            provider.calendar_user_addresses(&account()).await.unwrap(),
            CalendarUserAddresses::Unknown
        );
    }
}
