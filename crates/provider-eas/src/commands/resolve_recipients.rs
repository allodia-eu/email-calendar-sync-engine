// SPDX-License-Identifier: MPL-2.0
use super::*;

// ============================================================================
// ResolveRecipients (code page 10)
// ============================================================================
//
// Token table verified against [MS-ASWBXML] §2.1.2.1.11 (page 10) via
// code_pages.rs RECIPIENTS_TOKENS:
//   ResolveRecipients=0x05, Response=0x06, Status=0x07, Type=0x08,
//   Recipient=0x09, DisplayName=0x0A, EmailAddress=0x0B, Certificates=0x0C,
//   Certificate=0x0D, MiniCertificate=0x0E, Options=0x0F, To=0x10,
//   CertificateRetrieval=0x11, RecipientCount=0x12, MaxCertificates=0x13,
//   MaxAmbiguousRecipients=0x14, CertificateCount=0x15, Availability=0x16,
//   StartTime=0x17, EndTime=0x18, MergedFreeBusy=0x19, Picture=0x1A,
//   MaxSize=0x1B, Data=0x1C, MaxPictures=0x1D.
//
// Scope: recipient resolution (ANR) + free/busy. Certificates: parse
// status/count only (no cert bytes). Pictures: out of scope.

/// Build a ResolveRecipients request ([MS-ASCMD] §2.2.1.15 / §4.18.4.1;
/// schema §6.31 — `xs:choice`: To children, then an optional Options):
/// ```xml
/// <ResolveRecipients>                       <!-- page 10, 0x05 -->
///   <To>all@example.com</To>                <!-- page 10, 0x10 — 1..N -->
///   <Options>                               <!-- page 10, 0x0F — optional -->
///     <MaxAmbiguousRecipients>2</…>         <!-- page 10, 0x14 — optional -->
///     <Availability>                        <!-- page 10, 0x16 — optional -->
///       <StartTime>…</StartTime>            <!-- page 10, 0x17 -->
///       <EndTime>…</EndTime>                <!-- page 10, 0x18 -->
///     </Availability>
///   </Options>
/// </ResolveRecipients>
/// ```
/// Options is omitted entirely when BOTH optional fields are None (§6.31
/// marks it minOccurs=0). Its child order follows the §4.18.4.1 worked
/// example: MaxAmbiguousRecipients first, then Availability (whose
/// StartTime precedes EndTime). `to` is REQUIRED on the wire — the builder
/// does not police its input (mirrors `build_move_items_request`); the
/// client method rejects an empty list before any network I/O.
pub fn build_resolve_recipients_request(req: &ResolveRecipientsRequest) -> WbxmlElement {
    use crate::wbxml::tags::{pages, recipients as rr};
    let mut children: Vec<WbxmlElement> = req
        .to
        .iter()
        .map(|to| WbxmlElement::text(pages::RECIPIENTS, rr::TO, to.clone()))
        .collect();
    let mut options_children: Vec<WbxmlElement> = Vec::new();
    if let Some(max) = req.max_ambiguous_recipients {
        options_children.push(WbxmlElement::text(
            pages::RECIPIENTS,
            rr::MAX_AMBIGUOUS_RECIPIENTS,
            max.to_string(),
        ));
    }
    if let Some((start, end)) = &req.availability {
        options_children.push(WbxmlElement::container(
            pages::RECIPIENTS,
            rr::AVAILABILITY,
            vec![
                WbxmlElement::text(pages::RECIPIENTS, rr::START_TIME, start.clone()),
                WbxmlElement::text(pages::RECIPIENTS, rr::END_TIME, end.clone()),
            ],
        ));
    }
    if !options_children.is_empty() {
        children.push(WbxmlElement::container(
            pages::RECIPIENTS,
            rr::OPTIONS,
            options_children,
        ));
    }
    WbxmlElement::container(pages::RECIPIENTS, rr::RESOLVE_RECIPIENTS, children)
}

