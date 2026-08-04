//! Optional upload of a finished local backup to S3-compatible storage.
//!
//! # The plaintext gate is the point of this module
//!
//! When `REMIND_ME_DB_ENCRYPTION_KEY` is set, the backup file is already
//! ciphertext, so shipping it to a third-party bucket is safe by default.
//! When it is **not** set, that file is **plaintext personal data** — every
//! memory the vault holds, in the clear.
//!
//! Uploading it therefore requires the explicit
//! `REMIND_ME_BACKUP_S3_ALLOW_PLAINTEXT_UPLOAD` opt-in, checked **before any
//! client is constructed or any byte leaves the machine**. Without that gate,
//! "enable cloud backup" would quietly mean "start shipping an unencrypted
//! copy of everything you have ever stored to a third party" — a materially
//! different decision from the one the user thinks they are making.
//!
//! Uploading plaintext personal data to third-party storage needs explicit
//! consent, not silent default behaviour.
//!
//! # No bespoke credential configuration
//!
//! There are deliberately **no** `REMIND_ME_BACKUP_S3_*` credential variables.
//! The AWS SDK already has a standard credential chain — environment, shared
//! credentials file, instance role — and reinventing a parallel one would be a
//! second secret-storage convention to get right, with none of the existing
//! hardening. Only non-secret configuration is read here: bucket, prefix,
//! endpoint, region.
//!
//! # A post-backup hook that can never damage the backup
//!
//! This runs only after the local file is finalised. A failed, refused or
//! misconfigured upload never undoes or blocks the local backup that already
//! succeeded, and nothing here can make a half-written file visible. Every
//! failure is reported to the caller as an outcome, never raised into the
//! backup path — the same rule the notification channels follow.

/// Bucket to upload into. Empty means cloud backup is off.
pub const BUCKET_ENV: &str = "REMIND_ME_BACKUP_S3_BUCKET";
/// Key prefix within the bucket. Blank uploads at the root.
pub const PREFIX_ENV: &str = "REMIND_ME_BACKUP_S3_PREFIX";
/// Custom endpoint, for any S3-compatible provider that is not AWS.
pub const ENDPOINT_ENV: &str = "REMIND_ME_BACKUP_S3_ENDPOINT";
pub const REGION_ENV: &str = "REMIND_ME_BACKUP_S3_REGION";
/// Explicit consent to upload an unencrypted backup.
pub const ALLOW_PLAINTEXT_ENV: &str = "REMIND_ME_BACKUP_S3_ALLOW_PLAINTEXT_UPLOAD";
/// Presence of this means the backup file is already ciphertext.
pub const DB_ENCRYPTION_KEY_ENV: &str = "REMIND_ME_DB_ENCRYPTION_KEY";

pub const PLAINTEXT_REFUSED: &str =
    "Cloud backup upload refused: REMIND_ME_DB_ENCRYPTION_KEY is not set, so the \
     local backup file is plaintext personal data. Uploading plaintext personal \
     data to third-party cloud storage needs explicit consent, not silent \
     default behaviour. Set REMIND_ME_BACKUP_S3_ALLOW_PLAINTEXT_UPLOAD=1 to \
     upload anyway, or set REMIND_ME_DB_ENCRYPTION_KEY to encrypt backups at \
     rest (recommended) before enabling cloud backup upload.";

pub const FEATURE_MISSING: &str =
    "REMIND_ME_BACKUP_S3_BUCKET is set but this build has no cloud-backup \
     support: rebuild with the `cloud-backup` feature (cargo build --features \
     cloud-backup), or unset REMIND_ME_BACKUP_S3_BUCKET to disable upload.";

/// What an upload attempt did. Every variant is a *report*, never an error
/// raised into the backup path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UploadOutcome {
    /// No bucket configured. The ordinary case, and not a problem.
    NotConfigured,
    /// Configured, but this build cannot upload.
    Unavailable {
        reason: String,
    },
    /// Refused by the plaintext gate.
    Refused {
        reason: String,
    },
    Uploaded {
        key: String,
    },
    Failed {
        reason: String,
    },
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default().trim().to_string()
}

pub fn configured_bucket() -> String {
    env(BUCKET_ENV)
}

pub fn enabled() -> bool {
    !configured_bucket().is_empty()
}

