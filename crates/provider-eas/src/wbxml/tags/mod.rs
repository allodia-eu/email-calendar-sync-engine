// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.
//
// Tag constants and helper functions. Each constant packs a `(page, token)`
// pair into a `u16` — page in the high 8 bits, token in the low 8 bits —
// because that is what the ArkTS `Tags` class produces via `page << 6 | tag`.
// Callers can use these constants or pass `(page, token)` tuples directly to
// the serializer; either form is accepted via the `Into<Tag>` impl.
//
// Only the most commonly-used tags are enumerated here. The full table lives
// in `code_pages`; for ad-hoc tags, construct `WbxmlElement::empty(page, token)`
// directly.
//
// Visibility ruling: the tag constants stay `pub` — they are the crate's
// [MS-ASWBXML] protocol reference surface, and every one is live (reached by
// a builder, parser, or test), so none are private-and-dead.
/// Code page indices (0..=25). Source: `Tags` constants in `tags.ts`.
pub mod pages;

/// A few of the most-used AirSync (page 0) tag ids. Other pages are available
/// via the `pages` module and the `code_pages::code_page()` lookup.
pub mod airsync;

/// FolderHierarchy (page 7) tag ids.
pub mod folder;

/// Ping (page 13) tag ids.
pub mod ping;

/// Provision (page 14) tag ids.
pub mod provision;

/// ResolveRecipients (page 10) tag ids. Source: [MS-ASWBXML] §2.1.2.1.11,
/// verified against `RECIPIENTS_TOKENS` in `code_pages/pages_10_17.rs`.
pub mod recipients;

/// ValidateCert (page 11) tag ids. Source: [MS-ASWBXML] §2.1.2.1.12,
/// verified against `VALIDATE_TOKENS` in `code_pages/pages_10_17.rs`.
pub mod validatecert;

/// Settings (page 18) tag ids.
pub mod settings;

/// ItemOperations (page 20) tag ids.
pub mod item_operations;

/// ComposeMail (page 21) tag ids.
pub mod compose;

/// Search (page 15) tag ids.
pub mod search;

/// GAL (page 16) tag ids.
pub mod gal;

/// AirSyncBase (page 17) tag ids.
pub mod base;

/// Email (page 2) tag ids. Source: [MS-ASEMAIL] 2.2.2.
/// Used by the Sync-response parser to extract well-known email fields
/// out of `ApplicationData`.
pub mod email;

/// Email2 (page 22) tag ids. Source: [MS-ASEMAIL] 2.2.3.
/// Conversations / drafts / BCC live here because they postdate the
/// original Email code page.
pub mod email2;
