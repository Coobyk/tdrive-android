//! TDE1 stream encryption and password vault — compatible with TDrive's
//! `backend/crypto` (stream.go / vault.go).
//!
//! Vault: password → Argon2id KEK → XChaCha20-Poly1305 unwrap of a 32-byte
//! master key, verified by an encrypted `tdrive-key-check-v1` marker.
//!
//! TDE1 stream: 50-byte header (`TDE1` | flags | chunk-exp 16 | 16-byte file
//! salt | 20-byte nonce prefix | 8-byte LE plaintext size) followed by 64 KiB
//! plaintext chunks, each sealed with XChaCha20-Poly1305 under a nonce of
//! `prefix | BE32(counter)`, bit 31 marking the final chunk. Per-file keys
//! are HKDF-SHA256(master, file_salt, "tdrive/file/v1").

use anyhow::{Context, Result, anyhow, bail};
use argon2::{Algorithm, Argon2, Params as ArgonParams, Version};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;
use std::io::{Read, Write};
use std::path::Path;

pub const MASTER_KEY_LEN: usize = 32;
pub const WRAPPED_MASTER_KEY_LEN: usize = 24 + MASTER_KEY_LEN + 16; // 72
const KEY_CHECK_PLAINTEXT: &[u8] = b"tdrive-key-check-v1";
pub const KEY_CHECK_ENVELOPE_LEN: usize = 24 + 19 + 16; // 59

const MIN_ARGON2_MEMORY_KIB: u32 = 16 * 1024;
const MAX_ARGON2_MEMORY_KIB: u32 = 256 * 1024;
const MIN_ARGON2_TIME: u32 = 1;
const MAX_ARGON2_TIME: u32 = 10;
const MIN_ARGON2_PARALLEL: u8 = 1;
const MAX_ARGON2_PARALLEL: u8 = 16;
const MIN_SALT_LEN: usize = 16;
const MAX_SALT_LEN: usize = 64;
const MAX_PARAMS_JSON_LEN: usize = 1024;

pub const HEADER_LEN: usize = 50;
const CHUNK_EXP: u8 = 16;
const CHUNK_SIZE: usize = 1 << CHUNK_EXP; // 64 KiB
const TAG_LEN: usize = 16;
const NONCE_PREFIX_LEN: usize = 20;
const FILE_KEY_INFO: &[u8] = b"tdrive/file/v1";
const FINAL_BIT: u32 = 1 << 31;
const MAX_PLAINTEXT: u64 = (1 << 47) - 1;

// ---------------- vault ----------------

#[derive(Debug, Clone)]
pub struct VaultParams {
    pub memory: u32,
    pub time: u32,
    pub parallelism: u8,
    pub key_len: u32,
    pub salt_len: u32,
}

impl Default for VaultParams {
    fn default() -> Self {
        Self {
            memory: 64 * 1024,
            time: 3,
            parallelism: 4,
            key_len: 32,
            salt_len: 16,
        }
    }
}

/// Vault metadata carried by an `encfg` op (already base64-decoded).
#[derive(Debug, Clone)]
pub struct VaultConfig {
    pub salt: Vec<u8>,
    pub kdf_params_json: String,
    pub wrapped_master_key: Vec<u8>,
    pub key_check: Vec<u8>,
    pub hint: String,
    pub version: i32,
}

fn parse_params(json: &str) -> Result<VaultParams> {
    if json.is_empty() || json.len() > MAX_PARAMS_JSON_LEN {
        bail!("crypto: invalid KDF parameters");
    }
    let v: serde_json::Value =
        serde_json::from_str(json).context("crypto: invalid KDF parameters")?;
    let obj = v.as_object().context("crypto: invalid KDF parameters")?;
    // Legacy records with no kdf field select Argon2id.
    let kdf = obj
        .get("kdf")
        .map(|k| k.as_str().unwrap_or_default().to_string())
        .unwrap_or_else(|| "argon2id".into());
    if kdf != "argon2id" {
        bail!("crypto: unsupported KDF");
    }
    let num = |key: &str, default: u64| -> Result<u64> {
        match obj.get(key) {
            Some(v) => v.as_u64().context("crypto: invalid KDF parameters"),
            None => Ok(default),
        }
    };
    let p = VaultParams {
        memory: num("memory", 64 * 1024)? as u32,
        time: num("time", 3)? as u32,
        parallelism: num("parallelism", 4)? as u8,
        key_len: num("key_len", 32)? as u32,
        salt_len: num("salt_len", 16)? as u32,
    };
    p.validate()?;
    Ok(p)
}

