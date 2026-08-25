// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

/// `GAL` code-page index.
pub const PAGE: u8 = 16;
/// `DisplayName` (`GAL` page-16 token 0x05).
pub const DISPLAY_NAME: u8 = 0x05;
/// `Phone` (`GAL` page-16 token 0x06).
pub const PHONE: u8 = 0x06;
/// `Office` (`GAL` page-16 token 0x07).
pub const OFFICE: u8 = 0x07;
/// `Title` (`GAL` page-16 token 0x08).
pub const TITLE: u8 = 0x08;
/// `Company` (`GAL` page-16 token 0x09).
pub const COMPANY: u8 = 0x09;
/// `Alias` (`GAL` page-16 token 0x0a).
pub const ALIAS: u8 = 0x0A;
/// `FirstName` (`GAL` page-16 token 0x0b).
pub const FIRST_NAME: u8 = 0x0B;
/// `LastName` (`GAL` page-16 token 0x0c).
pub const LAST_NAME: u8 = 0x0C;
/// `HomePhone` (`GAL` page-16 token 0x0d).
pub const HOME_PHONE: u8 = 0x0D;
/// `MobilePhone` (`GAL` page-16 token 0x0e).
pub const MOBILE_PHONE: u8 = 0x0E;
/// `EmailAddress` (`GAL` page-16 token 0x0f).
pub const EMAIL_ADDRESS: u8 = 0x0F;
