//! Creating an address book over CardDAV: an extended `MKCOL` (RFC 5689) on the address-book
//! home, then the new collection read back as the listing reads one. The calendar's shape
//! (`calendar_create`): a fresh path segment each time, every property in the one request (both
//! harness servers keep the name and the description), and the answer as the server holds it.

use async_trait::async_trait;
use engine_core::{contact::AddressBook, ids::AccountId};
use engine_provider::{AddressBookCollections, NewAddressBook, ProviderError, ProviderResult};

use crate::{
    CardDavProvider,
    calendar_create::fresh_segment,
    carddav_ops::address_book,
    request::{ADDRESS_BOOK_LIST_PROPFIND, mkcol_address_book},
    response::HttpResponse,
    transport::{DavMethod, Precondition, WriteRequest},
};

#[async_trait]
impl AddressBookCollections for CardDavProvider {
    async fn create_address_book(
        &self,
        _account: &AccountId,
        new: &NewAddressBook,
    ) -> ProviderResult<AddressBook> {
        let href = format!(
            "{}/{}/",
            self.home_href.trim_end_matches('/'),
            fresh_segment()
        );
        self.executor
            .send_write(WriteRequest {
                method: DavMethod::Mkcol,
                href: href.clone(),
                content_type: Some("application/xml; charset=utf-8"),
                precondition: Precondition::None,
                body: mkcol_address_book(&new.name, new.description.as_deref()),
            })
            .await
            .and_then(HttpResponse::into_write_etag)
            .map_err(|err| err.content_free("creating an address book"))?;
        // Created: a failure from here on must not read as "not created".
        let created = || {
            ProviderError::needs_confirmation(
                "creating an address book: it was created and could not be read back; find it \
                 in the address-book list",
            )
        };
        let listed = self
            .executor
            .send(
                DavMethod::Propfind,
                &href,
                "0",
                ADDRESS_BOOK_LIST_PROPFIND.to_owned(),
            )
            .await
            .and_then(HttpResponse::into_multistatus)
            .map_err(|_| created())?;
        listed
            .responses
            .iter()
            .find(|response| response.props.is_address_book())
            .and_then(|response| address_book(response).ok())
            .ok_or_else(created)
    }
}

#[cfg(test)]
#[path = "contact_create_tests.rs"]
mod tests;
