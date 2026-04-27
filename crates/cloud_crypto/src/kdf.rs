//! HKDF-SHA-256. Single helper that returns a 32-byte derived key.
//!
//! Wraps `ring::hkdf` so the rest of the crate doesn't have to deal with
//! ring's `KeyType` trait gymnastics. All HKDF outputs in the SentryCloud
//! protocol are 32 bytes (per-Pi key, KEKs, transit keys), so a single
//! fixed-size helper covers every call site.

use ring::hkdf;

use crate::errors::CryptoError;

/// `HKDF-SHA-256(ikm, salt, info)` → 32 bytes.
pub fn derive_32(ikm: &[u8], salt: &[u8], info: &[u8]) -> Result<[u8; 32], CryptoError> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm);
    // The slice-of-slices needs a binding so the temporary outlives `okm`.
    let info_parts: [&[u8]; 1] = [info];
    let okm = prk
        .expand(&info_parts, Out32)
        .map_err(|_| CryptoError::HkdfFailed)?;
    let mut out = [0u8; 32];
    okm.fill(&mut out).map_err(|_| CryptoError::HkdfFailed)?;
    Ok(out)
}

// `KeyType` adapter: tells ring how many bytes we want from `expand`.
struct Out32;
impl hkdf::KeyType for Out32 {
    fn len(&self) -> usize {
        32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 5869 Test Case 1 (HKDF-SHA-256, IKM=22 B, salt=13 B, info=10 B,
    /// L=42). We can't ask for 42 bytes from `derive_32` directly, but we
    /// CAN verify that the first 32 bytes of the published expected OKM
    /// match what our 32-byte helper produces.
    #[test]
    fn rfc5869_test_case_1_first_32_bytes() {
        let ikm = hex_decode("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let salt = hex_decode("000102030405060708090a0b0c");
        let info = hex_decode("f0f1f2f3f4f5f6f7f8f9");
        let expected_first_32 = hex_decode(
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf",
        );
        let got = derive_32(&ikm, &salt, &info).unwrap();
        assert_eq!(got.as_slice(), expected_first_32.as_slice());
    }

    #[test]
    fn different_inputs_yield_different_outputs() {
        let a = derive_32(b"ikm", b"salt", b"info-a").unwrap();
        let b = derive_32(b"ikm", b"salt", b"info-b").unwrap();
        assert_ne!(a, b);
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        hex::decode(s).expect("hex decode")
    }
}
