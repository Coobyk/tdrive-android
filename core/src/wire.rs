//! TDX1 wire format — compatible with TDrive's `backend/projection/wire.go`.
//!
//! Every operation is a `TDX1|t=<type>|key=value|...` header stored as the
//! first line of a Telegram message text or document caption.

use std::collections::HashMap;
use std::fmt::Write as _;
use thiserror::Error;

pub const WIRE_PREFIX: &str = "TDX1";
pub const FOLDER_ID_PREFIX: &str = "d:";
pub const FILE_ID_PREFIX: &str = "f:";
/// Empty string marks the drive root.
pub const ROOT_PARENT: &str = "";
const MAX_WIRE_UNIX_TS: i64 = 253_402_300_799;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WireError {
    #[error("wire: header missing")]
    MissingHeader,
    #[error("wire: unknown version")]
    BadVersion,
    #[error("wire: unknown op type")]
    BadOpType,
    #[error("wire: malformed header")]
    Malformed,
    #[error("wire: bad object id")]
    BadObject,
    #[error("wire: bad parent id")]
    BadParent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OpType {
    FileUpload,
    Mkdir,
    Rename,
    Move,
    Rmdir,
    Tomb,
    Meta,
    EncConfig,
    FilePart,
    FileManifest,
    FileCommit,
    FileReplace,
    FolderCommit,
    Relocate,
    TrashTree,
    HardDeleteTree,
    RestoreTree,
}

impl OpType {
    fn as_str(self) -> &'static str {
        match self {
            OpType::FileUpload => "f",
            OpType::Mkdir => "mkdir",
            OpType::Rename => "rename",
            OpType::Move => "move",
            OpType::Rmdir => "rmdir",
            OpType::Tomb => "tomb",
            OpType::Meta => "meta",
            OpType::EncConfig => "encfg",
            OpType::FilePart => "part",
            OpType::FileManifest => "manifest",
            OpType::FileCommit => "fcommit",
            OpType::FileReplace => "freplace",
            OpType::FolderCommit => "dcommit",
            OpType::Relocate => "relocate",
            OpType::TrashTree => "trash",
            OpType::HardDeleteTree => "harddel",
            OpType::RestoreTree => "untrash",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "f" => OpType::FileUpload,
            "mkdir" => OpType::Mkdir,
            "rename" => OpType::Rename,
            "move" => OpType::Move,
            "rmdir" => OpType::Rmdir,
            "tomb" => OpType::Tomb,
            "meta" => OpType::Meta,
            "encfg" => OpType::EncConfig,
            "part" => OpType::FilePart,
            "manifest" => OpType::FileManifest,
            "fcommit" => OpType::FileCommit,
            "freplace" => OpType::FileReplace,
            "dcommit" => OpType::FolderCommit,
            "relocate" => OpType::Relocate,
            "trash" => OpType::TrashTree,
            "harddel" => OpType::HardDeleteTree,
            "untrash" => OpType::RestoreTree,
            _ => return None,
        })
    }
}

/// A parsed TDX1 operation. Fields mirror TDrive's `projection.Op`; the ones
/// the MVP never reads (renditions, encryption config bodies) are kept raw.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Op {
    pub op_type: OpTypeOrUnknown,
    pub protocol_version: i32,
    pub op_id: String,

    pub obj: String,
    pub parent: String,
    pub name: String,

    pub expected_revision: i64,
    pub expected_destination_revision: i64,

    pub content_msg_id: i64,
    pub content_hash: String,

    pub overwrite: bool,
    pub destination_obj: String,
    pub deleted_at: i64,
    pub purge_after: i64,
    pub retained_until: i64,

    pub file_size: i64,
    pub file_upload_time: i64,

    pub encrypted: bool,
    pub plaintext_size: i64,
    pub encryption_version: i32,

    pub upload_uuid: String,
    pub part_index: i32,
    pub part_count: i32,

    /// Vault fields on `encfg` ops (kept as raw wire strings — the crypto
    /// module base64-decodes them).
    pub kdf_salt_b64: String,
    pub kdf_params_json: String,
    pub wrapped_key_b64: String,
    pub key_check_b64: String,
    pub hint: String,
    pub config_version: i32,

    /// Raw `rend=` rendition envelope on `part` ops (base64url JSON).
    /// Captured so rendition-bearing messages round-trip.
    pub rendition: String,
}

/// TDrive `FileRendition` — a thumbnail/preview stored as a hidden `part`
/// document carrying a `rend=` envelope.