impl VaultParams {
    pub fn validate(&self) -> Result<()> {
        if !(MIN_ARGON2_MEMORY_KIB..=MAX_ARGON2_MEMORY_KIB).contains(&self.memory)
            || !(MIN_ARGON2_TIME..=MAX_ARGON2_TIME).contains(&self.time)
            || !(MIN_ARGON2_PARALLEL..=MAX_ARGON2_PARALLEL).contains(&self.parallelism)
            || self.key_len != MASTER_KEY_LEN as u32
            || !(MIN_SALT_LEN..=MAX_SALT_LEN).contains(&(self.salt_len as usize))
        {
            bail!("crypto: invalid KDF parameters");
        }
        Ok(())
    }
}

/// Parse vault material straight from an `encfg` op.
impl VaultConfig {
    pub fn from_op(
        salt_b64: &str,
        kdf_json: &str,
        wrap_b64: &str,
        check_b64: &str,
        hint: &str,
        version: i32,
    ) -> Result<Self> {
        let salt = B64.decode(salt_b64).context("crypto: bad salt")?;
        let wrapped_master_key = B64.decode(wrap_b64).context("crypto: bad wrapped key")?;
        let key_check = B64.decode(check_b64).context("crypto: bad key check")?;
        let params = parse_params(kdf_json)?;
        if salt.len() != params.salt_len as usize
            || !(MIN_SALT_LEN..=MAX_SALT_LEN).contains(&salt.len())
        {
            bail!("crypto: invalid KDF parameters");
        }
        if wrapped_master_key.len() != WRAPPED_MASTER_KEY_LEN
            || key_check.len() != KEY_CHECK_ENVELOPE_LEN
        {
            bail!("crypto: corrupt encryption key data");
        }
        Ok(Self {
            salt,
            kdf_params_json: kdf_json.to_string(),
            wrapped_master_key,
            key_check,
            hint: hint.to_string(),
            version,
        })
    }

    fn params(&self) -> Result<VaultParams> {
        parse_params(&self.kdf_params_json)
    }

    /// Argon2id(password, salt) → 32-byte KEK.
    fn derive_kek(&self, password: &[u8]) -> Result<[u8; MASTER_KEY_LEN]> {
        let p = self.params()?;
        let argon = Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            ArgonParams::new(p.memory, p.time, p.parallelism as u32, Some(MASTER_KEY_LEN))
                .map_err(|e| anyhow!("crypto: argon2 params: {e}"))?,
        );
        let mut kek = [0u8; MASTER_KEY_LEN];
        argon
            .hash_password_into(password, &self.salt, &mut kek)
            .map_err(|e| anyhow!("crypto: argon2: {e}"))?;
        Ok(kek)
    }

    /// Full unlock: derive KEK, unwrap the master key, verify the check
    /// marker. Wrong password and tampered material are indistinguishable.
    pub fn unlock(&self, password: &[u8]) -> Result<[u8; MASTER_KEY_LEN]> {
        let kek = self.derive_kek(password)?;
        if self.wrapped_master_key.len() != WRAPPED_MASTER_KEY_LEN {
            bail!("crypto: corrupt encryption key data");
        }
        let aead = XChaCha20Poly1305::new_from_slice(&kek).context("crypto: KEK length")?;
        let nonce = <&XNonce>::try_from(&self.wrapped_master_key[..24]).expect("nonce");
        let master = aead
            .decrypt(nonce, &self.wrapped_master_key[24..])
            .map_err(|_| anyhow!("crypto: wrong password"))?;
        let mut key = [0u8; MASTER_KEY_LEN];
        key.copy_from_slice(&master);
        self.verify_key_check(&key)?;
        Ok(key)
    }

    fn verify_key_check(&self, master: &[u8; MASTER_KEY_LEN]) -> Result<()> {
        if self.key_check.len() != KEY_CHECK_ENVELOPE_LEN {
            bail!("crypto: corrupt encryption key data");
        }
        let aead = XChaCha20Poly1305::new_from_slice(master).context("crypto: key length")?;
        let nonce = <&XNonce>::try_from(&self.key_check[..24]).expect("nonce");
        let pt = aead
            .decrypt(nonce, &self.key_check[24..])
            .map_err(|_| anyhow!("crypto: wrong password"))?;
        if pt != KEY_CHECK_PLAINTEXT {
            bail!("crypto: corrupt encryption key data");
        }
        Ok(())
    }
}

