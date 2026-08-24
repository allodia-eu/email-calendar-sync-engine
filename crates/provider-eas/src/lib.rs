// SPDX-License-Identifier: MPL-2.0
//! Exchange ActiveSync (EAS) protocol client.
//!
//! Standalone crate: no kylins dependencies. Modules land here slice by
//! slice (see docs/superpowers/plans/2026-08-12-m0-m3-provider-eas-crate.md).

pub mod auth;
pub mod autodiscover;
pub mod calendar;
pub mod calendar_write;
pub mod client;
pub mod commands;
pub mod contacts;
#[cfg(test)]
pub(crate) mod contacts_testutil;
pub mod meeting_uid;
pub mod multipart;
pub mod provision;
pub mod status;
pub mod types;
pub mod wbxml;