/// Whether this build can upload at all.
pub fn available() -> bool {
    cfg!(feature = "cloud-backup")
}

/// Join a prefix and filename into an object key.
///
/// Slashes are stripped from both ends of the prefix so `/host/backups/` and
/// `host/backups` produce an identical key, and a blank prefix uploads at the
/// bucket root rather than under a doubled `//`.
pub fn object_key(prefix: &str, filename: &str) -> String {
    let prefix = prefix.trim().trim_matches('/');
    if prefix.is_empty() {
        filename.to_string()
    } else {
        format!("{}/{}", prefix, filename)
    }
}

/// Is a truthy environment flag set?
fn flag(name: &str) -> bool {
    matches!(
        env(name).to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Whether an upload is permitted, and why not when it is not.
///
/// Deliberately a pure function of the environment, so the decision can be
/// tested without a bucket, a network, or the optional feature compiled in —
/// this is the part that must never be wrong.
pub fn plaintext_gate() -> Result<(), String> {
    // Already ciphertext: nothing to consent to.
    if !env(DB_ENCRYPTION_KEY_ENV).is_empty() {
        return Ok(());
    }
    if flag(ALLOW_PLAINTEXT_ENV) {
        return Ok(());
    }
    Err(PLAINTEXT_REFUSED.to_string())
}

/// Upload a finished backup file.
///
/// Never returns an error: the caller is `create_backup`, which has already
/// succeeded, and no outcome here may undo or obscure that.
pub fn upload_backup(path: &std::path::Path) -> UploadOutcome {
    let bucket = configured_bucket();
    if bucket.is_empty() {
        return UploadOutcome::NotConfigured;
    }

    // Before the client, before the file is read, before anything leaves the
    // machine.
    if let Err(reason) = plaintext_gate() {
        return UploadOutcome::Refused { reason };
    }

    if !available() {
        return UploadOutcome::Unavailable {
            reason: FEATURE_MISSING.to_string(),
        };
    }

    let filename = match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name.to_string(),
        None => {
            return UploadOutcome::Failed {
                reason: format!("unreadable backup path: {}", path.display()),
            }
        }
    };
    let key = object_key(&env(PREFIX_ENV), &filename);

    #[cfg(feature = "cloud-backup")]
    {
        match put_object(&bucket, &key, path) {
            Ok(()) => UploadOutcome::Uploaded { key },
            // The message is the SDK's, which never contains the credential
            // itself — but the bucket and endpoint are deliberately the only
            // configuration echoed back anywhere in this module.
            Err(reason) => UploadOutcome::Failed { reason },
        }
    }
    #[cfg(not(feature = "cloud-backup"))]
    {
        let _ = key;
        UploadOutcome::Unavailable {
            reason: FEATURE_MISSING.to_string(),
        }
    }
}

#[cfg(feature = "cloud-backup")]
fn put_object(bucket: &str, key: &str, path: &std::path::Path) -> Result<(), String> {
    use aws_sdk_s3::primitives::ByteStream;

    let body = std::fs::read(path).map_err(|e| format!("could not read backup: {}", e))?;

    // Its own runtime rather than assuming the caller has one: `create_backup`
    // is synchronous, and a backup must not depend on where it was called
    // from.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("could not start an upload runtime: {}", e))?;

    runtime.block_on(async {
        // `defaults(BehaviorVersion::latest())` rather than the deprecated
        // `from_env()`: the SDK pins behaviour to a version so a future
        // default change cannot silently alter how uploads are signed or
        // retried.
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        let region = env(REGION_ENV);
        if !region.is_empty() {
            loader = loader.region(aws_sdk_s3::config::Region::new(region));
        }
        let shared = loader.load().await;

        let mut builder = aws_sdk_s3::config::Builder::from(&shared);
        // A custom endpoint is what makes this work against Backblaze, MinIO
        // and the rest rather than AWS alone.
        let endpoint = env(ENDPOINT_ENV);
        if !endpoint.is_empty() {
            builder = builder.endpoint_url(endpoint).force_path_style(true);
        }

        aws_sdk_s3::Client::from_conf(builder.build())
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from(body))
            .send()
            .await
            .map(|_| ())
            .map_err(|e| format!("upload failed: {}", e))
    })
}
