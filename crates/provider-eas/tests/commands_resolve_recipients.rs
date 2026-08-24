// SPDX-License-Identifier: MPL-2.0
use provider_eas::commands::{tests_common::*, *};

#[test]
fn resolve_recipients_request_uses_spec_shape() {
    use provider_eas::wbxml::tags::{pages, recipients as rr};
    let req = ResolveRecipientsRequest {
        to: vec![
            "all@contoso.com".to_string(),
            "chris@contoso.com".to_string(),
            "Anat".to_string(),
        ],
        max_ambiguous_recipients: Some(2),
        availability: Some((
            "2008-12-01T08:00:00.000Z".to_string(),
            "2008-12-03T08:00:00.000Z".to_string(),
        )),
    };
    let tree = build_resolve_recipients_request(&req);
    assert_eq!(
        (tree.page, tree.token),
        (pages::RECIPIENTS, rr::RESOLVE_RECIPIENTS)
    );
    assert_eq!(tree.children.len(), 4, "three To children + one Options");
    for (i, to) in req.to.iter().enumerate() {
        let child = &tree.children[i];
        assert_eq!(
            (child.page, child.token),
            (pages::RECIPIENTS, rr::TO),
            "child {i} must be a To element"
        );
        assert_eq!(text_value(child).unwrap(), *to, "wire order preserved");
    }
    let options = &tree.children[3];
    assert_eq!(
        (options.page, options.token),
        (pages::RECIPIENTS, rr::OPTIONS)
    );
    assert_eq!(options.children.len(), 2);
    // Options child order per the §4.18.4.1 example:
    // MaxAmbiguousRecipients, then Availability.
    let max = &options.children[0];
    assert_eq!(
        (max.page, max.token),
        (pages::RECIPIENTS, rr::MAX_AMBIGUOUS_RECIPIENTS)
    );
    assert_eq!(text_value(max).unwrap(), "2");
    let avail = &options.children[1];
    assert_eq!(
        (avail.page, avail.token),
        (pages::RECIPIENTS, rr::AVAILABILITY)
    );
    assert_eq!(avail.children.len(), 2);
    assert_eq!(
        (avail.children[0].page, avail.children[0].token),
        (pages::RECIPIENTS, rr::START_TIME)
    );
    assert_eq!(
        text_value(&avail.children[0]).unwrap(),
        "2008-12-01T08:00:00.000Z"
    );
    assert_eq!(
        (avail.children[1].page, avail.children[1].token),
        (pages::RECIPIENTS, rr::END_TIME)
    );
    assert_eq!(
        text_value(&avail.children[1]).unwrap(),
        "2008-12-03T08:00:00.000Z"
    );
}

/// With neither optional field set, the Options element is omitted
/// entirely (§6.31 marks it minOccurs=0) — a plain ANR query is just
/// the To children.
#[test]
fn resolve_recipients_request_to_only_omits_options() {
    use provider_eas::wbxml::tags::{pages, recipients as rr};
    let req = ResolveRecipientsRequest {
        to: vec!["Testers".to_string()],
        max_ambiguous_recipients: None,
        availability: None,
    };
    let tree = build_resolve_recipients_request(&req);
    assert_eq!(tree.children.len(), 1, "only the To element");
    assert_eq!(
        (tree.children[0].page, tree.children[0].token),
        (pages::RECIPIENTS, rr::TO)
    );
    assert!(
        !tree.children.iter().any(|c| c.token == rr::OPTIONS),
        "Options must be absent when both optional fields are None"
    );
}

