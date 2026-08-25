// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

/// `Email` code-page index.
pub const PAGE: u8 = 2;
/// `DateReceived` (`Email` page-2 token 0x0f).
pub const DATE_RECEIVED: u8 = 0x0F;
/// `Subject` (`Email` page-2 token 0x14).
pub const SUBJECT: u8 = 0x14;
/// `Read` (`Email` page-2 token 0x15).
pub const READ: u8 = 0x15;
/// `To` (`Email` page-2 token 0x16).
pub const TO: u8 = 0x16;
/// `Cc` (`Email` page-2 token 0x17).
pub const CC: u8 = 0x17;
/// `From` (`Email` page-2 token 0x18).
pub const FROM: u8 = 0x18;
/// `ReplyTo` (`Email` page-2 token 0x19).
pub const REPLY_TO: u8 = 0x19;
/// `Importance` (`Email` page-2 token 0x12).
pub const IMPORTANCE: u8 = 0x12;
/// `Flag` (`Email` page-2 token 0x3a).
pub const FLAG: u8 = 0x3A;
// ---- Task 4: meeting-request tokens ([MS-ASEMAIL] §2.2.2) ----
/// Outlook/Exchange message class (`IPM.Note`,
/// `IPM.Schedule.Meeting.Request`, …). Drives the reading pane's
/// meeting banner.
pub const MESSAGE_CLASS: u8 = 0x13;
/// MeetingRequest child: `"1"`/`"0"` boolean.
pub const ALL_DAY_EVENT: u8 = 0x1A;
/// MeetingRequest child (xs:dateTime).
pub const END_TIME: u8 = 0x1E;
/// MeetingRequest child: 0=single, 1=master recurring, 2=exception
/// instance, 3=exception master ([MS-ASEMAIL] §2.2.2.36).
pub const INSTANCE_TYPE: u8 = 0x1F;
/// MeetingRequest child.
pub const LOCATION: u8 = 0x21;
/// Container for the meeting logistics children above
/// ([MS-ASEMAIL] §2.2.2.48).
pub const MEETING_REQUEST: u8 = 0x22;
/// MeetingRequest child (organizer's SMTP address).
pub const ORGANIZER: u8 = 0x23;
/// MeetingRequest child: `"1"`/`"0"` boolean.
pub const RESPONSE_REQUESTED: u8 = 0x26;
/// MeetingRequest child (xs:dateTime).
pub const START_TIME: u8 = 0x31;
/// MeetingRequest child (≤14.1): base64 GlobalObjectId — converted to
/// the calendar UID string per [MS-ASEMAIL] §3.1.4.7 before joining
/// against a calendar item ([MS-ASWBXML] §2.1.2.1.4 note 4: at 16.0/16.1
/// the calendar-page UID replaces this element, same value space).
pub const GLOBAL_OBJ_ID: u8 = 0x34;