/// TDrive rendition kinds — the `k` field of a FileRendition.
pub const RENDITION_THUMB: &str = "thumbnail";
pub const RENDITION_PREVIEW: &str = "preview";
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Rendition {
    /// `c`: channel id
    #[serde(rename = "c")]
    pub channel_id: i64,
    /// `f`: logical file msg id
    #[serde(rename = "f")]
    pub file_msg_id: i64,
    /// `m`: content msg id (single-body files)
    #[serde(default, rename = "m")]
    pub content_msg_id: i64,
    /// `u`: upload uuid (multipart files)
    #[serde(default, rename = "u")]
    pub upload_uuid: String,
    /// `k`: "thumbnail" | "preview" (TDrive's rendition kinds)
    #[serde(rename = "k")]
    pub kind: String,
    /// `v`: format version (1)
    #[serde(rename = "v")]
    pub version: i32,
    /// `s`: stored bytes
    #[serde(rename = "s")]
    pub size: i64,
    /// `p`: plaintext bytes
    #[serde(rename = "p")]
    pub plaintext_size: i64,
    /// `w`/`h`: pixel dimensions
    #[serde(rename = "w")]
    pub width: i32,
    #[serde(rename = "h")]
    pub height: i32,
    /// `e`: TDE1-encrypted body
    #[serde(default, rename = "e")]
    pub encrypted: bool,
}

impl Rendition {
    /// Decode a raw `rend=` field (base64url JSON).
    pub fn decode(raw: &str) -> Option<Self> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(raw)
            .ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Encode to the raw `rend=` field (base64url JSON, no padding).
    pub fn encode(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(self).unwrap_or_default())
    }
}

/// Unknown op types parse as `Unknown` so a newer TDrive's messages are
/// skipped instead of breaking the scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum OpTypeOrUnknown {
    Known(OpType),
    #[default]
    Unknown,
}

/// First line of a message/caption — the only place a header can live.
pub fn extract_header_line(raw: &str) -> &str {
    raw.split('\n').next().unwrap_or("")
}

fn query_unescape(s: &str) -> Result<String, WireError> {
    // application/x-www-form-urlencoded decoding: '+' -> ' ', %XX -> byte.
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(WireError::Malformed);
                }
                let hex =
                    std::str::from_utf8(&bytes[i + 1..i + 3]).map_err(|_| WireError::Malformed)?;
                let v = u8::from_str_radix(hex, 16).map_err(|_| WireError::Malformed)?;
                out.push(v);
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).map_err(|_| WireError::Malformed)
}

