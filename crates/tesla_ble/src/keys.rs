//! Load the user's Tesla BLE NIST P-256 private key and derive the
//! public key for SessionInfoRequest. Also generates fresh keypairs.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use p256::SecretKey;
use p256::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, LineEnding};

/// Loaded ECDH keypair. The private key is for signing/ECDH; the
/// `pub_uncompressed` bytes are the 65-byte SEC1 format Tesla expects
/// in SessionInfoRequest (`0x04 || X || Y`).
pub struct KeyPair {
    pub secret: SecretKey,
    pub pub_uncompressed: Vec<u8>,
}

impl KeyPair {
    /// Read a Tesla BLE key file. Accepts both SEC1 PEM
    /// (`-----BEGIN EC PRIVATE KEY-----`, which is what tesla-keygen
    /// produces) and PKCS#8 PEM (`-----BEGIN PRIVATE KEY-----`).
    pub fn load(path: &Path) -> Result<Self> {
        let pem_str = std::fs::read_to_string(path)
            .with_context(|| format!("reading key file {}", path.display()))?;
        let parsed = pem::parse(&pem_str).context("parsing PEM envelope")?;
        let secret = match parsed.tag() {
            "EC PRIVATE KEY" => secret_from_sec1_der(parsed.contents())
                .context("parsing SEC1 DER EC private key")?,
            "PRIVATE KEY" => SecretKey::from_pkcs8_der(parsed.contents())
                .context("parsing PKCS#8 DER private key")?,
            other => bail!(
                "unexpected PEM type label {:?}; expected 'EC PRIVATE KEY' or 'PRIVATE KEY'",
                other
            ),
        };
        let pub_uncompressed = secret.public_key().to_sec1_bytes().as_ref().to_vec();
        Ok(Self {
            secret,
            pub_uncompressed,
        })
    }
}

/// Load and validate the complete on-disk Tesla BLE keypair. The private key
/// is authoritative, but a usable installation also requires a parseable SPKI
/// public key derived from that same private scalar.
pub fn load_keypair(dir: &Path) -> Result<KeyPair> {
    load_keypair_paths(&dir.join("key_private.pem"), &dir.join("key_public.pem"))
}

fn load_keypair_paths(priv_path: &Path, pub_path: &Path) -> Result<KeyPair> {
    let keypair = load_regular_private_key(priv_path)?;
    require_regular_file(pub_path, "public key")?;
    let public_pem = std::fs::read_to_string(pub_path)
        .with_context(|| format!("reading public key file {}", pub_path.display()))?;
    let public = p256::PublicKey::from_public_key_pem(&public_pem)
        .with_context(|| format!("parsing public key file {}", pub_path.display()))?;
    if public.to_sec1_bytes().as_ref() != keypair.pub_uncompressed.as_slice() {
        bail!("Tesla BLE public key does not match the private key");
    }
    Ok(keypair)
}

fn require_regular_file(path: &Path, description: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading {description} metadata {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "{description} path {} is not a regular file",
            path.display()
        );
    }
    Ok(())
}

fn load_regular_private_key(path: &Path) -> Result<KeyPair> {
    require_regular_file(path, "private key")?;
    KeyPair::load(path).context("loading Tesla BLE private key")
}

struct KeyDirectoryLock {
    file: File,
}

impl KeyDirectoryLock {
    fn acquire(dir: &Path) -> Result<Self> {
        let path = dir.join(".keygen.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("opening BLE key lock {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("locking BLE key directory {}", dir.display()));
            }
        }
        Ok(Self { file })
    }
}

impl Drop for KeyDirectoryLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn stage_key_file(dir: &Path, name: &str, contents: &[u8], mode: u32) -> Result<PathBuf> {
    use p256::elliptic_curve::rand_core::{OsRng, RngCore};

