//! Every Gmail call the adapter makes, priced as Google's quota table prices it.

use reqwest::Method;

use super::cost;

const ME: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

fn priced(method: &Method, path: &str) -> u32 {
    cost(method, &format!("{ME}{path}"))
}

#[test]
fn every_call_the_adapter_makes_costs_what_google_publishes() {
    let get = Method::GET;
    let post = Method::POST;
    for (method, path, units) in [
        (&get, "/profile", 1),
        (&get, "/labels", 1),
        (&get, "/settings/sendAs", 1),
        (&get, "/history?startHistoryId=40656", 2),
        (&get, "/messages?maxResults=100&includeSpamTrash=true", 5),
        (&get, "/drafts", 5),
        (&post, "/labels", 5),
        (&Method::PATCH, "/labels/Label_4", 5),
        (&Method::DELETE, "/labels/Label_2", 5),
        (&post, "/messages/m-1/modify", 5),
        (&post, "/messages/m-1/untrash", 5),
        (&Method::DELETE, "/messages/m-1", 10),
        (&post, "/drafts", 10),
        (&Method::DELETE, "/drafts/r-1", 10),
        (&Method::PUT, "/drafts/r-1", 15),
        (
            &get,
            "/messages/m-1?format=metadata&metadataHeaders=Subject",
            20,
        ),
        (&get, "/messages/m-1?format=minimal", 20),
        (&post, "/messages/m-1/trash", 20),
        (&get, "/messages/m-1/attachments/a-1", 20),
        (&post, "/messages/import", 25),
        (&post, "/messages/batchModify", 50),
        (&get, "/messages/m-1?format=raw", 60),
        (&get, "/messages/m-1", 60),
        (&post, "/messages/send", 100),
        (&Method::PATCH, "/settings/sendAs/alice%40example.com", 100),
    ] {
        assert_eq!(priced(method, path), units, "{method} {path}");
    }
}

#[test]
fn an_upload_is_priced_like_the_call_it_uploads_for() {
    let url =
        "https://gmail.googleapis.com/upload/gmail/v1/users/me/messages/send?uploadType=media";
    assert_eq!(cost(&Method::POST, url), 100);
}

#[test]
fn calendar_and_contacts_spend_none_of_the_gmail_quota() {
    for url in [
        "https://www.googleapis.com/calendar/v3/users/me/calendarList?maxResults=250",
        "https://people.googleapis.com/v1/people/me/connections?personFields=names",
        "https://lh3.googleusercontent.com/a/photo",
    ] {
        assert_eq!(cost(&Method::GET, url), 0, "{url}");
    }
}

#[test]
fn a_call_the_table_does_not_list_is_charged_as_the_dearest() {
    // `threads.modify` is 10 in Google's table; the adapter never calls it, so it is not listed
    // here and is charged the most any single call costs.
    assert_eq!(priced(&Method::POST, "/threads/t-1/modify"), 100);
}

#[test]
fn a_snapshot_page_costs_more_than_a_third_of_a_minute() {
    // The arithmetic the pace exists for: a listing and a hundred message fetches.
    let page = priced(&Method::GET, "/messages?maxResults=100")
        + 100 * priced(&Method::GET, "/messages/m-1?format=metadata");
    assert_eq!(page, 2005);
    assert!(
        page * 3 > super::UNITS_PER_MINUTE,
        "three pages fit a minute"
    );
}
