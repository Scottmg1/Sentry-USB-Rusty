//! AES-256-GCM seal / open with associated data.
//!
//! Wraps `ring::aead`. Returns/accepts the universal `[ver|nonce|tag|ct]`
//! blob layout from `crate::blob`.
//!
//! `seal` generates a fresh 12-byte random nonce per call (NIST 96-bit).
//! Reusing a nonce under the same key would be catastrophic (key recovery
//! attack), so this helper is the ONLY supported encryption surface — there
//! is no "encrypt with my own nonce" escape hatch.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN as RING_NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

use crate::blob::{self, KEY_LEN, NONCE_LEN, TAG_LEN};
use crate::errors::CryptoError;

/// AES-256 key wrapper. Construct with [`Key::from_bytes`].
pub struct Key(LessSafeKey);

impl Key {
    pub fn from_bytes(bytes: &[u8; KEY_LEN]) -> Result<Self, CryptoError> {
        let unbound = UnboundKey::new(&AES_256_GCM, bytes).map_err(|_| {
            CryptoError::InvalidKeyLength {
                expected: KEY_LEN,
                actual: bytes.len(),
            }
        })?;
        Ok(Key(LessSafeKey::new(unbound)))
    }
}

// Compile-time sanity: ring's nonce length matches our NONCE_LEN.
const _: () = assert!(RING_NONCE_LEN == NONCE_LEN);

/// Seal `plaintext` under `key` with `aad`. Returns the full packed blob
/// `[ver|nonce|tag|ct]`.
pub fn seal(key: &Key, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| CryptoError::SealFailed)?;
    seal_with_nonce(key, &nonce_bytes, aad, plaintext)
}

/// Seal with a caller-provided nonce. Crate-private — use [`seal`] from
/// outside. Exists only so the test suite can verify against fixed-nonce
/// vectors; the random-nonce path is the production one.
pub(crate) fn seal_with_nonce(
    key: &Key,
    nonce_bytes: &[u8; NONCE_LEN],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let nonce = Nonce::assume_unique_for_key(*nonce_bytes);

    // ring's seal_in_place_separate_tag wants in/out=plaintext, returns tag.
    let mut in_out = plaintext.to_vec();
    let tag = key
        .0
        .seal_in_place_separate_tag(nonce, Aad::from(aad), &mut in_out)
        .map_err(|_| CryptoError::SealFailed)?;

    let tag_bytes: [u8; TAG_LEN] = tag
        .as_ref()
        .try_into()
        .map_err(|_| CryptoError::SealFailed)?;

    Ok(blob::pack(nonce_bytes, &tag_bytes, &in_out))
}

/// Open the packed blob under `key` with `aad`. Returns the recovered
/// plaintext. Errors uniformly on every failure mode (bad version, bad
/// length, wrong key, wrong AAD, corrupted ciphertext) — the distinction
/// would be a side-channel.
pub fn open(key: &Key, aad: &[u8], packed: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let parts = blob::unpack(packed)?;

    // ring wants ciphertext+tag concatenated in-place. Build that buffer.
    let mut combined = Vec::with_capacity(parts.ciphertext.len() + parts.tag.len());
    combined.extend_from_slice(parts.ciphertext);
    combined.extend_from_slice(parts.tag);

    let nonce_bytes: [u8; NONCE_LEN] = parts
        .nonce
        .try_into()
        .map_err(|_| CryptoError::OpenFailed)?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);

    let plaintext = key
        .0
        .open_in_place(nonce, Aad::from(aad), &mut combined)
        .map_err(|_| CryptoError::OpenFailed)?;
    let plaintext_len = plaintext.len();
    combined.truncate(plaintext_len);
    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_key() -> Key {
        Key::from_bytes(&[7u8; KEY_LEN]).expect("key")
    }

    #[test]
    fn seal_then_open_roundtrip() {
        let key = fixed_key();
        let aad = b"some-aad";
        let plaintext = b"hello world";
        let packed = seal(&key, aad, plaintext).unwrap();
        let recovered = open(&key, aad, &packed).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn open_rejects_wrong_aad() {
        let key = fixed_key();
        let packed = seal(&key, b"correct-aad", b"plaintext").unwrap();
        let err = open(&key, b"wrong-aad", &packed).unwrap_err();
        assert!(matches!(err, CryptoError::OpenFailed));
    }

    #[test]
    fn open_rejects_wrong_key() {
        let k1 = fixed_key();
        let k2 = Key::from_bytes(&[8u8; KEY_LEN]).unwrap();
        let packed = seal(&k1, b"aad", b"plaintext").unwrap();
        let err = open(&k2, b"aad", &packed).unwrap_err();
        assert!(matches!(err, CryptoError::OpenFailed));
    }

    #[test]
    fn open_rejects_corrupted_ciphertext() {
        let key = fixed_key();
        let mut packed = seal(&key, b"aad", b"plaintext").unwrap();
        // Flip a bit in the ciphertext region (after ver + nonce + tag).
        let pos = 1 + NONCE_LEN + TAG_LEN;
        packed[pos] ^= 0x01;
        let err = open(&key, b"aad", &packed).unwrap_err();
        assert!(matches!(err, CryptoError::OpenFailed));
    }

    #[test]
    fn fixed_nonce_seal_is_deterministic() {
        let key = fixed_key();
        let nonce = [3u8; NONCE_LEN];
        let a = seal_with_nonce(&key, &nonce, b"aad", b"plaintext").unwrap();
        let b = seal_with_nonce(&key, &nonce, b"aad", b"plaintext").unwrap();
        assert_eq!(a, b, "fixed-nonce seal must be deterministic for vector tests");
    }
}