/// Parse a ResolveRecipients response ([MS-ASCMD] §4.18.2/§4.18.4.2; schema
/// §6.32 — `xs:sequence`: Status first, then MULTIPLE Response siblings,
/// one per request To):
/// ```xml
/// <ResolveRecipients>            <!-- page 10, 0x05 -->
///   <Status>1</Status>           <!-- page 10, 0x07 — command-level -->
///   <Response>                   <!-- page 10, 0x06 — 0..N -->
///     <To>…</To>                 <!-- page 10, 0x10 -->
///     <Status>1</Status>         <!-- page 10, 0x07 — per-To (1 ok, 2/3 ambiguous, 4 no match) -->
///     <RecipientCount>…</>       <!-- page 10, 0x12 — optional -->
///     <Recipient>                <!-- page 10, 0x09 — 0..N -->
///       <Type>1</Type>           <!-- page 10, 0x08 (1 GAL, 2 contact) -->
///       <DisplayName>…</>        <!-- page 10, 0x0A -->
///       <EmailAddress>…</>       <!-- page 10, 0x0B -->
///       <Availability>           <!-- page 10, 0x16 — optional; absent on ambiguous suggestions -->
///         <Status>162</Status>   <!-- page 10, 0x07 -->
///         <MergedFreeBusy>…</>   <!-- page 10, 0x19 — preserved VERBATIM -->
///       </Availability>
///       <Certificates>           <!-- page 10, 0x0C — optional; status/count parsed ONLY -->
///         <Status>1</Status>     <!-- page 10, 0x07 -->
///         <CertificateCount>2</> <!-- page 10, 0x15 -->
///       </Certificates>
///     </Recipient>
///   </Response>
/// </ResolveRecipients>
/// ```
/// The top-level Status (§2.2.3.177.12: 1 = success, 5 = protocol error,
/// 6 = server error) gates the command — the client maps non-1 to
/// `EasError::CommandStatus`. Absent defaults to 1, mirroring the
/// GetItemEstimate/ValidateCert family convention. Per-To and per-recipient
/// statuses (ambiguous 2/3, availability 160/161/162) are DATA, not
/// errors — the caller decides what they mean. A malformed numeric element
/// is warn-logged and left `None` (never fabricated); DisplayName /
/// EmailAddress / MergedFreeBusy are captured verbatim. The Certificates
/// Certificate/MiniCertificate PAYLOADS are deliberately NOT parsed (status
/// /count only, by design — this client never requests certificates).
///
/// PRIVACY: recipient DisplayName/EmailAddress are directory PII. Parse
/// warnings here carry element NAMES and raw numeric text only — never
/// recipient identities.
pub fn parse_resolve_recipients_response(
    root: &WbxmlElement,
) -> Result<ResolveRecipientsResult, WbxmlError> {
    use crate::wbxml::tags::{pages, recipients as rr};
    expect_tag(root, pages::RECIPIENTS, rr::RESOLVE_RECIPIENTS)?;
    let mut result = ResolveRecipientsResult {
        status: 1, // success default when the Status element is absent
        ..ResolveRecipientsResult::default()
    };
    for child in &root.children {
        match (child.page, child.token) {
            (pages::RECIPIENTS, rr::STATUS) => {
                let raw = text_value(child).unwrap_or_default();
                result.status = match raw.parse() {
                    Ok(n) => n,
                    Err(_) => {
                        log::warn!(
                            "ResolveRecipients: malformed top-level Status \"{raw}\"; defaulting to 1"
                        );
                        1
                    }
                };
            }
            (pages::RECIPIENTS, rr::RESPONSE) => {
                result
                    .responses
                    .push(parse_recipients_response_element(child));
            }
            _ => {}
        }
    }
    Ok(result)
}

/// Parse one Response sibling (per-To status + its Recipient suggestions).
fn parse_recipients_response_element(resp: &WbxmlElement) -> ResolveRecipientsResponse {
    use crate::wbxml::tags::{pages, recipients as rr};
    let mut out = ResolveRecipientsResponse::default();
    for child in &resp.children {
        match (child.page, child.token) {
            (pages::RECIPIENTS, rr::TO) => {
                out.to = text_value(child).unwrap_or_else(|e| {
                    // Undecodable To text is malformed server data; keep an
                    // empty echo but never drop the Response silently.
                    log::warn!("ResolveRecipients: undecodable Response To text: {e}");
                    String::new()
                });
            }
            (pages::RECIPIENTS, rr::STATUS) => {
                let raw = text_value(child).unwrap_or_default();
                out.status = match raw.parse() {
                    Ok(n) => n,
                    Err(_) => {
                        log::warn!(
                            "ResolveRecipients: malformed Response Status \"{raw}\"; defaulting to 1"
                        );
                        1
                    }
                };
            }
            (pages::RECIPIENTS, rr::RECIPIENT_COUNT) => {
                let raw = text_value(child).unwrap_or_default();
                out.recipient_count = match raw.parse() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        log::warn!(
                            "ResolveRecipients: malformed RecipientCount \"{raw}\"; leaving unset"
                        );
                        None
                    }
                };
            }
            (pages::RECIPIENTS, rr::RECIPIENT) => {
                out.recipients.push(parse_recipient_element(child));
            }
            _ => {}
        }
    }
    out
}

