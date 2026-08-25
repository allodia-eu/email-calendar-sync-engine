// SPDX-License-Identifier: MPL-2.0
//! Settings-command tests, split along the [MS-ASCMD] sections; shared
//! imports live here and are reached via `super::`.

use provider_eas::commands::{tests_common::*, *};

#[path = "commands_settings/device_information.rs"]
mod device_information;
#[path = "commands_settings/device_password.rs"]
mod device_password;
#[path = "commands_settings/oof_request.rs"]
mod oof_request;
#[path = "commands_settings/oof_response.rs"]
mod oof_response;
#[path = "commands_settings/user_information.rs"]
mod user_information;
