//! Telegram worker — owns the grammers client on a dedicated tokio runtime
//! thread and translates UI commands into MTProto calls.

use crate::paths;
use crate::crypto::{self, VaultConfig};
use crate::journal::{DownloadJob, Journal, PartReceipt, UploadJob};
use crate::ops::{self, VfsOp};
use crate::project::Namespace;
use crate::remote::Remote;
use crate::wire::{self, Op};
use anyhow::{Context, Result, anyhow};
use futures_util::FutureExt;
use grammers_client::client::{LoginToken, PasswordToken};
use grammers_client::media::Media;
use grammers_client::message::InputMessage;
use grammers_client::peer::Peer;
use grammers_client::session::storages::SqliteSession;
use grammers_client::session::types::{ChannelKind, PeerAuth, PeerId, PeerRef};
use grammers_client::{Client, SignInError, sender::SenderPool};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod auth;
mod backup;
mod download;
mod drives;
mod media;
mod surfaces;
mod sync;
mod upload;
mod vault;
mod vfs;

pub(crate) use media::{is_image_name, is_video_name};

pub const MAX_PART_BYTES: i64 = 1900 * 1024 * 1024;
pub const LARGE_FILE_MAX_BYTES: i64 = 40 * 1024 * 1024 * 1024;
pub const MAX_PARTS: usize = 32;

/// The two drive channel shapes TDrive distinguishes — a personal
/// broadcast channel (encryption-eligible) vs a shared megagroup.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum DriveKind {
    Personal,
    Shared,
}

impl DriveKind {
    /// TDrive's channel-type string on the wire/UI boundary.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "broadcast",
            Self::Shared => "megagroup",
        }
    }

    /// Personal drives are the only encryption-eligible surface.
    pub fn is_personal(self) -> bool {
        matches!(self, Self::Personal)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChannelInfo {
    pub id: i64,
    pub access_hash: i64,
    pub title: String,
    pub kind: DriveKind,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "cmd", content = "args")]