/// Create a fresh vault (new random master key) wrapped under `password`.
pub fn create_vault(password: &[u8], hint: &str) -> Result<(VaultConfig, [u8; MASTER_KEY_LEN])> {
    let params = VaultParams::default();
    let mut salt = vec![0u8; params.salt_len as usize];
    rand::fill(&mut salt[..]);
    let mut master = [0u8; MASTER_KEY_LEN];
    rand::fill(&mut master);

    let argon = Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        ArgonParams::new(
            params.memory,
            params.time,
            params.parallelism as u32,
            Some(MASTER_KEY_LEN),
        )
        .map_err(|e| anyhow!("crypto: argon2 params: {e}"))?,
    );
    let mut kek = [0u8; MASTER_KEY_LEN];
    argon
        .hash_password_into(password, &salt, &mut kek)
        .map_err(|e| anyhow!("crypto: argon2: {e}"))?;

    let aead = XChaCha20Poly1305::new_from_slice(&kek).context("crypto: KEK")?;
    let mut nonce_bytes = [0u8; 24];
    rand::fill(&mut nonce_bytes);
    let wrapped_ct = aead
        .encrypt(
            <&XNonce>::try_from(&nonce_bytes[..]).expect("nonce"),
            master.as_ref(),
        )
        .map_err(|_| anyhow!("crypto: wrap failed"))?;
    let mut wrapped = Vec::with_capacity(WRAPPED_MASTER_KEY_LEN);
    wrapped.extend_from_slice(&nonce_bytes);
    wrapped.extend_from_slice(&wrapped_ct);

    let maead = XChaCha20Poly1305::new_from_slice(&master).context("crypto: key")?;
    let mut check_nonce = [0u8; 24];
    rand::fill(&mut check_nonce);
    let check_ct = maead
        .encrypt(
            <&XNonce>::try_from(&check_nonce[..]).expect("nonce"),
            KEY_CHECK_PLAINTEXT,
        )
        .map_err(|_| anyhow!("crypto: key check failed"))?;
    let mut check = Vec::with_capacity(KEY_CHECK_ENVELOPE_LEN);
    check.extend_from_slice(&check_nonce);
    check.extend_from_slice(&check_ct);

    let kdf_json = serde_json::json!({
        "kdf": "argon2id",
        "memory": params.memory,
        "time": params.time,
        "parallelism": params.parallelism,
        "key_len": params.key_len,
        "salt_len": params.salt_len,
    })
    .to_string();

    Ok((
        VaultConfig {
            salt,
            kdf_params_json: kdf_json,
            wrapped_master_key: wrapped,
            key_check: check,
            hint: hint.to_string(),
            version: 1,
        },
        master,
    ))
}

/// Encode vault material back to the `encfg` op field set.
impl VaultConfig {
    pub fn to_op_fields(&self) -> (String, String, String, String, String, i32) {
        (
            B64.encode(&self.salt),
            self.kdf_params_json.clone(),
            B64.encode(&self.wrapped_master_key),
            B64.encode(&self.key_check),
            self.hint.clone(),
            self.version,
        )
    }
}

