//! Peer identity: an Ed25519 keypair whose public key *is* the peer id.
//!
//! Tying identity to a keypair is what lets reputation persist and be portable:
//! a peer can't cheaply throw away a bad history and reappear as someone new
//! without also discarding the key others have credited. Receipts ([`crate::receipt`])
//! and the ledger ([`crate::ledger`]) are all keyed on [`PeerId`].

use std::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// A peer's public identity — the 32-byte Ed25519 verifying key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PeerId([u8; 32]);

impl PeerId {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<PeerId, hex::FromHexError> {
        let mut b = [0u8; 32];
        hex::decode_to_slice(s, &mut b)?;
        Ok(PeerId(b))
    }

    /// Verify a 64-byte signature over `msg` against this identity. Returns false
    /// on any malformed key/signature rather than panicking.
    pub fn verify(&self, msg: &[u8], sig: &[u8]) -> bool {
        let Ok(vk) = VerifyingKey::from_bytes(&self.0) else {
            return false;
        };
        let Ok(arr) = <[u8; 64]>::try_from(sig) else {
            return false;
        };
        vk.verify_strict(msg, &Signature::from_bytes(&arr)).is_ok()
    }
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerId({}…)", &self.to_hex()[..12])
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A peer's secret identity — holds the signing key. Keep this private; share
/// only the [`PeerId`].
pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    /// Create a fresh random identity from the OS CSPRNG.
    pub fn generate() -> Identity {
        Identity {
            signing: SigningKey::generate(&mut rand_core::OsRng),
        }
    }

    /// Reconstruct an identity from a 32-byte seed (for persistence and for
    /// deterministic tests).
    pub fn from_seed(seed: [u8; 32]) -> Identity {
        Identity {
            signing: SigningKey::from_bytes(&seed),
        }
    }

    /// The 32-byte seed, to persist this identity. Treat as secret.
    pub fn seed(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    /// This identity's public peer id.
    pub fn peer_id(&self) -> PeerId {
        PeerId(self.signing.verifying_key().to_bytes())
    }

    /// Sign a message, returning the 64-byte signature.
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_round_trips() {
        let id = Identity::from_seed([7u8; 32]);
        let sig = id.sign(b"hello");
        assert!(id.peer_id().verify(b"hello", &sig));
    }

    #[test]
    fn tampered_message_fails() {
        let id = Identity::from_seed([7u8; 32]);
        let sig = id.sign(b"hello");
        assert!(!id.peer_id().verify(b"hell0", &sig));
    }

    #[test]
    fn other_keys_signature_fails() {
        let a = Identity::from_seed([1u8; 32]);
        let b = Identity::from_seed([2u8; 32]);
        let sig = a.sign(b"msg");
        assert!(!b.peer_id().verify(b"msg", &sig));
    }

    #[test]
    fn from_seed_is_deterministic() {
        assert_eq!(
            Identity::from_seed([9u8; 32]).peer_id(),
            Identity::from_seed([9u8; 32]).peer_id()
        );
    }

    #[test]
    fn peer_id_hex_round_trips() {
        let pid = Identity::from_seed([3u8; 32]).peer_id();
        assert_eq!(PeerId::from_hex(&pid.to_hex()).unwrap(), pid);
    }

    #[test]
    fn malformed_signature_is_rejected_not_panicked() {
        let pid = Identity::from_seed([4u8; 32]).peer_id();
        assert!(!pid.verify(b"msg", &[0u8; 10])); // wrong length
    }
}
