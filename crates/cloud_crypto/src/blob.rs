

use crate::errors::CryptoError;

pub const VERSION: u8 = 0x01;

pub const NONCE_LEN: usize = 12;

pub const TAG_LEN: usize = 16;

pub const KEY_LEN: usize = 32;

pub const SALT_LEN: usize = 16;

pub const WRAPPED_KEY_BLOB_LEN: usize = 1 + NONCE_LEN + TAG_LEN + KEY_LEN;

pub const MIN_BLOB_LEN: usize = 1 + NONCE_LEN + TAG_LEN;

pub struct BlobParts<'a> {
    pub nonce: &'a [u8],
    pub tag: &'a [u8],
    pub ciphertext: &'a [u8],
}

pub fn pack(nonce: &[u8; NONCE_LEN], tag: &[u8; TAG_LEN], ciphertext: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(MIN_BLOB_LEN + ciphertext.len());
    out.push(VERSION);
    out.extend_from_slice(nonce);
    out.extend_from_slice(tag);
    out.extend_from_slice(ciphertext);
    out
}

pub fn unpack(buf: &[u8]) -> Result<BlobParts<'_>, CryptoError> {
    if buf.len() < MIN_BLOB_LEN {
        return Err(CryptoError::BlobTooShort(buf.len(), MIN_BLOB_LEN));
    }
    if buf[0] != VERSION {
        return Err(CryptoError::UnknownBlobVersion(buf[0]));
    }
    Ok(BlobParts {
        nonce: &buf[1..1 + NONCE_LEN],
        tag: &buf[1 + NONCE_LEN..1 + NONCE_LEN + TAG_LEN],
        ciphertext: &buf[1 + NONCE_LEN + TAG_LEN..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_roundtrip() {
        let nonce = [1u8; NONCE_LEN];
        let tag = [2u8; TAG_LEN];
        let ct = b"hello-ciphertext";
        let packed = pack(&nonce, &tag, ct);
        assert_eq!(packed[0], VERSION);
        assert_eq!(packed.len(), MIN_BLOB_LEN + ct.len());

        let parts = unpack(&packed).expect("unpack");
        assert_eq!(parts.nonce, &nonce);
        assert_eq!(parts.tag, &tag);
        assert_eq!(parts.ciphertext, ct);
    }

    #[test]
    fn unpack_rejects_short_blob() {
        let short = [VERSION; MIN_BLOB_LEN - 1];
        assert!(matches!(
            unpack(&short),
            Err(CryptoError::BlobTooShort(_, _))
        ));
    }

    #[test]
    fn unpack_rejects_bad_version() {
        let mut bad = vec![0u8; MIN_BLOB_LEN];
        bad[0] = 0xff;
        assert!(matches!(
            unpack(&bad),
            Err(CryptoError::UnknownBlobVersion(0xff))
        ));
    }

    #[test]
    fn wrapped_key_blob_len_constant_correct() {
        assert_eq!(WRAPPED_KEY_BLOB_LEN, 61);
    }
}
