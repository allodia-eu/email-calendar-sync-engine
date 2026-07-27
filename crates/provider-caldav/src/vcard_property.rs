//! vCard property-identity helpers.

use engine_core::contact::PropertyId;

use crate::error::CalDavError;

pub(super) fn property_id(
    parameters: &[&str],
    prefix: &str,
    index: usize,
) -> Result<PropertyId, CalDavError> {
    let value = parameters
        .iter()
        .filter_map(|parameter| parameter.split_once('='))
        .find(|(key, _)| key.eq_ignore_ascii_case("prop-id"))
        .map_or_else(
            || format!("{prefix}-{index}"),
            |(_, value)| value.to_owned(),
        );
    PropertyId::new(value).map_err(|error| CalDavError::protocol(error.to_string()))
}
