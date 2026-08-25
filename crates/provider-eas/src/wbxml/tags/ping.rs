// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

/// `Ping` (`Ping` page-13 token 0x05).
pub const PING: u8 = 0x05;
/// `Status` (`Ping` page-13 token 0x07).
pub const STATUS: u8 = 0x07;
/// `HeartbeatInterval` (`Ping` page-13 token 0x08).
pub const HEARTBEAT_INTERVAL: u8 = 0x08;
/// `Folders` (`Ping` page-13 token 0x09).
pub const FOLDERS: u8 = 0x09;
/// `Folder` (`Ping` page-13 token 0x0a).
pub const FOLDER: u8 = 0x0A;
/// `Id` (`Ping` page-13 token 0x0b).
pub const ID: u8 = 0x0B;
/// `Class` (`Ping` page-13 token 0x0c).
pub const CLASS: u8 = 0x0C;
/// `MaxFolders` (`Ping` page-13 token 0x0d).
pub const MAX_FOLDERS: u8 = 0x0D;
