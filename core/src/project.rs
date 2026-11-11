//! Namespace projection — folds an ordered stream of TDX1 ops into a
//! browsable file/folder tree. Ports TDrive's `backend/projection/apply.go`
//! semantics in memory: first-write-wins mkdir/parts, tombstoned=0 guards on
//! rename/move, move cycle rejection, file-meta upsert merging, versioned-op
//! dedup by op_id, expected-revision compare-and-swap, trash entries and
//! subtree tombstone/restore semantics.

use crate::wire::{self, Op, OpType, OpTypeOrUnknown};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Ancestor walks bound at this depth — a cycle or runaway chain stops
/// here rather than looping forever.
const MAX_DEPTH: i32 = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderEntry {
    pub id: String,
    pub name: String,
    pub parent: String,
    pub tombstoned: bool,
    pub revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Object id, `f:<msg_id>` of the uploading/manifest message.
    pub id: String,
    /// Message id carrying the op that made this file visible.
    pub msg_id: i64,
    pub name: String,
    /// Stored (on-Telegram) size in bytes.
    pub size: i64,
    pub parent: String,
    pub upload_time: i64,
    pub tombstoned: bool,
    pub encrypted: bool,
    pub plaintext_size: i64,
    /// Multipart upload grouping uuid, if the file was uploaded in parts.
    pub upload_uuid: String,
    pub part_count: i32,
    /// Single-body content message id (fcommit with cmid), else 0 meaning
    /// the body is `msg_id` itself or a multipart group.
    pub content_msg_id: i64,
    pub revision: i64,
}

/// A channel message carrying a document but no TDX1 op — an adoptable
/// raw attachment in a shared drive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnadoptedEntry {
    pub msg_id: i64,
    pub name: String,
    pub size: i64,
    pub sender_id: i64,
    pub date: i64,
}

/// A live or tombstoned object as seen by CAS writers.
#[derive(Debug, Clone)]
pub struct Dirent {
    pub is_folder: bool,
    pub revision: i64,
    pub tombstoned: bool,
    pub parent: String,
    pub name: String,
}