// ---------------- TDE1 stream ----------------

#[derive(Debug, Clone, Copy)]
pub struct Tde1Header {
    pub salt: [u8; 16],
    pub nonce_prefix: [u8; NONCE_PREFIX_LEN],
    pub plaintext_size: u64,
}

/// Ciphertext size for a given plaintext length, including header and tags.
pub fn ciphertext_size(plaintext_size: u64) -> u64 {
    let chunks = plaintext_size / CHUNK_SIZE as u64 + 1;
    HEADER_LEN as u64 + plaintext_size + TAG_LEN as u64 * chunks
}

fn parse_header(buf: &[u8]) -> Result<Tde1Header> {
    if buf.len() < HEADER_LEN {
        bail!("crypto: stream too short");
    }
    if &buf[..4] != b"TDE1" {
        bail!("crypto: not a TDE1 stream");
    }
    if buf[4] != 0 {
        bail!("crypto: nonzero TDE1 flags");
    }
    if buf[5] != CHUNK_EXP {
        bail!("crypto: unsupported chunk exponent");
    }
    let plaintext_size = u64::from_le_bytes(buf[42..50].try_into().unwrap());
    if plaintext_size > MAX_PLAINTEXT {
        bail!("crypto: plaintext size out of range");
    }
    let mut salt = [0u8; 16];
    salt.copy_from_slice(&buf[6..22]);
    let mut nonce_prefix = [0u8; NONCE_PREFIX_LEN];
    nonce_prefix.copy_from_slice(&buf[22..42]);
    Ok(Tde1Header {
        salt,
        nonce_prefix,
        plaintext_size,
    })
}

fn derive_file_key(master: &[u8; MASTER_KEY_LEN], salt: &[u8; 16]) -> [u8; MASTER_KEY_LEN] {
    let hkdf = Hkdf::<Sha256>::new(Some(salt), master);
    let mut key = [0u8; MASTER_KEY_LEN];
    hkdf.expand(FILE_KEY_INFO, &mut key).expect("hkdf length");
    key
}

fn chunk_nonce(prefix: &[u8; NONCE_PREFIX_LEN], counter: u32) -> [u8; 24] {
    let mut nonce = [0u8; 24];
    nonce[..NONCE_PREFIX_LEN].copy_from_slice(prefix);
    nonce[NONCE_PREFIX_LEN..].copy_from_slice(&counter.to_be_bytes());
    nonce
}

fn chunk_count(plaintext_size: u64) -> u64 {
    plaintext_size / CHUNK_SIZE as u64 + 1
}

/// Decrypt a complete TDE1 ciphertext stream into a plaintext writer.
/// Validates the header and exact ciphertext length; each chunk is
/// authenticated before its plaintext is released.
pub fn decrypt_stream<R: Read, W: Write>(
    master: &[u8; MASTER_KEY_LEN],
    mut src: R,
    mut dst: W,
) -> Result<u64> {
    let mut header = [0u8; HEADER_LEN];
    src.read_exact(&mut header)?;
    let h = parse_header(&header)?;
    let key = derive_file_key(master, &h.salt);
    let aead = XChaCha20Poly1305::new_from_slice(&key).context("crypto: key")?;

    let chunks = chunk_count(h.plaintext_size);
    let mut written = 0u64;
    for i in 0..chunks {
        let is_final = i == chunks - 1;
        let want = if is_final {
            (h.plaintext_size % CHUNK_SIZE as u64) as usize
        } else {
            CHUNK_SIZE
        };
        let mut buf = vec![0u8; want + TAG_LEN];
        src.read_exact(&mut buf)?;
        let counter = (i as u32) | if is_final { FINAL_BIT } else { 0 };
        let nonce = chunk_nonce(&h.nonce_prefix, counter);
        let pt = aead
            .decrypt(
                <&XNonce>::try_from(&nonce[..]).expect("nonce"),
                buf.as_ref(),
            )
            .map_err(|_| anyhow!("crypto: chunk {i} authentication failed"))?;
        dst.write_all(&pt)?;
        written += pt.len() as u64;
    }
    // Trailing bytes beyond the declared layout are corruption.
    let mut probe = [0u8; 1];
    if src.read(&mut probe)? != 0 {
        bail!("crypto: trailing bytes after final chunk");
    }
    Ok(written)
}

