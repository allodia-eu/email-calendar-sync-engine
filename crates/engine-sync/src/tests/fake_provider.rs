//! What [`FakeMail`](super::FakeMail) answers as a provider.
//!
//! Beside the fixtures rather than inside them because a trait impl is one block and
//! cannot be split, so the file holding both it and the fixtures splits here, along the
//! line between what the fake *is* and what it *answers*.

use super::*;

#[async_trait::async_trait]
impl Provider for FakeMail {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(self.caps)
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Mailbox,
        }
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        match &self.folder {
            Some(mailbox) => SyncScope::ImapMailbox {
                account: account.clone(),
                mailbox: mailbox.clone(),
            },
            None => SyncScope::JmapType {
                account: account.clone(),
                data_type: JmapDataType::Email,
            },
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        let present = self.mailboxes.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.mailboxes.clone(), present),
            self.cursor.clone(),
        ))
    }

    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        _window: SyncWindow,
        _fetch_batch: usize,
        _chunk_size: usize,
    ) -> EmailStream<'a> {
        if let Some(folder) = &self.folder {
            self.started.lock().unwrap().push(folder.clone());
        }
        // One chunk: a reconciling snapshot on first sync (so the drain tombstones),
        // an additive empty delta once a cursor exists.
        let chunk = if cursor.is_none() {
            let present: Vec<ProviderKey> =
                self.messages.iter().map(|m| m.id.key().clone()).collect();
            EmailChunk::reconcile_last(
                self.messages.clone(),
                present,
                Some(self.messages.len()),
                self.cursor.clone(),
            )
        } else {
            let changed = core::mem::take(&mut *self.delta.lock().expect("delta mutex poisoned"));
            let keywords = core::mem::take(
                &mut *self
                    .state_delta
                    .lock()
                    .expect("keyword delta mutex poisoned"),
            );
            EmailChunk::additive(changed, Vec::new(), None, self.cursor.clone())
                .with_patched(keywords)
        };
        Box::pin(futures_util::stream::iter(vec![Ok(chunk)]))
    }

    async fn put_draft(
        &self,
        _account: &AccountId,
        draft: &Draft,
        replacing: Option<&ProviderKey>,
    ) -> ProviderResult<ProviderKey> {
        {
            let mut left = self.failing_drafts.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return Err(ProviderError::retryable("no route to host"));
            }
        }
        let mut puts = self.draft_puts.lock().unwrap();
        let _ = replacing;
        puts.push(draft.message_id.as_str().to_owned());
        // A new key per save, so a test can see that the caller has to keep what comes
        // back rather than the key it passed in.
        ProviderKey::new(format!("draft-{}", puts.len()))
            .map_err(|e| ProviderError::permanent(e.to_string()))
    }

    async fn delete_draft(&self, _account: &AccountId, draft: &ProviderKey) -> ProviderResult<()> {
        {
            let mut left = self.failing_drafts.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return Err(ProviderError::retryable("no route to host"));
            }
        }
        self.draft_deletes.lock().unwrap().push(draft.clone());
        Ok(())
    }

    async fn submit_email(
        &self,
        _account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        if self.fails(Fault::AmbiguousSubmit) {
            Err(ProviderError::needs_confirmation(
                "post-DATA acknowledgement lost",
            ))
        } else if self.fails(Fault::PermanentSubmit) {
            Err(ProviderError::permanent("recipient rejected"))
        } else if self.fails(Fault::Submit) || self.send_is_out() {
            Err(ProviderError::rate_limited("slow down", None))
        } else if self.fails(Fault::UnfiledCopy) {
            Ok(SubmissionReceipt::unfiled(
                ProviderKey::new("sent-1").unwrap(),
                draft.message_id.clone(),
                "APPEND refused: over quota",
            ))
        } else {
            Ok(SubmissionReceipt::filed(
                ProviderKey::new("sent-1").unwrap(),
                draft.message_id.clone(),
            ))
        }
    }

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        if self.fails(Fault::CalendarFetch) {
            return Err(ProviderError::retryable("calendar list unreachable"));
        }
        let present = self.calendars.iter().map(|c| c.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.calendars.clone(), present),
            self.cursor.clone(),
        ))
    }

    async fn sync_events(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        let present = self.events.iter().map(|e| e.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.events.clone(), present),
            self.cursor.clone(),
        ))
    }

    async fn edit_mail(
        &self,
        _account: &AccountId,
        edit: &MailEdit,
    ) -> ProviderResult<MailEditReceipt> {
        if self.fails(Fault::WriteGuard) {
            // The IMAP analogue of a CalDAV 412: a stale UID under a changed
            // UIDVALIDITY (`imap-smtp.md`) — recompute after a re-sync.
            return Err(ProviderError::conflict("UIDVALIDITY changed"));
        }
        Ok(MailEditReceipt::new(edit.target().clone()))
    }

    async fn report_message(
        &self,
        _account: &AccountId,
        report: &MessageReport,
    ) -> ProviderResult<ReportReceipt> {
        if self.fails(Fault::Report) {
            return Err(ProviderError::rate_limited("slow down", None));
        }
        Ok(ReportReceipt::new(report.target.clone()))
    }
}
impl engine_provider::MailboxWrites for FakeMail {}

