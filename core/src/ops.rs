//! Named constructors for every TDX1 op this client emits.
//!
//! Two vocabularies for the same intents live here side by side, mirroring
//! TDrive: the interactive path uses legacy ops (`mkdir`, `trash`,
//! `f`/`manifest`), while the mounted write path (WebDAV/FUSE) uses the
//! versioned-envelope ops (`dcommit`, `relocate`, `harddel`, `fcommit`,
//! `freplace`). Field conventions — `v=1` envelopes, op_id derivation,
//! CAS revisions, retention windows — belong to each constructor, not to
//! the twenty call sites that used to repeat them.

use crate::crypto::VaultConfig;
use crate::wire::{self, Op, OpType, OpTypeOrUnknown};

/// TDrive's tombstone retention window (30 days) in seconds.
pub const RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn known(t: OpType) -> OpTypeOrUnknown {
    OpTypeOrUnknown::Known(t)
}

fn versioned_op_id(purpose: &str) -> String {
    format!("{purpose}-{}", uuid::Uuid::new_v4())
}

// ── Interactive vocabulary ──────────────────────────────────────────────────

pub fn mkdir(parent: impl Into<String>, name: impl Into<String>) -> Op {
    mkdir_with_id(wire::new_folder_id(), parent, name)
}

/// `mkdir` with a caller-minted folder id — the dir walker pre-mints ids
/// so children can reference a folder before its op lands.
pub fn mkdir_with_id(
    obj: impl Into<String>,
    parent: impl Into<String>,
    name: impl Into<String>,
) -> Op {
    Op {
        op_type: known(OpType::Mkdir),
        obj: obj.into(),
        parent: parent.into(),
        name: name.into(),
        ..Default::default()
    }
}

pub fn rename(obj: impl Into<String>, name: impl Into<String>) -> Op {
    Op {
        op_type: known(OpType::Rename),
        obj: obj.into(),
        name: name.into(),
        ..Default::default()
    }
}

pub fn move_obj(obj: impl Into<String>, parent: impl Into<String>) -> Op {
    Op {
        op_type: known(OpType::Move),
        obj: obj.into(),
        parent: parent.into(),
        ..Default::default()
    }
}

/// Move a live object to the trash — deterministic op id so replays dedup.
pub fn trash(obj: impl Into<String>, revision: i64) -> Op {
    let obj = obj.into();
    let now = unix_now();
    Op {
        op_type: known(OpType::TrashTree),
        protocol_version: 1,
        op_id: wire::deterministic_op_id("trash", &obj, revision),
        obj,
        expected_revision: revision,
        deleted_at: now,
        purge_after: now + RETENTION_SECS,
        ..Default::default()
    }
}

/// Permanent delete marker for a trashed object.
pub fn hard_delete(obj: impl Into<String>, revision: i64) -> Op {
    let obj = obj.into();
    Op {
        op_type: known(OpType::HardDeleteTree),
        protocol_version: 1,
        op_id: wire::deterministic_op_id("purge", &obj, revision),
        obj,
        expected_revision: revision,
        ..Default::default()
    }
}

/// Restore a trashed object under `parent` as `name`.
pub fn restore(
    obj: impl Into<String>,
    parent: impl Into<String>,
    name: impl Into<String>,
    revision: i64,
) -> Op {
    let obj = obj.into();
    Op {
        op_type: known(OpType::RestoreTree),
        protocol_version: 1,
        op_id: wire::deterministic_op_id("restore", &obj, revision),
        obj,
        parent: parent.into(),
        name: name.into(),
        expected_revision: revision,
        ..Default::default()
    }
}

/// Claim a raw channel attachment (`meta` op) — the object id derives from
/// the attachment's message id.
pub fn meta(
    msg_id: i64,
    parent: impl Into<String>,
    name: impl Into<String>,
    size: i64,
    date: i64,
) -> Op {
    Op {
        op_type: known(OpType::Meta),
        obj: wire::file_obj_id(msg_id),
        parent: parent.into(),
        name: name.into(),
        file_size: size,
        file_upload_time: date,
        ..Default::default()
    }
}

/// Single-body file record. `plaintext_size` is only emitted when
/// `encrypted` — the format layer drops it otherwise.
pub fn file_upload(
    parent: impl Into<String>,
    name: impl Into<String>,
    size: i64,
    encrypted: bool,
    plaintext_size: i64,
) -> Op {
    Op {
        op_type: known(OpType::FileUpload),
        parent: parent.into(),
        name: name.into(),
        file_size: size,
        file_upload_time: unix_now(),
        encrypted,
        plaintext_size: if encrypted { plaintext_size } else { 0 },
        ..Default::default()
    }
}

