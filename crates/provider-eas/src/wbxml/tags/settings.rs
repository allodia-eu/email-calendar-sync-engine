// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

/// `Settings` (`Settings` page-18 token 0x05).
pub const SETTINGS: u8 = 0x05;
/// `Status` (`Settings` page-18 token 0x06).
pub const STATUS: u8 = 0x06;
/// `Get` (`Settings` page-18 token 0x07).
pub const GET: u8 = 0x07;
/// `Set` (`Settings` page-18 token 0x08).
pub const SET: u8 = 0x08;
/// `Oof` (`Settings` page-18 token 0x09).
pub const OOF: u8 = 0x09;
/// Oof child: 0 = disabled, 1 = global, 2 = time-based ([MS-ASCMD]
/// §2.2.3.124; MUST be 2 when StartTime/EndTime are present).
pub const OOF_STATE: u8 = 0x0A;
/// `StartTime` (`Settings` page-18 token 0x0b).
pub const START_TIME: u8 = 0x0B;
/// `EndTime` (`Settings` page-18 token 0x0c).
pub const END_TIME: u8 = 0x0C;
/// `OofMessage` (`Settings` page-18 token 0x0d).
pub const OOF_MESSAGE: u8 = 0x0D;
/// OofMessage audience marker — empty element ([MS-ASCMD] §2.2.3.14).
pub const APPLIES_TO_INTERNAL: u8 = 0x0E;
/// OofMessage audience marker — empty element ([MS-ASCMD] §2.2.3.12).
pub const APPLIES_TO_EXTERNAL_KNOWN: u8 = 0x0F;
/// OofMessage audience marker — empty element ([MS-ASCMD] §2.2.3.13).
pub const APPLIES_TO_EXTERNAL_UNKNOWN: u8 = 0x10;
/// OofMessage child: "1"/"0" string ([MS-ASCMD] §2.2.3.59).
pub const ENABLED: u8 = 0x11;
/// `ReplyMessage` (`Settings` page-18 token 0x12).
pub const REPLY_MESSAGE: u8 = 0x12;
/// `BodyType` (`Settings` page-18 token 0x13).
pub const BODY_TYPE: u8 = 0x13;
/// `DevicePassword` (`Settings` page-18 token 0x14).
pub const DEVICE_PASSWORD: u8 = 0x14;
/// `Password` (`Settings` page-18 token 0x15).
pub const PASSWORD: u8 = 0x15;
/// `DeviceInformation` (`Settings` page-18 token 0x16).
pub const DEVICE_INFORMATION: u8 = 0x16;
/// `Model` (`Settings` page-18 token 0x17).
pub const MODEL: u8 = 0x17;
/// `IMEI` (`Settings` page-18 token 0x18).
pub const IMEI: u8 = 0x18;
/// `FriendlyName` (`Settings` page-18 token 0x19).
pub const FRIENDLY_NAME: u8 = 0x19;
/// `OS` (`Settings` page-18 token 0x1a).
pub const OS: u8 = 0x1A;
/// `OSLanguage` (`Settings` page-18 token 0x1b).
pub const OS_LANGUAGE: u8 = 0x1B;
/// `PhoneNumber` (`Settings` page-18 token 0x1c).
pub const PHONE_NUMBER: u8 = 0x1C;
/// `UserInformation` (`Settings` page-18 token 0x1d).
pub const USER_INFORMATION: u8 = 0x1D;
/// `EmailAddresses` (`Settings` page-18 token 0x1e).
pub const EMAIL_ADDRESSES: u8 = 0x1E;
/// `SMTPAddress` (`Settings` page-18 token 0x1f).
pub const SMTP_ADDRESS: u8 = 0x1F;