/// Decrypt a staged ciphertext file to a plaintext destination.
pub fn decrypt_file(master: &[u8; MASTER_KEY_LEN], ct_path: &Path, pt_path: &Path) -> Result<u64> {
    let src = std::fs::File::open(ct_path)?;
    let mut dst = std::fs::File::create(pt_path)?;
    let n = decrypt_stream(
        master,
        std::io::BufReader::with_capacity(1 << 20, src),
        &mut dst,
    )?;
    dst.sync_all()?;
    Ok(n)
}

/// Random-access TDE1 reads: open validates the header and authenticates the
/// final chunk up front, like TDrive's RandomAccessDecryptor. `read_range`
/// authenticates only the chunks a plaintext range touches.
pub struct RangeDecryptor {
    aead: XChaCha20Poly1305,
    nonce_prefix: [u8; NONCE_PREFIX_LEN],
    pub plaintext_size: u64,
}

impl RangeDecryptor {
    /// Parse a TDE1 header read from the ciphertext start.
    pub fn new(master: &[u8; MASTER_KEY_LEN], header: &[u8]) -> Result<Self> {
        let h = parse_header(header)?;
        let key = derive_file_key(master, &h.salt);
        let aead = XChaCha20Poly1305::new_from_slice(&key).context("crypto: key")?;
        Ok(Self {
            aead,
            nonce_prefix: h.nonce_prefix,
            plaintext_size: h.plaintext_size,
        })
    }

    /// Ciphertext byte range covering plaintext range `[start, end)`:
    /// whole touched chunks including their tags.
    pub fn ct_range(&self, start: u64, end: u64) -> (u64, u64) {
        let total = self.chunk_total();
        let first = (start / CHUNK_SIZE as u64).min(total - 1);
        let last = (end.saturating_sub(1) / CHUNK_SIZE as u64).min(total - 1);
        let ct_start = self.chunk_ct_offset(first);
        let ct_end = self.chunk_ct_offset(last) + self.chunk_pt_len(last) + TAG_LEN as u64;
        (ct_start, ct_end)
    }

    fn chunk_total(&self) -> u64 {
        chunk_count(self.plaintext_size)
    }

    /// Byte offset where chunk `i`'s ciphertext+tag begins.
    fn chunk_ct_offset(&self, i: u64) -> u64 {
        HEADER_LEN as u64 + i * (CHUNK_SIZE + TAG_LEN) as u64
    }

    /// Plaintext length of chunk `i`.
    fn chunk_pt_len(&self, i: u64) -> u64 {
        if i == self.chunk_total() - 1 {
            self.plaintext_size % CHUNK_SIZE as u64
        } else {
            CHUNK_SIZE as u64
        }
    }

    /// Decrypt the plaintext range `[start, end)` given the ciphertext bytes
    /// returned by `ct_range`. Returns exactly `end - start` bytes.
    pub fn decrypt_range(&self, start: u64, end: u64, ct: &[u8]) -> Result<Vec<u8>> {
        let first = start / CHUNK_SIZE as u64;
        let total = self.chunk_total();
        let mut out = Vec::new();
        let mut off = 0usize;
        let mut i = first;
        while off < ct.len() && i < total {
            let is_final = i == total - 1;
            let want = self.chunk_pt_len(i) as usize;
            let seg = want + TAG_LEN;
            if off + seg > ct.len() {
                bail!("crypto: truncated chunk data");
            }
            let counter = (i as u32) | if is_final { FINAL_BIT } else { 0 };
            let nonce = chunk_nonce(&self.nonce_prefix, counter);
            let pt = self
                .aead
                .decrypt(
                    <&XNonce>::try_from(&nonce[..]).expect("nonce"),
                    &ct[off..off + seg],
                )
                .map_err(|_| anyhow!("crypto: chunk {i} authentication failed"))?;
            // Trim to the requested range.
            let chunk_start = i * CHUNK_SIZE as u64;
            let rel = start.saturating_sub(chunk_start) as usize;
            if rel < pt.len() {
                out.extend_from_slice(&pt[rel..]);
            }
            off += seg;
            i += 1;
        }
        out.truncate((end - start) as usize);
        Ok(out)
    }
}

