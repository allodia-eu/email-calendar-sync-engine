//! Builds certificates packet by packet, so each edge case is exactly the one named.
//!
//! Keys are Ed25519 and X25519 (Ed25519Legacy and Curve25519Legacy for v4), made
//! from a seeded generator so every run forges the same bytes.

use pgp::{
    composed::{KeyType, SignedKeyDetails, SignedPublicKey, SignedPublicSubKey},
    crypto::{ecc_curve::ECCCurve, hash::HashAlgorithm},
    packet::{
        KeyFlags, PubKeyInner, PublicKey, PublicSubkey, RevocationCode, SecretKey, SecretSubkey,
        Signature, SignatureConfig, SignatureType, Subpacket, SubpacketData, UserId,
    },
    ser::Serialize,
    types::{Duration, KeyDetails, KeyVersion, Password, SignedUser, SigningKey, Tag, Timestamp},
};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

/// A primary key with its secret half, to sign with.
pub struct Primary {
    pub public: PublicKey,
    secret: SecretKey,
}

/// A subkey with its secret half.
pub struct Sub {
    pub public: PublicSubkey,
    secret: SecretSubkey,
}

/// What a subkey is for.
#[derive(Clone, Copy)]
pub enum SubKind {
    Encrypt,
    Sign,
}

/// What goes into one signature's hashed area.
#[derive(Clone, Default)]
pub struct Sig {
    created: u32,
    flags: Option<u8>,
    unhashed_flags: Option<u8>,
    expires: Option<u32>,
    signature_expires: Option<u32>,
    primary: bool,
    reason: Option<RevocationCode>,
    embedded: Option<Signature>,
}

impl Sig {
    /// A signature made at `created` (Unix seconds).
    pub fn at(created: u32) -> Self {
        Self {
            created,
            ..Self::default()
        }
    }

    pub fn flags(mut self, flags: u8) -> Self {
        self.flags = Some(flags);
        self
    }

    pub fn unhashed_flags(mut self, flags: u8) -> Self {
        self.unhashed_flags = Some(flags);
        self
    }

    /// Key Expiration Time, in seconds after the key's creation.
    pub fn expires(mut self, seconds: u32) -> Self {
        self.expires = Some(seconds);
        self
    }

    /// Signature Expiration Time, in seconds after the signature's creation.
    pub fn signature_expires(mut self, seconds: u32) -> Self {
        self.signature_expires = Some(seconds);
        self
    }

    pub fn primary(mut self) -> Self {
        self.primary = true;
        self
    }

    pub fn reason(mut self, reason: RevocationCode) -> Self {
        self.reason = Some(reason);
        self
    }

    pub fn embedded(mut self, signature: Signature) -> Self {
        self.embedded = Some(signature);
        self
    }
}

pub struct Forge {
    rng: ChaCha8Rng,
}

impl Forge {
    pub fn new() -> Self {
        Self {
            rng: ChaCha8Rng::seed_from_u64(9580),
        }
    }

    /// An Ed25519 primary key; version 6 or version 4 (Ed25519Legacy).
    pub fn primary(&mut self, version: KeyVersion, created: u32) -> Primary {
        let kind = match version {
            KeyVersion::V6 => KeyType::Ed25519,
            _ => KeyType::Ed25519Legacy,
        };
        let (params, secret) = kind.generate(&mut self.rng).expect("key material");
        let inner = PubKeyInner::new(
            version,
            kind.to_alg(),
            Timestamp::from_secs(created),
            None,
            params,
        )
        .expect("public key");
        let public = PublicKey::from_inner(inner).expect("public key packet");
        let secret = SecretKey::new(public.clone(), secret).expect("secret key packet");
        Primary { public, secret }
    }

    /// A subkey of the same version as `primary`.
    pub fn subkey(&mut self, primary: &Primary, kind: SubKind, created: u32) -> Sub {
        let version = primary.public.version();
        let key_type = match (kind, version) {
            (SubKind::Encrypt, KeyVersion::V6) => KeyType::X25519,
            (SubKind::Encrypt, _) => KeyType::ECDH(ECCCurve::Curve25519Legacy),
            (SubKind::Sign, KeyVersion::V6) => KeyType::Ed25519,
            (SubKind::Sign, _) => KeyType::Ed25519Legacy,
        };
        let (params, secret) = key_type.generate(&mut self.rng).expect("key material");
        let inner = PubKeyInner::new(
            version,
            key_type.to_alg(),
            Timestamp::from_secs(created),
            None,
            params,
        )
        .expect("public subkey");
        let public = PublicSubkey::from_inner(inner).expect("public subkey packet");
        let secret = SecretSubkey::new(public.clone(), secret).expect("secret subkey packet");
        Sub { public, secret }
    }