#[async_trait::async_trait]
impl CalendarWrites for FakeMail {
    async fn create_event(
        &self,
        _account: &AccountId,
        draft: &EventDraft,
    ) -> ProviderResult<EventWriteReceipt> {
        if self.fails(Fault::WriteGuard) {
            return Err(ProviderError::conflict("an event already exists there"));
        }
        // Mints the id the way a CalDAV adapter does — from the caller's UID. (A JMAP
        // adapter would return a server-assigned one; the driver cannot tell, which is
        // the point.)
        Ok(EventWriteReceipt::new(
            EventId::try_from(format!("/cal/{}.ics", draft.uid.as_str()).as_str()).unwrap(),
            draft.uid.clone(),
            RevisionTokens::from_etag(ETag::new("\"put-v1\"")),
        ))
    }

    async fn patch_event(
        &self,
        _account: &AccountId,
        _base: &Event,
        edit: &EventEdit,
    ) -> ProviderResult<EventWriteReceipt> {
        if self.fails(Fault::WriteGuard) {
            // A failed revision guard: a CalDAV `412`, or a JMAP `stateMismatch`.
            return Err(ProviderError::conflict("etag precondition failed"));
        }
        Ok(EventWriteReceipt::new(
            edit.event.clone(),
            edit.uid.clone(),
            RevisionTokens::from_etag(ETag::new("\"put-v1\"")),
        ))
    }

    async fn put_event(
        &self,
        _account: &AccountId,
        write: &EventWrite,
    ) -> ProviderResult<EventWriteReceipt> {
        if self.fails(Fault::WriteGuard) {
            return Err(ProviderError::conflict("etag precondition failed"));
        }
        Ok(EventWriteReceipt::new(
            write.event.clone(),
            write.uid.clone(),
            RevisionTokens::from_etag(ETag::new("\"put-v1\"")),
        ))
    }

    async fn rsvp_event(
        &self,
        _account: &AccountId,
        _base: &Event,
        rsvp: &EventRsvp,
    ) -> ProviderResult<EventWriteReceipt> {
        if self.fails(Fault::WriteGuard) {
            return Err(ProviderError::conflict("etag precondition failed"));
        }
        // Stands in for every adapter's refusal of a control it cannot honour — the
        // driver must record and surface it, not swallow it.
        if rsvp.comment.is_some() {
            return Err(ProviderError::invalid_state("no note on this transport"));
        }
        Ok(EventWriteReceipt::new(
            rsvp.event.clone(),
            rsvp.uid.clone(),
            RevisionTokens::from_etag(ETag::new("\"put-v1\"")),
        ))
    }

    async fn delete_event(
        &self,
        _account: &AccountId,
        _base: Option<&Event>,
        _deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        if self.fails(Fault::WriteGuard) {
            return Err(ProviderError::conflict("etag precondition failed"));
        }
        Ok(())
    }
}