/// Encrypt a plaintext buffer to a full TDE1 stream — for rendition bodies
/// and other small payloads.
pub fn encrypt_bytes(master: &[u8; MASTER_KEY_LEN], plaintext: &[u8]) -> Result<Vec<u8>> {
    let plaintext_size = plaintext.len() as u64;
    let mut salt = [0u8; 16];
    rand::fill(&mut salt);
    let mut nonce_prefix = [0u8; NONCE_PREFIX_LEN];
    rand::fill(&mut nonce_prefix);
    let key = derive_file_key(master, &salt);
    let aead = XChaCha20Poly1305::new_from_slice(&key).context("crypto: key")?;
    let mut out = Vec::with_capacity(ciphertext_size(plaintext_size) as usize);
    out.extend_from_slice(b"TDE1");
    out.push(0);
    out.push(CHUNK_EXP);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce_prefix);
    out.extend_from_slice(&plaintext_size.to_le_bytes());
    let total = chunk_count(plaintext_size);
    for i in 0..total {
        let is_final = i == total - 1;
        let want = if is_final {
            (plaintext_size % CHUNK_SIZE as u64) as usize
        } else {
            CHUNK_SIZE
        };
        let begin = i as usize * CHUNK_SIZE;
        let end = begin + want;
        let counter = (i as u32) | if is_final { FINAL_BIT } else { 0 };
        let nonce = chunk_nonce(&nonce_prefix, counter);
        let ct = aead
            .encrypt(
                <&XNonce>::try_from(&nonce[..]).expect("nonce"),
                &plaintext[begin..end],
            )
            .map_err(|_| anyhow!("crypto: encryption failed"))?;
        out.extend_from_slice(&ct);
    }
    Ok(out)
}

