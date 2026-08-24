// SPDX-License-Identifier: MPL-2.0
use provider_eas::commands::{tests_common::*, *};

#[test]
fn validate_cert_request_uses_spec_shape() {
    use provider_eas::wbxml::tags::{pages, validatecert};
    let req = ValidateCertRequest {
        certificate_chain: vec![
            "MIICYjCCAcugAwIBAgIUYGs8jZbX0Vxj".to_string(),
            "MIIB8zCCAVygAwIBAgIUdhWamYEKM9ea".to_string(),
        ],
        certificates: vec!["MIICYjCCAcugAwIBAgIUYGs8jZbX0VxjObu4nw0".to_string()],
        check_crl: true,
    };
    let tree = build_validate_cert_request(&req);
    assert_eq!(
        (tree.page, tree.token),
        (pages::VALIDATE, validatecert::VALIDATE_CERT)
    );
    assert_eq!(tree.children.len(), 3);

    let chain = &tree.children[0];
    assert_eq!(
        (chain.page, chain.token),
        (pages::VALIDATE, validatecert::CERTIFICATE_CHAIN)
    );
    assert_eq!(
        chain.children.len(),
        2,
        "one Certificate child per chain cert"
    );
    for (i, cert) in chain.children.iter().enumerate() {
        assert_eq!(
            (cert.page, cert.token),
            (pages::VALIDATE, validatecert::CERTIFICATE)
        );
        assert_eq!(text_value(cert).unwrap(), req.certificate_chain[i]);
    }

    let certs = &tree.children[1];
    assert_eq!(
        (certs.page, certs.token),
        (pages::VALIDATE, validatecert::CERTIFICATES)
    );
    assert_eq!(certs.children.len(), 1);
    assert_eq!(
        (certs.children[0].page, certs.children[0].token),
        (pages::VALIDATE, validatecert::CERTIFICATE)
    );
    assert_eq!(text_value(&certs.children[0]).unwrap(), req.certificates[0]);

    let crl = &tree.children[2];
    assert_eq!(
        (crl.page, crl.token),
        (pages::VALIDATE, validatecert::CHECK_CRL)
    );
    assert!(matches!(&crl.value, WbxmlValue::Text(t) if t == "1"));
}

/// An empty `certificate_chain` omits the CertificateChain element
/// entirely (§2.2.3.20: optional child) — the Certificates container
/// becomes the first child.
#[test]
fn validate_cert_request_certs_only_omits_certificate_chain() {
    use provider_eas::wbxml::tags::{pages, validatecert};
    let req = ValidateCertRequest {
        certificate_chain: vec![],
        certificates: vec![
            "MIICYjCCAcugAwIBAgIUYGs8jZbX0Vxj".to_string(),
            "MIIB8zCCAVygAwIBAgIUdhWamYEKM9ea".to_string(),
        ],
        check_crl: true,
    };
    let tree = build_validate_cert_request(&req);
    assert_eq!(tree.children.len(), 2, "no CertificateChain element");
    let certs = &tree.children[0];
    assert_eq!(
        (certs.page, certs.token),
        (pages::VALIDATE, validatecert::CERTIFICATES)
    );
    assert_eq!(certs.children.len(), 2, "wire order preserved");
    assert_eq!(text_value(&certs.children[0]).unwrap(), req.certificates[0]);
    assert_eq!(text_value(&certs.children[1]).unwrap(), req.certificates[1]);
    assert!(
        !tree
            .children
            .iter()
            .any(|c| c.token == validatecert::CERTIFICATE_CHAIN),
        "CertificateChain must not appear anywhere for an empty chain"
    );
}

/// `check_crl: false` omits the CheckCRL element entirely (§2.2.3.26:
/// optional child; absence means the server MAY ignore an unverifiable
/// revocation status).
#[test]
fn validate_cert_request_omits_check_crl_when_false() {
    use provider_eas::wbxml::tags::validatecert;
    let req = ValidateCertRequest {
        certificate_chain: vec![],
        certificates: vec!["MIICYjCCAcugAwIBAgIUYGs8jZbX0Vxj".to_string()],
        check_crl: false,
    };
    let tree = build_validate_cert_request(&req);
    assert_eq!(tree.children.len(), 1, "only the Certificates container");
    assert!(
        !tree
            .children
            .iter()
            .any(|c| c.token == validatecert::CHECK_CRL),
        "CheckCRL must be absent when check_crl is false"
    );
}

#[test]
fn validate_cert_request_round_trips() {
    let req = ValidateCertRequest {
        certificate_chain: vec!["MIIB8zCCAVygAwIBAgIUdhWamYEKM9ea".to_string()],
        certificates: vec!["MIICYjCCAcugAwIBAgIUYGs8jZbX0Vxj".to_string()],
        check_crl: true,
    };
    let tree = build_validate_cert_request(&req);
    let back = round_trip(&tree);
    assert_eq!(tree, back);
}

/// Response shape per [MS-ASCMD] §4.20.2: top-level Status plus one
/// Certificate child per validated certificate, each contributing its own
/// Status:
/// ```text
/// ValidateCert (11,0x05) > Status (11,0x0A) = "1",
///   Certificate (11,0x07) > Status (11,0x0A) = "1"
/// ```
#[test]
fn validate_cert_response_parses_spec_shape() {
    use provider_eas::wbxml::tags::{pages, validatecert};
    let tree = WbxmlElement::container(
        pages::VALIDATE,
        validatecert::VALIDATE_CERT,
        vec![
            WbxmlElement::text(pages::VALIDATE, validatecert::STATUS, "1"),
            WbxmlElement::container(
                pages::VALIDATE,
                validatecert::CERTIFICATE,
                vec![WbxmlElement::text(
                    pages::VALIDATE,
                    validatecert::STATUS,
                    "1",
                )],
            ),
        ],
    );
    let parsed = parse_validate_cert_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.certificate_statuses, vec![1]);
}