/// Options is emitted when only `max_ambiguous_recipients` is set, and
/// carries no Availability element (no free/busy requested).
#[test]
fn resolve_recipients_request_max_ambiguous_only_omits_availability() {
    use provider_eas::wbxml::tags::{pages, recipients as rr};
    let req = ResolveRecipientsRequest {
        to: vec!["Anat".to_string()],
        max_ambiguous_recipients: Some(99),
        availability: None,
    };
    let tree = build_resolve_recipients_request(&req);
    let options = tree
        .children
        .iter()
        .find(|c| c.token == rr::OPTIONS)
        .expect("Options must be present when max_ambiguous_recipients is set");
    assert_eq!(options.children.len(), 1);
    assert_eq!(
        (options.children[0].page, options.children[0].token),
        (pages::RECIPIENTS, rr::MAX_AMBIGUOUS_RECIPIENTS)
    );
    assert_eq!(text_value(&options.children[0]).unwrap(), "99");
    assert!(
        !options.children.iter().any(|c| c.token == rr::AVAILABILITY),
        "Availability must be absent when availability is None"
    );
}

#[test]
fn resolve_recipients_request_round_trips() {
    let req = ResolveRecipientsRequest {
        to: vec!["all@contoso.com".to_string(), "Anat".to_string()],
        max_ambiguous_recipients: Some(2),
        availability: Some((
            "2008-12-01T08:00:00.000Z".to_string(),
            "2008-12-03T08:00:00.000Z".to_string(),
        )),
    };
    let tree = build_resolve_recipients_request(&req);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// The §4.18.4.2 worked example: MULTIPLE Response siblings (one per
/// request To), statuses 1 and 3, recipients with and without
/// Availability, MergedFreeBusy digit strings preserved verbatim, and
/// availability failure codes (162/161) surfaced as data — NOT as
/// errors.
#[test]
fn resolve_recipients_response_parses_multi_response_fixture() {
    use provider_eas::wbxml::tags::{pages, recipients as rr};
    let recipient = |rtype: &str, name: &str, email: &str, avail: Option<(&str, Option<&str>)>| {
        let mut children = vec![
            WbxmlElement::text(pages::RECIPIENTS, rr::TYPE, rtype),
            WbxmlElement::text(pages::RECIPIENTS, rr::DISPLAY_NAME, name),
            WbxmlElement::text(pages::RECIPIENTS, rr::EMAIL_ADDRESS, email),
        ];
        if let Some((status, mfb)) = avail {
            let mut avail_children =
                vec![WbxmlElement::text(pages::RECIPIENTS, rr::STATUS, status)];
            if let Some(mfb) = mfb {
                avail_children.push(WbxmlElement::text(
                    pages::RECIPIENTS,
                    rr::MERGED_FREE_BUSY,
                    mfb,
                ));
            }
            children.push(WbxmlElement::container(
                pages::RECIPIENTS,
                rr::AVAILABILITY,
                avail_children,
            ));
        }
        WbxmlElement::container(pages::RECIPIENTS, rr::RECIPIENT, children)
    };
    let response = |to: &str, status: &str, count: &str, recipients: Vec<WbxmlElement>| {
        let mut children = vec![
            WbxmlElement::text(pages::RECIPIENTS, rr::TO, to),
            WbxmlElement::text(pages::RECIPIENTS, rr::STATUS, status),
            WbxmlElement::text(pages::RECIPIENTS, rr::RECIPIENT_COUNT, count),
        ];
        children.extend(recipients);
        WbxmlElement::container(pages::RECIPIENTS, rr::RESPONSE, children)
    };
    let tree = WbxmlElement::container(
        pages::RECIPIENTS,
        rr::RESOLVE_RECIPIENTS,
        vec![
            WbxmlElement::text(pages::RECIPIENTS, rr::STATUS, "1"),
            response(
                "all@contoso.com",
                "1",
                "1",
                vec![recipient(
                    "1",
                    "All Contoso Full Time Employees",
                    "all@contoso.com",
                    Some(("162", None)),
                )],
            ),
            response(
                "ryan@contoso.com",
                "1",
                "1",
                vec![recipient(
                    "1",
                    "Chris Gray",
                    "chris@contoso.com",
                    Some((
                        "1",
                        Some("002000000000000000000000001002002200000010000000"),
                    )),
                )],
            ),
            response(
                "tom",
                "3",
                "30",
                vec![
                    recipient("2", "Anat Kerry", "anatk@contoso.com", None),
                    recipient("1", "Anat Reding", "anetr@contoso.com", None),
                ],
            ),
            response(
                "myPersonalDistributionList",
                "1",
                "4",
                vec![
                    recipient(
                        "2",
                        "chris@fourthcoffee.com",
                        "chris@fourthcoffee.com",
                        Some(("162", None)),
                    ),
                    recipient("1", "Anet Reding", "anetr@contoso.com", Some(("161", None))),
                    recipient(
                        "2",
                        "Dag Rovik",
                        "dag@contoso.com",
                        Some((
                            "1",
                            Some("333333333333333333330000001002002200000010000000"),
                        )),
                    ),
                    recipient(
                        "2",
                        "fabrice@fourthcoffee.com",
                        "fabrice@fourthcoffee.com",
                        Some(("162", None)),
                    ),
                ],
            ),
        ],
    );
    let parsed = parse_resolve_recipients_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.responses.len(), 4);

    // Response 1: exact match, free/busy retrieval FAILED (162) — the
    // failure code is data, not an error, and no MergedFreeBusy rides.
    let r0 = &parsed.responses[0];
    assert_eq!(r0.to, "all@contoso.com");
    assert_eq!(r0.status, 1);
    assert_eq!(r0.recipient_count, Some(1));
    assert_eq!(r0.recipients.len(), 1);
    let rec = &r0.recipients[0];
    assert_eq!(rec.recipient_type, Some(1));
    assert_eq!(
        rec.display_name.as_deref(),
        Some("All Contoso Full Time Employees")
    );
    assert_eq!(rec.email_address.as_deref(), Some("all@contoso.com"));
    assert_eq!(rec.availability_status, Some(162));
    assert_eq!(rec.merged_free_busy, None);

    // Response 2: free/busy retrieved — MergedFreeBusy preserved VERBATIM.
    let r1 = &parsed.responses[1];
    assert_eq!(r1.to, "ryan@contoso.com");
    assert_eq!(r1.recipients.len(), 1);
    let rec = &r1.recipients[0];
    assert_eq!(rec.availability_status, Some(1));
    assert_eq!(
        rec.merged_free_busy.as_deref(),
        Some("002000000000000000000000001002002200000010000000"),
        "MergedFreeBusy digits must survive verbatim"
    );

    // Response 3: ambiguous (Status 3) — TWO suggestion recipients that
    // per §4.18.4.2 carry NO Availability element.
    let r2 = &parsed.responses[2];
    assert_eq!(r2.to, "tom");
    assert_eq!(r2.status, 3);
    assert_eq!(r2.recipient_count, Some(30));
    assert_eq!(r2.recipients.len(), 2);
    assert_eq!(r2.recipients[0].recipient_type, Some(2));
    assert_eq!(r2.recipients[0].display_name.as_deref(), Some("Anat Kerry"));
    assert_eq!(r2.recipients[1].recipient_type, Some(1));
    assert_eq!(
        r2.recipients[1].email_address.as_deref(),
        Some("anetr@contoso.com")
    );
    for rec in &r2.recipients {
        assert_eq!(
            rec.availability_status, None,
            "ambiguous suggestions carry no Availability"
        );
        assert_eq!(rec.merged_free_busy, None);
    }

    // Response 4: personal DL — mixed per-recipient availability
    // outcomes (162 failure, 161 over-20-member DL, 1 success with MFB).
    let r3 = &parsed.responses[3];
    assert_eq!(r3.to, "myPersonalDistributionList");
    assert_eq!(r3.status, 1);
    assert_eq!(r3.recipients.len(), 4);
    let statuses: Vec<Option<u32>> = r3
        .recipients
        .iter()
        .map(|r| r.availability_status)
        .collect();
    assert_eq!(
        statuses,
        vec![Some(162), Some(161), Some(1), Some(162)],
        "per-recipient availability statuses in wire order"
    );
    assert_eq!(
        r3.recipients[2].merged_free_busy.as_deref(),
        Some("333333333333333333330000001002002200000010000000")
    );
    assert_eq!(r3.recipients[0].merged_free_busy, None);
}