    for _ in 0..16 {
        let mut rng = OsRng;
        let path = dir.join(format!(".{name}.{:016x}.tmp", rng.next_u64()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        match options.open(&path) {
            Ok(mut file) => {
                let result = (|| -> Result<()> {
                    file.write_all(contents)
                        .with_context(|| format!("writing staged key {}", path.display()))?;
                    #[cfg(unix)]
                    std::fs::set_permissions(
                        &path,
                        std::os::unix::fs::PermissionsExt::from_mode(mode),
                    )
                    .with_context(|| format!("setting permissions on {}", path.display()))?;
                    file.sync_all()
                        .with_context(|| format!("syncing staged key {}", path.display()))?;
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = std::fs::remove_file(&path);
                    return Err(error);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating staged key {}", path.display()));
            }
        }
    }
    bail!("could not allocate a unique staged BLE key file")
}

#[cfg(unix)]
fn sync_key_directory(dir: &Path) -> Result<()> {
    File::open(dir)
        .with_context(|| format!("opening key directory {} for sync", dir.display()))?
        .sync_all()
        .with_context(|| format!("syncing key directory {}", dir.display()))
}

#[cfg(not(unix))]
fn sync_key_directory(_dir: &Path) -> Result<()> {
    Ok(())
}

fn publish_public_key(dir: &Path, keypair: &KeyPair) -> Result<()> {
    use p256::pkcs8::EncodePublicKey;

    let public_pem = keypair
        .secret
        .public_key()
        .to_public_key_pem(LineEnding::LF)
        .context("encoding SPKI public key")?;
    let staged = stage_key_file(dir, "key_public.pem", public_pem.as_bytes(), 0o644)?;
    let destination = dir.join("key_public.pem");
    if let Err(error) = std::fs::rename(&staged, &destination) {
        let _ = std::fs::remove_file(&staged);
        return Err(error).with_context(|| format!("publishing {}", destination.display()));
    }
    sync_key_directory(dir)
}

/// Validate the private key and repair a missing, malformed, or mismatched
/// public key without rotating the private key that may already be paired.
pub fn ensure_keypair_files(dir: &Path) -> Result<KeyPair> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating key dir {}", dir.display()))?;
    let _lock = KeyDirectoryLock::acquire(dir)?;
    let priv_path = dir.join("key_private.pem");
    let keypair = load_regular_private_key(&priv_path)?;
    if load_keypair(dir).is_ok() {
        return Ok(keypair);
    }
    publish_public_key(dir, &keypair)?;
    load_keypair(dir).context("validating repaired Tesla BLE keypair")
}

/// Generate a fresh P-256 BLE keypair, write both halves to disk, and
/// return the loaded keypair. Writes:
///   * `<dir>/key_private.pem` — PKCS#8 PEM, 0600. (`KeyPair::load` also
///     accepts the SEC1 PEM that older tesla-keygen installs use.)
///   * `<dir>/key_public.pem` — SPKI PEM, 0644 (what the pair flow reads).
pub fn generate_keypair(dir: &Path) -> Result<KeyPair> {
    use p256::elliptic_curve::rand_core::OsRng;
    use p256::pkcs8::EncodePublicKey;

    std::fs::create_dir_all(dir).with_context(|| format!("creating key dir {}", dir.display()))?;
    let _lock = KeyDirectoryLock::acquire(dir)?;

    let priv_path = dir.join("key_private.pem");
    let pub_path = dir.join("key_public.pem");
    if std::fs::symlink_metadata(&priv_path).is_ok() {
        return load_keypair(dir).with_context(|| {
            format!(
                "existing Tesla BLE private key {} is invalid or incomplete; refusing to overwrite it",
                priv_path.display()
            )
        });
    }

    let secret = SecretKey::random(&mut OsRng);
    let private_pem = secret
        .to_pkcs8_pem(LineEnding::LF)
        .context("encoding PKCS#8 private key")?;
    let public_pem = secret
        .public_key()
        .to_public_key_pem(LineEnding::LF)
        .context("encoding SPKI public key")?;

    let staged_private = stage_key_file(dir, "key_private.pem", private_pem.as_bytes(), 0o600)?;
    let staged_public = match stage_key_file(dir, "key_public.pem", public_pem.as_bytes(), 0o644) {
        Ok(path) => path,
        Err(error) => {
            let _ = std::fs::remove_file(&staged_private);
            return Err(error);
        }
    };
    if let Err(error) = load_keypair_paths(&staged_private, &staged_public) {
        let _ = std::fs::remove_file(&staged_private);
        let _ = std::fs::remove_file(&staged_public);
        return Err(error).context("validating staged Tesla BLE keypair");
    }

    // The private key is the durable source of truth. Publish it first, then
    // the derivable public key; if the second rename is interrupted, the next
    // install safely repairs the public half without rotating the private key.
    if let Err(error) = std::fs::rename(&staged_private, &priv_path) {
        let _ = std::fs::remove_file(&staged_private);
        let _ = std::fs::remove_file(&staged_public);
        return Err(error).with_context(|| format!("publishing {}", priv_path.display()));
    }
    sync_key_directory(dir)?;
    if let Err(error) = std::fs::rename(&staged_public, &pub_path) {
        let _ = std::fs::remove_file(&staged_public);
        return Err(error).with_context(|| format!("publishing {}", pub_path.display()));
    }
    sync_key_directory(dir)?;

    load_keypair(dir).context("validating published Tesla BLE keypair")
}

/// Hand-parse SEC1 ECPrivateKey DER to extract the 32-byte scalar.
/// p256 0.13 doesn't expose `from_sec1_pem`/`from_sec1_der` directly
/// under the feature set we use, so we walk the small fixed-shape
/// ASN.1 ourselves.
///
/// SEC1 layout (RFC 5915):
///   SEQUENCE {
///     INTEGER 1                              // version
///     OCTET STRING (32 bytes)                // privateKey
///     [0] OID 1.2.840.10045.3.1.7  OPTIONAL  // P-256 curve
///     [1] BIT STRING (uncompressed pubkey) OPTIONAL
///   }
fn secret_from_sec1_der(der: &[u8]) -> Result<SecretKey> {
    let mut i = 0;
    // Expect SEQUENCE
    if der.get(i) != Some(&0x30) {
        bail!("SEC1: expected SEQUENCE at offset 0");
    }
    i += 1;
    // Skip length bytes. ASN.1 length: if high bit set on first byte,
    // low bits are the count of further length bytes (we don't actually
    // care about the value, just how many to skip).
    let first_len = der.get(i).copied().context("SEC1: truncated length")?;
    if first_len & 0x80 == 0 {
        i += 1;
    } else {
        i += 1 + (first_len & 0x7f) as usize;
    }
    // Expect INTEGER 1 (`02 01 01`)
    if der.get(i..i + 3) != Some(&[0x02, 0x01, 0x01]) {
        bail!("SEC1: expected INTEGER version 1 at offset {}", i);
    }
    i += 3;
    // Expect OCTET STRING length 32 (`04 20`)
    if der.get(i..i + 2) != Some(&[0x04, 0x20]) {
        bail!("SEC1: expected 32-byte OCTET STRING at offset {}", i);
    }
    i += 2;
    let scalar = der
        .get(i..i + 32)
        .context("SEC1: truncated private key bytes")?;
    SecretKey::from_slice(scalar).context("invalid P-256 scalar")
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::SecretKey;
    use p256::elliptic_curve::rand_core::OsRng;
    use p256::pkcs8::EncodePrivateKey;

    #[test]
    fn round_trip_generated_pkcs8_key() {
        let key = SecretKey::random(&mut OsRng);
        let pem = key.to_pkcs8_pem(p256::pkcs8::LineEnding::LF).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), pem.as_bytes()).unwrap();

        let loaded = KeyPair::load(tmp.path()).unwrap();
        assert_eq!(loaded.pub_uncompressed.len(), 65);
        assert_eq!(loaded.pub_uncompressed[0], 0x04);
    }

    #[test]
    fn generated_keypair_round_trips_through_disk() {
        // Generate into a tempdir; verify both files land + the
        // private key loads back via KeyPair::load + the loaded
        // pubkey matches the generated one.
        let dir = tempfile::tempdir().unwrap();
        let kp = generate_keypair(dir.path()).unwrap();

        let priv_path = dir.path().join("key_private.pem");
        let pub_path = dir.path().join("key_public.pem");
        assert!(priv_path.exists(), "private key file should exist");
        assert!(pub_path.exists(), "public key file should exist");

        let priv_pem = std::fs::read_to_string(&priv_path).unwrap();
        assert!(
            priv_pem.contains("-----BEGIN PRIVATE KEY-----"),
            "private key should be PKCS#8 PEM; got: {}",
            &priv_pem[..priv_pem.len().min(60)]
        );
        let pub_pem = std::fs::read_to_string(&pub_path).unwrap();
        assert!(
            pub_pem.contains("-----BEGIN PUBLIC KEY-----"),
            "public key should be SPKI PEM"
        );

        // Loader round-trip — confirms the file is a valid P-256 key.
        let loaded = KeyPair::load(&priv_path).unwrap();
        assert_eq!(
            loaded.pub_uncompressed.len(),
            65,
            "uncompressed SEC1 pubkey is 65 bytes"
        );
        assert_eq!(loaded.pub_uncompressed, kp.pub_uncompressed);
    }

    #[test]
    fn empty_key_files_are_not_a_valid_pair() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("key_private.pem"), b"").unwrap();
        std::fs::write(dir.path().join("key_public.pem"), b"").unwrap();