    fn config(
        &mut self,
        signer: &impl SigningKey,
        typ: SignatureType,
        spec: Sig,
    ) -> SignatureConfig {
        let mut config = match signer.version() {
            KeyVersion::V6 => SignatureConfig::v6(
                &mut self.rng,
                typ,
                signer.algorithm(),
                HashAlgorithm::Sha256,
            )
            .expect("v6 config"),
            _ => SignatureConfig::v4(typ, signer.algorithm(), HashAlgorithm::Sha256),
        };
        let mut hashed = vec![
            Subpacket::critical(SubpacketData::SignatureCreationTime(Timestamp::from_secs(
                spec.created,
            ))),
            Subpacket::regular(SubpacketData::IssuerFingerprint(signer.fingerprint())),
        ];
        if let Some(flags) = spec.flags {
            hashed.push(Subpacket::critical(SubpacketData::KeyFlags(key_flags(
                flags,
            ))));
        }
        if let Some(expires) = spec.expires {
            hashed.push(Subpacket::critical(SubpacketData::KeyExpirationTime(
                Duration::from_secs(expires),
            )));
        }
        if let Some(expires) = spec.signature_expires {
            hashed.push(Subpacket::critical(SubpacketData::SignatureExpirationTime(
                Duration::from_secs(expires),
            )));
        }
        if spec.primary {
            hashed.push(Subpacket::regular(SubpacketData::IsPrimary(true)));
        }
        if let Some(reason) = spec.reason {
            hashed.push(Subpacket::regular(SubpacketData::RevocationReason(
                reason,
                Vec::new().into(),
            )));
        }
        if let Some(embedded) = spec.embedded {
            hashed.push(Subpacket::regular(SubpacketData::EmbeddedSignature(
                Box::new(embedded),
            )));
        }
        config.hashed_subpackets = hashed.into_iter().map(|p| p.expect("subpacket")).collect();
        if let Some(flags) = spec.unhashed_flags {
            config.unhashed_subpackets = vec![
                Subpacket::regular(SubpacketData::KeyFlags(key_flags(flags))).expect("subpacket"),
            ];
        }
        config
    }

    pub fn direct(&mut self, primary: &Primary, spec: Sig) -> Signature {
        self.config(&primary.secret, SignatureType::Key, spec)
            .sign_key(&primary.secret, &Password::empty(), &primary.public)
            .expect("direct key signature")
    }

    pub fn key_revocation(&mut self, primary: &Primary, spec: Sig) -> Signature {
        self.config(&primary.secret, SignatureType::KeyRevocation, spec)
            .sign_key(&primary.secret, &Password::empty(), &primary.public)
            .expect("key revocation")
    }

    pub fn certify(&mut self, primary: &Primary, user_id: &UserId, spec: Sig) -> Signature {
        self.user_id_signature(primary, user_id, SignatureType::CertPositive, spec)
    }

    pub fn revoke_user_id(&mut self, primary: &Primary, user_id: &UserId, spec: Sig) -> Signature {
        self.user_id_signature(primary, user_id, SignatureType::CertRevocation, spec)
    }

    fn user_id_signature(
        &mut self,
        primary: &Primary,
        user_id: &UserId,
        typ: SignatureType,
        spec: Sig,
    ) -> Signature {
        self.config(&primary.secret, typ, spec)
            .sign_certification(
                &primary.secret,
                &primary.public,
                &Password::empty(),
                Tag::UserId,
                user_id,
            )
            .expect("certification")
    }

    pub fn bind(&mut self, primary: &Primary, sub: &Sub, spec: Sig) -> Signature {
        self.config(&primary.secret, SignatureType::SubkeyBinding, spec)
            .sign_subkey_binding(
                &primary.secret,
                &primary.public,
                &Password::empty(),
                &sub.public,
            )
            .expect("subkey binding")
    }

    pub fn revoke_subkey(&mut self, primary: &Primary, sub: &Sub, spec: Sig) -> Signature {
        self.config(&primary.secret, SignatureType::SubkeyRevocation, spec)
            .sign_subkey_binding(
                &primary.secret,
                &primary.public,
                &Password::empty(),
                &sub.public,
            )
            .expect("subkey revocation")
    }

    /// The 0x19 back-signature a signing subkey makes over its primary key.
    pub fn back_signature(&mut self, primary: &Primary, sub: &Sub, created: u32) -> Signature {
        self.config(&sub.secret, SignatureType::KeyBinding, Sig::at(created))
            .sign_primary_key_binding(
                &sub.secret,
                &sub.public,
                &Password::empty(),
                &primary.public,
            )
            .expect("back-signature")
    }
}

fn key_flags(bits: u8) -> KeyFlags {
    KeyFlags::try_from_reader(&[bits][..]).expect("key flags")
}

pub fn user_id(text: &str) -> UserId {
    UserId::from_str(pgp::types::PacketHeaderVersion::default(), text).expect("user id")
}

/// A certificate being assembled.
pub struct Cert {
    primary: PublicKey,
    direct: Vec<Signature>,
    revocations: Vec<Signature>,
    users: Vec<SignedUser>,
    subkeys: Vec<SignedPublicSubKey>,
}

impl Cert {
    pub fn new(primary: &Primary) -> Self {
        Self {
            primary: primary.public.clone(),
            direct: Vec::new(),
            revocations: Vec::new(),
            users: Vec::new(),
            subkeys: Vec::new(),
        }
    }

    pub fn direct(mut self, signature: Signature) -> Self {
        self.direct.push(signature);
        self
    }

    pub fn revocation(mut self, signature: Signature) -> Self {
        self.revocations.push(signature);
        self
    }

    pub fn user(mut self, user_id: UserId, signatures: Vec<Signature>) -> Self {
        self.users.push(SignedUser::new(user_id, signatures));
        self
    }

    pub fn subkey(mut self, sub: &Sub, signatures: Vec<Signature>) -> Self {
        self.subkeys
            .push(SignedPublicSubKey::new(sub.public.clone(), signatures));
        self
    }

    /// The transferable public key, as binary packets.
    pub fn bytes(self) -> Vec<u8> {
        let details = SignedKeyDetails::new(self.revocations, self.direct, self.users, Vec::new());
        SignedPublicKey::new(self.primary, details, self.subkeys)
            .to_bytes()
            .expect("serialize")
    }
}
