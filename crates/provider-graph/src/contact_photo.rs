//! Fetching a Graph contact or directory-user photo, and telling "there is none"
//! apart from "the fetch failed".

use engine_core::contact::{ContactCard, ContactResource};
use engine_provider::{ContactPhoto, ProviderResult};

use crate::{error::GraphError, transport::GraphClient};

/// The fixed photo size asked of the resources that offer sizes, in pixels.
///
/// One of Microsoft's documented sizes; must stay on that list, because Graph answers
/// an unlisted size with a **404**, the same status as "no image" (`ErrorInvalidImageId`
/// against `ImageNotFound` — the codes differ, the statuses do not). 240 serves both a
/// 3x phone list row and a reading header at roughly 15 KB, where the unsized resource
/// returns whatever was uploaded — routinely a megabyte for a directory user.
const PHOTO_SIZE: &str = "240x240";

/// Fetches the photo for the card served at `item_url`, or `None` when the source
/// holds no image for it.
///
/// `sized` says whether this item is a resource that offers the `photos/{size}`
/// collection. **Only `user` does.** A `contact` has the singular `photo` navigation
/// property and nothing else, and asking it for a size is not a missing photo but a
/// malformed URL: Graph answers `400 RequestBroker--ParseUri`, "Resource not found for
/// the segment 'photos'". That is a different status from every absence, so it can
/// never be mistaken for one — it simply fails the fetch outright.
pub(crate) async fn fetch(
    client: &GraphClient,
    item_url: &str,
    sized: bool,
    card: &ContactCard,
    media: &ContactResource,
) -> ProviderResult<Option<ContactPhoto>> {
    let bytes = if media.uri.is_empty() {
        photo_for(client, item_url, sized).await?
    } else {
        optional(client.get_bytes(&media.uri).await)?
    };
    Ok(bytes.map(|bytes| {
        ContactPhoto::new(
            bytes,
            media.media_type.clone(),
            media.fingerprint.clone().unwrap_or_else(|| {
                card.revisions
                    .change_key
                    .as_ref()
                    .map_or("photo", |key| key.as_str())
                    .into()
            }),
        )
    }))
}

/// Reads the item's own photo, preferring an avatar-sized rendering where the resource
/// offers one.
///
/// The sized read can still 404 while the unsized resource serves the image — a photo
/// Graph cannot resize — and 404 is also how "there is no photo" arrives, so the first
/// 404 is not yet an answer. Only a second one settles it.
async fn photo_for(
    client: &GraphClient,
    item_url: &str,
    sized: bool,
) -> Result<Option<Vec<u8>>, GraphError> {
    let original = format!("{item_url}/photo/$value");
    if !sized {
        return optional(client.get_bytes(&original).await);
    }
    match client
        .get_bytes(&format!("{item_url}/photos/{PHOTO_SIZE}/$value"))
        .await
    {
        Ok(bytes) => Ok(Some(bytes)),
        Err(GraphError::Status { status: 404, .. }) => optional(client.get_bytes(&original).await),
        Err(error) => Err(error),
    }
}

/// Maps Graph's "there is no photo here" 404 onto an absent photo, leaving every
/// other failure an error. Personal contacts answer `ErrorItemNotFound` and users
/// answer `ImageNotFound`; both mean the same thing to a caller.
fn optional(result: Result<Vec<u8>, GraphError>) -> Result<Option<Vec<u8>>, GraphError> {
    match result {
        Ok(bytes) => Ok(Some(bytes)),
        Err(GraphError::Status { status: 404, .. }) => Ok(None),
        Err(error) => Err(error),
    }
}
