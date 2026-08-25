// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

/// `Email2` code-page index.
pub const PAGE: u8 = 22;
/// `ConversationId` (`Email2` page-22 token 0x09).
pub const CONVERSATION_ID: u8 = 0x09;
/// `IsDraft` (`Email2` page-22 token 0x15).
pub const IS_DRAFT: u8 = 0x15;
/// `Bcc` (`Email2` page-22 token 0x16).
pub const BCC: u8 = 0x16;
/// [MS-ASEMAIL] §2.2.2.47 (v20220429): 0=silent update/unspecified,
/// 1=initial meeting request, 2=full update, 3=informational update,
/// 4=outdated, 5=delegator's copy. [MS-ASCMD] §3.1.5.6: only 1|2 (the
/// initial request + the full update) arm the Accept/Tentative/Decline
/// response UI. (An earlier comment here carried a value-off-by-one
/// mapping; corrected against the spec table 2026-08-18 — the 1|2 gate
/// itself was already right.)
pub const MEETING_MESSAGE_TYPE: u8 = 0x13;
