// SPDX-License-Identifier: MPL-2.0
use super::{
    AS_CLASS, AS_COLLECTION_ID, EasItem, GalEntry, PAGE_AIRSYNC, SearchRequest, SearchResult,
    SearchResultItem, WbxmlElement, WbxmlError, parse_application_data, tags, text_value,
    text_value_opt,
};

// ============================================================================
// Search (code page 15) + GAL (code page 16)
// ============================================================================

/// Build a Search request ([MS-ASCMD] §2.2.1.16).
///
/// Mailbox wire shape (spec §4.12.1.1):
/// ```xml
/// <Search>
///   <Store>
///     <Name>Mailbox</Name>
///     <Query>
///       <And>
///         <airsync:Class>Email</>
///         <airsync:CollectionId>{collection_id}</>   <!-- only when set -->
///         <FreeText>{query}</FreeText>
///       </And>
///     </Query>
///     <Options>
///       <RebuildResults/>
///       <Range>{range}</Range>
///       <DeepTraversal/>                             <!-- only when set -->
///     </Options>
///   </Store>
/// </Search>
/// ```
///
/// GAL wire shape (spec §4.13.1) uses `Name>GAL</Name>` and a plain text
/// `<Query>{query}</Query>` leaf (no `And` container). Options order is
/// Range, RebuildResults, DeepTraversal.
pub fn build_search_request(req: &SearchRequest) -> WbxmlElement {
    use tags::search as sr;

    let name = WbxmlElement::text(sr::PAGE, sr::NAME, req.store.clone());

    let query = if req.store == "GAL" {
        WbxmlElement::text(sr::PAGE, sr::QUERY, req.query.clone())
    } else {
        let mut and_children = vec![WbxmlElement::text(PAGE_AIRSYNC, AS_CLASS, "Email")];
        if let Some(collection_id) = &req.collection_id {
            and_children.push(WbxmlElement::text(
                PAGE_AIRSYNC,
                AS_COLLECTION_ID,
                collection_id.clone(),
            ));
        }
        and_children.push(WbxmlElement::text(
            sr::PAGE,
            sr::FREE_TEXT,
            req.query.clone(),
        ));
        WbxmlElement::container(
            sr::PAGE,
            sr::QUERY,
            vec![WbxmlElement::container(sr::PAGE, sr::AND, and_children)],
        )
    };

    let mut options_children = Vec::new();
    if req.store == "GAL" {
        options_children.push(WbxmlElement::text(sr::PAGE, sr::RANGE, req.range.clone()));
        options_children.push(WbxmlElement::empty(sr::PAGE, sr::REBUILD_RESULTS));
    } else {
        options_children.push(WbxmlElement::empty(sr::PAGE, sr::REBUILD_RESULTS));
        options_children.push(WbxmlElement::text(sr::PAGE, sr::RANGE, req.range.clone()));
    }
    if req.deep_traversal {
        options_children.push(WbxmlElement::empty(sr::PAGE, sr::DEEP_TRAVERSAL));
    }

    let store = WbxmlElement::container(
        sr::PAGE,
        sr::STORE,
        vec![
            name,
            query,
            WbxmlElement::container(sr::PAGE, sr::OPTIONS, options_children),
        ],
    );

    WbxmlElement::container(sr::PAGE, sr::SEARCH, vec![store])
}

/// Parse a Search response ([MS-ASCMD] §2.2.1.16).
///
/// Response shape: `Search > Status + Response > Store > Status, Result*, Range, Total`.
/// Each `Result` carries `Class`, `LongId`, `CollectionId`, `Properties`.
/// Mailbox `Properties` reuse `parse_application_data`; GAL `Properties` are
/// parsed by tag name into `GalEntry`. A command-level error (no `Response`)
/// yields empty results with the status surfaced.
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
pub fn parse_search_response(root: &WbxmlElement) -> Result<SearchResult, WbxmlError> {
    use tags::search as sr;

    let mut result = SearchResult {
        status: 1,
        ..SearchResult::default()
    };

    for child in &root.children {
        if child.page == sr::PAGE && child.token == sr::STATUS {
            let raw = text_value(child).unwrap_or_default();
            result.status = if let Ok(n) = raw.parse() {
                n
            } else {
                log::warn!("Search: malformed top-level Status \"{raw}\"; defaulting to 1");
                1
            };
        } else if child.page == sr::PAGE && child.token == sr::RESPONSE {
            for resp_child in &child.children {
                if resp_child.page == sr::PAGE && resp_child.token == sr::STORE {
                    parse_search_store(resp_child, &mut result);
                }
            }
        }
    }

    Ok(result)
}

fn parse_search_store(store: &WbxmlElement, result: &mut SearchResult) {
    use tags::search as sr;

    for child in &store.children {
        match (child.page, child.token) {
            (sr::PAGE, sr::STATUS) => {
                let raw = text_value(child).unwrap_or_default();
                result.store_status = if let Ok(n) = raw.parse() {
                    Some(n)
                } else {
                    log::warn!("Search Store: malformed Status \"{raw}\"; ignoring");
                    None
                };
            }
            (sr::PAGE, sr::RESULT) => {
                result.results.push(parse_search_result(child));
            }
            (sr::PAGE, sr::RANGE) => {
                result.range = text_value_opt(child);
            }
            (sr::PAGE, sr::TOTAL) => {
                result.total = text_value_opt(child).and_then(|s| s.parse().ok());
            }
            _ => {}
        }
    }
}

fn parse_search_result(result_el: &WbxmlElement) -> SearchResultItem {
    use tags::{gal, search as sr};

    let mut item = SearchResultItem::default();
    let mut props_el: Option<&WbxmlElement> = None;

    for child in &result_el.children {
        match (child.page, child.token) {
            (PAGE_AIRSYNC, AS_CLASS) => item.class = text_value_opt(child),
            (sr::PAGE, sr::LONG_ID) => item.long_id = text_value_opt(child),
            (PAGE_AIRSYNC, AS_COLLECTION_ID) => item.collection_id = text_value_opt(child),
            (sr::PAGE, sr::PROPERTIES) => props_el = Some(child),
            _ => {}
        }
    }

    if let Some(props) = props_el {
        let is_gal = props.children.iter().any(|c| c.page == gal::PAGE);
        if is_gal {
            let mut entry = GalEntry::default();
            for child in &props.children {
                if child.page != gal::PAGE {
                    continue;
                }
                match child.token {
                    gal::DISPLAY_NAME => entry.display_name = text_value_opt(child),
                    gal::PHONE => entry.phone = text_value_opt(child),
                    gal::OFFICE => entry.office = text_value_opt(child),
                    gal::TITLE => entry.title = text_value_opt(child),
                    gal::COMPANY => entry.company = text_value_opt(child),
                    gal::ALIAS => entry.alias = text_value_opt(child),
                    gal::FIRST_NAME => entry.first_name = text_value_opt(child),
                    gal::LAST_NAME => entry.last_name = text_value_opt(child),
                    gal::HOME_PHONE => entry.home_phone = text_value_opt(child),
                    gal::MOBILE_PHONE => entry.mobile_phone = text_value_opt(child),
                    gal::EMAIL_ADDRESS => entry.email_address = text_value_opt(child),
                    _ => {}
                }
            }
            item.gal = Some(entry);
        } else {
            let mut eas_item = EasItem::default();
            parse_application_data(props, &mut eas_item);
            item.item = Some(eas_item);
        }
    }

    item
}
