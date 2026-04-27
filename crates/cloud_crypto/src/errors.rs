

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("blob too short ({0} bytes; minimum {1})")]
    BlobTooShort(usize, usize),

    #[error("unknown blob version: 0x{0:02x}")]
    UnknownBlobVersion(u8),

    #[error("AEAD seal failed")]
    SealFailed,

    #[error("AEAD open failed (auth tag verification)")]
    OpenFailed,

    #[error("HKDF expand failed (output length out of range)")]
    HkdfFailed,

    #[error("X25519 operation failed")]
    X25519Failed,

    #[error("invalid key length: expected {expected}, got {actual}")]
    InvalidKeyLength { expected: usize, actual: usize },

    #[error("base64 decode failed")]
    Base64Decode,
}

#[derive(Debug, Error)]
pub enum CredentialsError {
    #[error("credentials I/O")]
    Io(#[from] std::io::Error),

    #[error("credentials JSON parse")]
    Parse(#[from] serde_json::Error),

    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),

    #[error("base64 decode failed in credentials field")]
    Base64Decode,

    #[error("unsupported credentials file version: {0}")]
    UnsupportedVersion(u32),

    #[error("SBC serial-number file missing or unreadable at {path}")]
    SerialMissing { path: String },

    #[error("SBC serial-number too short ({len} bytes; expected at least 8)")]
    SerialTooShort { len: usize },
}
