// SPDX-License-Identifier: MPL-2.0
//! Provision command (MS-ASPROV). Two-phase handshake:
//!   Phase 1: client requests the policy → server returns a TEMP PolicyKey
//!           and the policy XML in <Data>.
//!   Phase 2: client acknowledges with the temp PolicyKey and <Status>1</Status>
//!           → server returns a PERMANENT PolicyKey that the client must send
//!           in the X-MS-PolicyKey header on every subsequent command.
//!
//! RemoteWipe: if the server returns <RemoteWipe>, we surface it as a
//! permanent error — never auto-execute. The UI is a follow-up.

use crate::wbxml::{
    WbxmlError,
    tags::{pages, provision},
    types::{WbxmlElement, WbxmlValue},
};

const MS_EAS_PROVISIONING_WBXML: &str = "MS-EAS-Provisioning-WBXML";

/// Build the Phase-1 Provision request (no policy key yet).
///
/// The request EMBEDS a `<DeviceInformation>` element as its FIRST child
/// (before `<Policies>`), on the Settings code page (18) — per
/// ExchangeActiveSync-master (ASProvisionRequest.cs) and Android Gmail
/// (EasProvision.java). Exchange 2019 answers 165 (DeviceInformationRequired)
/// when DI is missing, and refuses the standalone Settings command with 142
/// until provisioning completes, so embedding DI here is the only way to
/// break that bootstrap deadlock. The serializer emits SWITCH_PAGE tokens
/// for the 14 → 18 → 14 page transitions automatically.
pub fn build_provision_phase1_request(
    model: &str,
    friendly_name: &str,
    os: &str,
    os_language: &str,
) -> WbxmlElement {
    WbxmlElement::container(
        pages::PROVISION,
        provision::PROVISION,
        vec![
            crate::commands::device_information_element(model, friendly_name, os, os_language),
            WbxmlElement::container(
                pages::PROVISION,
                provision::POLICIES,
                vec![WbxmlElement::container(
                    pages::PROVISION,
                    provision::POLICY,
                    vec![WbxmlElement::text(
                        pages::PROVISION,
                        provision::POLICY_TYPE,
                        MS_EAS_PROVISIONING_WBXML,
                    )],
                )],
            ),
        ],
    )
}

