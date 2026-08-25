// SPDX-License-Identifier: MPL-2.0
//! ItemOperations-command tests, split along the [MS-ASCMD] operations;
//! shared imports live here and are reached via `super::`.

use provider_eas::commands::{tests_common::*, *};

#[path = "commands_item_operations/attachment.rs"]
mod attachment;
#[path = "commands_item_operations/conversation_move.rs"]
mod conversation_move;
#[path = "commands_item_operations/empty_folder_contents.rs"]
mod empty_folder_contents;
#[path = "commands_item_operations/fetch_requests.rs"]
mod fetch_requests;
#[path = "commands_item_operations/fetch_responses.rs"]
mod fetch_responses;
