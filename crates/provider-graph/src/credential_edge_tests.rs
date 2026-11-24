//! What a client does with a reply the credential funnel leaves alone, a redirect, and with a
//! base it cannot use.

use std::sync::Arc;

use engine_http::test_server::scripted;
use engine_provider::CredentialSource;

use super::{Tokens, client, reply};
use crate::{
    GraphClient, GraphError,
    test_support::{retry, tls, user_agent},
};

#[tokio::test]
async fn an_authenticated_redirect_comes_back_as_it_came() {
    let (base, seen) = scripted(vec![
        "HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_owned(),
        reply("200 OK", "{}"),
    ]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client(&base, &source);

    let err = client.get(&client.url("/mailFolders")).await.unwrap_err();

    assert!(
        matches!(err, GraphError::Status { status: 302, .. }),
        "{err}"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "the redirect was not followed"
    );
    assert_eq!(source.replaced(), 0);
}

#[test]
fn a_base_that_is_not_an_absolute_url_is_refused() {
    for base in ["not a url", "/v1.0"] {
        let fixed = GraphClient::with_base("tok", base, tls(), &retry(), &user_agent());
        assert!(matches!(fixed, Err(GraphError::Protocol(_))), "{base}");
        let source = Tokens::rotating("tok-a", &[]);
        let sourced = GraphClient::with_base_and_source(
            source as Arc<dyn CredentialSource>,
            base,
            tls(),
            &retry(),
            &user_agent(),
        );
        assert!(matches!(sourced, Err(GraphError::Protocol(_))), "{base}");
    }
}
