//! The sealed envelope a contribution travels in, and the key that opens it.
//!
//! One symmetric key per fleet, copied between the user's own machines the way
//! a Syncthing device id is. See `docs/adr/0002-symmetric-fleet-key.md` for what
//! that buys and, more importantly, what it does not: there is no revocation.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

const MAGIC: &[u8; 7] = b"TGSYNC1";
const ALG_XCHACHA20_POLY1305: u8 = 1;
const COMP_NONE: u8 = 0;
const COMP_GZIP: u8 = 1;
const KEY_ID_LEN: usize = 4;
const HEADER_LEN: usize = MAGIC.len() + 2 + KEY_ID_LEN;
const NONCE_LEN: usize = 24;

/// Domain separator for the key id. Bumping it re-keys nothing, but every
/// device would read every object as foreign until they agree again.
const KEY_ID_DOMAIN: &str = "tokengauge.sync.key-id.v1";
const OBJECT_NAME_DOMAIN: &str = "tokengauge.sync.object-name.v1";

const KEY_PREFIX: &str = "tgsync1";
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// The secret every device in a fleet holds. Possession of it is membership.
#[derive(Clone)]
pub struct FleetKey([u8; 32]);

impl fmt::Debug for FleetKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FleetKey({})", self.id_hex())
    }
}

impl FleetKey {
    pub fn generate() -> Self {
        Self(XChaCha20Poly1305::generate_key(&mut OsRng).into())
    }

    pub fn parse(text: &str) -> Result<Self> {
        let trimmed = text.trim();
        let body = trimmed
            .strip_prefix(KEY_PREFIX)
            .with_context(|| format!("a fleet key starts with `{KEY_PREFIX}`"))?;
        let bytes = base32_decode(body).context("fleet key is not valid base32")?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("a fleet key is 32 bytes"))?;
        Ok(Self(bytes))
    }

    /// The form a user copies to the next machine.
    pub fn display(&self) -> String {
        format!("{KEY_PREFIX}{}", base32_encode(&self.0))
    }

    fn id(&self) -> [u8; KEY_ID_LEN] {
        let mut hasher = Sha256::new();
        hasher.update(KEY_ID_DOMAIN.as_bytes());
        hasher.update([0u8]);
        hasher.update(self.0);
        let digest = hasher.finalize();
        let mut id = [0u8; KEY_ID_LEN];
        id.copy_from_slice(&digest[..KEY_ID_LEN]);
        id
    }

    pub fn id_hex(&self) -> String {
        self.id().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The object one device writes to. Keyed rather than the raw device id, so
    /// whoever holds the folder or bucket cannot count the fleet or link an
    /// object to a machine.
    pub fn object_name(&self, device_id: &str) -> String {
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(&self.0).expect("hmac takes any length");
        mac.update(OBJECT_NAME_DOMAIN.as_bytes());
        mac.update(&[0u8]);
        mac.update(device_id.as_bytes());
        let tag = mac.finalize().into_bytes();
        let hex: String = tag.iter().take(16).map(|b| format!("{b:02x}")).collect();
        format!("{hex}.tgsync")
    }

    /// Seal a contribution for `object_name`, which is bound in as associated
    /// data so the storage holder cannot move one device's bytes onto another
    /// device's name.
    pub fn seal(&self, object_name: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
        let compressed = gzip(plaintext)?;
        let (comp, body) = if compressed.len() < plaintext.len() {
            (COMP_GZIP, compressed)
        } else {
            (COMP_NONE, plaintext.to_vec())
        };

        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(MAGIC);
        header.push(ALG_XCHACHA20_POLY1305);
        header.push(comp);
        header.extend_from_slice(&self.id());

        let cipher = XChaCha20Poly1305::new(Key::from_slice(&self.0));
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &body,
                    aad: &aad(&header, object_name),
                },
            )
            .map_err(|_| anyhow::anyhow!("could not seal the contribution"))?;

        let mut sealed = header;
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        Ok(sealed)
    }

    pub fn open(&self, object_name: &str, sealed: &[u8]) -> Result<Vec<u8>, OpenError> {
        // Magic before length: something that was never one of ours should say
        // so whatever its size, and truncation should mean a real object cut
        // short.
        if sealed.len() < MAGIC.len() || &sealed[..MAGIC.len()] != MAGIC {
            return Err(OpenError::NotAnEnvelope);
        }
        if sealed.len() < HEADER_LEN + NONCE_LEN {
            return Err(OpenError::Truncated);
        }
        let alg = sealed[MAGIC.len()];
        if alg != ALG_XCHACHA20_POLY1305 {
            return Err(OpenError::UnsupportedAlg(alg));
        }
        let comp = sealed[MAGIC.len() + 1];
        let key_id = &sealed[MAGIC.len() + 2..HEADER_LEN];
        if key_id != self.id() {
            return Err(OpenError::ForeignKey {
                key_id: key_id.iter().map(|b| format!("{b:02x}")).collect(),
            });
        }

        let header = &sealed[..HEADER_LEN];
        let nonce = XNonce::from_slice(&sealed[HEADER_LEN..HEADER_LEN + NONCE_LEN]);
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&self.0));
        let body = cipher
            .decrypt(
                nonce,
                Payload {
                    msg: &sealed[HEADER_LEN + NONCE_LEN..],
                    aad: &aad(header, object_name),
                },
            )
            .map_err(|_| OpenError::Authentication)?;

        match comp {
            COMP_NONE => Ok(body),
            COMP_GZIP => gunzip(&body).map_err(|_| OpenError::Decompress),
            other => Err(OpenError::UnsupportedCompression(other)),
        }
    }
}

