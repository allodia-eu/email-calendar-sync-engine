//! Whether an account belongs to a person or to an organization.
//!
//! A provider that serves both behind one sign-in (Microsoft Graph serves personal Microsoft
//! accounts and work or school ones; Google serves Gmail and Workspace) offers some surfaces to an
//! organization's accounts only: a directory of colleagues, a mailbox shared between members. A
//! host asks once and keeps the answer, because it decides what to offer before it reaches any of
//! those surfaces.

use serde::{Deserialize, Serialize};

use crate::ids::OrganizationId;

/// Who an account belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Affiliation {
    /// A person's own account, which no organization administers.
    Personal,
    /// An account the organization administers.
    Organization(OrganizationId),
}
