//! Passphrase encryption for export bundles.
//!
//! Mirrors `src/dbs/crypto.py` in baileyrd/Daily-Backup-System (pinned
//! `@6cc6491`). The DB aggregates private bookmarks, saved posts, and
//! archived page copies; the copies people move *off*-machine (export
//! bundles) are the most exposed. This module encrypts any export
//! stream with a passphrase so an encrypted export's output is safe to
//! park on untrusted storage.
//!
//! **Format** (magic `DBSENC01`):
//! ```text
//! DBSENC01 || salt(16) || nonce_prefix(8) || frame*
//!
//! frame = len(u32 BE) || AES-256-GCM ciphertext
//! nonce = nonce_prefix(8) || counter(u32 BE)     // unique per frame
//! AAD   = b"dbs-final" on the last frame, b"dbs" otherwise
//! ```
//!
//! Design notes:
//!
//! * Key = `scrypt(passphrase, salt, n=2^14, r=8, p=1)` — memory-hard,
//!   so an offline brute-force of a weak passphrase stays expensive.
//! * Chunked (1 MiB plaintext per frame) so multi-GB archives stream
//!   through without buffering; the counter nonce makes frame
//!   *reordering* fail authentication, and the `dbs-final` AAD on the
//!   terminator frame makes *truncation* detectable — a prefix of a
//!   valid file never decrypts clean.
//! * The passphrase arrives via an env var (default
//!   `DBS_EXPORT_PASSPHRASE`), never argv — command lines leak into
//!   shell history and process listings.
//! * Unlike the reference, where `cryptography` is an optional
//!   `[crypto]` extra imported lazily, `aes-gcm`/`scrypt`/`rand` are
//!   unconditional dependencies of this crate — Cargo has no
//!   lazy-import equivalent, and both are small, audited, RustCrypto
//!   crates (pre-approved per `gap-analysis.md`'s Decisions section);
//!   `rusty_tls` was checked first per this issue's own acceptance
//!   criterion and is unreachable from this session (cross-tier repo
//!   access refused), so there was nothing there to verify against.
//! * [`EncryptingWriter::finish`] is this port's equivalent of the
//!   reference's explicit `close()`/context-manager requirement:
//!   Rust's `Drop` can't propagate errors, so the final frame (the one
//!   that makes truncation detectable) is only written by an explicit
//!   call, not implicitly on drop.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::rngs::OsRng;
use rand::RngCore;

use crate::errors::DbsError;

pub const MAGIC: &[u8] = b"DBSENC01";
pub const DEFAULT_PASSPHRASE_ENV: &str = "DBS_EXPORT_PASSPHRASE";

const CHUNK: usize = 1 << 20; // 1 MiB plaintext per frame
const AAD: &[u8] = b"dbs";
const AAD_FINAL: &[u8] = b"dbs-final";
const SCRYPT_LOG_N: u8 = 14; // n = 2^14
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;

fn io_err(e: std::io::Error) -> DbsError {
    DbsError::Storage(format!("encrypted stream I/O failed: {e}"))
}

fn derive_key(passphrase: &str, salt: &[u8]) -> Result<[u8; 32], DbsError> {
    let params = scrypt::Params::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P, 32)
        .map_err(|e| DbsError::Config(format!("invalid scrypt parameters: {e}")))?;
    let mut key = [0u8; 32];
    scrypt::scrypt(passphrase.as_bytes(), salt, &params, &mut key)
        .map_err(|e| DbsError::Config(format!("scrypt key derivation failed: {e}")))?;
    Ok(key)
}

fn build_cipher(passphrase: &str, salt: &[u8]) -> Result<Aes256Gcm, DbsError> {
    let key = derive_key(passphrase, salt)?;
    Ok(Aes256Gcm::new_from_slice(&key).expect("a 32-byte key is always valid for AES-256-GCM"))
}

fn frame_nonce(prefix: &[u8; 8], counter: u32) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(prefix);
    nonce[8..].copy_from_slice(&counter.to_be_bytes());
    nonce
}

