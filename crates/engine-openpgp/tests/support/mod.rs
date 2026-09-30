//! Shared helpers: instants, fixtures, and the forge.

#![allow(dead_code, unreachable_pub)]

pub mod forge;

use engine_core::time::UtcDateTime;

/// The instant at midnight UTC on `date` (`YYYY-MM-DD`).
pub fn at(date: &str) -> UtcDateTime {
    format!("{date}T00:00:00Z").parse().expect("a date")
}

/// The same instant as OpenPGP's four-octet seconds.
pub fn secs(date: &str) -> u32 {
    u32::try_from(at(date).unix_seconds()).expect("after 1970")
}

/// A day, in seconds.
pub const DAY: u32 = 86_400;

/// A certificate made by GnuPG (`tests/fixtures/gnupg/make.sh`).
pub fn gnupg(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/gnupg/{name}.asc",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// A detached signature GnuPG made over [`signed_entity`].
pub fn gnupg_signature(name: &str) -> Vec<u8> {
    gnupg(&format!("signatures/{name}"))
}

/// The MIME entity the GnuPG signatures cover.
pub fn signed_entity() -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/gnupg/signed-entity.txt",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// RFC 9580 Appendix A.3: a version 6 certificate with no User ID.
pub const RFC9580_A3: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----

xioGY4d/4xsAAAAg+U2nu0jWCmHlZ3BqZYfQMxmZu52JGggkLq2EVD34laPCsQYf
GwoAAABCBYJjh3/jAwsJBwUVCg4IDAIWAAKbAwIeCSIhBssYbE8GCaaX5NUt+mxy
KwwfHifBilZwj2Ul7Ce62azJBScJAgcCAAAAAK0oIBA+LX0ifsDm185Ecds2v8lw
gyU2kCcUmKfvBXbAf6rhRYWzuQOwEn7E/aLwIwRaLsdry0+VcallHhSu4RN6HWaE
QsiPlR4zxP/TP7mhfVEe7XWPxtnMUMtf15OyA51YBM4qBmOHf+MZAAAAIIaTJINn
+eUBXbki+PSAld2nhJh/LVmFsS+60WyvXkQ1wpsGGBsKAAAALAWCY4d/4wKbDCIh
BssYbE8GCaaX5NUt+mxyKwwfHifBilZwj2Ul7Ce62azJAAAAAAQBIKbpGG2dWTX8
j+VjFM21J0hqWlEg+bdiojWnKfA5AQpWUWtnNwDEM0g12vYxoWM8Y81W+bHBw805
I8kWVkXU6vFOi+HWvv/ira7ofJu16NnoUkhclkUrk0mXubZvyl4GBg==
-----END PGP PUBLIC KEY BLOCK-----
";

/// RFC 9580 Appendix A.1: a bare version 4 Ed25519Legacy key packet, unsigned.
pub const RFC9580_A1: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----

xjMEU/NfCxYJKwYBBAHaRw8BAQdAPwmJlL3ZFu1AUxl5NOSofIBzOhKA1i+AEJku
Q+47JAY=
-----END PGP PUBLIC KEY BLOCK-----
";

/// Uppercase hex, the way the RFC prints fingerprints.
pub fn hex(bytes: &[u8]) -> String {
    engine_core::e2e::CertificateId::open_pgp(bytes)
        .map(|id| id.to_string())
        .expect("a fingerprint")
}