/// Borrowed child entry — zero-cost view for path resolution.
#[derive(Debug, Clone, Copy)]
pub enum EntryRef<'a> {
    Folder(&'a FolderEntry),
    File(&'a FileEntry),
}

impl FileEntry {
    /// Size as the user sees it: plaintext length when encrypted.
    pub fn logical_size(&self) -> i64 {
        if self.encrypted {
            self.plaintext_size
        } else {
            self.size
        }
    }
}

/// Restorable-trash record created by a `trash` op.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashEntry {
    pub object_id: String,
    pub is_folder: bool,
    pub original_parent: String,
    pub original_name: String,
    pub original_revision: i64,
    pub deleted_at: i64,
    pub purge_after: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PartGroup {
    /// pix -> (msg_id, size)
    parts: HashMap<i32, (i64, i64)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NamespaceSnapshot {
    pub folders: HashMap<String, FolderEntry>,
    pub files: HashMap<String, FileEntry>,
    parts: HashMap<String, PartGroup>,
    pub trash: HashMap<String, TrashEntry>,
    seen_ops: HashSet<String>,
    pub last_msg_id: i64,
    /// Latest `encfg` op — the drive's encryption vault descriptor.
    pub encfg: Option<Op>,
    #[serde(default)]
    pub renditions: HashMap<String, HashMap<String, (i64, wire::Rendition)>>,
    #[serde(default)]
    pub unadopted: Vec<UnadoptedEntry>,
}

#[derive(Debug, Default, Clone)]
pub struct Namespace {
    pub folders: HashMap<String, FolderEntry>,
    pub files: HashMap<String, FileEntry>,
    parts: HashMap<String, PartGroup>,
    /// Restorable objects (trash view), keyed by object id.
    pub trash: HashMap<String, TrashEntry>,
    /// Applied/skipped versioned-op ids (replay dedup).
    seen_ops: HashSet<String>,
    /// Highest message id observed while scanning.
    pub last_msg_id: i64,
    /// Latest `encfg` op seen — the drive's encryption vault, if configured.
    pub encfg: Option<Op>,
    /// file id → kind ("thumb"|"preview") → (body msg id, descriptor)
    pub renditions: HashMap<String, HashMap<String, (i64, wire::Rendition)>>,
    /// Adoptable raw attachments (shared drives), oldest first.
    pub unadopted: Vec<UnadoptedEntry>,
}

/// A visible listing row.
#[derive(Debug, Clone)]
pub enum Entry {
    Folder(FolderEntry),
    File(FileEntry),
}

impl Entry {
    pub fn id(&self) -> &str {
        match self {
            Self::Folder(d) => &d.id,
            Self::File(f) => &f.id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Folder(d) => &d.name,
            Self::File(f) => &f.name,
        }
    }

    pub fn is_folder(&self) -> bool {
        matches!(self, Self::Folder(_))
    }
}

impl Namespace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any rendition (thumb/preview) is recorded for `file_id`.
    pub fn has_rendition(&self, file_id: &str) -> bool {
        self.renditions.contains_key(file_id)
    }

    /// Apply one message. `msg_id` is the Telegram message id, `raw` is the
    /// full text/caption. Returns true when the message carried a valid op.
    pub fn apply_message(&mut self, msg_id: i64, raw: &str) -> bool {
        if msg_id > self.last_msg_id {
            self.last_msg_id = msg_id;
        }
        let op = match wire::parse(raw) {
            Ok(op) => op,
            Err(_) => return false,
        };
        self.apply(msg_id, &op);
        // Projection-owned messages aren't adoptable: the op itself, body
        // messages bound by fcommit/freplace, and meta targets.
        self.unadopted.retain(|e| {
            e.msg_id != msg_id
                && e.msg_id != op.content_msg_id
                && file_msg_id(&op.obj) != Some(e.msg_id)
        });
        true
    }

    /// Record a channel document message carrying no TDX1 header — a raw
    /// attachment that can be adopted with a `meta` op.
    pub fn record_unadopted(
        &mut self,
        msg_id: i64,
        name: String,
        size: i64,
        sender_id: i64,
        date: i64,
    ) {
        if msg_id > self.last_msg_id {
            self.last_msg_id = msg_id;
        }
        if self.unadopted.iter().all(|e| e.msg_id != msg_id) {
            self.unadopted.push(UnadoptedEntry {
                msg_id,
                name,
                size,
                sender_id,
                date,
            });
        }
    }

    pub fn apply(&mut self, msg_id: i64, op: &Op) {
        let OpTypeOrUnknown::Known(t) = op.op_type else {
            return;
        };
        // Versioned (v1-envelope) ops: validate envelope, dedup by op_id.
        if matches!(
            t,
            OpType::FileCommit
                | OpType::FileReplace
                | OpType::FolderCommit
                | OpType::Relocate
                | OpType::TrashTree
                | OpType::HardDeleteTree
                | OpType::RestoreTree
        ) {
            if op.protocol_version != 1
                || op.op_id.is_empty()
                || op.op_id.len() > 128
                || op.op_id.chars().any(|c| c.is_control() || c == '|')
            {
                return;
            }
            if !self.seen_ops.insert(op.op_id.clone()) {
                return;
            }
        }
        // Ops that derive object ids (or body references) from the message
        // id must never apply with an uncertain id — otherwise every file
        // collapses into `f:0`. They arrive properly on the next sync.
        if msg_id <= 0
            && matches!(
                t,
                OpType::FileUpload
                    | OpType::FileManifest
                    | OpType::FileCommit
                    | OpType::FileReplace
                    | OpType::FilePart
            )
        {
            return;
        }
        match t {
            OpType::FileUpload => self.apply_file_meta(msg_id, op, msg_id),
            OpType::Meta => {
                let Some(file_msg_id) = file_msg_id(&op.obj) else {
                    return;
                };
                self.apply_file_meta(file_msg_id, op, file_msg_id)
            }
            OpType::FilePart => {
                // Rendition envelope: binds this part message to a logical
                // file as a thumbnail/preview body.
                if !op.rendition.is_empty() {
                    if let Some(r) = wire::Rendition::decode(&op.rendition) {
                        self.renditions
                            .entry(wire::file_obj_id(r.file_msg_id))
                            .or_default()
                            .entry(r.kind.clone())
                            .or_insert((msg_id, r));
                    }
                    return;
                }
                if op.part_index < 0 || op.upload_uuid.trim().is_empty() {
                    return;
                }
                // First-write-wins, like applyFilePart.
                self.parts
                    .entry(op.upload_uuid.clone())
                    .or_default()
                    .parts
                    .entry(op.part_index)
                    .or_insert((msg_id, op.file_size));
            }
            OpType::FileManifest => {
                if op.upload_uuid.trim().is_empty()
                    || op.part_count <= 0
                    || op.name.trim().is_empty()
                    || !valid_parent(&op.parent)
                {
                    return;
                }
                let id = wire::file_obj_id(msg_id);
                let entry = self.files.entry(id.clone()).or_insert_with(|| FileEntry {
                    id: id.clone(),
                    msg_id,
                    name: String::new(),
                    size: 0,
                    parent: String::new(),
                    upload_time: 0,
                    tombstoned: false,
                    encrypted: false,
                    plaintext_size: 0,
                    upload_uuid: String::new(),
                    part_count: 0,
                    content_msg_id: 0,
                    revision: 1,
                });
                entry.name = op.name.clone();
                entry.parent = op.parent.clone();
                if op.file_size > 0 {
                    entry.size = op.file_size;
                }
                if op.file_upload_time > 0 {
                    entry.upload_time = op.file_upload_time;
                }
                if op.encrypted {
                    entry.encrypted = true;
                }
                if op.plaintext_size > 0 {
                    entry.plaintext_size = op.plaintext_size;
                }
                entry.upload_uuid = op.upload_uuid.clone();
                entry.part_count = op.part_count;
            }
            OpType::Mkdir => {
                if !wire::is_folder_id(&op.obj)
                    || op.name.trim().is_empty()
                    || !valid_parent(&op.parent)
                {
                    return;
                }
                // First-mkdir-wins.
                self.folders
                    .entry(op.obj.clone())
                    .or_insert_with(|| FolderEntry {
                        id: op.obj.clone(),
                        name: op.name.clone(),
                        parent: op.parent.clone(),
                        tombstoned: false,
                        revision: 1,
                    });
            }
            OpType::Rename => {
                if op.name.trim().is_empty() {
                    return;
                }
                self.rename(&op.obj, &op.name);
            }
            OpType::Move => {
                if !valid_parent(&op.parent) {
                    return;
                }
                self.move_obj(&op.obj, &op.parent);
            }
            OpType::Rmdir => {
                if !wire::is_folder_id(&op.obj) {
                    return;
                }
                if let Some(d) = self.folders.get_mut(&op.obj) {
                    if !d.tombstoned {
                        d.tombstoned = true;
                        d.revision += 1;
                    }
                }
            }
            OpType::Tomb => {
                if !wire::is_file_id(&op.obj) {
                    return;
                }
                if let Some(f) = self.files.get_mut(&op.obj) {
                    if !f.tombstoned {
                        f.tombstoned = true;
                        f.revision += 1;
                    }
                }
            }
            OpType::FolderCommit => {
                if !wire::is_folder_id(&op.obj)
                    || op.name.trim().is_empty()
                    || !valid_parent(&op.parent)
                    || self.folders.contains_key(&op.obj)
                {
                    return;
                }
                self.folders.insert(
                    op.obj.clone(),
                    FolderEntry {
                        id: op.obj.clone(),
                        name: op.name.clone(),
                        parent: op.parent.clone(),
                        tombstoned: false,
                        revision: 1,
                    },
                );
            }
            OpType::FileCommit => {
                let id = wire::file_obj_id(msg_id);
                if op.name.trim().is_empty()
                    || !valid_parent(&op.parent)
                    || self.files.contains_key(&id)
                {
                    return;
                }
                self.files.insert(
                    id.clone(),
                    FileEntry {
                        id,
                        msg_id,
                        name: op.name.clone(),
                        size: op.file_size,
                        parent: op.parent.clone(),
                        upload_time: op.file_upload_time,
                        tombstoned: false,
                        encrypted: op.encrypted,
                        plaintext_size: op.plaintext_size,
                        upload_uuid: op.upload_uuid.clone(),
                        part_count: op.part_count,
                        content_msg_id: op.content_msg_id,
                        revision: 1,
                    },
                );
            }
            OpType::FileReplace => {
                let Some(f) = self.files.get_mut(&op.obj) else {
                    return;
                };
                if f.tombstoned || f.revision != op.expected_revision {
                    return;
                }
                f.content_msg_id = op.content_msg_id;
                f.upload_uuid = op.upload_uuid.clone();
                f.part_count = op.part_count;
                if op.file_size > 0 {
                    f.size = op.file_size;
                }
                if op.file_upload_time > 0 {
                    f.upload_time = op.file_upload_time;
                }
                if op.encrypted {
                    f.encrypted = true;
                }
                if op.plaintext_size > 0 {
                    f.plaintext_size = op.plaintext_size;
                }
                f.revision += 1;
            }
            OpType::Relocate => {
                if op.name.trim().is_empty() || !valid_parent(&op.parent) {
                    return;
                }
                // Source CAS first — Go rejects before touching the dst.
                let Some((src_is_folder, live_rev)) =
                    self.dirent(&op.obj).map(|d| (d.is_folder, d.revision))
                else {
                    return;
                };
                if live_rev != op.expected_revision {
                    return;
                }
                if src_is_folder && would_cycle(self, &op.obj, &op.parent) {
                    return;
                }
                // `dst` names the occupant this relocate displaces; `ow`
                // must permit it, drev CAS applies — files only (directory
                // overwrite is unsupported in TDrive).
                if !op.destination_obj.is_empty() {
                    if !op.overwrite {
                        return;
                    }
                    let Some(v) = self.dirent(&op.destination_obj) else {
                        return;
                    };
                    let (v_is_folder, v_rev, v_tomb, v_parent, v_name) =
                        (v.is_folder, v.revision, v.tombstoned, v.parent, v.name);
                    if v_is_folder
                        || v_tomb
                        || (op.expected_destination_revision > 0
                            && v_rev != op.expected_destination_revision)
                    {
                        return;
                    }
                    let dst = op.destination_obj.clone();
                    if op.deleted_at > 0 {
                        // `del` carries the victim's trash timestamp.
                        self.trash.insert(
                            dst.clone(),
                            TrashEntry {
                                object_id: dst.clone(),
                                is_folder: false,
                                original_parent: v_parent,
                                original_name: v_name,
                                original_revision: v_rev,
                                deleted_at: op.deleted_at,
                                purge_after: op.purge_after,
                            },
                        );
                        if let Some(f) = self.files.get_mut(&dst) {
                            f.tombstoned = true;
                            f.revision += 1;
                        }
                    } else {
                        // purge → remove outright.
                        self.remove_object(&dst);
                    }
                }
                // One mutation — Go's UPDATE bumps revision exactly once.
                let obj = op.obj.clone();
                if let Some(f) = self.files.get_mut(&obj) {
                    f.name = op.name.clone();
                    f.parent = op.parent.clone();
                    f.revision += 1;
                } else if let Some(d) = self.folders.get_mut(&obj) {
                    d.name = op.name.clone();
                    d.parent = op.parent.clone();
                    d.revision += 1;
                }
            }
            OpType::TrashTree => {
                if op.expected_revision <= 0
                    || op.deleted_at <= 0
                    || op.purge_after <= op.deleted_at
                {
                    return;
                }
                self.apply_trash(op);
            }
            OpType::RestoreTree => {
                if op.expected_revision <= 0 {
                    return;
                }
                self.apply_restore(op);
            }
            OpType::HardDeleteTree => {
                if op.expected_revision <= 0 {
                    return;
                }
                self.apply_hard_delete(op);
            }
            OpType::EncConfig => {
                self.encfg = Some(op.clone());
            }
        }
    }

    /// Legacy `f`/`meta` upsert: name and parent always overwrite; size,
    /// upload time, encryption fields only replace when present (>0).
    fn apply_file_meta(&mut self, file_msg_id: i64, op: &Op, msg_id: i64) {
        if op.name.trim().is_empty() || !valid_parent(&op.parent) {
            return;
        }
        let id = wire::file_obj_id(file_msg_id);
        let entry = self.files.entry(id.clone()).or_insert_with(|| FileEntry {
            id,
            msg_id,
            name: String::new(),
            size: 0,
            parent: String::new(),
            upload_time: 0,
            tombstoned: false,
            encrypted: false,
            plaintext_size: 0,
            upload_uuid: String::new(),
            part_count: 0,
            content_msg_id: file_msg_id,
            revision: 1,
        });
        entry.name = op.name.clone();
        entry.parent = op.parent.clone();
        if op.file_size > 0 {
            entry.size = op.file_size;
        }
        if op.file_upload_time > 0 {
            entry.upload_time = op.file_upload_time;
        }
        if op.encrypted {
            entry.encrypted = true;
        }
        if op.plaintext_size > 0 {
            entry.plaintext_size = op.plaintext_size;
        }
    }

    /// `trash` apply: CAS on live dirent revision, record trash entry,
    /// tombstone object + subtree, each revision+1.
    fn apply_trash(&mut self, op: &Op) {
        let Some((is_folder, live_parent, live_name, live_rev)) = (|| {
            if let Some(f) = self.files.get(&op.obj) {
                (!f.tombstoned).then(|| (false, f.parent.clone(), f.name.clone(), f.revision))
            } else {
                self.folders
                    .get(&op.obj)
                    .filter(|d| !d.tombstoned)
                    .map(|d| (true, d.parent.clone(), d.name.clone(), d.revision))
            }
        })() else {
            return;
        };
        if live_rev != op.expected_revision {
            return;
        }
        let (orig_parent, orig_name) = (live_parent, live_name);
        self.trash.insert(
            op.obj.clone(),
            TrashEntry {
                object_id: op.obj.clone(),
                is_folder,
                original_parent: orig_parent,
                original_name: orig_name,
                original_revision: live_rev,
                deleted_at: op.deleted_at,
                purge_after: op.purge_after,
            },
        );
        let obj = op.obj.clone();
        if is_folder {
            self.tombstone_subtree_folder(&obj);
        } else if let Some(f) = self.files.get_mut(&obj) {
            f.tombstoned = true;
            f.revision += 1;
        }
    }

    /// `untrash` apply: CAS on tombstoned revision, restore subtree, land at
    /// the caller-resolved parent/name, drop the trash entry.
    fn apply_restore(&mut self, op: &Op) {
        let Some((is_folder, live_rev)) = (|| {
            if let Some(f) = self.files.get(&op.obj) {
                f.tombstoned.then_some((false, f.revision))
            } else {
                self.folders
                    .get(&op.obj)
                    .and_then(|d| d.tombstoned.then_some((true, d.revision)))
            }
        })() else {
            return;
        };
        if live_rev != op.expected_revision {
            return;
        }
        // Go requires a trash entry — objects tombstoned by legacy means
        // (tomb/rmdir) stay dead.
        if !self.trash.contains_key(&op.obj) {
            return;
        }
        let obj = op.obj.clone();
        if is_folder {
            self.restore_subtree_folder(&obj);
        } else if let Some(f) = self.files.get_mut(&obj) {
            f.tombstoned = false;
            f.revision += 1;
        }
        // Land at the resolved destination.
        if let Some(d) = self.folders.get_mut(&obj) {
            d.parent = op.parent.clone();
            if !op.name.is_empty() {
                d.name = op.name.clone();
            }
        } else if let Some(f) = self.files.get_mut(&obj) {
            f.parent = op.parent.clone();
            if !op.name.is_empty() {
                f.name = op.name.clone();
            }
        }
        self.trash.remove(&obj);
    }

    /// `harddel` apply: CAS on tombstoned revision, permanently drop the
    /// object (+ subtree) and its trash entry.
    fn apply_hard_delete(&mut self, op: &Op) {
        // TDrive's harddel targets live objects (mount delete) or
        // tombstoned ones (trash purge) — CAS on the current revision.
        let live_rev = if let Some(f) = self.files.get(&op.obj) {
            Some(f.revision)
        } else if let Some(d) = self.folders.get(&op.obj) {
            Some(d.revision)
        } else {
            return;
        };
        if live_rev != Some(op.expected_revision) {
            return;
        }
        self.remove_object(&op.obj);
    }

    /// Permanently drop an object (plus folder subtree, upload parts,
    /// renditions, and its trash entry) — shared by `harddel` and
    /// purge-style relocate victims.
    fn remove_object(&mut self, obj: &str) {
        let upload_uuids: Vec<String> = self
            .subtree_files(obj)
            .iter()
            .filter_map(|f| {
                if f.upload_uuid.is_empty() {
                    None
                } else {
                    Some(f.upload_uuid.clone())
                }
            })
            .collect();
        if self.folders.remove(obj).is_some() {
            // Remove descendant folders/files recursively.
            let mut queue: Vec<String> = self
                .folders
                .values()
                .filter(|d| d.parent == obj)
                .map(|d| d.id.clone())
                .collect();
            while let Some(fid) = queue.pop() {
                self.folders.remove(&fid);
                queue.extend(
                    self.folders
                        .values()
                        .filter(|d| d.parent == fid)
                        .map(|d| d.id.clone()),
                );
            }
            self.files.retain(|_, f| {
                let mut anc = f.parent.clone();
                let mut guard = 0;
                while !anc.is_empty() && guard < MAX_DEPTH {
                    guard += 1;
                    if anc == obj {
                        return false;
                    }
                    anc = self
                        .folders
                        .get(&anc)
                        .map(|d| d.parent.clone())
                        .unwrap_or_default();
                }
                true
            });
        }
        self.files.remove(obj);
        self.renditions.remove(obj);
        for u in upload_uuids {
            self.parts.remove(&u);
        }
        self.trash.remove(obj);
    }

    fn subtree_files(&self, folder_id: &str) -> Vec<&FileEntry> {
        let mut ids = HashSet::new();
        ids.insert(folder_id.to_string());
        let mut changed = true;
        let mut guard = 0;
        while changed && guard < MAX_DEPTH {
            changed = false;
            guard += 1;
            for d in self.folders.values() {
                if ids.contains(&d.parent) && ids.insert(d.id.clone()) {
                    changed = true;
                }
            }
        }
        self.files
            .values()
            .filter(|f| ids.contains(&f.parent))
            .collect()
    }

    fn tombstone_subtree_folder(&mut self, root: &str) {
        if let Some(d) = self.folders.get_mut(root) {
            d.tombstoned = true;
            d.revision += 1;
        }
        let children: Vec<String> = self
            .folders
            .values()
            .filter(|c| c.parent == root && !c.tombstoned)
            .map(|c| c.id.clone())
            .collect();
        for c in children {
            self.tombstone_subtree_folder(&c);
        }
        for f in self.files.values_mut() {
            if f.parent == root && !f.tombstoned {
                f.tombstoned = true;
                f.revision += 1;
            }
        }
    }

    fn restore_subtree_folder(&mut self, root: &str) {
        if let Some(d) = self.folders.get_mut(root) {
            d.tombstoned = false;
            d.revision += 1;
        }
        // Children trashed independently keep their own trash entry and
        // stay dead when the parent restores (Go's withRestorable).
        let children: Vec<String> = self
            .folders
            .values()
            .filter(|c| c.parent == root && c.tombstoned && !self.trash.contains_key(&c.id))
            .map(|c| c.id.clone())
            .collect();
        for c in children {
            self.restore_subtree_folder(&c);
        }
        for f in self.files.values_mut() {
            if f.parent == root && f.tombstoned && !self.trash.contains_key(&f.id) {
                f.tombstoned = false;
                f.revision += 1;
            }
        }
    }

    fn rename(&mut self, obj: &str, name: &str) {
        if let Some(f) = self.files.get_mut(obj) {
            if !f.tombstoned {
                f.name = name.to_string();
                f.revision += 1;
            }
        } else if let Some(d) = self.folders.get_mut(obj) {
            if !d.tombstoned {
                d.name = name.to_string();
                d.revision += 1;
            }
        }
    }

    fn move_obj(&mut self, obj: &str, parent: &str) {
        if let Some(f) = self.files.get_mut(obj) {
            if !f.tombstoned {
                f.parent = parent.to_string();
                f.revision += 1;
            }
        } else {
            let live = self
                .folders
                .get(obj)
                .map(|d| !d.tombstoned)
                .unwrap_or(false);
            if live && !would_cycle(self, obj, parent) {
                if let Some(d) = self.folders.get_mut(obj) {
                    d.parent = parent.to_string();
                    d.revision += 1;
                }
            }
        }
    }

    /// Tombstoned trash entries, most recently deleted first.
    pub fn list_trash(&self) -> Vec<TrashEntry> {
        let mut v: Vec<_> = self.trash.values().cloned().collect();
        v.sort_by(|a, b| {
            b.deleted_at
                .cmp(&a.deleted_at)
                .then(a.object_id.cmp(&b.object_id))
        });
        v
    }

    /// Live entries directly under `parent` ("" = root), folders first then
    /// files, each sorted by name.
    pub fn list(&self, parent: &str) -> Vec<Entry> {
        let mut folders: Vec<_> = self
            .folders
            .values()
            .filter(|f| !f.tombstoned && f.parent == parent)
            .cloned()
            .collect();
        folders.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        let mut files: Vec<_> = self
            .files
            .values()
            .filter(|f| !f.tombstoned && f.parent == parent)
            .cloned()
            .collect();
        files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        folders
            .into_iter()
            .map(Entry::Folder)
            .chain(files.into_iter().map(Entry::File))
            .collect()
    }

    /// Remove entries created under an uncertain message id (`f:0`,
    /// `d:0`) — legacy ghosts from pre-recovery sends. The real objects
    /// come back via sync.
    pub fn drop_zero_ids(&mut self) {
        self.files.retain(|id, _| id != "f:0");
        self.folders.retain(|id, _| id != "d:0");
        self.renditions.retain(|fid, _| fid != "f:0");
        self.trash.retain(|id, _| id != "f:0" && id != "d:0");
    }

    /// Multipart parts in index order: (body msg_id, stored size).
    pub fn part_sizes(&self, upload_uuid: &str) -> Vec<(i64, i64)> {
        let Some(g) = self.parts.get(upload_uuid) else {
            return Vec::new();
        };
        let mut by_pix: Vec<(i32, i64, i64)> = g
            .parts
            .iter()
            .map(|(pix, (id, sz))| (*pix, *id, *sz))
            .collect();
        by_pix.sort_by_key(|(pix, _, _)| *pix);
        by_pix.iter().map(|(_, id, sz)| (*id, *sz)).collect()
    }

    /// Body parts carrying a file's bytes, in read order:
    /// (body message id, stored size).
    pub fn body_parts(&self, file: &FileEntry) -> Vec<(i64, i64)> {
        if !file.upload_uuid.is_empty() {
            let parts = self.part_sizes(&file.upload_uuid);
            if !parts.is_empty() {
                return parts;
            }
        }
        if file.content_msg_id > 0 {
            return vec![(file.content_msg_id, file.size)];
        }
        vec![(file.msg_id, file.size)]
    }

    /// Every body message a permanent delete must remove for `obj`:
    /// subtree bodies (or the file's own) plus each rendition bound to
    /// any of those files. Shared by purge and mount deletes.
    pub fn delete_body_msg_ids(&self, obj: &str) -> Vec<i64> {
        let mut bodies = if wire::is_folder_id(obj) {
            self.subtree_body_msg_ids(obj)
        } else {
            self.files
                .get(obj)
                .map(|f| self.body_msg_ids(f))
                .unwrap_or_default()
        };
        let file_ids: Vec<&String> = if wire::is_folder_id(obj) {
            self.files
                .values()
                .filter(|f| self.subtree_contains(obj, &f.id))
                .map(|f| &f.id)
                .collect()
        } else {
            self.files.get(obj).map(|f| &f.id).into_iter().collect()
        };
        for fid in file_ids {
            if let Some(kinds) = self.renditions.get(fid) {
                bodies.extend(kinds.values().map(|(id, _)| *id));
            }
        }
        bodies
    }

    /// Message ids carrying a file's bytes, in read order.
    pub fn body_msg_ids(&self, file: &FileEntry) -> Vec<i64> {
        self.body_parts(file)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    /// Dirent for CAS writers: kind, revision, tombstone, parent, name.
    pub fn dirent(&self, obj: &str) -> Option<Dirent> {
        if let Some(f) = self.files.get(obj) {
            return Some(Dirent {
                is_folder: false,
                revision: f.revision,
                tombstoned: f.tombstoned,
                parent: f.parent.clone(),
                name: f.name.clone(),
            });
        }
        self.folders.get(obj).map(|d| Dirent {
            is_folder: true,
            revision: d.revision,
            tombstoned: d.tombstoned,
            parent: d.parent.clone(),
            name: d.name.clone(),
        })
    }

    /// Live entries whose name contains `q` (case-insensitive):
    /// folders first, then files, each name-sorted.
    pub fn search(&self, q: &str) -> Vec<Entry> {
        let q = q.to_lowercase();
        let by_name = |a: &Entry, b: &Entry| {
            let an = match a {
                Entry::Folder(d) => &d.name,
                Entry::File(f) => &f.name,
            };
            let bn = match b {
                Entry::Folder(d) => &d.name,
                Entry::File(f) => &f.name,
            };
            an.to_lowercase().cmp(&bn.to_lowercase())
        };
        let mut out: Vec<Entry> = self
            .folders
            .values()
            .filter(|d| !d.tombstoned && d.name.to_lowercase().contains(&q))
            .cloned()
            .map(Entry::Folder)
            .collect();
        let mut files: Vec<Entry> = self
            .files
            .values()
            .filter(|f| !f.tombstoned && f.name.to_lowercase().contains(&q))
            .cloned()
            .map(Entry::File)
            .collect();
        out.sort_by(by_name);
        files.sort_by(by_name);
        out.extend(files);
        out
    }

    /// Trash rows as view entries — the real entry where the object still
    /// exists, a thin shell (name, kind, delete time) where it doesn't.
    pub fn trash_entries(&self) -> Vec<Entry> {
        self.list_trash()
            .into_iter()
            .map(|t| {
                if t.is_folder {
                    self.folders
                        .get(&t.object_id)
                        .map(|d| Entry::Folder(d.clone()))
                        .unwrap_or(Entry::Folder(FolderEntry {
                            id: t.object_id,
                            name: t.original_name,
                            parent: t.original_parent,
                            tombstoned: true,
                            revision: t.original_revision,
                        }))
                } else {
                    self.files
                        .get(&t.object_id)
                        .map(|f| Entry::File(f.clone()))
                        .unwrap_or(Entry::File(FileEntry {
                            id: t.object_id,
                            msg_id: 0,
                            name: t.original_name,
                            size: 0,
                            parent: t.original_parent,
                            upload_time: t.deleted_at,
                            tombstoned: true,
                            encrypted: false,
                            plaintext_size: 0,
                            upload_uuid: String::new(),
                            part_count: 0,
                            content_msg_id: 0,
                            revision: t.original_revision,
                        }))
                }
            })
            .collect()
    }

    /// Live child of `parent` named `name` (exact match).
    pub fn find_child<'a>(&'a self, parent: &str, name: &str) -> Option<EntryRef<'a>> {
        if let Some(d) = self
            .folders
            .values()
            .find(|d| !d.tombstoned && d.parent == parent && d.name == name)
        {
            return Some(EntryRef::Folder(d));
        }
        self.files
            .values()
            .find(|f| !f.tombstoned && f.parent == parent && f.name == name)
            .map(EntryRef::File)
    }

    /// Resolve a `/`-separated path to a live entry.
    pub fn resolve_path<'a>(&'a self, path: &str) -> Option<EntryRef<'a>> {
        let segs: Vec<&str> = path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        let mut parent = "";
        let mut cur: Option<EntryRef<'a>> = None;
        for (i, seg) in segs.iter().enumerate() {
            let c = self.find_child(parent, seg)?;
            match c {
                // A file mid-path means the rest can never resolve.
                EntryRef::File(_) if i + 1 < segs.len() => return None,
                EntryRef::Folder(d) => parent = &d.id,
                _ => {}
            }
            cur = Some(c);
        }
        cur
    }

    /// Whether `obj` sits under folder `root` (bounded walk).
    pub fn subtree_contains(&self, root: &str, obj: &str) -> bool {
        let mut parent = match self.dirent(obj) {
            Some(d) => d.parent,
            None => return false,
        };
        let mut guard = 0;
        while !parent.is_empty() && guard < MAX_DEPTH {
            if parent == root {
                return true;
            }
            guard += 1;
            parent = match self.folders.get(&parent) {
                Some(d) => d.parent.clone(),
                None => return false,
            };
        }
        false
    }

    /// Body message ids for every file under a folder subtree.
    pub fn subtree_body_msg_ids(&self, folder_id: &str) -> Vec<i64> {
        let mut out = Vec::new();
        for f in self.subtree_files(folder_id) {
            out.extend(self.body_msg_ids(f));
        }
        out
    }

    /// First free sibling name under `parent`, TDrive's restore rule: the
    /// name if free, else "name (k)" — counter before the extension for
    /// files ("photo (2).jpg").
    pub fn next_free_sibling_name(&self, parent: &str, name: &str, is_folder: bool) -> String {
        let (stem, ext) = if is_folder {
            (name.to_string(), String::new())
        } else {
            match name.rfind('.') {
                Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
                _ => (name.to_string(), String::new()),
            }
        };
        for attempt in 1..=1000 {
            let candidate = if attempt == 1 {
                name.to_string()
            } else {
                format!("{stem} ({attempt}){ext}")
            };
            let key = candidate.to_lowercase();
            let taken = self.list(parent).iter().any(|e| match e {
                Entry::Folder(d) => d.name.to_lowercase() == key,
                Entry::File(f) => f.name.to_lowercase() == key,
            });
            if !taken {
                return candidate;
            }
        }
        name.to_string()
    }

    /// Snapshot for durable storage — mirrors TDrive's persisted projection.
    pub fn snapshot(&self) -> NamespaceSnapshot {
        NamespaceSnapshot {
            folders: self.folders.clone(),
            files: self.files.clone(),
            parts: self.parts.clone(),
            trash: self.trash.clone(),
            seen_ops: self.seen_ops.clone(),
            last_msg_id: self.last_msg_id,
            encfg: self.encfg.clone(),
            renditions: self.renditions.clone(),
            unadopted: self.unadopted.clone(),
        }
    }

    pub fn restore_snapshot(s: NamespaceSnapshot) -> Self {
        Self {
            folders: s.folders,
            files: s.files,
            parts: s.parts,
            trash: s.trash,
            seen_ops: s.seen_ops,
            last_msg_id: s.last_msg_id,
            encfg: s.encfg,
            renditions: s.renditions,
            unadopted: s.unadopted,
        }
    }

    /// Breadcrumb path from root to a folder id.
    pub fn folder_path<'a>(&'a self, mut id: &'a str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut guard = 0;
        while !id.is_empty() && guard < MAX_DEPTH {
            guard += 1;
            if let Some(d) = self.folders.get(id) {
                out.push((d.id.clone(), d.name.clone()));
                id = &d.parent;
            } else {
                break;
            }
        }
        out.reverse();
        out
    }
}

