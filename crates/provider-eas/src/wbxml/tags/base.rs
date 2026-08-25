// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

/// `BodyPreference` (`AirSyncBase` page-17 token 0x05).
pub const BODY_PREFERENCE: u8 = 0x05;
/// `Type` (`AirSyncBase` page-17 token 0x06).
pub const TYPE: u8 = 0x06;
/// `TruncationSize` (`AirSyncBase` page-17 token 0x07).
pub const TRUNCATION_SIZE: u8 = 0x07;
/// `AllOrNone` (`AirSyncBase` page-17 token 0x08).
pub const ALL_OR_NONE: u8 = 0x08;
/// `Body` (`AirSyncBase` page-17 token 0x0a).
pub const BODY: u8 = 0x0A;
/// `Data` (`AirSyncBase` page-17 token 0x0b).
pub const DATA: u8 = 0x0B;
/// `EstimatedDataSize` (`AirSyncBase` page-17 token 0x0c).
pub const ESTIMATED_DATA_SIZE: u8 = 0x0C;
/// `Truncated` (`AirSyncBase` page-17 token 0x0d).
pub const TRUNCATED: u8 = 0x0D;
/// `Attachments` (`AirSyncBase` page-17 token 0x0e).
pub const ATTACHMENTS: u8 = 0x0E;
/// `Attachment` (`AirSyncBase` page-17 token 0x0f).
pub const ATTACHMENT: u8 = 0x0F;
/// `DisplayName` (`AirSyncBase` page-17 token 0x10).
pub const DISPLAY_NAME: u8 = 0x10;
/// `FileReference` (`AirSyncBase` page-17 token 0x11).
pub const FILE_REFERENCE: u8 = 0x11;
/// `Method` (`AirSyncBase` page-17 token 0x12).
pub const METHOD: u8 = 0x12;
/// `ContentId` (`AirSyncBase` page-17 token 0x13).
pub const CONTENT_ID: u8 = 0x13;
/// `ContentLocation` (`AirSyncBase` page-17 token 0x14).
pub const CONTENT_LOCATION: u8 = 0x14;
/// `IsInline` (`AirSyncBase` page-17 token 0x15).
pub const IS_INLINE: u8 = 0x15;
/// `NativeBodyType` (`AirSyncBase` page-17 token 0x16).
pub const NATIVE_BODY_TYPE: u8 = 0x16;
/// `ContentType` (`AirSyncBase` page-17 token 0x17).
pub const CONTENT_TYPE: u8 = 0x17;
/// `Preview` (`AirSyncBase` page-17 token 0x18).
pub const PREVIEW: u8 = 0x18;
/// `Location` = 0x20 (16.0/16.1 only; [MS-ASWBXML] §2.1.2.1.18 note —
/// with 16.0/16.1 `airsyncbase:Location` replaces `calendar:Location`
/// (4, 0x17)). CONTAINER type per [MS-ASAIRS] §2.2.2.28: the
/// human-readable value is the `DisplayName` child (0x10 above).
/// Registered here (M8-L1 variant) because both the Calendar
/// ApplicationData parse and the Email MeetingRequest parse read it —
/// see `calendar::parse_location_16x`.
pub const LOCATION: u8 = 0x20;
