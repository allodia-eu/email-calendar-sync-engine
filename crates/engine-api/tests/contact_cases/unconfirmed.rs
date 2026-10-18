//! A contact write that failed reaches the host saying whether it may have been applied, and
//! the provider's own answer stays reachable through the error's sources.

use std::error::Error;

use super::*;

/// What a host does to find the provider's answer under an engine error.
fn provider_error_of<'a>(error: &'a (dyn Error + 'static)) -> Option<&'a ProviderError> {
    let mut next = Some(error);
    while let Some(current) = next {
        if let Some(found) = current.downcast_ref::<ProviderError>() {
            return Some(found);
        }
        next = current.source();
    }
    None
}

#[tokio::test]
async fn a_failed_contact_write_says_whether_it_may_have_been_applied() {
    let cases: [(fn() -> ProviderError, bool); 4] = [
        (
            || ProviderError::needs_confirmation("the reply never came"),
            true,
        ),
        (|| ProviderError::retryable("connection refused"), false),
        (|| ProviderError::permanent("403 Forbidden"), false),
        (|| ProviderError::conflict("412 Precondition Failed"), false),
    ];
    for (fail, applied) in cases {
        let engine = Engine::open_in_memory().unwrap();
        let account = AccountId::try_from("account-1").unwrap();
        let provider = FakeContacts {
            fail_writes: Some(fail),
            ..FakeContacts::default()
        };
        let base = FakeContacts::card("c1", "Ada", "ada@example.test");
        let draft = ContactDraft {
            address_book: AddressBookId::try_from("book").unwrap(),
            card: FakeContacts::card("new", "Grace", "grace@example.test"),
        };
        let mut patch = ContactPatch::default();
        patch.fields.insert(
            ContactField::Name,
            FieldPatch::Set(serde_json::json!({"full": "Ada L."})),
        );
        let errors = [
            engine
                .create_contact(&provider, &account, "create", &draft)
                .await
                .err(),
            engine
                .patch_contact(&provider, &account, "patch", &base, &patch)
                .await
                .err(),
            engine
                .delete_contact(&provider, &account, "delete", &base)
                .await
                .err(),
        ];
        for error in errors {
            let error = error.expect("the write failed");
            assert_eq!(error.may_have_applied(), applied, "{error}");
            let answer = provider_error_of(&error).expect("the provider's answer is a source");
            assert_eq!(answer.requires_confirmation(), applied, "{answer}");
        }
    }
}