fn query_escape(s: &str) -> String {
    // Go's url.QueryEscape: unreserved [A-Za-z0-9-_.~] kept, space -> "+",
    // everything else percent-encoded (uppercase hex).
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

fn set_obj(op: &mut Op, raw: &str, prefix: &str) -> Result<(), WireError> {
    if raw.is_empty() || !raw.starts_with(prefix) {
        return Err(WireError::BadObject);
    }
    op.obj = raw.to_string();
    Ok(())
}

fn set_obj_any(op: &mut Op, raw: &str) -> Result<(), WireError> {
    set_obj_any_raw(&mut op.obj, raw)
}

fn set_obj_any_raw(dst: &mut String, raw: &str) -> Result<(), WireError> {
    if raw.is_empty() || (!raw.starts_with(FOLDER_ID_PREFIX) && !raw.starts_with(FILE_ID_PREFIX)) {
        return Err(WireError::BadObject);
    }
    *dst = raw.to_string();
    Ok(())
}

fn set_parent(op: &mut Op, raw: &str) -> Result<(), WireError> {
    if raw.is_empty() {
        op.parent = ROOT_PARENT.to_string();
        return Ok(());
    }
    if !raw.starts_with(FOLDER_ID_PREFIX) {
        return Err(WireError::BadParent);
    }
    op.parent = raw.to_string();
    Ok(())
}

fn set_name(op: &mut Op, raw: &str) -> Result<(), WireError> {
    if raw.is_empty() {
        return Err(WireError::Malformed);
    }
    op.name = query_unescape(raw)?;
    Ok(())
}

fn set_positive_i64(dst: &mut i64, raw: &str) -> Result<(), WireError> {
    let n: i64 = raw.parse().map_err(|_| WireError::Malformed)?;
    if n <= 0 {
        return Err(WireError::Malformed);
    }
    *dst = n;
    Ok(())
}

fn set_writable_envelope(op: &mut Op, kv: &HashMap<String, String>) -> Result<(), WireError> {
    let version: i32 = kv
        .get("v")
        .and_then(|v| v.parse().ok())
        .ok_or(WireError::Malformed)?;
    if version != 1 {
        return Err(WireError::Malformed);
    }
    let raw_id = kv.get("oid").cloned().unwrap_or_default();
    let op_id = query_unescape(&raw_id)?;
    if op_id.is_empty() {
        return Err(WireError::Malformed);
    }
    op.protocol_version = version;
    op.op_id = op_id;
    Ok(())
}

fn set_content_reference(op: &mut Op, kv: &HashMap<String, String>) -> Result<(), WireError> {
    if let Some(raw) = kv.get("cmid") {
        if !raw.is_empty() {
            set_positive_i64(&mut op.content_msg_id, raw)?;
        }
    }
    if let Some(raw) = kv.get("u") {
        if raw.is_empty() {
            return Err(WireError::Malformed);
        }
        op.upload_uuid = raw.clone();
    }
    if let Some(raw) = kv.get("pc") {
        let n: i32 = raw.parse().map_err(|_| WireError::Malformed)?;
        if n <= 0 {
            return Err(WireError::Malformed);
        }
        op.part_count = n;
    }
    if let Some(raw) = kv.get("hash") {
        op.content_hash = query_unescape(raw)?;
    }
    if op.content_msg_id <= 0 && (op.upload_uuid.is_empty() || op.part_count <= 0) {
        return Err(WireError::Malformed);
    }
    if op.content_msg_id > 0 && (!op.upload_uuid.is_empty() || op.part_count != 0) {
        return Err(WireError::Malformed);
    }
    Ok(())
}

pub fn parse(raw: &str) -> Result<Op, WireError> {
    let header = extract_header_line(raw).trim();
    if header.is_empty() {
        return Err(WireError::MissingHeader);
    }
    let parts: Vec<&str> = header.split('|').collect();
    if parts.len() < 2 {
        return Err(WireError::Malformed);
    }
    if parts[0] != WIRE_PREFIX {
        return Err(WireError::BadVersion);
    }

    let mut kv: HashMap<String, String> = HashMap::with_capacity(parts.len() - 1);
    for p in &parts[1..] {
        let eq = p.find('=').ok_or(WireError::Malformed)?;
        if eq == 0 {
            return Err(WireError::Malformed);
        }
        kv.insert(p[..eq].to_string(), p[eq + 1..].to_string());
    }

    let t = kv.get("t").ok_or(WireError::Malformed)?;
    let mut op = Op::default();
    let op_type = OpType::from_str(t).ok_or(WireError::BadOpType)?;
    op.op_type = OpTypeOrUnknown::Known(op_type);

    let get = |k: &str| kv.get(k).map(String::as_str).unwrap_or("");

    match op_type {
        OpType::FileUpload => {
            set_parent(&mut op, get("p"))?;
            set_name(&mut op, get("n"))?;
        }
        OpType::Meta => {
            set_obj(&mut op, get("obj"), FILE_ID_PREFIX)?;
            set_parent(&mut op, get("p"))?;
            set_name(&mut op, get("n"))?;
        }
        OpType::Mkdir => {
            set_obj(&mut op, get("obj"), FOLDER_ID_PREFIX)?;
            set_parent(&mut op, get("p"))?;
            set_name(&mut op, get("n"))?;
        }
        OpType::Rename => {
            set_obj_any(&mut op, get("obj"))?;
            set_name(&mut op, get("n"))?;
        }
        OpType::Move => {
            set_obj_any(&mut op, get("obj"))?;
            set_parent(&mut op, get("p"))?;
        }
        OpType::Rmdir => set_obj(&mut op, get("obj"), FOLDER_ID_PREFIX)?,
        OpType::Tomb => set_obj(&mut op, get("obj"), FILE_ID_PREFIX)?,
        OpType::FilePart => {
            let u = get("u");
            if u.is_empty() {
                return Err(WireError::Malformed);
            }
            op.upload_uuid = u.to_string();
            let pix: i32 = get("pix").parse().map_err(|_| WireError::Malformed)?;
            if pix < 0 {
                return Err(WireError::Malformed);
            }
            op.part_index = pix;
        }
        OpType::FileManifest => {
            let u = get("u");
            if u.is_empty() {
                return Err(WireError::Malformed);
            }
            op.upload_uuid = u.to_string();
            set_parent(&mut op, get("p"))?;
            set_name(&mut op, get("n"))?;
            let pc: i32 = get("pc").parse().map_err(|_| WireError::Malformed)?;
            if pc <= 0 {
                return Err(WireError::Malformed);
            }
            op.part_count = pc;
        }
        OpType::EncConfig => {
            for (key, dst) in [
                ("salt", &mut op.kdf_salt_b64),
                ("kdf", &mut op.kdf_params_json),
                ("wrap", &mut op.wrapped_key_b64),
                ("check", &mut op.key_check_b64),
            ] {
                let raw = get(key);
                if raw.is_empty() {
                    return Err(WireError::Malformed);
                }
                // kdf JSON is url-escaped on the wire; the rest are base64url.
                *dst = if key == "kdf" {
                    query_unescape(raw)?
                } else {
                    raw.to_string()
                };
            }
            if let Some(hint) = kv.get("hint") {
                op.hint = query_unescape(hint)?;
            }
            op.config_version = kv.get("cv").and_then(|s| s.parse().ok()).unwrap_or(1);
        }
        OpType::FileCommit => {
            set_writable_envelope(&mut op, &kv)?;
            set_parent(&mut op, get("p"))?;
            set_name(&mut op, get("n"))?;
            set_content_reference(&mut op, &kv)?;
        }
        OpType::FolderCommit => {
            set_writable_envelope(&mut op, &kv)?;
            set_obj(&mut op, get("obj"), FOLDER_ID_PREFIX)?;
            set_parent(&mut op, get("p"))?;
            set_name(&mut op, get("n"))?;
        }
        OpType::FileReplace => {
            set_writable_envelope(&mut op, &kv)?;
            set_obj(&mut op, get("obj"), FILE_ID_PREFIX)?;
            set_positive_i64(&mut op.expected_revision, get("rev"))?;
            set_content_reference(&mut op, &kv)?;
            set_positive_i64(&mut op.retained_until, get("retain"))?;
        }
        OpType::Relocate => {
            set_writable_envelope(&mut op, &kv)?;
            set_obj_any(&mut op, get("obj"))?;
            set_parent(&mut op, get("p"))?;
            set_name(&mut op, get("n"))?;
            set_positive_i64(&mut op.expected_revision, get("rev"))?;
            op.overwrite = get("ow") == "1";
            if !get("dst").is_empty() {
                let mut dst = String::new();
                set_obj_any_raw(&mut dst, get("dst"))?;
                op.destination_obj = dst;
            }
            if let Some(raw) = kv.get("drev") {
                if !raw.is_empty() {
                    set_positive_i64(&mut op.expected_destination_revision, raw)?;
                }
            }
            if let Some(raw) = kv.get("del") {
                if !raw.is_empty() {
                    set_positive_i64(&mut op.deleted_at, raw)?;
                }
            }
            if let Some(raw) = kv.get("purge") {
                if !raw.is_empty() {
                    set_positive_i64(&mut op.purge_after, raw)?;
                }
            }
        }
        OpType::TrashTree => {
            set_writable_envelope(&mut op, &kv)?;
            set_obj_any(&mut op, get("obj"))?;
            set_positive_i64(&mut op.expected_revision, get("rev"))?;
            set_positive_i64(&mut op.deleted_at, get("del"))?;
            set_positive_i64(&mut op.purge_after, get("purge"))?;
        }
        OpType::HardDeleteTree => {
            set_writable_envelope(&mut op, &kv)?;
            set_obj_any(&mut op, get("obj"))?;
            set_positive_i64(&mut op.expected_revision, get("rev"))?;
        }
        OpType::RestoreTree => {
            set_writable_envelope(&mut op, &kv)?;
            set_obj_any(&mut op, get("obj"))?;
            set_parent(&mut op, get("p"))?;
            set_name(&mut op, get("n"))?;
            set_positive_i64(&mut op.expected_revision, get("rev"))?;
        }
    }

    if let Some(s) = kv.get("sz") {
        let n: i64 = s.parse().map_err(|_| WireError::Malformed)?;
        if n < 0 {
            return Err(WireError::Malformed);
        }
        op.file_size = n;
    }
    if let Some(s) = kv.get("ts") {
        let n: i64 = s.parse().map_err(|_| WireError::Malformed)?;
        if !(0..=MAX_WIRE_UNIX_TS).contains(&n) {
            return Err(WireError::Malformed);
        }
        op.file_upload_time = n;
    }
    if let Some(s) = kv.get("enc") {
        if s == "1" {
            op.encrypted = true;
            op.encryption_version = 1;
        }
    }
    if let Some(s) = kv.get("ev") {
        op.encryption_version = s.parse().map_err(|_| WireError::Malformed)?;
    }
    if let Some(s) = kv.get("psz") {
        let n: i64 = s.parse().map_err(|_| WireError::Malformed)?;
        if n < 0 {
            return Err(WireError::Malformed);
        }
        op.plaintext_size = n;
    }
    // `rend` is only valid on part ops — TDrive rejects it elsewhere.
    if let Some(rend) = kv.get("rend") {
        if op.op_type != OpTypeOrUnknown::Known(OpType::FilePart) || rend.len() > 2048 {
            return Err(WireError::Malformed);
        }
        op.rendition = rend.clone();
    }
    Ok(op)
}

fn append_file_attrs(b: &mut String, op: &Op) {
    if op.file_size > 0 {
        let _ = write!(b, "|sz={}", op.file_size);
    }
    if op.file_upload_time > 0 {
        let _ = write!(b, "|ts={}", op.file_upload_time);
    }
    if op.encrypted {
        b.push_str("|enc=1");
        let v = if op.encryption_version == 0 {
            1
        } else {
            op.encryption_version
        };
        if v != 1 {
            let _ = write!(b, "|ev={v}");
        }
        if op.plaintext_size > 0 {
            let _ = write!(b, "|psz={}", op.plaintext_size);
        }
    }
}

/// Serialize an op to its TDX1 header. Only the op types this client emits
/// are supported — parse accepts more than format writes.
pub fn format(op: &Op) -> String {
    let op_type = match op.op_type {
        OpTypeOrUnknown::Known(t) => t,
        OpTypeOrUnknown::Unknown => panic!("cannot format unknown op"),
    };
    let mut b = String::with_capacity(64);
    let _ = write!(b, "{WIRE_PREFIX}|t={}", op_type.as_str());
    match op_type {
        OpType::FileUpload => {
            let _ = write!(b, "|p={}|n={}", op.parent, query_escape(&op.name));
            append_file_attrs(&mut b, op);
        }
        OpType::Meta => {
            let _ = write!(
                b,
                "|obj={}|p={}|n={}",
                op.obj,
                op.parent,
                query_escape(&op.name)
            );
            append_file_attrs(&mut b, op);
        }
        OpType::Mkdir | OpType::FolderCommit => {
            if op_type == OpType::FolderCommit {
                let _ = write!(
                    b,
                    "|v={}|oid={}",
                    op.protocol_version,
                    query_escape(&op.op_id)
                );
            }
            let _ = write!(
                b,
                "|obj={}|p={}|n={}",
                op.obj,
                op.parent,
                query_escape(&op.name)
            );
        }
        OpType::Rename => {
            let _ = write!(b, "|obj={}|n={}", op.obj, query_escape(&op.name));
        }
        OpType::Move => {
            let _ = write!(b, "|obj={}|p={}", op.obj, op.parent);
        }
        OpType::Rmdir | OpType::Tomb => {
            let _ = write!(b, "|obj={}", op.obj);
        }
        OpType::FilePart => {
            let _ = write!(b, "|u={}|pix={}", op.upload_uuid, op.part_index);
            if op.file_size > 0 {
                let _ = write!(b, "|sz={}", op.file_size);
            }
        }
        OpType::FileManifest => {
            let _ = write!(
                b,
                "|u={}|p={}|n={}|pc={}",
                op.upload_uuid,
                op.parent,
                query_escape(&op.name),
                op.part_count
            );
            append_file_attrs(&mut b, op);
        }
        OpType::EncConfig => {
            let _ = write!(
                b,
                "|salt={}|kdf={}|wrap={}|check={}",
                op.kdf_salt_b64,
                query_escape(&op.kdf_params_json),
                op.wrapped_key_b64,
                op.key_check_b64
            );
            if !op.hint.is_empty() {
                let _ = write!(b, "|hint={}", query_escape(&op.hint));
            }
            if op.config_version > 1 {
                let _ = write!(b, "|cv={}", op.config_version);
            }
        }
        OpType::FileCommit => {
            let _ = write!(
                b,
                "|v={}|oid={}",
                op.protocol_version,
                query_escape(&op.op_id)
            );
            let _ = write!(b, "|p={}|n={}", op.parent, query_escape(&op.name));
            if op.content_msg_id > 0 {
                let _ = write!(b, "|cmid={}", op.content_msg_id);
            } else {
                let _ = write!(b, "|u={}|pc={}", op.upload_uuid, op.part_count);
            }
            if !op.content_hash.is_empty() {
                let _ = write!(b, "|hash={}", query_escape(&op.content_hash));
            }
            append_file_attrs(&mut b, op);
        }
        OpType::FileReplace => {
            let _ = write!(
                b,
                "|v={}|oid={}",
                op.protocol_version,
                query_escape(&op.op_id)
            );
            let _ = write!(b, "|obj={}|rev={}", op.obj, op.expected_revision);
            if op.content_msg_id > 0 {
                let _ = write!(b, "|cmid={}", op.content_msg_id);
            } else {
                let _ = write!(b, "|u={}|pc={}", op.upload_uuid, op.part_count);
            }
            if !op.content_hash.is_empty() {
                let _ = write!(b, "|hash={}", query_escape(&op.content_hash));
            }
            let _ = write!(b, "|retain={}", op.retained_until);
            append_file_attrs(&mut b, op);
        }
        OpType::Relocate => {
            let _ = write!(
                b,
                "|v={}|oid={}",
                op.protocol_version,
                query_escape(&op.op_id)
            );
            let _ = write!(
                b,
                "|obj={}|p={}|n={}|rev={}",
                op.obj,
                op.parent,
                query_escape(&op.name),
                op.expected_revision
            );
            if op.overwrite {
                let _ = write!(b, "|ow=1");
            }
            if !op.destination_obj.is_empty() {
                let _ = write!(b, "|dst={}", op.destination_obj);
            }
            if op.expected_destination_revision > 0 {
                let _ = write!(b, "|drev={}", op.expected_destination_revision);
            }
            if op.deleted_at > 0 {
                let _ = write!(b, "|del={}", op.deleted_at);
            }
            if op.purge_after > 0 {
                let _ = write!(b, "|purge={}", op.purge_after);
            }
        }
        OpType::TrashTree => {
            let _ = write!(
                b,
                "|v={}|oid={}",
                op.protocol_version,
                query_escape(&op.op_id)
            );
            let _ = write!(
                b,
                "|obj={}|rev={}|del={}|purge={}",
                op.obj, op.expected_revision, op.deleted_at, op.purge_after
            );
        }
        OpType::HardDeleteTree => {
            let _ = write!(
                b,
                "|v={}|oid={}",
                op.protocol_version,
                query_escape(&op.op_id)
            );
            let _ = write!(b, "|obj={}|rev={}", op.obj, op.expected_revision);
        }
        OpType::RestoreTree => {
            let _ = write!(
                b,
                "|v={}|oid={}",
                op.protocol_version,
                query_escape(&op.op_id)
            );
            let _ = write!(
                b,
                "|obj={}|p={}|n={}|rev={}",
                op.obj,
                op.parent,
                query_escape(&op.name),
                op.expected_revision
            );
        }
    }
    if !op.rendition.is_empty() && op.op_type == OpTypeOrUnknown::Known(OpType::FilePart) {
        let _ = write!(b, "|rend={}", op.rendition);
    }
    b
}

pub fn file_obj_id(msg_id: i64) -> String {
    format!("{FILE_ID_PREFIX}{msg_id}")
}

pub fn is_file_id(obj: &str) -> bool {
    obj.starts_with(FILE_ID_PREFIX)
}

pub fn is_folder_id(obj: &str) -> bool {
    obj.starts_with(FOLDER_ID_PREFIX)
}

/// TDrive's `DeterministicOpID`: purpose + "-" + hex16 of
/// sha256("tdrive.trash.v1\x00" | purpose | "\x00" | obj | "\x00" | rev).
pub fn deterministic_op_id(purpose: &str, object_id: &str, revision: i64) -> String {
    use sha2::Digest;
    let payload = format!("tdrive.trash.v1\0{purpose}\0{object_id}\0{revision}");
    let digest = sha2::Sha256::digest(payload.as_bytes());
    let mut hex16 = String::with_capacity(32);
    for b in &digest[..16] {
        let _ = write!(hex16, "{b:02x}");
    }
    format!("{purpose}-{hex16}")
}

pub fn new_folder_id() -> String {
    format!("{FOLDER_ID_PREFIX}{}", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_file_upload() {
        let op = parse("TDX1|t=f|p=|n=hello+world.txt|sz=42|ts=1700000000").unwrap();
        assert_eq!(op.op_type, OpTypeOrUnknown::Known(OpType::FileUpload));
        assert_eq!(op.parent, "");
        assert_eq!(op.name, "hello world.txt");
        assert_eq!(op.file_size, 42);
        assert_eq!(op.file_upload_time, 1700000000);
    }

    #[test]
    fn parse_mkdir() {
        let op = parse("TDX1|t=mkdir|obj=d:abc-123|p=d:parent|n=My+Dir").unwrap();
        assert_eq!(op.obj, "d:abc-123");
        assert_eq!(op.parent, "d:parent");
        assert_eq!(op.name, "My Dir");
    }

    #[test]
    fn parse_manifest() {
        let op = parse("TDX1|t=manifest|u=uuid-1|p=|n=big.bin|pc=3|sz=6000").unwrap();
        assert_eq!(op.upload_uuid, "uuid-1");
        assert_eq!(op.part_count, 3);
        assert_eq!(op.file_size, 6000);
    }

    #[test]
    fn parse_part() {
        let op = parse("TDX1|t=part|u=uuid-1|pix=2|sz=1900").unwrap();
        assert_eq!(op.upload_uuid, "uuid-1");
        assert_eq!(op.part_index, 2);
    }

    /// Byte-exact parity with TDrive's `projection.Format` — expected strings
    /// were generated by running the Go formatter on the same ops.
    #[test]
    fn golden_format_parity() {
        let cases: Vec<(Op, &str)> = vec![
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::FileUpload), parent: "".into(), name: "a file.txt".into(), file_size: 42, file_upload_time: 1700000000, ..Default::default() },
                "TDX1|t=f|p=|n=a+file.txt|sz=42|ts=1700000000",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::FileUpload), parent: "d:abc-123".into(), name: "weird nàme+100%|x".into(), file_size: 1, encrypted: true, plaintext_size: 5, ..Default::default() },
                "TDX1|t=f|p=d:abc-123|n=weird+n%C3%A0me%2B100%25%7Cx|sz=1|enc=1|psz=5",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::Meta), obj: "f:7".into(), name: "renamed.txt".into(), ..Default::default() },
                "TDX1|t=meta|obj=f:7|p=|n=renamed.txt",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::Mkdir), obj: "d:f1d0d1d0-0000-4000-8000-000000000001".into(), name: "New Folder".into(), ..Default::default() },
                "TDX1|t=mkdir|obj=d:f1d0d1d0-0000-4000-8000-000000000001|p=|n=New+Folder",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::Rename), obj: "f:55".into(), name: "hello world".into(), ..Default::default() },
                "TDX1|t=rename|obj=f:55|n=hello+world",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::Move), obj: "d:x".into(), parent: "d:y".into(), ..Default::default() },
                "TDX1|t=move|obj=d:x|p=d:y",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::Rmdir), obj: "d:x".into(), ..Default::default() },
                "TDX1|t=rmdir|obj=d:x",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::Tomb), obj: "f:9".into(), ..Default::default() },
                "TDX1|t=tomb|obj=f:9",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::FilePart), upload_uuid: "uuid-1".into(), part_index: 0, file_size: 1024, ..Default::default() },
                "TDX1|t=part|u=uuid-1|pix=0|sz=1024",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::FileManifest), upload_uuid: "uuid-1".into(), parent: "d:p".into(), name: "big.bin".into(), part_count: 3, file_size: 3000, file_upload_time: 123, ..Default::default() },
                "TDX1|t=manifest|u=uuid-1|p=d:p|n=big.bin|pc=3|sz=3000|ts=123",
            ),
            (
                Op {
                    op_type: OpTypeOrUnknown::Known(OpType::EncConfig),
                    kdf_salt_b64: "MDEyMzQ1Njc4OWFiY2RlZg".into(),
                    kdf_params_json: r#"{"kdf":"argon2id","memory":65536,"time":3,"parallelism":4,"key_len":32,"salt_len":16}"#.into(),
                    wrapped_key_b64: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
                    key_check_b64: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
                    hint: "my hint".into(),
                    ..Default::default()
                },
                "TDX1|t=encfg|salt=MDEyMzQ1Njc4OWFiY2RlZg|kdf=%7B%22kdf%22%3A%22argon2id%22%2C%22memory%22%3A65536%2C%22time%22%3A3%2C%22parallelism%22%3A4%2C%22key_len%22%3A32%2C%22salt_len%22%3A16%7D|wrap=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA|check=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA|hint=my+hint",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::FileCommit), protocol_version: 1, op_id: "commit-1".into(), name: "m.txt".into(), content_msg_id: 42, file_size: 9, ..Default::default() },
                "TDX1|t=fcommit|v=1|oid=commit-1|p=|n=m.txt|cmid=42|sz=9",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::FileCommit), protocol_version: 1, op_id: "commit-2".into(), parent: "d:z".into(), name: "m2".into(), upload_uuid: "uu".into(), part_count: 2, content_hash: "abc/123+".into(), ..Default::default() },
                "TDX1|t=fcommit|v=1|oid=commit-2|p=d:z|n=m2|u=uu|pc=2|hash=abc%2F123%2B",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::FolderCommit), protocol_version: 1, op_id: "dc-1".into(), obj: "d:f".into(), name: "docs".into(), ..Default::default() },
                "TDX1|t=dcommit|v=1|oid=dc-1|obj=d:f|p=|n=docs",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::FileReplace), protocol_version: 1, op_id: "fr-1".into(), obj: "f:10".into(), expected_revision: 2, content_msg_id: 77, retained_until: 1700000000, ..Default::default() },
                "TDX1|t=freplace|v=1|oid=fr-1|obj=f:10|rev=2|cmid=77|retain=1700000000",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::Relocate), protocol_version: 1, op_id: "rel-1".into(), obj: "f:1".into(), parent: "d:p".into(), name: "moved".into(), expected_revision: 3, ..Default::default() },
                "TDX1|t=relocate|v=1|oid=rel-1|obj=f:1|p=d:p|n=moved|rev=3",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::Relocate), protocol_version: 1, op_id: "rel-2".into(), obj: "f:2".into(), name: "mv2".into(), expected_revision: 1, overwrite: true, destination_obj: "f:9".into(), expected_destination_revision: 4, deleted_at: 100, purge_after: 200, ..Default::default() },
                "TDX1|t=relocate|v=1|oid=rel-2|obj=f:2|p=|n=mv2|rev=1|ow=1|dst=f:9|drev=4|del=100|purge=200",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::TrashTree), protocol_version: 1, op_id: "trash-abc".into(), obj: "f:5".into(), expected_revision: 1, deleted_at: 1000, purge_after: 2000, ..Default::default() },
                "TDX1|t=trash|v=1|oid=trash-abc|obj=f:5|rev=1|del=1000|purge=2000",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::HardDeleteTree), protocol_version: 1, op_id: "purge-x".into(), obj: "d:dead".into(), expected_revision: 5, ..Default::default() },
                "TDX1|t=harddel|v=1|oid=purge-x|obj=d:dead|rev=5",
            ),
            (
                Op { op_type: OpTypeOrUnknown::Known(OpType::RestoreTree), protocol_version: 1, op_id: "restore-y".into(), obj: "f:5".into(), parent: "d:home".into(), name: "restored (2).txt".into(), expected_revision: 2, ..Default::default() },
                "TDX1|t=untrash|v=1|oid=restore-y|obj=f:5|p=d:home|n=restored+%282%29.txt|rev=2",
            ),
        ];
        for (op, want) in cases {
            let got = format(&op);
            assert_eq!(got, want, "op {want}");
            // And the emitted string must round-trip through the parser.
            let back = parse(&got).unwrap();
            assert_eq!(back.op_type, op.op_type);
        }
    }

    #[test]
    fn deterministic_op_ids() {
        // Generated by Go's projection.DeterministicOpID.
        assert_eq!(
            deterministic_op_id("trash", "f:5", 1),
            "trash-f6ca09149f0f7da37cf6c8f1601ad5ba"
        );
        assert_eq!(
            deterministic_op_id("restore", "d:a", 2),
            "restore-d3424abed2791d0939d3f8b3a9b170fa"
        );
        assert_eq!(
            deterministic_op_id("purge", "f:9", 7),
            "purge-1c0352db1ac93301faf95c25e8db3f3d"
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse("hello world"), Err(WireError::Malformed));
        assert_eq!(parse(""), Err(WireError::MissingHeader));
        assert_eq!(parse("TDX2|t=f|n=x"), Err(WireError::BadVersion));
        assert_eq!(parse("TDX1|t=nope|n=x"), Err(WireError::BadOpType));
    }

    #[test]
    fn format_roundtrip() {
        let op = Op {
            op_type: OpTypeOrUnknown::Known(OpType::FileUpload),
            parent: "d:abc".into(),
            name: "a file+.txt".into(),
            file_size: 10,
            file_upload_time: 42,
            ..Default::default()
        };
        let s = format(&op);
        assert_eq!(s, "TDX1|t=f|p=d:abc|n=a+file%2B.txt|sz=10|ts=42");
        let back = parse(&s).unwrap();
        assert_eq!(back.name, "a file+.txt");
        assert_eq!(back.parent, "d:abc");
    }

    #[test]
    fn format_percent20_decodes() {
        // Rust-side %20 (non-Go form) still decodes for us.
        let op = parse("TDX1|t=f|p=|n=a%20b").unwrap();
        assert_eq!(op.name, "a b");
    }

    #[test]
    fn parse_writable_ops() {
        let op = parse("TDX1|t=fcommit|v=1|oid=op-1|p=|n=x|cmid=55|sz=9").unwrap();
        assert_eq!(op.op_id, "op-1");
        assert_eq!(op.content_msg_id, 55);
        let op = parse("TDX1|t=trash|v=1|oid=o2|obj=f:9|rev=2|del=1|purge=2").unwrap();
        assert_eq!(op.expected_revision, 2);
        let op = parse("TDX1|t=relocate|v=1|oid=o3|obj=d:x|p=|n=y|rev=1|ow=1").unwrap();
        assert!(op.overwrite);
    }
}
