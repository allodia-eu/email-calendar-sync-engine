// SPDX-License-Identifier: MPL-2.0
use super::{
    ValidateCertRequest, ValidateCertResult, WbxmlElement, WbxmlError, expect_tag, text_value,
};

// ============================================================================
// ValidateCert (code page 11)
// ============================================================================
//
// Token table verified against [MS-ASWBXML] §2.1.2.1.12 (page 11) via
// code_pages.rs VALIDATE_TOKENS:
//   ValidateCert=0x05, Certificates=0x06, Certificate=0x07,
//   CertificateChain=0x08, CheckCrl=0x09, Status=0x0A.

/// Build a ValidateCert request ([MS-ASCMD] §2.2.1.22 / §4.20.1; schema
/// §6.47 — `xs:all`, child order follows the §4.20.1 worked example):
/// ```xml
/// <ValidateCert>                <!-- page 11, 0x05 -->
///   <CertificateChain>          <!-- page 11, 0x08 — optional -->
///     <Certificate>…</>         <!-- page 11, 0x07 — base64 DER, 1..N -->
///   </CertificateChain>
///   <Certificates>              <!-- page 11, 0x06 — required -->
///     <Certificate>…</>         <!-- page 11, 0x07 — base64 DER, 1..N -->
///   </Certificates>
///   <CheckCRL>1</CheckCRL>      <!-- page 11, 0x09 — optional -->
/// </ValidateCert>
/// ```
/// An empty `certificate_chain` omits the CertificateChain element (§2.2.3.20
/// marks it optional); `check_crl: false` omits CheckCRL (§2.2.3.26 — absence
/// lets the server ignore an unverifiable revocation status). The
/// Certificates container is emitted unconditionally: §2.2.3.23.2 requires it
/// with 1..N Certificate children, so callers MUST pass at least one
/// certificate (the builder mirrors `build_move_items_request` and does not
/// police its input).
///
/// SECURITY: the certificate strings are opaque base64 DER payloads — large
/// and security-sensitive. They are never logged here; the transport layer's
/// body dumps are redacted for this command (see `client::body_dump_allowed`)
/// and errors carry status codes only.
pub fn build_validate_cert_request(req: &ValidateCertRequest) -> WbxmlElement {
    use crate::wbxml::tags::{pages, validatecert as vc};
    let mut children: Vec<WbxmlElement> = Vec::new();
    if !req.certificate_chain.is_empty() {
        children.push(WbxmlElement::container(
            pages::VALIDATE,
            vc::CERTIFICATE_CHAIN,
            req.certificate_chain
                .iter()
                .map(|c| WbxmlElement::text(pages::VALIDATE, vc::CERTIFICATE, c.clone()))
                .collect(),
        ));
    }
    children.push(WbxmlElement::container(
        pages::VALIDATE,
        vc::CERTIFICATES,
        req.certificates
            .iter()
            .map(|c| WbxmlElement::text(pages::VALIDATE, vc::CERTIFICATE, c.clone()))
            .collect(),
    ));
    if req.check_crl {
        children.push(WbxmlElement::text(pages::VALIDATE, vc::CHECK_CRL, "1"));
    }
    WbxmlElement::container(pages::VALIDATE, vc::VALIDATE_CERT, children)
}

/// Parse a ValidateCert response ([MS-ASCMD] §4.20.2; schema §6.48 —
/// `xs:sequence`: Status first, then one Certificate per validated cert):
/// ```xml
/// <ValidateCert>                <!-- page 11, 0x05 -->
///   <Status>1</Status>          <!-- page 11, 0x0A — command-level -->
///   <Certificate>               <!-- page 11, 0x07 — 0..N -->
///     <Status>1</Status>        <!-- page 11, 0x0A — per-certificate -->
///   </Certificate>
/// </ValidateCert>
/// ```
/// The top-level Status (§2.2.3.177.18: 1 = success, 17 = failure) is read
/// first; absent defaults to 1, mirroring the GetItemEstimate/Settings
/// family convention (§6.48 makes it required, so this only guards lenient
/// servers). Each Certificate child contributes its own Status to
/// `certificate_statuses` IN DOCUMENT ORDER — the caller correlates entries
/// with the request order. A Certificate without a parsable Status is
/// warn-logged and SKIPPED (it contributes no entry): attributing a status
/// by position would be a guess, and fabricating success (1) for a
/// malformed validation verdict is a security hazard — never swallow, never
/// invent.
///
/// # Errors
///
/// Returns `WbxmlError` when the response tree is malformed — an unexpected
/// root or child tag, non-UTF-8 content, or non-numeric text where a number is
/// required.
pub fn parse_validate_cert_response(root: &WbxmlElement) -> Result<ValidateCertResult, WbxmlError> {
    use crate::wbxml::tags::{pages, validatecert as vc};
    expect_tag(root, pages::VALIDATE, vc::VALIDATE_CERT)?;
    let mut result = ValidateCertResult {
        status: 1, // success default when the Status element is absent
        ..ValidateCertResult::default()
    };
    for child in &root.children {
        if child.page == pages::VALIDATE && child.token == vc::STATUS {
            let raw = text_value(child).unwrap_or_default();
            result.status = if let Ok(n) = raw.parse() {
                n
            } else {
                log::warn!("ValidateCert: malformed top-level Status \"{raw}\"; defaulting to 1");
                1
            };
        }
    }
    for child in &root.children {
        if child.page != pages::VALIDATE || child.token != vc::CERTIFICATE {
            continue;
        }
        let mut cert_status: Option<u32> = None;
        for c in &child.children {
            if c.page == pages::VALIDATE && c.token == vc::STATUS {
                let raw = text_value(c).unwrap_or_default();
                match raw.parse() {
                    Ok(n) => cert_status = Some(n),
                    Err(_) => {
                        log::warn!(
                            "ValidateCert: malformed Certificate Status \"{raw}\"; this certificate contributes no status"
                        );
                    }
                }
            }
        }
        match cert_status {
            Some(n) => result.certificate_statuses.push(n),
            None => {
                log::warn!(
                    "ValidateCert: skipping Certificate element without a parsable Status ({} children)",
                    child.children.len()
                );
            }
        }
    }
    Ok(result)
}