pub enum Command {
    Connect {
        api_id: i32,
        api_hash: String,
    },
    RequestCode {
        phone: String,
    },
    SubmitCode {
        code: String,
    },
    SubmitPassword {
        password: String,
    },
    ListChannels,
    CreateChannel {
        title: String,
    },
    CreateSharedChannel {
        title: String,
    },
    EmptyTrash,
    /// Scan the op log and physically delete messages whose objects were
    /// created and later hard-deleted — dead weight in the channel.
    OptimizeDrive,
    /// Execute the plan staged by OptimizeDrive.
    OptimizeApply,
    OpenChannel {
        id: i64,
        access_hash: i64,
        title: String,
        kind: DriveKind,
    },
    Sync,
    Upload {
        path: PathBuf,
        parent: String,
    },
    /// Upload a local folder — preserves structure via mkdir ops.
    UploadDir {
        dir: PathBuf,
        parent: String,
    },
    /// 7z-archive a local folder, then upload the archive.
    UploadArchive {
        dir: PathBuf,
        parent: String,
    },
    SetUploadLimit {
        n: usize,
    },
    ResumeUploads,
    Download {
        file_id: String,
        dest: PathBuf,
    },
    Mkdir {
        parent: String,
        name: String,
    },
    Rename {
        obj: String,
        name: String,
    },
    Move {
        obj: String,
        parent: String,
    },
    Delete {
        obj: String,
    },
    Restore {
        obj: String,
    },
    Purge {
        obj: String,
    },
    Cancel {
        id: String,
    },
    Unlock {
        password: String,
    },
    Lock,
    EnableEncryption {
        password: String,
        hint: String,
    },
    JoinInvite {
        link: String,
    },
    FetchThumbnail {
        file_id: String,
    },
    /// Fetch a viewable rendition (1600px preview preferred) for the
    /// in-app file viewer, falling back to the raw body for images.
    FetchPreview {
        file_id: String,
    },
    Adopt {
        msg_id: i64,
        parent: String,
    },
    /// Write op issued by the WebDAV server; reply carries the result.
    #[serde(skip)]
    VfsWrite {
        op: VfsOp,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    BackupPhotos {
        paths: Vec<PathBuf>,
    },
    /// Continuous photo backup — watch a folder for new images.
    WatchBackup {
        dir: PathBuf,
    },
    BackupTick,
    LiveSync,
    SignOut,
    // Internal task→worker messages — spawned transfers report journal
    // and namespace mutations through the loop so state stays owned here.
    /// A locally-emitted op that must be folded into the namespace.
    OpApplied {
        header: String,
        msg_id: i64,
    },
    UploadPartConfirmed {
        uuid: String,
        pix: i32,
        msg_id: i64,
    },
    UploadJobPatch {
        uuid: String,
        local_path: PathBuf,
        manifest_msg_id: Option<i64>,
    },
    DlConfirmed {
        file_id: String,
        written: i64,
    },
    TransferDone {
        id: String,
    },
    /// A pending-join poll resolved — open the approved shared drive.
    JoinApproved {
        id: i64,
        access_hash: i64,
        title: String,
        kind: DriveKind,
    },
    TransferFailed {
        id: String,
        label: String,
        err: String,
    },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TransferUpdate {
    pub id: String,
    pub label: String,
    pub sent: i64,
    pub total: i64,
    pub state: TransferState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TransferState {
    Active,
    Paused,
    Done,
    Failed,
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "ev", content = "args")]
pub enum Event {
    Connected {
        authorized: bool,
    },
    CodeSent,
    NeedPassword {
        hint: String,
    },
    SignedIn {
        name: String,
    },
    Channels(Vec<ChannelInfo>),
    ChannelOpened {
        info: ChannelInfo,
        pending_uploads: usize,
    },
    Namespace(Arc<Namespace>),
    Transfer(TransferUpdate),
    Error {
        context: &'static str,
        message: String,
    },
    VaultState {
        configured: bool,
        unlocked: bool,
        hint: String,
    },
    InviteLink {
        link: String,
    },
    ThumbnailReady {
        file_id: String,
        path: PathBuf,
    },
    ThumbnailFailed {
        file_id: String,
    },
    PreviewReady {
        file_id: String,
        path: PathBuf,
    },
    PreviewFailed {
        file_id: String,
    },
    /// Sync started/finished — drives the UI's fetch indicator.
    SyncState {
        active: bool,
    },
    /// Transient status line (e.g. flood-wait notices).
    Status {
        message: String,
    },
    /// OptimizeDrive finished scanning — how many channel messages are
    /// dead ops/orphaned bodies eligible for deletion.
    OptimizePlan {
        dead: usize,
        scanned: usize,
    },
    SignedOut,
}

/// FLOOD_WAIT is a scheduling signal, not a failure — a storage client
/// must always sleep it out rather than kill the transfer. We sleep the
/// exact requested duration with no try cap (Telegram's waits escalate
/// for hours; giving up means lost uploads), retry I/O blips briefly,
/// and propagate everything else. Each wait is surfaced as a Status so
/// the UI can show "rate-limited — resuming in Ns" instead of looking
/// frozen.
struct FloodSleep {
    tx: async_channel::Sender<Event>,
}

impl grammers_client::sender::RetryPolicy for FloodSleep {
    fn should_retry(
        &self,
        ctx: &grammers_client::sender::RetryContext,
    ) -> std::ops::ControlFlow<(), std::time::Duration> {
        use grammers_client::sender::InvocationError;
        match &ctx.error {
            InvocationError::Rpc(e) if e.code == 420 => {
                let secs = (e.value.unwrap_or(60) as u64).clamp(1, 86_400);
                let _ = self.tx.try_send(Event::Status {
                    message: format!("rate-limited — resuming in {secs}s"),
                });
                std::ops::ControlFlow::Continue(std::time::Duration::from_secs(secs))
            }
            InvocationError::Io(_) if ctx.fail_count.get() < 120 => {
                std::ops::ControlFlow::Continue(std::time::Duration::from_secs(1))
            }
            _ => std::ops::ControlFlow::Break(()),
        }
    }
}

pub struct Handle {
    pub tx: mpsc::UnboundedSender<Command>,
    pub rx: async_channel::Receiver<Event>,
}

pub fn spawn() -> Handle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (ev_tx, ev_rx) = async_channel::unbounded::<Event>();
    let cmd_tx_worker = cmd_tx.clone();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            // Grammers' recursive TL deserialization (nested Update vectors
            // in Updates responses) overflows the default 2 MiB stack.
            .thread_stack_size(16 * 1024 * 1024)
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async move {
            // Periodic backup watcher — the poll loop TDrive runs for its
            // photo-backup lifecycle.
            {
                let tx = cmd_tx_worker.clone();
                tokio::spawn(async move {
                    let mut iv = tokio::time::interval(std::time::Duration::from_secs(60));
                    loop {
                        iv.tick().await;
                        if tx.send(Command::BackupTick).is_err() {
                            return;
                        }
                    }
                });
            }
            let mut worker = Worker {
                cmd_tx: cmd_tx_worker,
                client: None,
                api_hash: String::new(),
                login_token: None,
                password_token: None,
                peer_cell: Arc::new(Mutex::new(None)),
                channel_id: 0,
                namespace: Arc::new(Namespace::new()),
                master_key: Arc::new(Mutex::new(None)),
                actor_id: 0,
                channel_kind: DriveKind::Personal,
                paused_downloads: std::collections::HashSet::new(),
                cancelled: std::collections::HashSet::new(),
                pending_optimize: Vec::new(),
                live_sync_queued: Default::default(),
                ns_shared: Arc::new(Mutex::new(Arc::new(Namespace::new()))),
                thumb_sem: Arc::new(tokio::sync::Semaphore::new(8)),
                upload_sem: Arc::new(tokio::sync::Semaphore::new(
                    crate::paths::AppConfig::load()
                        .ok()
                        .flatten()
                        .map(|c| c.upload_limit)
                        .unwrap_or_else(crate::paths::default_upload_limit)
                        .max(1),
                )),
                cancels: HashMap::new(),
                last_live_sync: std::time::Instant::now() - std::time::Duration::from_secs(10),
                journal_path: crate::paths::journal_path()
                    .unwrap_or_else(|_| std::env::temp_dir().join("tdrive-rs-journal.json")),
                journal: Journal::default(),
                rx: cmd_rx,
                tx: ev_tx,
            };
            worker.journal = Journal::load(&worker.journal_path);
            worker.run().await;
        });
    });

    Handle {
        tx: cmd_tx,
        rx: ev_rx,
    }
}

