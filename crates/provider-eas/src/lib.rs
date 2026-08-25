// SPDX-License-Identifier: MPL-2.0
//! Exchange ActiveSync (EAS) protocol client.
//!
//! Standalone crate: no kylins dependencies. Modules land here slice by
//! slice (see docs/superpowers/plans/2026-08-12-m0-m3-provider-eas-crate.md).

/// Auth payloads (Basic header vs OAuth bearer) the transport sends.
pub mod auth;
/// Autodiscover ([MS-ASAUTOD]): resolve a user's EAS endpoint URL.
pub mod autodiscover;
/// Calendar-class ApplicationData parsing and building ([MS-ASCAL]).
pub mod calendar;
/// Calendar-class upsync (Sync Add/Change request building).
pub mod calendar_write;
/// HTTP transport: the `EasClient` command executor with retry/redirect.
pub mod client;
/// WBXML marshalers for the EAS commands (build request / parse response).
pub mod commands;
/// Contacts-class ApplicationData parsing and building ([MS-ASCONTACTS]).
pub mod contacts;
#[cfg(test)]
pub(crate) mod contacts_testutil;
/// GlobalObjectId ↔ calendar-UID conversion ([MS-ASEMAIL] §3.1.4.7).
pub mod meeting_uid;
/// Decoder for `application/vnd.ms-sync.multipart` responses.
pub mod multipart;
/// Provision command ([MS-ASPROV]) policy handshake.
pub mod provision;
/// Shared EAS status-code → message mapping.
pub mod status;
/// Request/response payload types shared by the commands and the host.
pub mod types;
/// The MS-ASWBXML codec (serializer, deserializer, code pages, tags).
pub mod wbxml;
