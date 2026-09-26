//! Layers, and the one status a message's envelope reduces to.

use serde::{Deserialize, Serialize};

use super::{SenderBinding, SignatureVerdict};

/// How a signing layer carries its protected part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SignatureForm {
    /// The protected part stays readable beside a separate signature
    /// (`multipart/signed`, RFC 1847; RFC 3156 §5; RFC 8551 §3.5.3).
    Detached,
    /// The protected part is wrapped inside the signature object, and is read by
    /// unwrapping it (S/MIME `signed-data`, RFC 8551 §3.5.2).
    Embedded,
}

/// What one cryptographic layer does (RFC 9787 §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LayerKind {
    /// A signature over the protected part.
    Signed(SignatureForm),
    /// Encryption of the protected part.
    Encrypted,
    /// Encryption whose content carries a signature inside it: PGP/MIME's combined
    /// form (RFC 3156 §6.2, RFC 9787 §4.4.1). A layer is recognised as
    /// [`Encrypted`](Self::Encrypted) and becomes this once opening it finds a
    /// signature inside.
    SignedAndEncrypted,
}

impl LayerKind {
    /// Whether the layer encrypts its protected part.
    pub fn encrypts(self) -> bool {
        matches!(self, Self::Encrypted | Self::SignedAndEncrypted)
    }
}

/// One layer of an envelope with the verdicts on its signatures, as
/// [`Summary::derive`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerSignatures<'a> {
    /// What the layer does.
    pub kind: LayerKind,
    /// The verdicts on the layer's signatures; empty for a layer that only encrypts,
    /// or a signed layer nobody has checked yet.
    pub verdicts: &'a [SignatureVerdict],
}

/// The cryptographic status of a message (RFC 9787 §3.1, §4.6): one per message,
/// derived from its envelope, never from errant layers (§4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Summary {
    /// No end-to-end protection that holds. A cleartext message whose signature does
    /// not check out is this too (§3.1, §6.4).
    Unprotected,
    /// Signed by the apparent sender, and unaltered.
    Verified,
    /// Encrypted, and signed inside the encryption by the apparent sender.
    Confidential,
    /// Encrypted, without a signature from the apparent sender inside the
    /// encryption. An error state: the reader cannot tell who sent it (§3.1).
    EncryptedButUnverified,
}

impl Summary {
    /// Derives the summary of an envelope whose payload was reached.
    ///
    /// `layers` runs from the outermost layer inwards. A signature counts only when
    /// it is **inside** every encryption layer: one outside the encryption signed
    /// the ciphertext, which does not tie its signer to what the reader sees
    /// (RFC 9787 §5.2). And it counts only when it is
    /// [good](SignatureVerdict::Good) and its certificate
    /// [binds the sender](SenderBinding::Matches) (§3.1: "a valid signature from the
    /// apparent sender").
    ///
    /// An empty envelope is [`Unprotected`](Self::Unprotected).
    pub fn derive(layers: &[LayerSignatures<'_>]) -> Self {
        let innermost_encryption = layers.iter().rposition(|layer| layer.kind.encrypts());
        let counted = &layers[innermost_encryption.unwrap_or(0)..];
        let verified = counted
            .iter()
            .flat_map(|layer| layer.verdicts)
            .filter_map(SignatureVerdict::as_good)
            .any(|good| good.sender == SenderBinding::Matches);
        match (innermost_encryption.is_some(), verified) {
            (false, false) => Self::Unprotected,
            (false, true) => Self::Verified,
            (true, true) => Self::Confidential,
            (true, false) => Self::EncryptedButUnverified,
        }
    }
}

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