/// Multiple Certificate children contribute their statuses in response
/// (document) order — the caller correlates them with the request order.
/// Per-certificate codes per §2.2.3.177.18 (7 = expired / not yet valid).
#[test]
fn validate_cert_response_parses_multiple_certificates_in_order() {
    use provider_eas::wbxml::tags::{pages, validatecert};
    let tree = WbxmlElement::container(
        pages::VALIDATE,
        validatecert::VALIDATE_CERT,
        vec![
            WbxmlElement::text(pages::VALIDATE, validatecert::STATUS, "1"),
            WbxmlElement::container(
                pages::VALIDATE,
                validatecert::CERTIFICATE,
                vec![WbxmlElement::text(
                    pages::VALIDATE,
                    validatecert::STATUS,
                    "1",
                )],
            ),
            WbxmlElement::container(
                pages::VALIDATE,
                validatecert::CERTIFICATE,
                vec![WbxmlElement::text(
                    pages::VALIDATE,
                    validatecert::STATUS,
                    "7",
                )],
            ),
            WbxmlElement::container(
                pages::VALIDATE,
                validatecert::CERTIFICATE,
                vec![WbxmlElement::text(
                    pages::VALIDATE,
                    validatecert::STATUS,
                    "3",
                )],
            ),
        ],
    );
    let parsed = parse_validate_cert_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.certificate_statuses, vec![1, 7, 3]);
}

/// Command-level failure (§2.2.3.177.18: top-level 17 = failure): the
/// top-level Status surfaces and no per-certificate entries are present.
/// The client maps this to `EasError::CommandStatus`.
#[test]
fn validate_cert_response_command_level_error() {
    use provider_eas::wbxml::tags::{pages, validatecert};
    let tree = WbxmlElement::container(
        pages::VALIDATE,
        validatecert::VALIDATE_CERT,
        vec![WbxmlElement::text(
            pages::VALIDATE,
            validatecert::STATUS,
            "17",
        )],
    );
    let parsed = parse_validate_cert_response(&tree).expect("parse");
    assert_eq!(parsed.status, 17);
    assert!(parsed.certificate_statuses.is_empty());
}

/// A Certificate element without a Status child is warn-logged and
/// SKIPPED — never attributed by position and never swallowed. The
/// remaining certificates still contribute their statuses, in order.
#[test]
fn validate_cert_response_certificate_without_status_is_skipped() {
    use provider_eas::wbxml::tags::{pages, validatecert};
    let tree = WbxmlElement::container(
        pages::VALIDATE,
        validatecert::VALIDATE_CERT,
        vec![
            WbxmlElement::text(pages::VALIDATE, validatecert::STATUS, "1"),
            WbxmlElement::container(pages::VALIDATE, validatecert::CERTIFICATE, vec![]),
            WbxmlElement::container(
                pages::VALIDATE,
                validatecert::CERTIFICATE,
                vec![WbxmlElement::text(
                    pages::VALIDATE,
                    validatecert::STATUS,
                    "6",
                )],
            ),
        ],
    );
    let parsed = parse_validate_cert_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.certificate_statuses, vec![6]);
}

/// A Certificate whose Status is not a parsable integer is skipped with a
/// warn-log as well — fabricating a success (1) for a malformed
/// validation verdict would be a security hazard.
#[test]
fn validate_cert_response_malformed_certificate_status_is_skipped() {
    use provider_eas::wbxml::tags::{pages, validatecert};
    let tree = WbxmlElement::container(
        pages::VALIDATE,
        validatecert::VALIDATE_CERT,
        vec![
            WbxmlElement::text(pages::VALIDATE, validatecert::STATUS, "1"),
            WbxmlElement::container(
                pages::VALIDATE,
                validatecert::CERTIFICATE,
                vec![WbxmlElement::text(
                    pages::VALIDATE,
                    validatecert::STATUS,
                    "not-a-number",
                )],
            ),
        ],
    );
    let parsed = parse_validate_cert_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert!(parsed.certificate_statuses.is_empty());
}

/// Absent top-level Status defaults `status` to 1, mirroring the
/// GetItemEstimate/UserInformation/DevicePassword family convention
/// (§6.48 makes Status required, so this only guards against lenient
/// servers).
#[test]
fn validate_cert_response_defaults_status_when_absent() {
    use provider_eas::wbxml::tags::{pages, validatecert};
    let tree = WbxmlElement::container(
        pages::VALIDATE,
        validatecert::VALIDATE_CERT,
        vec![WbxmlElement::container(
            pages::VALIDATE,
            validatecert::CERTIFICATE,
            vec![WbxmlElement::text(
                pages::VALIDATE,
                validatecert::STATUS,
                "1",
            )],
        )],
    );
    let parsed = parse_validate_cert_response(&tree).expect("parse");
    assert_eq!(parsed.status, 1);
    assert_eq!(parsed.certificate_statuses, vec![1]);
}

/// A non-ValidateCert root is a parse error, not a silent success.
#[test]
fn validate_cert_response_rejects_wrong_root() {
    let response = WbxmlElement::container(PAGE_FOLDER, FH_FOLDER_SYNC, vec![]);
    assert!(parse_validate_cert_response(&response).is_err());
}