        let error = match load_keypair(dir.path()) {
            Ok(_) => panic!("empty PEM files must be rejected"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("private key"),
            "unexpected validation error: {error:#}"
        );
    }

    #[test]
    fn valid_private_key_repairs_a_missing_public_key_without_rotation() {
        let dir = tempfile::tempdir().unwrap();
        generate_keypair(dir.path()).unwrap();
        let priv_path = dir.path().join("key_private.pem");
        let pub_path = dir.path().join("key_public.pem");
        let private_before = std::fs::read(&priv_path).unwrap();
        std::fs::remove_file(&pub_path).unwrap();

        let repaired = ensure_keypair_files(dir.path()).unwrap();

        assert_eq!(std::fs::read(&priv_path).unwrap(), private_before);
        assert_eq!(
            load_keypair(dir.path()).unwrap().pub_uncompressed,
            repaired.pub_uncompressed
        );
    }

    #[test]
    fn concurrent_generation_converges_on_one_valid_keypair() {
        let dir = tempfile::tempdir().unwrap();
        let path = std::sync::Arc::new(dir.path().to_path_buf());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                generate_keypair(&path)
            }));
        }

        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        load_keypair(dir.path()).unwrap();
    }

    #[test]
    fn invalid_existing_private_key_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let priv_path = dir.path().join("key_private.pem");
        std::fs::write(&priv_path, b"broken-private-key").unwrap();

        assert!(ensure_keypair_files(dir.path()).is_err());
        assert_eq!(std::fs::read(&priv_path).unwrap(), b"broken-private-key");
        assert!(generate_keypair(dir.path()).is_err());
        assert_eq!(std::fs::read(&priv_path).unwrap(), b"broken-private-key");
    }

    #[cfg(unix)]
    #[test]
    fn generated_key_files_have_safe_modes_and_no_staging_remnants() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        generate_keypair(dir.path()).unwrap();

        let private_mode = std::fs::metadata(dir.path().join("key_private.pem"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let public_mode = std::fs::metadata(dir.path().join("key_public.pem"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(private_mode, 0o600);
        assert_eq!(public_mode, 0o644);
        let staged: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(staged.is_empty(), "staging files remain: {staged:?}");
    }

    #[cfg(unix)]
    #[test]
    fn private_key_symlinks_are_not_accepted_as_install_state() {
        let source = tempfile::tempdir().unwrap();
        generate_keypair(source.path()).unwrap();
        let install = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            source.path().join("key_private.pem"),
            install.path().join("key_private.pem"),
        )
        .unwrap();
        std::fs::copy(
            source.path().join("key_public.pem"),
            install.path().join("key_public.pem"),
        )
        .unwrap();

        assert!(load_keypair(install.path()).is_err());
        assert!(ensure_keypair_files(install.path()).is_err());
        assert!(install.path().join("key_private.pem").is_symlink());
    }

    #[test]
    fn parses_sec1_pem_from_openssl() {
        // SEC1 PEM equivalent to the format `tesla-keygen` produces.
        // Generated via:
        //   openssl ecparam -name prime256v1 -genkey -noout
        // The exact bytes don't matter — just that the SEC1 path works.
        let pem = "-----BEGIN EC PRIVATE KEY-----\n\
                   MHcCAQEEIBnEX3tDgQHQX5IcAOA2RrvHV7ZzNeb7BLJ3vh7zVRpJoAoGCCqGSM49\n\
                   AwEHoUQDQgAEpUEnGcbqLEKMRwH69lcLN1H3xR/Mp3CY+QhBZkS1eOPF8Pdvkk0Q\n\
                   jiNAS/lZJaufnRu3WSjNu5xAvI4lNYjPiQ==\n\
                   -----END EC PRIVATE KEY-----\n";
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), pem).unwrap();

        let loaded = KeyPair::load(tmp.path()).unwrap();
        assert_eq!(loaded.pub_uncompressed.len(), 65);
        assert_eq!(loaded.pub_uncompressed[0], 0x04);
    }
}
