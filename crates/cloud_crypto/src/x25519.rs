//! X25519 ECDH for the SentryCloud pairing + rekey flows.
//!
//! Two distinct lifetimes:
//!
//! * **Ephemeral** — used during pairing handshake and during rekey on
//!   the browser side. Generated, used once, discarded. We use
//!   `ring::agreement::EphemeralPrivateKey` for these so the privkey is
//!   opaque-by-design and zeroed on drop.
//!
//! * **Long-term** — generated at pairing time on the Pi, persisted
//!   wrapped in `cloud-credentials.json`, used at rekey time
//!   (ENCRYPTION.md §16) to ECDH against a fresh browser-side ephemeral
//!   pubkey. ring's `agreement` API does not expose private-key bytes
//!   for serialize/restore; for this we use `x25519-dalek 2`'s
//!   `StaticSecret`. Project plan "Known protocol gap" Option 1.

use ring::agreement;
use ring::rand::{SecureRandom, SystemRandom};
use x25519_dalek::{PublicKey as DalekPublic, StaticSecret};

use crate::errors::CryptoError;

/// Length of an X25519 public or private key in bytes.
pub const KEY_BYTES: usize = 32;

// ----- Ephemeral keypair (ring::agreement) -------------------------------

/// One-shot ephemeral X25519 private key. Generates a fresh keypair on
/// construction; [`compute_shared`] consumes it (the privkey is dropped
/// after one ECDH operation, by design).
pub struct EphemeralPrivate {
    inner: agreement::EphemeralPrivateKey,
}

impl EphemeralPrivate {
    /// Generate a fresh ephemeral keypair.
    pub fn generate() -> Result<Self, CryptoError> {
        let rng = SystemRandom::new();
        let inner = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng)
            .map_err(|_| CryptoError::X25519Failed)?;
        Ok(Self { inner })
    }

    /// Return the corresponding public key as raw 32 bytes.
    pub fn public_bytes(&self) -> Result<[u8; KEY_BYTES], CryptoError> {
        let pk = self
            .inner
            .compute_public_key()
            .map_err(|_| CryptoError::X25519Failed)?;
        let bytes: [u8; KEY_BYTES] = pk
            .as_ref()
            .try_into()
            .map_err(|_| CryptoError::X25519Failed)?;
        Ok(bytes)
    }

    /// Consume the ephemeral privkey to derive the shared secret with
    /// `their_public`. Returns the raw 32 bytes of the X25519 shared
    /// secret (apply HKDF on top before using as a KEK).
    pub fn compute_shared(self, their_public: &[u8; KEY_BYTES]) -> Result<[u8; 32], CryptoError> {
        let peer = agreement::UnparsedPublicKey::new(&agreement::X25519, their_public);
        agreement::agree_ephemeral(self.inner, &peer, |shared| {
            let mut out = [0u8; 32];
            out.copy_from_slice(shared);
            out
        })
        .map_err(|_| CryptoError::X25519Failed)
    }
}

// ----- Long-term keypair (x25519-dalek::StaticSecret) --------------------

/// Persistable X25519 private key. The Pi generates one of these at
/// pairing time, wraps the 32 raw seed bytes under the local wrap key,
/// and stores the wrapped form in `cloud-credentials.json`. On each
/// cold start it unwraps + rebuilds the StaticSecret from the seed.
///
/// `x25519-dalek::StaticSecret` clamps the scalar on construction per
/// RFC 7748, so the raw seed bytes round-trip through `from_seed`.
#[derive(Clone)]
pub struct LongTermPrivate {
    inner: StaticSecret,
}

impl LongTermPrivate {
    /// Generate a fresh long-term keypair using OS randomness.
    pub fn generate() -> Result<Self, CryptoError> {
        let mut seed = [0u8; KEY_BYTES];
        SystemRandom::new()
            .fill(&mut seed)
            .map_err(|_| CryptoError::X25519Failed)?;
        Ok(Self {
            inner: StaticSecret::from(seed),
        })
    }

    /// Reconstruct from previously-wrapped seed bytes.
    pub fn from_seed(seed: [u8; KEY_BYTES]) -> Self {
        Self {
            inner: StaticSecret::from(seed),
        }
    }

    /// Raw 32-byte seed for at-rest wrapping. After wrapping, drop this
    /// and re-derive on demand. The bytes returned are the clamped
    /// scalar — same value that would round-trip through `from_seed`.
    pub fn to_seed(&self) -> [u8; KEY_BYTES] {
        self.inner.to_bytes()
    }

    /// Public key as raw 32 bytes (sent to the cloud at pairing).
    pub fn public_bytes(&self) -> [u8; KEY_BYTES] {
        DalekPublic::from(&self.inner).to_bytes()
    }

    /// Compute the X25519 shared secret with `their_public`. Reusable
    /// (the long-term privkey is not consumed). Apply HKDF on top
    /// before using as a KEK.
    pub fn compute_shared(&self, their_public: &[u8; KEY_BYTES]) -> [u8; 32] {
        let peer = DalekPublic::from(*their_public);
        self.inner.diffie_hellman(&peer).to_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two ephemeral peers must agree on the same shared secret.
    #[test]
    fn ephemeral_x25519_agreement() {
        let a = EphemeralPrivate::generate().unwrap();
        let b = EphemeralPrivate::generate().unwrap();
        let a_pub = a.public_bytes().unwrap();
        let b_pub = b.public_bytes().unwrap();
        let s_a = a.compute_shared(&b_pub).unwrap();
        let s_b = b.compute_shared(&a_pub).unwrap();
        assert_eq!(s_a, s_b, "X25519(a, B) must equal X25519(b, A)");
    }

    /// Long-term key + ephemeral key must agree (this is the rekey shape:
    /// browser ephemeral × Pi long-term).
    #[test]
    fn ephemeral_meets_long_term() {
        let lt = LongTermPrivate::generate().unwrap();
        let lt_pub = lt.public_bytes();

        let eph = EphemeralPrivate::generate().unwrap();
        let eph_pub = eph.public_bytes().unwrap();

        let s_browser_side = eph.compute_shared(&lt_pub).unwrap();
        let s_pi_side = lt.compute_shared(&eph_pub);
        assert_eq!(s_browser_side, s_pi_side);
    }

    /// Long-term seed must round-trip through `to_seed` / `from_seed`,
    /// producing a key with the same public bytes (clamping is idempotent).
    #[test]
    fn long_term_seed_roundtrip() {
        let original = LongTermPrivate::generate().unwrap();
        let seed = original.to_seed();
        let restored = LongTermPrivate::from_seed(seed);
        assert_eq!(original.public_bytes(), restored.public_bytes());
    }

    /// Different seeds → different public keys with overwhelming probability.
    #[test]
    fn distinct_long_term_keys_have_distinct_pubs() {
        let a = LongTermPrivate::generate().unwrap();
        let b = LongTermPrivate::generate().unwrap();
        assert_ne!(a.public_bytes(), b.public_bytes());
    }
}