/// Why an object could not be opened. A foreign key is named rather than
/// reported as corruption: sharing storage with another fleet is a
/// configuration, not a fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    NotAnEnvelope,
    Truncated,
    UnsupportedAlg(u8),
    UnsupportedCompression(u8),
    ForeignKey { key_id: String },
    Authentication,
    Decompress,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnEnvelope => write!(f, "not a TokenGauge sync object"),
            Self::Truncated => write!(f, "object is truncated"),
            Self::UnsupportedAlg(alg) => {
                write!(f, "unsupported cipher {alg}, written by a newer TokenGauge")
            }
            Self::UnsupportedCompression(comp) => {
                write!(
                    f,
                    "unsupported compression {comp}, written by a newer TokenGauge"
                )
            }
            Self::ForeignKey { key_id } => write!(f, "sealed for another fleet key ({key_id})"),
            Self::Authentication => write!(
                f,
                "object failed authentication; it was altered or truncated in transit"
            ),
            Self::Decompress => write!(f, "object did not decompress"),
        }
    }
}

impl std::error::Error for OpenError {}

fn aad(header: &[u8], object_name: &str) -> Vec<u8> {
    let mut aad = header.to_vec();
    aad.push(0);
    aad.extend_from_slice(object_name.as_bytes());
    aad
}

fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

/// Derived from the snapshot's parent, like every other state file.
pub fn key_path(cache_file: &Path) -> PathBuf {
    cache_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("tokengauge-sync-key")
}