/// Encrypt a plaintext file to a TDE1 ciphertext file. Returns the stored
/// (ciphertext) size.
pub fn encrypt_file(master: &[u8; MASTER_KEY_LEN], pt_path: &Path, ct_path: &Path) -> Result<u64> {
    let plaintext_size = std::fs::metadata(pt_path)?.len();
    if plaintext_size > MAX_PLAINTEXT {
        bail!("crypto: plaintext size out of range");
    }
    let mut salt = [0u8; 16];
    rand::fill(&mut salt);
    let mut nonce_prefix = [0u8; NONCE_PREFIX_LEN];
    rand::fill(&mut nonce_prefix);
    let key = derive_file_key(master, &salt);
    let aead = XChaCha20Poly1305::new_from_slice(&key).context("crypto: key")?;

    let mut src = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(pt_path)?);
    let mut dst = std::fs::File::create(ct_path)?;
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(b"TDE1");
    header.push(0);
    header.push(CHUNK_EXP);
    header.extend_from_slice(&salt);
    header.extend_from_slice(&nonce_prefix);
    header.extend_from_slice(&plaintext_size.to_le_bytes());
    dst.write_all(&header)?;

    let chunks = chunk_count(plaintext_size);
    for i in 0..chunks {
        let is_final = i == chunks - 1;
        let want = if is_final {
            (plaintext_size % CHUNK_SIZE as u64) as usize
        } else {
            CHUNK_SIZE
        };
        let mut buf = vec![0u8; want];
        src.read_exact(&mut buf)?;
        let counter = (i as u32) | if is_final { FINAL_BIT } else { 0 };
        let nonce = chunk_nonce(&nonce_prefix, counter);
        let ct = aead
            .encrypt(
                <&XNonce>::try_from(&nonce[..]).expect("nonce"),
                buf.as_ref(),
            )
            .map_err(|_| anyhow!("crypto: encryption failed"))?;
        dst.write_all(&ct)?;
    }
    dst.sync_all()?;
    Ok(HEADER_LEN as u64 + plaintext_size + TAG_LEN as u64 * chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ciphertext_layout() {
        assert_eq!(ciphertext_size(0), 66); // empty: header + 1 empty tagged chunk
        assert_eq!(ciphertext_size(65536), 50 + 65536 + 32); // two chunks
        assert_eq!(ciphertext_size(1), 50 + 1 + 16);
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let master = [7u8; 32];
        let dir = std::env::temp_dir().join(format!("tde1-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pt = dir.join("plain.bin");
        let ct = dir.join("cipher.bin");
        let out = dir.join("out.bin");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&pt, &data).unwrap();
        let stored = encrypt_file(&master, &pt, &ct).unwrap();
        assert_eq!(stored, ciphertext_size(200_000));
        let n = decrypt_file(&master, &ct, &out).unwrap();
        assert_eq!(n, 200_000);
        assert_eq!(std::fs::read(&out).unwrap(), data);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wrong_key_fails() {
        let master = [7u8; 32];
        let wrong = [9u8; 32];
        let dir = std::env::temp_dir().join(format!("tde1-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pt = dir.join("plain.bin");
        let ct = dir.join("cipher.bin");
        std::fs::write(&pt, b"hello world").unwrap();
        encrypt_file(&master, &pt, &ct).unwrap();
        let out = dir.join("out.bin");
        assert!(decrypt_file(&wrong, &ct, &out).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn range_decryptor_reads_arbitrary_ranges() {
        let master = [9u8; MASTER_KEY_LEN];
        let size = 200_000usize; // spans 3+ chunks
        let plain: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let tmp = std::env::temp_dir();
        let pt = tmp.join("range-pt.bin");
        let ct = tmp.join("range-ct.bin");
        std::fs::write(&pt, &plain).unwrap();
        encrypt_file(&master, &pt, &ct).unwrap();
        let ct_bytes = std::fs::read(&ct).unwrap();

        let dec = RangeDecryptor::new(&master, &ct_bytes[..HEADER_LEN]).unwrap();
        assert_eq!(dec.plaintext_size, size as u64);
        for (s, e) in [
            (0u64, 1000u64),
            (65536, 70000),
            (199000, 200000),
            (10, 66000),
        ] {
            let (cs, ce) = dec.ct_range(s, e);
            let out = dec
                .decrypt_range(s, e, &ct_bytes[cs as usize..ce as usize])
                .unwrap();
            assert_eq!(&out[..], &plain[s as usize..e as usize], "range {s}..{e}");
        }
    }

    #[test]
    fn vault_roundtrip_and_wrong_password() {
        let (vault, master) = create_vault(b"correct horse", "hint").unwrap();
        let (salt, kdf, wrap, check, _, _) = vault.to_op_fields();
        let parsed = VaultConfig::from_op(&salt, &kdf, &wrap, &check, "hint", 1).unwrap();
        let unlocked = parsed.unlock(b"correct horse").unwrap();
        assert_eq!(unlocked, master);
        assert!(parsed.unlock(b"wrong").is_err());
    }

    #[test]
    fn decrypt_empty_file() {
        let master = [1u8; 32];
        let dir = std::env::temp_dir().join(format!("tde1-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pt = dir.join("e");
        let ct = dir.join("c");
        std::fs::write(&pt, b"").unwrap();
        encrypt_file(&master, &pt, &ct).unwrap();
        assert_eq!(std::fs::metadata(&ct).unwrap().len(), 66);
        let out = dir.join("o");
        let n = decrypt_file(&master, &ct, &out).unwrap();
        assert_eq!(n, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_go_params() {
        let json = r#"{"kdf":"argon2id","memory":65536,"time":3,"parallelism":4,"key_len":32,"salt_len":16}"#;
        let p = parse_params(json).unwrap();
        assert_eq!(p.memory, 65536);
        assert_eq!(p.key_len, 32);
    }
}
