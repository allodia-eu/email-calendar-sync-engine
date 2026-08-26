// SPDX-License-Identifier: MPL-2.0
//! ItemOperations command marshalers ([MS-ASCMD] §2.2.1.11): Fetch,
//! EmptyFolderContents and conversation Move, all on page 20.

mod empty_folder;
mod fetch;
mod r#move;

pub use empty_folder::{build_empty_folder_contents_request, parse_empty_folder_contents_response};
pub use fetch::{build_item_operations_request, parse_item_operations_response};
pub use r#move::{build_conversation_move_request, parse_conversation_move_response};