pub fn load_key(cache_file: &Path) -> Result<Option<FleetKey>> {
    let path = key_path(cache_file);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(FleetKey::parse(&text).with_context(|| {
            format!("{} does not hold a fleet key", path.display())
        })?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

/// Written 0600 from the start rather than relaxed and tightened, so the secret
/// is never briefly world-readable.
///
/// Replacing a key is destructive in a way that is not obvious: this machine
/// stops being able to read its fleet's objects, and its own become unreadable
/// to them. Writing the key already present is a no-op, so only an actual
/// change needs `overwrite`.
pub fn store_key(cache_file: &Path, key: &FleetKey, overwrite: bool) -> Result<PathBuf> {
    let path = key_path(cache_file);
    if !overwrite && let Some(existing) = load_key(cache_file)? {
        if existing.id() == key.id() {
            return Ok(path);
        }
        bail!(
            "{} already holds fleet key {}; replacing it leaves this machine unable to read that fleet. Pass --sync-force if that is what you want",
            path.display(),
            existing.id_hex()
        );
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    write_private(&tmp, key.display().as_bytes())
        .with_context(|| format!("could not write {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    file.write_all(b"\n")
}

/// No permission is set on non-unix targets: the key inherits the directory's
/// ACL, which on Windows means the user profile's. `docs/sync.md` states the
/// limitation rather than implying the 0600 above applies everywhere.
#[cfg(not(unix))]
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut body = contents.to_vec();
    body.push(b'\n');
    fs::write(path, body)
}

fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for byte in data {
        acc = (acc << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for ch in text.chars() {
        let value = BASE32.iter().position(|c| *c == ch as u8)? as u32;
        acc = (acc << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: &str = "9f3c1d2e4a5b6c7d";

    fn sealed_for(key: &FleetKey, body: &[u8]) -> (String, Vec<u8>) {
        let name = key.object_name(DEVICE);
        let sealed = key.seal(&name, body).expect("seal");
        (name, sealed)
    }

    #[test]
    fn a_sealed_contribution_round_trips() {
        let key = FleetKey::generate();
        let body = br#"{"schemaVersion":1,"buckets":[]}"#;
        let (name, sealed) = sealed_for(&key, body);

        assert_eq!(key.open(&name, &sealed).expect("open"), body.to_vec());
    }

    #[test]
    fn plaintext_never_appears_in_the_sealed_bytes() {
        let key = FleetKey::generate();
        let body = b"claude-opus-5 on boreas";
        let (_, sealed) = sealed_for(&key, body);

        assert!(
            !sealed.windows(body.len()).any(|w| w == body),
            "the model and hostname leaked into the object"
        );
    }

    #[test]
    fn an_altered_byte_fails_authentication() {
        let key = FleetKey::generate();
        let (name, mut sealed) = sealed_for(&key, b"payload worth tampering with");
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;

        assert_eq!(key.open(&name, &sealed), Err(OpenError::Authentication));
    }

    #[test]
    fn another_fleets_object_is_named_not_reported_as_corruption() {
        let mine = FleetKey::generate();
        let theirs = FleetKey::generate();
        let (name, sealed) = sealed_for(&theirs, b"not for me");

        match mine.open(&name, &sealed) {
            Err(OpenError::ForeignKey { key_id }) => assert_eq!(key_id, theirs.id_hex()),
            other => panic!("expected a named foreign key, got {other:?}"),
        }
    }

    #[test]
    fn an_object_moved_onto_another_name_fails_authentication() {
        let key = FleetKey::generate();
        let (_, sealed) = sealed_for(&key, b"device a's tokens");
        let elsewhere = key.object_name("some-other-device");

        assert_eq!(
            key.open(&elsewhere, &sealed),
            Err(OpenError::Authentication)
        );
    }

    #[test]
    fn a_truncated_object_is_not_read_as_an_envelope() {
        let key = FleetKey::generate();
        let (name, sealed) = sealed_for(&key, b"whatever");

        assert_eq!(key.open(&name, &sealed[..8]), Err(OpenError::Truncated));
        assert_eq!(
            key.open(&name, b"not ours at all right"),
            Err(OpenError::NotAnEnvelope)
        );
    }

    #[test]
    fn a_key_survives_the_form_a_user_copies() {
        let key = FleetKey::generate();
        let copied = key.display();

        assert!(copied.starts_with("tgsync1"));
        let parsed = FleetKey::parse(&format!("  {copied}\n")).expect("parse");
        assert_eq!(parsed.id_hex(), key.id_hex());
        assert_eq!(parsed.display(), copied);

        assert!(FleetKey::parse("nope").is_err());
        assert!(FleetKey::parse("tgsync1zzz").is_err());
    }

    #[test]
    fn object_names_are_keyed_so_two_fleets_do_not_collide() {
        let a = FleetKey::generate();
        let b = FleetKey::generate();

        assert_ne!(a.object_name(DEVICE), b.object_name(DEVICE));
        assert_ne!(a.object_name(DEVICE), a.object_name("another-device"));
        assert_eq!(a.object_name(DEVICE), a.object_name(DEVICE));
        assert!(
            !a.object_name(DEVICE).contains(DEVICE),
            "the device id leaked into the object name"
        );
    }

    #[test]
    fn the_key_file_is_written_private_and_read_back() {
        let dir = std::env::temp_dir().join(format!("tokengauge-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let cache_file = dir.join("tokengauge-usage.json");
        let key = FleetKey::generate();

        let path = store_key(&cache_file, &key, false).expect("store");

        // Writing the key already present changes nothing, so it is not an
        // error; replacing it with a different one cuts this machine off from
        // its fleet, so it is.
        assert!(
            store_key(&cache_file, &key, false).is_ok(),
            "the same key is a no-op"
        );
        let other = FleetKey::generate();
        let refused = store_key(&cache_file, &other, false).expect_err("must refuse");
        assert!(format!("{refused}").contains("--sync-force"), "{refused}");
        assert_eq!(
            load_key(&cache_file)
                .expect("load")
                .expect("present")
                .id_hex(),
            key.id_hex(),
            "a refused replacement must leave the key alone"
        );
        assert!(
            store_key(&cache_file, &other, true).is_ok(),
            "unless told to"
        );
        assert!(
            store_key(&cache_file, &key, true).is_ok(),
            "back to the first"
        );

        let loaded = load_key(&cache_file).expect("load").expect("present");
        assert_eq!(loaded.id_hex(), key.id_hex());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o077, 0, "the fleet key was group or world readable");
        }
        let _ = path;

        std::fs::remove_dir_all(&dir).ok();
        assert!(
            load_key(&cache_file)
                .expect("absent is not an error")
                .is_none()
        );
    }
}