/// Parse one Recipient element by tag name. Certificate payloads are NOT
/// captured — Certificates contributes its Status/CertificateCount only.
fn parse_recipient_element(rec: &WbxmlElement) -> ResolvedRecipient {
    use crate::wbxml::tags::{pages, recipients as rr};
    let mut out = ResolvedRecipient::default();
    for child in &rec.children {
        match (child.page, child.token) {
            (pages::RECIPIENTS, rr::TYPE) => {
                let raw = text_value(child).unwrap_or_default();
                out.recipient_type = match raw.parse() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        log::warn!("ResolveRecipients: malformed Type \"{raw}\"; leaving unset");
                        None
                    }
                };
            }
            (pages::RECIPIENTS, rr::DISPLAY_NAME) => {
                out.display_name = text_value_opt(child);
            }
            (pages::RECIPIENTS, rr::EMAIL_ADDRESS) => {
                out.email_address = text_value_opt(child);
            }
            (pages::RECIPIENTS, rr::AVAILABILITY) => {
                for avail in &child.children {
                    match (avail.page, avail.token) {
                        (pages::RECIPIENTS, rr::STATUS) => {
                            let raw = text_value(avail).unwrap_or_default();
                            out.availability_status = match raw.parse() {
                                Ok(n) => Some(n),
                                Err(_) => {
                                    log::warn!(
                                        "ResolveRecipients: malformed Availability Status \"{raw}\"; leaving unset"
                                    );
                                    None
                                }
                            };
                        }
                        (pages::RECIPIENTS, rr::MERGED_FREE_BUSY) => {
                            // Verbatim: the digit-per-slot string is opaque
                            // to this layer — trimming/parsing would corrupt
                            // the caller's slot alignment.
                            out.merged_free_busy = text_value_opt(avail);
                        }
                        _ => {}
                    }
                }
            }
            (pages::RECIPIENTS, rr::CERTIFICATES) => {
                for cert in &child.children {
                    match (cert.page, cert.token) {
                        (pages::RECIPIENTS, rr::STATUS) => {
                            let raw = text_value(cert).unwrap_or_default();
                            out.certificates_status = match raw.parse() {
                                Ok(n) => Some(n),
                                Err(_) => {
                                    log::warn!(
                                        "ResolveRecipients: malformed Certificates Status \"{raw}\"; leaving unset"
                                    );
                                    None
                                }
                            };
                        }
                        (pages::RECIPIENTS, rr::CERTIFICATE_COUNT) => {
                            let raw = text_value(cert).unwrap_or_default();
                            out.certificate_count = match raw.parse() {
                                Ok(n) => Some(n),
                                Err(_) => {
                                    log::warn!(
                                        "ResolveRecipients: malformed CertificateCount \"{raw}\"; leaving unset"
                                    );
                                    None
                                }
                            };
                        }
                        // Certificate / MiniCertificate / the certificates'
                        // own RecipientCount: BY DESIGN not parsed — this
                        // client never requests cert bytes; status/count
                        // suffice for callers that surface retrieval health.
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

// ============================================================================
// Internal helpers
// ============================================================================

/// Format a `SystemTime` as an EAS UTC datetime string
/// (`yyyy-MM-dd'T'HH:mm:ss.fff'Z'`) — the same shape Android's
/// `Eas.formatDateTime` produces for Flag start/due dates. Implemented with
/// std-only civil-date math so no `chrono` runtime dependency is introduced.
pub fn format_eas_datetime_utc(t: std::time::SystemTime) -> String {
    let millis: i128 = match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i128,
        Err(e) => -(e.duration().as_millis() as i128),
    };
    let days = millis.div_euclid(86_400_000) as i64;
    let ms_of_day = millis.rem_euclid(86_400_000) as u64;
    let (y, m, d) = civil_from_days(days);
    let hour = ms_of_day / 3_600_000;
    let min = (ms_of_day % 3_600_000) / 60_000;
    let sec = (ms_of_day % 60_000) / 1_000;
    let ms = ms_of_day % 1_000;
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}.{ms:03}Z")
}

/// Days-from-Unix-epoch → (year, month, day), proleptic Gregorian calendar.
/// Howard Hinnant's `civil_from_days` algorithm (public domain).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ============================================================================
