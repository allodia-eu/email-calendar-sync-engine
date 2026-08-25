// SPDX-License-Identifier: MPL-2.0
// Ported from mailkit_arkts (user-owned; confirmed 2026-08-12). See ATTRIBUTIONS.md.

/// `ValidateCert` (`ValidateCert` page-11 token 0x05).
pub const VALIDATE_CERT: u8 = 0x05;
/// `Certificates` (`ValidateCert` page-11 token 0x06).
pub const CERTIFICATES: u8 = 0x06;
/// `Certificate` (`ValidateCert` page-11 token 0x07).
pub const CERTIFICATE: u8 = 0x07;
/// `CertificateChain` (`ValidateCert` page-11 token 0x08).
pub const CERTIFICATE_CHAIN: u8 = 0x08;
/// `CheckCrl` (`ValidateCert` page-11 token 0x09).
pub const CHECK_CRL: u8 = 0x09;
/// `Status` (`ValidateCert` page-11 token 0x0a).
pub const STATUS: u8 = 0x0A;
