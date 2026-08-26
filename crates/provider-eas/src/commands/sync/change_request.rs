// SPDX-License-Identifier: MPL-2.0
// Sync Change request building (email + calendar upsync).

use super::change::{
    CalendarChange, EMAIL_FLAG_STATUS, EMAIL_FLAG_TYPE, EasChange, FLAG_DUE_OFFSET_SECS,
    PAGE_TASKS, TASK_DUE_DATE, TASK_START_DATE, TASK_UTC_DUE_DATE, TASK_UTC_START_DATE,
};
use crate::{
    calendar_write::build_calendar_application_data,
    commands::{
        AS_ADD, AS_APPLICATION_DATA, AS_CHANGE, AS_CLIENT_ID, AS_COLLECTION, AS_COLLECTION_ID,
        AS_COLLECTIONS, AS_COMMANDS, AS_DELETE, AS_SERVER_ID, AS_SYNC, AS_SYNC_KEY, PAGE_AIRSYNC,
        WbxmlElement, format_eas_datetime_utc, tags,
    },
};
/// Build a Sync request carrying client-side `Commands > Change` elements
/// (the upsync direction of [MS-ASSYNC] §2.2.2).
///
/// WBXML shape:
/// ```xml
/// <Sync>
///   <Collections>
///     <Collection>
///       <SyncKey>{sync_key}</SyncKey>
///       <CollectionId>{collection_id}</CollectionId>
///       <Commands>
///         <Change>
///           <ServerId>{server_id}</ServerId>
///           <ApplicationData>
///             <email:Read>1</email:Read>   <!-- only when change.read is Some -->
///             <email:Flag>…</email:Flag>   <!-- only when change.starred is Some -->
///           </ApplicationData>
///         </Change>
///         …
///       </Commands>
///     </Collection>
///   </Collections>
/// </Sync>
/// ```
///
/// Same element gates as `build_sync_request`: NO `airsync:Class` (14.0+
/// rejects it — CollectionId identifies the collection) and NO `GetChanges`
/// (invalid in 16.1). `ApplicationData` is always emitted (schema-required
/// for a client Change). This wrapper stamps Flag dates from the wall clock;
/// tests use `build_sync_change_request_at` to pin the instant.
pub fn build_sync_change_request(
    collection_id: &str,
    sync_key: &str,
    changes: &[EasChange],
) -> WbxmlElement {
    build_sync_change_request_at(
        collection_id,
        sync_key,
        changes,
        std::time::SystemTime::now(),
    )
}

/// `build_sync_change_request` with an injectable clock for the Flag dates.
///
/// Flag emission (Android EasSync.java:295-315):
/// - `starred: Some(true)` → full container: `email:Flag` > `email:Status "2"` + `email:FlagType
///   "FollowUp"` + `tasks:StartDate`/`tasks:UtcStartDate` = now UTC +
///   `tasks:DueDate`/`tasks:UtcDueDate` = now + 7 days UTC (dates ISO-8601
///   `yyyy-MM-dd'T'HH:mm:ss.fff'Z'`). The tasks-page date elements switch the code page email(2) →
///   tasks(9) mid-container.
/// - `starred: Some(false)` → an empty `<email:Flag/>` element (no children).
/// - `starred: None` → no Flag element.
pub fn build_sync_change_request_at(
    collection_id: &str,
    sync_key: &str,
    changes: &[EasChange],
    now: std::time::SystemTime,
) -> WbxmlElement {
    let change_elements: Vec<WbxmlElement> = changes
        .iter()
        .map(|change| {
            let mut app_data_children = Vec::new();
            if let Some(read) = change.read {
                app_data_children.push(WbxmlElement::text(
                    tags::email::PAGE,
                    tags::email::READ,
                    if read { "1" } else { "0" },
                ));
            }
            if let Some(starred) = change.starred {
                if starred {
                    let start = format_eas_datetime_utc(now);
                    let due = format_eas_datetime_utc(
                        now + std::time::Duration::from_secs(FLAG_DUE_OFFSET_SECS),
                    );
                    app_data_children.push(WbxmlElement::container(
                        tags::email::PAGE,
                        tags::email::FLAG,
                        vec![
                            WbxmlElement::text(tags::email::PAGE, EMAIL_FLAG_STATUS, "2"),
                            WbxmlElement::text(tags::email::PAGE, EMAIL_FLAG_TYPE, "FollowUp"),
                            WbxmlElement::text(PAGE_TASKS, TASK_START_DATE, start.clone()),
                            WbxmlElement::text(PAGE_TASKS, TASK_UTC_START_DATE, start),
                            WbxmlElement::text(PAGE_TASKS, TASK_DUE_DATE, due.clone()),
                            WbxmlElement::text(PAGE_TASKS, TASK_UTC_DUE_DATE, due),
                        ],
                    ));
                } else {
                    // Clearing a flag is an empty <Flag/> element (Android's
                    // `s.tag(Tags.EMAIL_FLAG)`) — no children, no dates.
                    app_data_children
                        .push(WbxmlElement::empty(tags::email::PAGE, tags::email::FLAG));
                }
            }
            WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_CHANGE,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, change.server_id.clone()),
                    WbxmlElement::container(PAGE_AIRSYNC, AS_APPLICATION_DATA, app_data_children),
                ],
            )
        })
        .collect();

    let collection = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COLLECTION,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, sync_key),
            WbxmlElement::text(PAGE_AIRSYNC, AS_COLLECTION_ID, collection_id),
            WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, change_elements),
        ],
    );

    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![collection],
        )],
    )
}