/// Multipart manifest — the file's visibility boundary.
pub fn file_manifest(
    upload_uuid: impl Into<String>,
    parent: impl Into<String>,
    name: impl Into<String>,
    part_count: i32,
    stored_size: i64,
    started_at: i64,
    encrypted: bool,
    plaintext_size: i64,
) -> Op {
    Op {
        op_type: known(OpType::FileManifest),
        upload_uuid: upload_uuid.into(),
        parent: parent.into(),
        name: name.into(),
        part_count,
        file_size: stored_size,
        file_upload_time: started_at,
        encrypted,
        plaintext_size: if encrypted { plaintext_size } else { 0 },
        ..Default::default()
    }
}

/// One body part of a multipart upload.
pub fn file_part(upload_uuid: impl Into<String>, index: i32, size: i64) -> Op {
    Op {
        op_type: known(OpType::FilePart),
        upload_uuid: upload_uuid.into(),
        part_index: index,
        file_size: size,
        ..Default::default()
    }
}

/// A rendition body bound to a logical file (thumbnail/preview).
pub fn rendition_part(size: i64, rendition: &wire::Rendition) -> Op {
    Op {
        op_type: known(OpType::FilePart),
        upload_uuid: uuid::Uuid::new_v4().to_string(),
        part_index: 0,
        file_size: size,
        rendition: rendition.encode(),
        ..Default::default()
    }
}

/// The drive's encryption vault descriptor.
pub fn enc_config(vault: &VaultConfig) -> Op {
    let (salt, kdf, wrap, check, _, cv) = vault.to_op_fields();
    Op {
        op_type: known(OpType::EncConfig),
        kdf_salt_b64: salt,
        kdf_params_json: kdf,
        wrapped_key_b64: wrap,
        key_check_b64: check,
        hint: vault.hint.clone(),
        config_version: cv,
        ..Default::default()
    }
}

// ── Mounted-write vocabulary ────────────────────────────────────────────────

/// Write operations arriving over the mounted surfaces (WebDAV + FUSE).
/// Semantics mirror TDrive's writable mount: mkdir is a folder commit,
/// delete is a durable hard-delete, move is a CAS relocate.
/// Largest payload a mount write may carry — dav PUT bodies and FUSE
/// write buffers share the cap.
pub const MAX_PUT_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug)]
pub enum VfsOp {
    /// Create folder `name` under `parent` (folder id or "").
    Mkdir { parent: String, name: String },
    /// Permanently remove an object (hard-delete flow).
    Delete { obj: String },
    /// Relocate `obj` under `dst_parent` as `name`.
    Move {
        obj: String,
        dst_parent: String,
        name: String,
    },
    /// Write `bytes` as `name` under `parent` — single-body fcommit.
    Put {
        parent: String,
        name: String,
        bytes: Vec<u8>,
    },
    /// Overwrite an existing file's content — single-body freplace.
    Replace { obj: String, bytes: Vec<u8> },
}

pub fn folder_commit(parent: impl Into<String>, name: impl Into<String>) -> Op {
    Op {
        op_type: known(OpType::FolderCommit),
        protocol_version: 1,
        op_id: versioned_op_id("dcommit"),
        obj: wire::new_folder_id(),
        parent: parent.into(),
        name: name.into(),
        ..Default::default()
    }
}

pub fn relocate(
    obj: impl Into<String>,
    dst_parent: impl Into<String>,
    name: impl Into<String>,
    revision: i64,
) -> Op {
    Op {
        op_type: known(OpType::Relocate),
        protocol_version: 1,
        op_id: versioned_op_id("relocate"),
        obj: obj.into(),
        parent: dst_parent.into(),
        name: name.into(),
        expected_revision: revision,
        ..Default::default()
    }
}

/// Bind a freshly uploaded body to a new file object.
pub fn file_commit(
    parent: impl Into<String>,
    name: impl Into<String>,
    stored_size: i64,
    encrypted: bool,
    plaintext_size: i64,
    content_msg_id: i64,
) -> Op {
    Op {
        op_type: known(OpType::FileCommit),
        protocol_version: 1,
        op_id: versioned_op_id("fcommit"),
        parent: parent.into(),
        name: name.into(),
        file_size: stored_size,
        file_upload_time: unix_now(),
        encrypted,
        plaintext_size: if encrypted { plaintext_size } else { 0 },
        content_msg_id,
        ..Default::default()
    }
}

/// Rebind an existing file to a freshly uploaded body.
pub fn file_replace(
    obj: impl Into<String>,
    stored_size: i64,
    encrypted: bool,
    plaintext_size: i64,
    content_msg_id: i64,
    revision: i64,
) -> Op {
    Op {
        op_type: known(OpType::FileReplace),
        protocol_version: 1,
        retained_until: unix_now() + RETENTION_SECS,
        op_id: versioned_op_id("freplace"),
        obj: obj.into(),
        file_size: stored_size,
        file_upload_time: unix_now(),
        encrypted,
        plaintext_size: if encrypted { plaintext_size } else { 0 },
        content_msg_id,
        expected_revision: revision,
        ..Default::default()
    }
}
