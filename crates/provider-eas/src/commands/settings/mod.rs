// SPDX-License-Identifier: MPL-2.0
//! Settings command marshalers ([MS-ASCMD] §2.2.1.18): DeviceInformation,
//! UserInformation, DevicePassword and OOF get/set, all on Settings page 18.

mod device_information;
mod device_password;
mod oof;
mod user_information;

pub use device_information::{
    build_settings_device_information_request, device_information_element, parse_settings_response,
};
pub use device_password::{
    build_settings_device_password_request, parse_settings_device_password_response,
};
pub use oof::{
    build_settings_oof_get_request, build_settings_oof_set_request,
    parse_settings_oof_get_response, parse_settings_oof_set_response,
};
pub use user_information::{
    build_settings_user_information_request, parse_settings_user_information_response,
};
