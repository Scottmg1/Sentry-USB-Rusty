//! SentryCloud zero-knowledge encryption protocol primitives (Pi side).
//!
//! Source of truth: `SentryCloud/ENCRYPTION.md`. Browser-side counterpart:
//! `SentryCloud/web/src/lib/encryption/format.js`. This crate must agree
//! byte-for-byte with both.
//!
//! No I/O, no async, no `reqwest`. The daemon-glue side lives in
//! `sentryusb-cloud-uploader`.

pub mod aad;
pub mod aead;
pub mod blob;
pub mod credentials;
pub mod errors;
pub mod ids;
pub mod kdf;
pub mod x25519;

pub use errors::{CryptoError, CredentialsError};
