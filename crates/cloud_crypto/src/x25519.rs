

use ring::agreement;
use ring::rand::{SecureRandom, SystemRandom};
use x25519_dalek::{PublicKey as DalekPublic, StaticSecret};

use crate::errors::CryptoError;

pub const KEY_BYTES: usize = 32;

pub struct EphemeralPrivate {
    inner: agreement::EphemeralPrivateKey,
}

impl EphemeralPrivate {

    pub fn generate() -> Result<Self, CryptoError> {
        let rng = SystemRandom::new();
        let inner = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng)
            .map_err(|_| CryptoError::X25519Failed)?;
        Ok(Self { inner })
    }

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

#[derive(Clone)]
pub struct LongTermPrivate {
    inner: StaticSecret,
}

impl LongTermPrivate {

    pub fn generate() -> Result<Self, CryptoError> {
        let mut seed = [0u8; KEY_BYTES];
        SystemRandom::new()
            .fill(&mut seed)
            .map_err(|_| CryptoError::X25519Failed)?;
        Ok(Self {
            inner: StaticSecret::from(seed),
        })
    }

    pub fn from_seed(seed: [u8; KEY_BYTES]) -> Self {
        Self {
            inner: StaticSecret::from(seed),
        }
    }

    pub fn to_seed(&self) -> [u8; KEY_BYTES] {
        self.inner.to_bytes()
    }

    pub fn public_bytes(&self) -> [u8; KEY_BYTES] {
        DalekPublic::from(&self.inner).to_bytes()
    }

    pub fn compute_shared(&self, their_public: &[u8; KEY_BYTES]) -> [u8; 32] {
        let peer = DalekPublic::from(*their_public);
        self.inner.diffie_hellman(&peer).to_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn long_term_seed_roundtrip() {
        let original = LongTermPrivate::generate().unwrap();
        let seed = original.to_seed();
        let restored = LongTermPrivate::from_seed(seed);
        assert_eq!(original.public_bytes(), restored.public_bytes());
    }

    #[test]
    fn distinct_long_term_keys_have_distinct_pubs() {
        let a = LongTermPrivate::generate().unwrap();
        let b = LongTermPrivate::generate().unwrap();
        assert_ne!(a.public_bytes(), b.public_bytes());
    }
}
