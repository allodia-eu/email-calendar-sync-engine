//! The [`CalendarWrites`] half of the JMAP adapter: the four verbs, each one
//! `CalendarEvent/set` call.
//!
//! Beside `provider.rs` rather than in it, because that file is at the 500-line limit and
//! these four belong with the request builders they delegate to (`crate::calendar_write`,
//! `crate::calendar_rsvp`) rather than with the mail spine.

use async_trait::async_trait;
use engine_core::{calendar::Event, ids::AccountId};
use engine_provider::{CalendarWrites, ProviderResult};

use crate::JmapProvider;

#[async_trait]
impl CalendarWrites for JmapProvider {
    /// One `CalendarEvent/set` `create`. The **server** assigns the id, so the receipt is
    /// the only place the caller learns it (`crate::calendar_write`).
    async fn create_event(
        &self,
        _account: &AccountId,
        draft: &engine_provider::EventDraft,
    ) -> ProviderResult<engine_provider::EventWriteReceipt> {
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
        crate::session::JMAP_RSVP.accept(rsvp)?;
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
}