/// Build the Phase-2 ack: client has received the temp policy and accepts it
/// (Status 1 = client compliant). Server replies with the permanent key.
pub fn build_provision_phase2_request(temp_policy_key: &str) -> WbxmlElement {
    WbxmlElement::container(
        pages::PROVISION,
        provision::PROVISION,
        vec![WbxmlElement::container(
            pages::PROVISION,
            provision::POLICIES,
            vec![WbxmlElement::container(
                pages::PROVISION,
                provision::POLICY,
                vec![
                    WbxmlElement::text(
                        pages::PROVISION,
                        provision::POLICY_TYPE,
                        MS_EAS_PROVISIONING_WBXML,
                    ),
                    WbxmlElement::text(pages::PROVISION, provision::POLICY_KEY, temp_policy_key),
                    WbxmlElement::text(pages::PROVISION, provision::STATUS, "1"),
                ],
            )],
        )],
    )
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvisionResult {
    /// Top-level Provision Status. 1 = success.
    pub status: u32,
    /// Permanent (Phase 2) or temp (Phase 1) policy key returned by the server.
    pub policy_key: Option<String>,
    /// True if the server sent a `<RemoteWipe>` element. Caller MUST surface,
    /// never auto-wipe.
    pub remote_wipe: bool,
}

/// Parse a Provision response. Extracts the top-level Status, the nested
/// Policy's PolicyKey, and detects a RemoteWipe element.
pub fn parse_provision_response(root: &WbxmlElement) -> Result<ProvisionResult, WbxmlError> {
    let mut out = ProvisionResult {
        status: 1,
        ..Default::default()
    };
    for child in &root.children {
        // Match on (page, token) — provision page is unambiguous in the root.
        match (child.page, child.token) {
            (pages::PROVISION, provision::STATUS) => {
                out.status = text(child).parse().unwrap_or(1);
            }
            (pages::PROVISION, provision::POLICIES) => {
                if let Some(key) = find_policy_key(child) {
                    out.policy_key = Some(key);
                }
            }
            (pages::PROVISION, provision::REMOTE_WIPE) => {
                out.remote_wipe = true;
            }
            _ => {}
        }
    }
    Ok(out)
}

fn find_policy_key(policies_el: &WbxmlElement) -> Option<String> {
    for policy in &policies_el.children {
        if policy.token != provision::POLICY {
            continue;
        }
        for field in &policy.children {
            if field.token == provision::POLICY_KEY {
                return Some(text(field));
            }
        }
    }
    None
}

fn text(el: &WbxmlElement) -> String {
    match &el.value {
        WbxmlValue::Text(s) => s.clone(),
        WbxmlValue::Opaque(b) => String::from_utf8_lossy(b).into_owned(),
        WbxmlValue::Empty => String::new(),
    }
}

#[cfg(test)]
mod tests {
    //! Provision command round-trip tests. See
    //! docs/superpowers/plans/2026-06-30-sync-engine-phase3-eas-hardening.md
    use super::*;
    use crate::wbxml::{
        tags::{pages, provision},
        types::{WbxmlElement, WbxmlValue},
    };

    /// Pull a text leaf's string. `WbxmlElement` has no `text_str()` helper on
    /// the codec (intentionally; the codec is kept protocol-agnostic), so we
    /// inline the extraction here.
    fn text_str(el: &WbxmlElement) -> String {
        match &el.value {
            WbxmlValue::Text(s) => s.clone(),
            _ => panic!(
                "expected Text value on (page={}, token={}), got {:?}",
                el.page, el.token, el.value
            ),
        }
    }

    /// Device-info arguments reused across the Phase-1 builder tests. These
    /// mirror the values `EasClient::provision_phase1` passes in production.
    const TEST_MODEL: &str = "KylinsMail";
    const TEST_FRIENDLY_NAME: &str = "Kylins Mail desktop";
    const TEST_OS: &str = "windows";
    const TEST_OS_LANGUAGE: &str = "en-US";

    #[test]
    fn phase1_request_has_policy_type_ms_eas_provisioning_wbxml() {
        let tree = build_provision_phase1_request(
            TEST_MODEL,
            TEST_FRIENDLY_NAME,
            TEST_OS,
            TEST_OS_LANGUAGE,
        );
        // Root: Provision (page 14, 0x05)
        assert_eq!(tree.page, pages::PROVISION);
        assert_eq!(tree.token, provision::PROVISION);
        // Walk: Provision > Policies > Policy > PolicyType == "MS-EAS-Provisioning-WBXML"
        let policies = tree
            .children
            .iter()
            .find(|c| c.token == provision::POLICIES)
            .expect("Policies");
        let policy = policies
            .children
            .iter()
            .find(|c| c.token == provision::POLICY)
            .expect("Policy");
        let ptype = policy
            .children
            .iter()
            .find(|c| c.token == provision::POLICY_TYPE)
            .expect("PolicyType");
        assert_eq!(text_str(ptype), "MS-EAS-Provisioning-WBXML");
    }

    /// Fix round 1 (2026-08-01): live probe against Exchange 2019 returned
    /// Provision status 165 (DeviceInformationRequired) while the Settings
    /// retry was itself gated on provisioning (status 142) — a bootstrap
    /// deadlock. Per ExchangeActiveSync-master (ASProvisionRequest.cs) and
    /// Android Gmail (EasProvision.java), the Phase-1 Provision request must
    /// EMBED a <DeviceInformation> element as its FIRST child (before
    /// <Policies>), using the Settings code page (18) for the DI subtree.
    #[test]
    fn phase1_request_embeds_device_information_before_policies() {
        use crate::wbxml::tags::settings;

        let tree = build_provision_phase1_request(
            TEST_MODEL,
            TEST_FRIENDLY_NAME,
            TEST_OS,
            TEST_OS_LANGUAGE,
        );
        assert_eq!(
            (tree.page, tree.token),
            (pages::PROVISION, provision::PROVISION)
        );
        assert_eq!(
            tree.children.len(),
            2,
            "Phase-1 Provision must have exactly DeviceInformation + Policies"
        );

        // FIRST child: DeviceInformation on the Settings page (18).
        let di = &tree.children[0];
        assert_eq!(
            (di.page, di.token),
            (pages::SETTINGS, settings::DEVICE_INFORMATION),
            "DeviceInformation must be the first child of Provision, page 18"
        );
        let set = &di.children[0];
        assert_eq!((set.page, set.token), (pages::SETTINGS, settings::SET));
        let tokens: Vec<u8> = set.children.iter().map(|c| c.token).collect();
        assert_eq!(
            tokens,
            vec![
                settings::MODEL,
                settings::FRIENDLY_NAME,
                settings::OS,
                settings::OS_LANGUAGE
            ]
        );
        assert!(
            set.children.iter().all(|c| c.page == pages::SETTINGS),
            "every DI leaf must carry the Settings page (18)"
        );
        assert_eq!(text_str(&set.children[0]), TEST_MODEL);
        assert_eq!(text_str(&set.children[1]), TEST_FRIENDLY_NAME);
        assert_eq!(text_str(&set.children[2]), TEST_OS);
        assert_eq!(text_str(&set.children[3]), TEST_OS_LANGUAGE);

        // SECOND child: Policies, back on the Provision page (14).
        let policies = &tree.children[1];
        assert_eq!(
            (policies.page, policies.token),
            (pages::PROVISION, provision::POLICIES),
            "Policies must be the second child, page 14"
        );
    }

    #[test]
    fn phase1_request_embedded_di_round_trips_through_wbxml_codec() {
        use crate::wbxml::{deserialize_to_tree, serialize_tree, tags::settings};

        let tree = build_provision_phase1_request(
            TEST_MODEL,
            TEST_FRIENDLY_NAME,
            TEST_OS,
            TEST_OS_LANGUAGE,
        );
        let bytes = serialize_tree(&tree).expect("serialize");
        let back = deserialize_to_tree(&bytes).expect("deserialize");

        // The page switch 14 → 18 → 14 must survive the codec: the server
        // reads DI off the Settings code page, so a serializer that failed
        // to emit SWITCH_PAGE for the nested page change would corrupt this.
        let di = back
            .children
            .iter()
            .find(|c| c.page == pages::SETTINGS && c.token == settings::DEVICE_INFORMATION)
            .expect("DeviceInformation survived round-trip on page 18");
        let set = &di.children[0];
        assert_eq!((set.page, set.token), (pages::SETTINGS, settings::SET));
        let model = set
            .children
            .iter()
            .find(|c| c.token == settings::MODEL)
            .expect("Model leaf");
        assert_eq!(text_str(model), TEST_MODEL);
        assert!(
            back.children
                .iter()
                .any(|c| c.page == pages::PROVISION && c.token == provision::POLICIES),
            "Policies survived round-trip on page 14"
        );
    }

    #[test]
    fn parse_phase1_response_extracts_temp_policy_key() {
        // Build a tree mimicking:
        // <Provision><Status>1</Status><Policies><Policy><PolicyType>...</PolicyType>
        //   <Status>1</Status><PolicyKey>{TEMP-123}</PolicyKey><Data>...</Data></Policy></
        // Policies></Provision>
        let tree = WbxmlElement::container(
            pages::PROVISION,
            provision::PROVISION,
            vec![
                WbxmlElement::text(pages::PROVISION, provision::STATUS, "1"),
                WbxmlElement::container(
                    pages::PROVISION,
                    provision::POLICIES,
                    vec![WbxmlElement::container(
                        pages::PROVISION,
                        provision::POLICY,
                        vec![
                            WbxmlElement::text(
                                pages::PROVISION,
                                provision::POLICY_TYPE,
                                "MS-EAS-Provisioning-WBXML",
                            ),
                            WbxmlElement::text(pages::PROVISION, provision::STATUS, "1"),
                            WbxmlElement::text(
                                pages::PROVISION,
                                provision::POLICY_KEY,
                                "{TEMP-123}",
                            ),
                        ],
                    )],
                ),
            ],
        );
        let r = parse_provision_response(&tree).unwrap();
        assert_eq!(r.status, 1);
        assert_eq!(r.policy_key.as_deref(), Some("{TEMP-123}"));
        assert!(!r.remote_wipe);
    }

    #[test]
    fn parse_response_flags_remote_wipe() {
        // <Provision><Status>1</Status><RemoteWipe>...</RemoteWipe></Provision>
        let tree = WbxmlElement::container(
            pages::PROVISION,
            provision::PROVISION,
            vec![
                WbxmlElement::text(pages::PROVISION, provision::STATUS, "1"),
                WbxmlElement::empty(pages::PROVISION, provision::REMOTE_WIPE),
            ],
        );
        let r = parse_provision_response(&tree).unwrap();
        assert!(r.remote_wipe, "must flag RemoteWipe so caller surfaces it");
    }

    /// WBXML codec round-trip integration test.
    /// The tests above build the request tree and parse a hand-built response
    /// tree, but never push the tree through the WBXML codec. This proves
    /// `build_provision_phase1_request()` → bytes → tree survives a full
    /// serialize/deserialize cycle with the PolicyType leaf intact, so the
    /// orchestrator can rely on the codec for the live transport path.
    #[test]
    fn phase1_request_round_trips_through_wbxml_codec() {
        use crate::wbxml::{deserialize_to_tree, serialize_tree};

        let tree = build_provision_phase1_request(
            TEST_MODEL,
            TEST_FRIENDLY_NAME,
            TEST_OS,
            TEST_OS_LANGUAGE,
        );
        let bytes = serialize_tree(&tree).expect("serialize Phase-1 Provision request");
        assert!(
            !bytes.is_empty(),
            "serializer must emit a non-empty WBXML document"
        );
        let back = deserialize_to_tree(&bytes).expect("deserialize round-tripped bytes");

        // Root must still be Provision (page 14, token 0x05).
        assert_eq!(back.page, pages::PROVISION);
        assert_eq!(back.token, provision::PROVISION);

        // Walk Provision > Policies > Policy > PolicyType and confirm the
        // leaf text survived the codec round-trip. This is the exact contract
        // the live request relies on (server rejects empty/wrong PolicyType).
        let policies = back
            .children
            .iter()
            .find(|c| c.token == provision::POLICIES)
            .expect("Policies container survived round-trip");
        let policy = policies
            .children
            .iter()
            .find(|c| c.token == provision::POLICY)
            .expect("Policy container survived round-trip");
        let ptype = policy
            .children
            .iter()
            .find(|c| c.token == provision::POLICY_TYPE)
            .expect("PolicyType leaf survived round-trip");
        assert_eq!(
            text_str(ptype),
            "MS-EAS-Provisioning-WBXML",
            "PolicyType leaf text must survive the WBXML codec round-trip"
        );
    }
}
