//! Transfer journal — persists resumable upload/download progress.
//!
//! Upload jobs key on the TDX1 `u=` upload uuid: each part records the
//! Telegram message id assigned once its send was accepted, so a restarted
//! job re-sends only missing parts. Download jobs track confirmed 512 KiB
//! blocks already written to the destination.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartReceipt {
    pub index: i32,
    /// Telegram message id assigned when this part was accepted.
    pub msg_id: Option<i64>,
    pub size: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadJob {
    pub upload_uuid: String,
    pub channel_id: i64,
    pub local_path: PathBuf,
    /// Original plaintext source — needed to re-encrypt on resume.
    #[serde(default)]
    pub source_path: PathBuf,
    pub name: String,
    pub parent: String,
    pub stored_size: i64,
    #[serde(default)]
    pub plaintext_size: i64,
    #[serde(default)]
    pub encrypted: bool,
    pub parts: Vec<PartReceipt>,
    /// Set once the manifest message was accepted — file is live.
    pub manifest_msg_id: Option<i64>,
    pub started_at: i64,
}

impl UploadJob {
    pub fn pending_parts(&self) -> Vec<i32> {
        self.parts
            .iter()
            .filter(|p| p.msg_id.is_none())
            .map(|p| p.index)
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadJob {
    pub channel_id: i64,
    /// `f:` object id of the file.
    pub file_id: String,
    pub name: String,
    /// Ordered body message ids (single body or multipart parts).
    pub body_msg_ids: Vec<i64>,
    pub total_size: i64,
    pub dest: PathBuf,
    /// Blocks confirmed (flushed) so far, 512 KiB each.
    pub confirmed_bytes: i64,
    /// Ciphertext staged on disk; decrypted to `dest` when complete.
    #[serde(default)]
    pub encrypted: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Journal {
    #[serde(default)]
    pub uploads: Vec<UploadJob>,
    #[serde(default)]
    pub downloads: Vec<DownloadJob>,
    /// Photo-backup ledger: paths already uploaded by the watcher.
    #[serde(default)]
    pub backup_seen: std::collections::HashSet<String>,
    /// Watched backup directory, if any.
    #[serde(default)]
    pub backup_dir: Option<PathBuf>,
}

impl Journal {
    /// Uploads that can resume on the next open: this channel's jobs
    /// that never committed a manifest. TDrive's rule — a manifest op
    /// means done; everything else re-runs from its part receipts.
    pub fn resumable_uploads(&self, channel_id: i64) -> Vec<&UploadJob> {
        self.uploads
            .iter()
            .filter(|j| j.channel_id == channel_id && j.manifest_msg_id.is_none())
            .collect()
    }

    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        crate::paths::atomic_json_write(path, self)
    }
}