/// Clones handed to a spawned transfer task — everything it needs to do
/// IO off the command loop. Journal/namespace mutations are reported
/// back as Commands so the worker keeps sole ownership of that state.
#[derive(Clone)]
struct TransferCtx {
    remote: Remote,
    /// Drive pinned at spawn — an in-flight transfer finishes on the
    /// drive it started on even if the user switches channels.
    peer: PeerRef,
    tx: async_channel::Sender<Event>,
    cmd: mpsc::UnboundedSender<Command>,
    token: CancellationToken,
    upload_sem: Arc<tokio::sync::Semaphore>,
}

impl TransferCtx {
    fn emit(&self, ev: Event) {
        let _ = self.tx.try_send(ev);
    }
    fn update(&self, id: &str, label: &str, sent: i64, total: i64, state: TransferState) {
        self.emit(Event::Transfer(TransferUpdate {
            id: id.to_string(),
            label: label.to_string(),
            sent,
            total,
            state,
        }));
    }
    fn journal(&self, c: Command) {
        let _ = self.cmd.send(c);
    }
    fn op_applied(&self, header: String, msg_id: i64) {
        self.journal(Command::OpApplied { header, msg_id });
    }
    async fn send_op(&self, header: &str) -> Result<i64> {
        self.remote.send_op(self.peer, header).await
    }
}