/// Build a Sync request carrying client-side Calendar `Commands` (the
/// upsync direction of [MS-ASSYNC] §2.2.2) — the Calendar twin of
/// [`build_sync_change_request`].
///
/// WBXML shape (see [`CalendarChange`] for the OUR-vocabulary → wire
/// mapping):
/// ```xml
/// <Sync>
///   <Collections>
///     <Collection>
///       <SyncKey>{sync_key}</SyncKey>
///       <CollectionId>{collection_id}</CollectionId>
///       <Commands>
///         <Add>                                    <!-- CalendarChange::Add -->
///           <ClientId>{client_id}</ClientId>
///           <ApplicationData>calendar:Timezone, … (M8 Task 1)</ApplicationData>
///         </Add>
///         <Change>                                 <!-- CalendarChange::Replace -->
///           <ServerId>{server_id}</ServerId>
///           <ApplicationData>…</ApplicationData>
///         </Change>
///         <Delete>                                 <!-- CalendarChange::Remove -->
///           <ServerId>{server_id}</ServerId>
///         </Delete>
///       </Commands>
///     </Collection>
///   </Collections>
/// </Sync>
/// ```
///
/// - `ApplicationData` is
///   [`build_calendar_application_data`](crate::calendar_write::build_calendar_application_data)'s
///   output VERBATIM — this builder adds no calendar properties.
/// - Same element gates as the email builder: NO `airsync:Class` (14.0+ rejects it — CollectionId
///   identifies the collection) and NO `GetChanges` (invalid in 16.1).
/// - Infallible like the email precedent: callers run
///   [`CalendarEventWrite::validate`](crate::calendar_write::CalendarEventWrite::validate) first,
///   and supply the Add `client_id` themselves (synthesize with
///   [`new_calendar_client_id`](crate::types::new_calendar_client_id), which guarantees the
///   [MS-ASCMD] 40-char cap) — the builder never synthesizes or clamps ids.
pub fn build_calendar_change_request(
    collection_id: &str,
    sync_key: &str,
    changes: &[CalendarChange],
    protocol_version: &str,
) -> WbxmlElement {
    let command_elements: Vec<WbxmlElement> = changes
        .iter()
        .map(|change| match change {
            // Add: ClientId + ApplicationData. The added item has no
            // ServerId yet — the server correlates its response (and the
            // new ServerId) through the ClientId.
            CalendarChange::Add { client_id, props } => WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_ADD,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_CLIENT_ID, client_id.clone()),
                    build_calendar_application_data(props, protocol_version),
                ],
            ),
            // Replace → wire Change ([MS-ASSYNC] §2.2.2): ServerId +
            // ApplicationData, the same envelope shape the email builder
            // emits for its Change commands.
            CalendarChange::Replace { server_id, props } => WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_CHANGE,
                vec![
                    WbxmlElement::text(PAGE_AIRSYNC, AS_SERVER_ID, server_id.clone()),
                    build_calendar_application_data(props, protocol_version),
                ],
            ),
            // Remove → wire Delete ([MS-ASSYNC] §2.2.2.4): a CONTAINER whose
            // ServerId is a child element ([MS-ASCMD] §2.2.3.42.2), with no
            // ApplicationData.
            CalendarChange::Remove { server_id } => WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_DELETE,
                vec![WbxmlElement::text(
                    PAGE_AIRSYNC,
                    AS_SERVER_ID,
                    server_id.clone(),
                )],
            ),
        })
        .collect();

    let collection = WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_COLLECTION,
        vec![
            WbxmlElement::text(PAGE_AIRSYNC, AS_SYNC_KEY, sync_key),
            WbxmlElement::text(PAGE_AIRSYNC, AS_COLLECTION_ID, collection_id),
            WbxmlElement::container(PAGE_AIRSYNC, AS_COMMANDS, command_elements),
        ],
    );

    WbxmlElement::container(
        PAGE_AIRSYNC,
        AS_SYNC,
        vec![WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_COLLECTIONS,
            vec![collection],
        )],
    )
}
