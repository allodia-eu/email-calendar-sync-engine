//! What opening an encrypted layer found, and the weaknesses worth telling a
//! reader about.

use core::fmt;

use serde::{Deserialize, Serialize};

/// Whether decrypted content was protected against modification in transit.
///
/// OpenPGP content is always authenticated when it decrypts at all: the engine
/// refuses the unprotected packet (RFC 9580 §5.7) and releases nothing when a
/// modification detection code or an AEAD tag fails (§13.7). S/MIME's
/// `enveloped-data` is unauthenticated CBC and still common, which is what the EFAIL
/// attacks exploited; a host decides what to show for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Integrity {
    /// A modification would have been detected, and none was.
    Authenticated,
    /// The encryption carries no integrity protection.
    Unauthenticated,
}

/// The name of an algorithm, as its mechanism's registry spells it.
///
/// OpenPGP names come from the RFC 9580 registries (`AES-256`, `OCB`, `SHA2-256`,
/// `RSA`). The engine reports algorithms by name rather than by a shared enum
/// because each mechanism has its own registry, and a name is all a host shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AlgorithmName(Box<str>);

impl AlgorithmName {
    /// Creates an algorithm name.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into().into_boxed_str())
    }

    /// Returns the name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AlgorithmName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A weakness a message was accepted despite, which the reader should be told
/// about.
///
/// RFC 9580 requires some of these to be surfaced: decrypting with a deprecated
/// cipher (§9.3), or with one missing from the reader's own preferences (§12.2);
/// and it discourages weak RSA (§12.4) and Elgamal (§12.6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Warning {
    /// The content was encrypted with a cipher its mechanism deprecates.
    DeprecatedCipher {
        /// The cipher used.
        cipher: AlgorithmName,
    },
    /// The content was encrypted with a cipher the reader's own certificate does not
    /// list among its preferences.
    CipherNotPreferred {
        /// The cipher used.
        cipher: AlgorithmName,
    },
    /// A public key is shorter than its mechanism recommends.
    WeakPublicKey {
        /// The public-key algorithm.
        algorithm: AlgorithmName,
        /// The key size, in bits.
        bits: u32,
    },
    /// A public-key algorithm its mechanism deprecates.
    DeprecatedPublicKeyAlgorithm {
        /// The public-key algorithm.
        algorithm: AlgorithmName,
    },
    /// A signature relies on a hash its mechanism deprecates, still accepted because
    /// of when it was made.
    DeprecatedHash {
        /// The hash algorithm.
        hash: AlgorithmName,
    },
}

/// A successful decryption, and what it was made with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decryption {
    /// Whether the content was integrity-protected.
    pub integrity: Integrity,
    /// The symmetric cipher.
    pub cipher: AlgorithmName,
    /// The AEAD mode, when the content used one; `None` when it did not (OpenPGP's
    /// version 1 encrypted data packet, for one, protects integrity another way).
    pub aead: Option<AlgorithmName>,
    /// Weaknesses the content was decrypted despite.
    pub warnings: Vec<Warning>,
}

/// The result of trying to open one encrypted layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecryptionOutcome {
    /// The layer decrypted.
    Decrypted(Decryption),
    /// The layer did not decrypt, and released nothing.
    Failed(DecryptionFailure),
}

/// Why an encrypted layer did not open.
///
/// Every failure releases **no** plaintext: an integrity failure found at the end of
/// the content does not hand back the part before it (RFC 9580 §13.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DecryptionFailure {
    /// None of the secret keys offered can decrypt the layer.
    NoMatchingKey,
    /// The content was modified, or cut short, after it was encrypted.
    IntegrityFailure,
    /// The layer could not be parsed.
    Malformed,
    /// The layer uses a version or an algorithm this engine does not implement, or
    /// one it refuses (such as encryption without integrity protection).
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algorithm_names_display_verbatim() {
        let name = AlgorithmName::new("AES-256");
        assert_eq!(name.as_str(), "AES-256");
        assert_eq!(name.to_string(), "AES-256");
    }

    #[test]
    fn outcomes_round_trip_through_json() {
        let outcomes = vec![
            DecryptionOutcome::Decrypted(Decryption {
                integrity: Integrity::Authenticated,
                cipher: AlgorithmName::new("AES-128"),
                aead: Some(AlgorithmName::new("OCB")),
                warnings: vec![Warning::WeakPublicKey {
                    algorithm: AlgorithmName::new("RSA"),
                    bits: 2048,
                }],
            }),
            DecryptionOutcome::Failed(DecryptionFailure::IntegrityFailure),
        ];
        let json = serde_json::to_string(&outcomes).expect("serialize");
        let back: Vec<DecryptionOutcome> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, outcomes);
    }
}