struct Worker {
    client: Option<Client>,
    api_hash: String,
    login_token: Option<LoginToken>,
    password_token: Option<PasswordToken>,
    /// Current drive peer, shared with dav/stream/fuse so a
    /// channel switch repoints mounted surfaces live.
    peer_cell: Arc<Mutex<Option<PeerRef>>>,
    channel_id: i64,
    namespace: Arc<Namespace>,
    master_key: Arc<Mutex<Option<[u8; crypto::MASTER_KEY_LEN]>>>,
    /// Our own user id — adoption ownership checks.
    actor_id: i64,
    /// Encryption-eligibility: only personal (broadcast) drives.
    channel_kind: DriveKind,
    /// Paused downloads (transfer ids) — resume looks the job up
    /// in the journal, which still holds the destination.
    paused_downloads: std::collections::HashSet<String>,
    /// User-cancelled ids — suppress the unwind's failure report the
    /// same way paused ids do.
    cancelled: std::collections::HashSet<String>,
    /// Message ids the optimize scan marked for deletion — apply only
    /// runs after the UI confirms this plan.
    pending_optimize: Vec<i64>,
    live_sync_queued: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Latest namespace snapshot shared with the WebDAV server.
    ns_shared: Arc<Mutex<Arc<Namespace>>>,
    /// Live FUSE mount session — dropping it unmounts.
    /// Bounds concurrent thumbnail fetches — they spawn as tasks so the
    /// command loop stays free instead of serializing RTTs.
    thumb_sem: Arc<tokio::sync::Semaphore>,
    upload_sem: Arc<tokio::sync::Semaphore>,
    cancels: HashMap<String, CancellationToken>,
    last_live_sync: std::time::Instant,
    journal: Journal,
    journal_path: PathBuf,
    rx: mpsc::UnboundedReceiver<Command>,
    tx: async_channel::Sender<Event>,
    cmd_tx: mpsc::UnboundedSender<Command>,
}

impl Worker {
    fn emit(&self, ev: Event) {
        let _ = self.tx.try_send(ev);
    }

    fn save_journal(&self) {
        if let Err(e) = self.journal.save(&self.journal_path) {
            self.emit(Event::Error {
                context: "journal",
                message: e.to_string(),
            });
        }
    }

    async fn run(&mut self) {
        while let Some(cmd) = self.rx.recv().await {
            // Panics inside grammers (min entities, malformed TL) must not
            // unwind through the loop — a dead worker leaves the UI dumb.
            let res = std::panic::AssertUnwindSafe(self.handle(cmd))
                .catch_unwind()
                .await;
            match res {
                Ok(Err(e)) => self.emit(Event::Error {
                    context: "telegram",
                    message: format!("{e:#}"),
                }),
                Err(panic) => self.emit(Event::Error {
                    context: "panic",
                    message: format!("{panic:?}"),
                }),
                Ok(Ok(())) => {}
            }
        }
    }