fn valid_parent(p: &str) -> bool {
    p.is_empty() || wire::is_folder_id(p)
}

fn file_msg_id(obj: &str) -> Option<i64> {
    let raw = obj.strip_prefix("f:")?;
    if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: i64 = raw.parse().ok()?;
    (n > 0).then_some(n)
}

/// Move cycle check: walking ancestors of `parent` must not reach `obj`.
fn would_cycle(ns: &Namespace, obj: &str, parent: &str) -> bool {
    if obj == parent {
        return true;
    }
    let mut cur = parent.to_string();
    let mut guard = 0;
    while !cur.is_empty() && guard < MAX_DEPTH {
        guard += 1;
        if cur == obj {
            return true;
        }
        cur = ns
            .folders
            .get(&cur)
            .map(|d| d.parent.clone())
            .unwrap_or_default();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(s: &str) -> Op {
        wire::parse(s).unwrap()
    }

    #[test]
    fn single_file_listing() {
        let mut ns = Namespace::new();
        ns.apply(10, &op("TDX1|t=f|p=|n=a.txt|sz=5"));
        let items = ns.list("");
        assert_eq!(items.len(), 1);
        match &items[0] {
            Entry::File(f) => {
                assert_eq!(f.id, "f:10");
                assert_eq!(f.name, "a.txt");
                assert_eq!(ns.body_msg_ids(f), vec![10]);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn multipart_file() {
        let mut ns = Namespace::new();
        ns.apply(11, &op("TDX1|t=part|u=u1|pix=0|sz=100"));
        ns.apply(12, &op("TDX1|t=part|u=u1|pix=1|sz=50"));
        ns.apply(13, &op("TDX1|t=manifest|u=u1|p=|n=big|pc=2|sz=150"));
        let items = ns.list("");
        assert_eq!(items.len(), 1);
        match &items[0] {
            Entry::File(f) => {
                assert_eq!(f.size, 150);
                assert_eq!(ns.body_msg_ids(f), vec![11, 12]);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn folders_and_tombstone() {
        let mut ns = Namespace::new();
        ns.apply(1, &op("TDX1|t=mkdir|obj=d:1|p=|n=sub"));
        ns.apply(2, &op("TDX1|t=f|p=d:1|n=x|sz=1"));
        assert_eq!(ns.list("").len(), 1);
        assert_eq!(ns.list("d:1").len(), 1);
        ns.apply(3, &op("TDX1|t=rmdir|obj=d:1"));
        assert_eq!(ns.list("").len(), 0);
        // rmdir tombstones only the folder — children become unreachable
        // but are not individually trashed.
        assert_eq!(ns.files.get("f:2").unwrap().tombstoned, false);
        assert!(ns.trash.is_empty());
    }

    #[test]
    fn rename_and_move() {
        let mut ns = Namespace::new();
        ns.apply(1, &op("TDX1|t=mkdir|obj=d:1|p=|n=sub"));
        ns.apply(2, &op("TDX1|t=f|p=|n=x|sz=1"));
        ns.apply(3, &op("TDX1|t=rename|obj=f:2|n=y"));
        ns.apply(4, &op("TDX1|t=move|obj=f:2|p=d:1"));
        assert_eq!(ns.list("").len(), 1); // just the folder
        let Entry::File(f) = &ns.list("d:1")[0] else {
            panic!()
        };
        assert_eq!(f.name, "y");
        assert_eq!(f.revision, 3); // create=1, rename=2, move=3
    }

    #[test]
    fn trash_cas_and_restore() {
        let mut ns = Namespace::new();
        ns.apply(1, &op("TDX1|t=mkdir|obj=d:a|p=|n=dir"));
        ns.apply(2, &op("TDX1|t=f|p=d:a|n=x.txt|sz=5"));
        // Wrong revision → refused.
        ns.apply(
            3,
            &op("TDX1|t=trash|v=1|oid=bad|obj=d:a|rev=9|del=100|purge=200"),
        );
        assert_eq!(ns.list("").len(), 1);
        // Correct revision: folder at rev 1 → CAS trashes whole subtree.
        ns.apply(
            4,
            &op("TDX1|t=trash|v=1|oid=t1|obj=d:a|rev=1|del=100|purge=200"),
        );
        assert!(ns.files.get("f:2").unwrap().tombstoned);
        assert_eq!(ns.trash.get("d:a").unwrap().original_name, "dir");
        assert_eq!(ns.list_trash().len(), 1);
        // Replay of the same op_id is a no-op.
        ns.apply(
            5,
            &op("TDX1|t=trash|v=1|oid=t1|obj=d:a|rev=1|del=100|purge=200"),
        );
        // Restore: folder is at revision 2 after trash.
        ns.apply(6, &op("TDX1|t=untrash|v=1|oid=r1|obj=d:a|p=|n=dir|rev=2"));
        assert!(!ns.folders.get("d:a").unwrap().tombstoned);
        assert!(!ns.files.get("f:2").unwrap().tombstoned);
        assert!(ns.trash.is_empty());
    }

    #[test]
    fn hard_delete_removes() {
        let mut ns = Namespace::new();
        ns.apply(1, &op("TDX1|t=f|p=|n=x|sz=1"));
        ns.apply(
            2,
            &op("TDX1|t=trash|v=1|oid=t|obj=f:1|rev=1|del=10|purge=40"),
        );
        ns.apply(3, &op("TDX1|t=harddel|v=1|oid=p|obj=f:1|rev=2"));
        assert!(ns.files.get("f:1").is_none());
        assert!(ns.trash.is_empty());
        // harddel on a live object (mount delete) with matching revision
        // removes it; a wrong revision is refused.
        ns.apply(4, &op("TDX1|t=f|p=|n=y|sz=1"));
        ns.apply(5, &op("TDX1|t=harddel|v=1|oid=bad|obj=f:4|rev=9"));
        assert!(ns.files.get("f:4").is_some());
        ns.apply(6, &op("TDX1|t=harddel|v=1|oid=p2|obj=f:4|rev=1"));
        assert!(ns.files.get("f:4").is_none());
    }

    #[test]
    fn move_cycle_rejected() {
        let mut ns = Namespace::new();
        ns.apply(1, &op("TDX1|t=mkdir|obj=d:a|p=|n=a"));
        ns.apply(2, &op("TDX1|t=mkdir|obj=d:b|p=d:a|n=b"));
        ns.apply(3, &op("TDX1|t=move|obj=d:a|p=d:b"));
        assert_eq!(ns.folders.get("d:a").unwrap().parent, "");
    }

    #[test]
    fn rename_on_tombstoned_ignored() {
        let mut ns = Namespace::new();
        ns.apply(1, &op("TDX1|t=f|p=|n=x|sz=1"));
        ns.apply(2, &op("TDX1|t=tomb|obj=f:1"));
        ns.apply(3, &op("TDX1|t=rename|obj=f:1|n=y"));
        assert_eq!(ns.files.get("f:1").unwrap().name, "x");
    }

    #[test]
    fn free_sibling_name() {
        let mut ns = Namespace::new();
        ns.apply(1, &op("TDX1|t=f|p=|n=photo.jpg|sz=1"));
        assert_eq!(
            ns.next_free_sibling_name("", "photo.jpg", false),
            "photo (2).jpg"
        );
        assert_eq!(ns.next_free_sibling_name("", "other", false), "other");
    }
}

#[cfg(test)]
mod mkdir_pipeline_test {
    use super::*;
    use crate::wire::{Op, OpType, OpTypeOrUnknown, format as wfmt};

    #[test]
    fn mkdir_format_apply_list() {
        // Exact send_and_project path: format -> apply_message -> list.
        let op = Op {
            op_type: OpTypeOrUnknown::Known(OpType::Mkdir),
            obj: "d:test-uuid".into(),
            parent: String::new(),
            name: "Folder1".into(),
            ..Default::default()
        };
        let header = wfmt(&op);
        let mut ns = Namespace::new();
        assert!(ns.apply_message(1, &header), "apply failed for: {header}");
        let l = ns.list("");
        assert_eq!(l.len(), 1, "list empty after mkdir");
        match &l[0] {
            Entry::Folder(d) => assert_eq!(d.name, "Folder1"),
            _ => panic!("not a folder"),
        }
    }
}

impl serde::Serialize for Namespace {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.snapshot().serialize(s)
    }
}