/// Reads until `buf` is full or the stream is exhausted, returning the
/// number of bytes actually read — unlike a single [`Read::read`] call,
/// which may return short even mid-stream.
fn read_upto(src: &mut impl Read, buf: &mut [u8]) -> Result<usize, DbsError> {
    let mut total = 0;
    while total < buf.len() {
        match src.read(&mut buf[total..]).map_err(io_err)? {
            0 => break,
            n => total += n,
        }
    }
    Ok(total)
}

/// `true` if `path` starts with the encrypted-export magic header.
/// Any I/O error (missing file, permissions, ...) is treated as "not
/// encrypted" rather than propagated, matching the reference.
pub fn is_encrypted(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 8];
    matches!(read_upto(&mut file, &mut buf), Ok(n) if n == buf.len() && buf == *MAGIC)
}

/// A write-only stream adapter that encrypts what flows through it.
///
/// Exporters write plain bytes to their `out` handle; wrapping that
/// handle with this type is the whole integration — no exporter knows
/// encryption exists. [`Self::finish`] writes the final (possibly
/// empty) frame carrying the `dbs-final` AAD and returns the inner
/// writer; a writer that is never finished produces an *invalid* file
/// rather than a silently short one, since the terminator frame — the
/// one a decrypt run checks for — was never written.
pub struct EncryptingWriter<W: Write> {
    out: W,
    gcm: Aes256Gcm,
    prefix: [u8; 8],
    counter: u32,
    buf: Vec<u8>,
    finished: bool,
}

impl<W: Write> EncryptingWriter<W> {
    pub fn new(mut out: W, passphrase: &str) -> Result<Self, DbsError> {
        let mut salt = [0u8; 16];
        OsRng.fill_bytes(&mut salt);
        let mut prefix = [0u8; 8];
        OsRng.fill_bytes(&mut prefix);
        let gcm = build_cipher(passphrase, &salt)?;
        out.write_all(MAGIC).map_err(io_err)?;
        out.write_all(&salt).map_err(io_err)?;
        out.write_all(&prefix).map_err(io_err)?;
        Ok(Self {
            out,
            gcm,
            prefix,
            counter: 0,
            buf: Vec::new(),
            finished: false,
        })
    }

    fn emit(&mut self, plaintext: &[u8], final_frame: bool) -> Result<(), DbsError> {
        let nonce_bytes = frame_nonce(&self.prefix, self.counter);
        self.counter += 1;
        let aad: &[u8] = if final_frame { AAD_FINAL } else { AAD };
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .gcm
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|e| DbsError::Storage(format!("encryption failed: {e}")))?;
        self.out
            .write_all(&(ciphertext.len() as u32).to_be_bytes())
            .map_err(io_err)?;
        self.out.write_all(&ciphertext).map_err(io_err)?;
        Ok(())
    }

    /// Writes the final frame (whatever is left buffered, possibly
    /// none) and returns the inner writer. Idempotent — a second call
    /// is a no-op that returns the writer unchanged.
    pub fn finish(mut self) -> Result<W, DbsError> {
        if !self.finished {
            self.finished = true;
            let remaining = std::mem::take(&mut self.buf);
            self.emit(&remaining, true)?;
        }
        Ok(self.out)
    }
}