    async fn handle(&mut self, cmd: Command) -> Result<()> {
        match cmd {
            Command::Connect { api_id, api_hash } => self.connect(api_id, api_hash).await,
            Command::RequestCode { phone } => self.request_code(phone).await,
            Command::SubmitCode { code } => self.submit_code(code).await,
            Command::SubmitPassword { password } => self.submit_password(password).await,
            Command::ListChannels => self.list_channels().await,
            Command::CreateChannel { title } => self.create_channel(title).await,
            Command::CreateSharedChannel { title } => self.create_channel_inner(title, true).await,
            Command::EmptyTrash => self.empty_trash().await,
            Command::OptimizeDrive => self.optimize_scan().await,
            Command::OptimizeApply => self.optimize_apply().await,
            Command::OpenChannel {
                id,
                access_hash,
                title,
                kind,
            } => self.open_channel(id, access_hash, title, kind).await,
            Command::Sync => self.sync().await,
            Command::Upload { path, parent } => self.upload(path, parent).await,
            Command::UploadDir { dir, parent } => self.upload_dir(dir, parent).await,
            Command::UploadArchive { dir, parent } => self.upload_archive(dir, parent).await,
            Command::SetUploadLimit { n } => {
                let n = n.clamp(1, crate::paths::MAX_UPLOAD_LIMIT);
                self.upload_sem = Arc::new(tokio::sync::Semaphore::new(n));
                if let Ok(Some(mut cfg)) = crate::paths::AppConfig::load() {
                    cfg.upload_limit = n;
                    let _ = cfg.save();
                }
                Ok(())
            }
            Command::ResumeUploads => self.resume_uploads().await,
            Command::Download { file_id, dest } => self.download(file_id, dest).await,
            Command::Mkdir { parent, name } => self.mkdir(parent, name).await,
            Command::Rename { obj, name } => self.rename(obj, name).await,
            Command::Move { obj, parent } => self.move_obj(obj, parent).await,
            Command::Delete { obj } => self.delete(obj).await,
            Command::Restore { obj } => self.restore(obj).await,
            Command::Purge { obj } => self.purge(obj).await,
            Command::Cancel { id } => self.cancel(&id),
            Command::Unlock { password } => self.unlock(&password).await,
            Command::Lock => {
                *self.master_key.lock().unwrap() = None;
                self.emit_vault_state();
                Ok(())
            }
            Command::EnableEncryption { password, hint } => {
                self.enable_encryption(&password, &hint).await
            }
            Command::JoinInvite { link } => self.join_invite(&link).await,
            Command::FetchThumbnail { file_id } => self.spawn_thumbnail_fetch(&file_id),
            Command::FetchPreview { file_id } => self.spawn_preview_fetch(&file_id),
            Command::Adopt { msg_id, parent } => self.adopt(msg_id, parent).await,
            Command::VfsWrite { op, reply } => {
                let _ = reply.send(self.vfs_op(op).await.map_err(|e| format!("{e:#}")));
                Ok(())
            }
            Command::Pause { id } => self.pause(&id).await,
            Command::Resume { id } => self.resume(&id).await,
            Command::BackupPhotos { paths } => self.backup_photos(paths).await,
            Command::WatchBackup { dir } => {
                self.journal.backup_dir = Some(dir.clone());
                self.save_journal();
                self.backup_photos(
                    std::fs::read_dir(dir)
                        .map(|rd| rd.flatten().map(|e| e.path()).collect())
                        .unwrap_or_default(),
                )
                .await
            }
            Command::BackupTick => self.backup_tick().await,
            Command::LiveSync => self.live_sync().await,
            Command::SignOut => self.sign_out().await,
            Command::OpApplied { header, msg_id } => {
                let mut ns = (*self.namespace).clone();
                if !ns.apply_message(msg_id, &header) {
                    self.emit(Event::Error {
                        context: "projection",
                        message: format!("locally-emitted op at {msg_id} failed to apply"),
                    });
                }
                self.namespace = Arc::new(ns);
                self.save_namespace();
                self.push_namespace();
                Ok(())
            }
            Command::UploadPartConfirmed { uuid, pix, msg_id } => {
                if let Some(j) = self
                    .journal
                    .uploads
                    .iter_mut()
                    .find(|j| j.upload_uuid == uuid)
                {
                    if let Some(p) = j.parts.get_mut(pix as usize) {
                        p.msg_id = Some(msg_id);
                    }
                }
                self.save_journal();
                Ok(())
            }
            Command::UploadJobPatch {
                uuid,
                local_path,
                manifest_msg_id,
            } => {
                if let Some(j) = self
                    .journal
                    .uploads
                    .iter_mut()
                    .find(|j| j.upload_uuid == uuid)
                {
                    j.local_path = local_path;
                    if let Some(id) = manifest_msg_id {
                        j.manifest_msg_id = Some(id);
                    }
                }
                self.save_journal();
                Ok(())
            }
            Command::DlConfirmed { file_id, written } => {
                if let Some(j) = self
                    .journal
                    .downloads
                    .iter_mut()
                    .find(|j| j.file_id == file_id)
                {
                    j.confirmed_bytes = written;
                }
                self.save_journal();
                Ok(())
            }
            Command::JoinApproved {
                id,
                access_hash,
                title,
                kind,
            } => {
                self.open_channel(id, access_hash, title, kind).await?;
                self.list_channels().await
            }
            Command::TransferDone { id } => {
                self.cancels.remove(&id);
                match parse_transfer_id(&id) {
                    Some(TransferTarget::Upload(uuid)) => {
                        self.journal.uploads.retain(|j| j.upload_uuid != uuid)
                    }
                    Some(TransferTarget::Download(fid)) => {
                        self.journal.downloads.retain(|j| j.file_id != fid)
                    }
                    None => {}
                }
                self.save_journal();
                Ok(())
            }
            Command::TransferFailed { id, label, err } => {
                self.cancels.remove(&id);
                // A pause also unwinds as an error — don't overwrite the
                // Paused state the pause() handler already set.
                if !self.paused_downloads.contains(&id) && !self.cancelled.remove(&id) {
                    self.emit(Event::Error {
                        context: "transfer",
                        message: format!("{label}: {err}"),
                    });
                    self.emit(Event::Transfer(TransferUpdate {
                        id: id.clone(),
                        label,
                        sent: 0,
                        total: 0,
                        state: TransferState::Failed,
                    }));
                }
                Ok(())
            }
        }
    }