/// The §4.18.2 Certificates node: we parse ONLY its Status and
/// CertificateCount — certificate bytes (Certificate / MiniCertificate)
/// are deliberately NOT captured (this client never requests them; the
/// type carries no field for them).
#[test]
fn resolve_recipients_response_parses_certificates_status_and_count_only() {
    use provider_eas::wbxml::tags::{pages, recipients as rr};
    let tree = WbxmlElement::container(
        pages::RECIPIENTS,
        rr::RESOLVE_RECIPIENTS,
        vec![
            WbxmlElement::text(pages::RECIPIENTS, rr::STATUS, "1"),
            WbxmlElement::container(
                pages::RECIPIENTS,
                rr::RESPONSE,
                vec![
                    WbxmlElement::text(pages::RECIPIENTS, rr::TO, "Testers"),
                    WbxmlElement::text(pages::RECIPIENTS, rr::STATUS, "1"),
                    WbxmlElement::text(pages::RECIPIENTS, rr::RECIPIENT_COUNT, "2"),
                    WbxmlElement::container(
                        pages::RECIPIENTS,
                        rr::RECIPIENT,
                        vec![
                            WbxmlElement::text(pages::RECIPIENTS, rr::TYPE, "1"),
                            WbxmlElement::text(pages::RECIPIENTS, rr::DISPLAY_NAME, "Testers"),
                            WbxmlElement::text(
                                pages::RECIPIENTS,
                                rr::EMAIL_ADDRESS,
                                "testers@example.com",
                            ),
                            WbxmlElement::container(
                                pages::RECIPIENTS,
                                rr::CERTIFICATES,
                                vec![
                                    WbxmlElement::text(pages::RECIPIENTS, rr::STATUS, "1"),
                                    WbxmlElement::text(
                                        pages::RECIPIENTS,
                                        rr::CERTIFICATE_COUNT,
                                        "2",
                                    ),
                                    WbxmlElement::text(pages::RECIPIENTS, rr::RECIPIENT_COUNT, "3"),
                                    WbxmlElement::text(
                                        pages::RECIPIENTS,
                                        rr::MINI_CERTIFICATE,
                                        "AAAAAEfXfBA=",
                                    ),
                                ],
                            ),
                        ],
                    ),
                ],
            ),
        ],
    );
    let parsed = parse_resolve_recipients_response(&tree).expect("parse");
    assert_eq!(parsed.responses.len(), 1);
    let rec = &parsed.responses[0].recipients[0];
    assert_eq!(rec.certificates_status, Some(1));
    assert_eq!(rec.certificate_count, Some(2));
    assert_eq!(
        rec.availability_status, None,
        "no Availability element present"
    );
    assert_eq!(rec.merged_free_busy, None);
}

/// Command-level failure (§2.2.3.177.12: top-level 5 = protocol error,
/// 6 = server error): the top-level Status surfaces and NO Response
/// elements are present. The client maps non-1 to
/// `EasError::CommandStatus`.
#[test]
fn resolve_recipients_response_command_level_error() {
    use provider_eas::wbxml::tags::{pages, recipients as rr};
    let tree = WbxmlElement::container(
        pages::RECIPIENTS,
        rr::RESOLVE_RECIPIENTS,
        vec![WbxmlElement::text(pages::RECIPIENTS, rr::STATUS, "5")],
    );
    let parsed = parse_resolve_recipients_response(&tree).expect("parse");
    assert_eq!(parsed.status, 5);
    assert!(parsed.responses.is_empty());
}

/// A non-ResolveRecipients root is a parse error, not a silent success.
#[test]
fn resolve_recipients_response_rejects_wrong_root() {
    let response = WbxmlElement::container(PAGE_FOLDER, FH_FOLDER_SYNC, vec![]);
    assert!(parse_resolve_recipients_response(&response).is_err());
}