impl<W: Write> Write for EncryptingWriter<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        while self.buf.len() >= CHUNK {
            let chunk: Vec<u8> = self.buf.drain(..CHUNK).collect();
            self.emit(&chunk, false)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// Decrypts `src` into `dest`; returns the number of plaintext bytes
/// written.
///
/// Errors on a wrong passphrase, corruption, frame reordering, or
/// truncation (a missing final frame) — never partial silence.
pub fn decrypt_stream<R: Read, W: Write>(
    src: &mut R,
    dest: &mut W,
    passphrase: &str,
) -> Result<u64, DbsError> {
    let mut header = [0u8; 8 + 16 + 8];
    let header_len = read_upto(src, &mut header)?;
    if header_len < header.len() || &header[..8] != MAGIC {
        return Err(DbsError::Config(
            "not a dbs-encrypted file (bad magic header)".to_string(),
        ));
    }
    let salt = &header[8..24];
    let prefix: [u8; 8] = header[24..32].try_into().expect("exactly 8 bytes");
    let gcm = build_cipher(passphrase, salt)?;

    let mut counter: u32 = 0;
    let mut total: u64 = 0;
    let mut saw_final = false;

    loop {
        let mut len_buf = [0u8; 4];
        let len_read = read_upto(src, &mut len_buf)?;
        if len_read == 0 {
            break;
        }
        if len_read < 4 {
            return Err(DbsError::Config(
                "encrypted file is truncated mid-frame".to_string(),
            ));
        }
        let clen = u32::from_be_bytes(len_buf) as usize;
        let mut ciphertext = vec![0u8; clen];
        let ct_read = read_upto(src, &mut ciphertext)?;
        if ct_read < clen {
            return Err(DbsError::Config(
                "encrypted file is truncated mid-frame".to_string(),
            ));
        }

        let nonce_bytes = frame_nonce(&prefix, counter);
        counter += 1;
        let nonce = Nonce::from_slice(&nonce_bytes);

        // Try the normal-frame AAD first; only a genuine final frame
        // fails it and needs the fallback.
        let plaintext = match gcm.decrypt(
            nonce,
            Payload {
                msg: &ciphertext,
                aad: AAD,
            },
        ) {
            Ok(pt) => pt,
            Err(_) => match gcm.decrypt(
                nonce,
                Payload {
                    msg: &ciphertext,
                    aad: AAD_FINAL,
                },
            ) {
                Ok(pt) => {
                    saw_final = true;
                    pt
                }
                Err(_) => {
                    return Err(DbsError::Config(
                        "decryption failed — wrong passphrase, or the file is corrupt/tampered"
                            .to_string(),
                    ));
                }
            },
        };
        dest.write_all(&plaintext).map_err(io_err)?;
        total += plaintext.len() as u64;
        if saw_final {
            break;
        }
    }

    if !saw_final {
        return Err(DbsError::Config(
            "encrypted file is truncated (missing final frame) — refuse to treat a prefix as the whole backup"
                .to_string(),
        ));
    }
    let mut trailing = [0u8; 1];
    if read_upto(src, &mut trailing)? > 0 {
        return Err(DbsError::Config(
            "trailing data after the final encrypted frame".to_string(),
        ));
    }
    Ok(total)
}

/// Decrypts the file at `src` into `dest`; returns the number of
/// plaintext bytes written.
pub fn decrypt_file(src: &Path, dest: &Path, passphrase: &str) -> Result<u64, DbsError> {
    let mut fin = std::fs::File::open(src).map_err(io_err)?;
    let mut fout = std::fs::File::create(dest).map_err(io_err)?;
    decrypt_stream(&mut fin, &mut fout, passphrase)
}

/// Resolves the export passphrase: `secret_store[env_name]` first (when
/// given and non-empty), falling back to the process environment.
/// Errors if neither has it — a passphrase is required, never silently
/// treated as "no encryption".
pub fn resolve_passphrase(
    secret_store: Option<&HashMap<String, String>>,
    env_name: &str,
) -> Result<String, DbsError> {
    let mut value = secret_store
        .and_then(|s| s.get(env_name))
        .cloned()
        .unwrap_or_default();
    if value.is_empty() {
        value = std::env::var(env_name).unwrap_or_default();
    }
    if value.is_empty() {
        return Err(DbsError::Config(format!(
            "no passphrase: set {env_name} in the environment or .env (never on the command line)"
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn encrypt_all(passphrase: &str, plaintext: &[u8]) -> Vec<u8> {
        let mut writer = EncryptingWriter::new(Vec::new(), passphrase).unwrap();
        writer.write_all(plaintext).unwrap();
        writer.finish().unwrap()
    }

    #[test]
    fn round_trips_small_plaintext() {
        let ciphertext = encrypt_all("hunter2", b"hello world");
        let mut dest = Vec::new();
        let n = decrypt_stream(&mut Cursor::new(ciphertext), &mut dest, "hunter2").unwrap();
        assert_eq!(n, 11);
        assert_eq!(dest, b"hello world");
    }

    #[test]
    fn round_trips_across_a_chunk_boundary() {
        let plaintext = vec![0x42u8; CHUNK + 100];
        let ciphertext = encrypt_all("hunter2", &plaintext);
        let mut dest = Vec::new();
        let n = decrypt_stream(&mut Cursor::new(ciphertext), &mut dest, "hunter2").unwrap();
        assert_eq!(n as usize, plaintext.len());
        assert_eq!(dest, plaintext);
    }

    #[test]
    fn round_trips_empty_plaintext() {
        let ciphertext = encrypt_all("hunter2", b"");
        let mut dest = Vec::new();
        let n = decrypt_stream(&mut Cursor::new(ciphertext), &mut dest, "hunter2").unwrap();
        assert_eq!(n, 0);
        assert_eq!(dest, Vec::<u8>::new());
    }

    #[test]
    fn wrong_passphrase_fails_to_decrypt() {
        let ciphertext = encrypt_all("correct-horse", b"secret data");
        let mut dest = Vec::new();
        let err =
            decrypt_stream(&mut Cursor::new(ciphertext), &mut dest, "wrong-guess").unwrap_err();
        assert!(err.to_string().contains("decryption failed"));
    }

    #[test]
    fn tampered_ciphertext_fails_authentication() {
        let mut ciphertext = encrypt_all("hunter2", b"important backup data");
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xFF;
        let mut dest = Vec::new();
        let err = decrypt_stream(&mut Cursor::new(ciphertext), &mut dest, "hunter2").unwrap_err();
        assert!(err.to_string().contains("decryption failed"));
    }

    #[test]
    fn truncated_file_missing_final_frame_is_rejected() {
        let mut ciphertext = encrypt_all("hunter2", b"data that will be cut short");
        ciphertext.truncate(ciphertext.len() - 5);
        let mut dest = Vec::new();
        let err = decrypt_stream(&mut Cursor::new(ciphertext), &mut dest, "hunter2").unwrap_err();
        assert!(err.to_string().contains("truncated"));
    }

    #[test]
    fn bad_magic_header_is_rejected() {
        let mut dest = Vec::new();
        let err = decrypt_stream(
            &mut Cursor::new(b"not-encrypted-data-at-all".to_vec()),
            &mut dest,
            "hunter2",
        )
        .unwrap_err();
        assert!(err.to_string().contains("bad magic header"));
    }

    #[test]
    fn is_encrypted_detects_the_magic_header() {
        let dir = std::env::temp_dir().join(format!("dbs-crypto-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let encrypted_path = dir.join("bundle.enc");
        std::fs::write(&encrypted_path, encrypt_all("hunter2", b"data")).unwrap();
        let plain_path = dir.join("bundle.txt");
        std::fs::write(&plain_path, b"just plaintext").unwrap();

        assert!(is_encrypted(&encrypted_path));
        assert!(!is_encrypted(&plain_path));
        assert!(!is_encrypted(&dir.join("does-not-exist")));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn decrypt_file_round_trips_through_real_files() {
        let dir = std::env::temp_dir().join(format!("dbs-crypto-file-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let enc_path = dir.join("bundle.enc");
        let dec_path = dir.join("bundle.dec");
        std::fs::write(&enc_path, encrypt_all("hunter2", b"round trip me")).unwrap();

        let n = decrypt_file(&enc_path, &dec_path, "hunter2").unwrap();
        assert_eq!(n, 13);
        assert_eq!(std::fs::read(&dec_path).unwrap(), b"round trip me");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_passphrase_prefers_the_secret_store_over_the_environment() {
        let mut store = HashMap::new();
        store.insert(
            "DBS_EXPORT_PASSPHRASE".to_string(),
            "from-store".to_string(),
        );
        let value = resolve_passphrase(Some(&store), "DBS_EXPORT_PASSPHRASE").unwrap();
        assert_eq!(value, "from-store");
    }

    #[test]
    fn resolve_passphrase_falls_back_to_the_environment() {
        let env_name = "DBS_TEST_PASSPHRASE_FALLBACK";
        std::env::set_var(env_name, "from-env");
        let value = resolve_passphrase(None, env_name).unwrap();
        assert_eq!(value, "from-env");
        std::env::remove_var(env_name);
    }

    #[test]
    fn resolve_passphrase_errors_when_nothing_is_set() {
        let env_name = "DBS_TEST_PASSPHRASE_UNSET";
        std::env::remove_var(env_name);
        let err = resolve_passphrase(None, env_name).unwrap_err();
        assert!(err.to_string().contains("no passphrase"));
    }
}