    /// Bundle the worker's shared handles for a detached transfer task.
    fn transfer_ctx(&self, token: CancellationToken) -> Result<TransferCtx> {
        Ok(TransferCtx {
            remote: self.remote()?,
            peer: self.channel()?,
            tx: self.tx.clone(),
            cmd: self.cmd_tx.clone(),
            token,
            upload_sem: self.upload_sem.clone(),
        })
    }

    /// Which job family a transfer id refers to — `up:` vs `dl:` is the
    /// whole convention, and it only exists here.
    // (free fns at module bottom)
    /// Mint the cancellation token, register it under `id`, and spawn —
    /// every cancellable transfer goes through here so pause/cancel can
    /// always find the token the task actually checks.
    fn spawn_cancellable<F, Fut>(&mut self, id: &str, label: &str, f: F) -> Result<()>
    where
        F: FnOnce(TransferCtx) -> Fut,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        let token = CancellationToken::new();
        self.cancels.insert(id.to_string(), token.clone());
        let ctx = self.transfer_ctx(token)?;
        self.spawn_transfer(id, label, f(ctx));
        Ok(())
    }

    /// Spawn a transfer task and route its failure back as a Command —
    /// the loop is never blocked by transfer IO.
    fn spawn_transfer(
        &self,
        id: &str,
        label: &str,
        fut: impl std::future::Future<Output = Result<()>> + Send + 'static,
    ) {
        let cmd = self.cmd_tx.clone();
        let id = id.to_string();
        let label = label.to_string();
        tokio::spawn(async move {
            if let Err(e) = fut.await {
                let _ = cmd.send(Command::TransferFailed {
                    id,
                    label,
                    err: format!("{e:#}"),
                });
            }
        });
    }

    fn client(&self) -> Result<&Client> {
        self.client.as_ref().context("not connected")
    }

    fn channel(&self) -> Result<PeerRef> {
        self.peer_cell.lock().unwrap().context("no drive open")
    }

    /// The live drive connection handed to dav/stream/fuse and
    /// transfer contexts — the cells are shared, so callers ride along
    /// on channel switches and vault lock/unlock automatically.
    fn remote(&self) -> Result<Remote> {
        Ok(Remote::new(
            self.client()?.clone(),
            self.peer_cell.clone(),
            self.master_key.clone(),
            self.ns_shared.clone(),
        ))
    }

    /// Publish the namespace to the UI and the shared DAV snapshot.
    fn push_namespace(&self) {
        *self.ns_shared.lock().unwrap() = self.namespace.clone();
        self.emit(Event::Namespace(self.namespace.clone()));
    }

    fn save_namespace(&self) {
        if self.channel_id == 0 {
            return;
        }
        if let Ok(path) = crate::paths::namespace_path(self.channel_id) {
            let _ = crate::paths::atomic_json_write(&path, &self.namespace.snapshot());
        }
    }

    async fn send_op(&self, header: &str) -> Result<i64> {
        self.remote()?.send_op(self.channel()?, header).await
    }

    /// Send a control op and fold it into the local namespace.
    async fn send_and_project(&mut self, op: &Op) -> Result<()> {
        let header = wire::format(op);
        let msg_id = self.send_op(&header).await?;
        let mut ns = (*self.namespace).clone();
        if !ns.apply_message(msg_id, &header) {
            anyhow::bail!("op emitted but rejected by projection");
        }
        self.namespace = Arc::new(ns);
        self.save_namespace();
        self.push_namespace();
        Ok(())
    }

    /// Pause suspends the transfer but keeps journal state, unlike cancel
    /// which discards it — the same distinction TDrive makes.
    async fn pause(&mut self, id: &str) -> Result<()> {
        // Single uploads aren't journaled — they can't resume, so pause
        // degrades to cancel rather than offering a dead button.
        if let Some(TransferTarget::Upload(uuid)) = parse_transfer_id(id) {
            if !self.journal.uploads.iter().any(|j| j.upload_uuid == uuid) {
                return self.cancel(id);
            }
        }
        if let Some(token) = self.cancels.remove(id) {
            // Drop the cancelled token — resume must mint a fresh one or
            // is_cancelled() fires instantly and the job can never resume.
            token.cancel();
        }
        if let Some(TransferTarget::Download(_)) = parse_transfer_id(id) {
            self.paused_downloads.insert(id.to_string());
        }
        self.emit(Event::Transfer(TransferUpdate {
            id: id.to_string(),
            label: String::new(),
            sent: 0,
            total: 0,
            state: TransferState::Paused,
        }));
        Ok(())
    }

    async fn resume(&mut self, id: &str) -> Result<()> {
        match parse_transfer_id(id) {
            Some(TransferTarget::Upload(_)) => self.resume_uploads().await,
            Some(TransferTarget::Download(file_id)) => {
                if !self.paused_downloads.remove(id) {
                    return Err(anyhow!("nothing to resume"));
                }
                // Reuse the journal row — a fresh DownloadJob would reset
                // confirmed_bytes and re-download from scratch.
                let Some(job) = self
                    .journal
                    .downloads
                    .iter()
                    .find(|j| j.file_id == file_id)
                    .cloned()
                else {
                    return Err(anyhow!("nothing to resume"));
                };
                let name = job.name.clone();
                self.spawn_cancellable(id, &name, move |ctx| async move {
                    Self::run_download_task(&ctx, job).await
                })
            }
            None => Err(anyhow!("nothing to resume")),
        }
    }

    fn cancel(&mut self, id: &str) -> Result<()> {
        if let Some(token) = self.cancels.get(id) {
            token.cancel();
        }
        if let Some(TransferTarget::Upload(uuid)) = parse_transfer_id(id) {
            self.journal.uploads.retain(|j| j.upload_uuid != uuid);
            self.save_journal();
        }
        if let Some(TransferTarget::Download(fid)) = parse_transfer_id(id) {
            let dest = self
                .journal
                .downloads
                .iter()
                .find(|j| j.file_id == fid)
                .map(|j| download::staging_path(&j.dest));
            self.journal.downloads.retain(|j| j.file_id != fid);
            if let Some(staging) = dest {
                let _ = std::fs::remove_file(staging);
            }
            self.save_journal();
        }
        self.cancelled.insert(id.to_string());
        self.emit(Event::Transfer(TransferUpdate {
            id: id.to_string(),
            label: String::new(),
            sent: 0,
            total: 0,
            state: TransferState::Failed,
        }));
        Ok(())
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ProgressReader<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        match std::pin::Pin::new(&mut self.inner).poll_read(cx, buf) {
            std::task::Poll::Ready(Ok(())) => {
                let got = (buf.filled().len() - before) as u64;
                self.sent += got;
                // Emit at ~10 Hz or every 64 KiB — whichever comes first
                // keeps bars smooth on both fast and slow links.
                if self.sent - self.last_emit >= 64 * 1024
                    || self.last_emit_at.elapsed() >= std::time::Duration::from_millis(100)
                {
                    self.last_emit = self.sent;
                    self.last_emit_at = std::time::Instant::now();
                    (self.on_progress)(self.sent as i64);
                }
                std::task::Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

pub(super) fn media_size(media: &Media) -> usize {
    match media {
        Media::Document(d) => d.size().unwrap_or(0),
        Media::Photo(p) => p.size().unwrap_or(0),
        _ => 0,
    }
}

/// AsyncRead wrapper that reports bytes consumed upstream (throttled).
pub(super) struct ProgressReader<R> {
    inner: R,
    sent: u64,
    last_emit: u64,
    last_emit_at: std::time::Instant,
    on_progress: Box<dyn Fn(i64) + Send + Sync>,
}

impl<R: AsyncRead + Unpin> ProgressReader<R> {
    pub(super) fn new(inner: R, on_progress: impl Fn(i64) + Send + Sync + 'static) -> Self {
        Self {
            inner,
            sent: 0,
            last_emit: 0,
            last_emit_at: std::time::Instant::now(),
            on_progress: Box::new(on_progress),
        }
    }
}

/// `up:`/`dl:` transfer ids — built and parsed only through these.
pub(super) fn upload_transfer_id(key: &str) -> String {
    format!("up:{key}")
}

pub(super) fn download_transfer_id(file_id: &str) -> String {
    format!("dl:{file_id}")
}

enum TransferTarget {
    Upload(String),
    Download(String),
}

fn parse_transfer_id(id: &str) -> Option<TransferTarget> {
    id.strip_prefix("up:")
        .map(|k| TransferTarget::Upload(k.to_string()))
        .or_else(|| {
            id.strip_prefix("dl:")
                .map(|k| TransferTarget::Download(k.to_string()))
        })
}
