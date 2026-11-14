//! Upload engine — single files, archives, directory walks, multipart
//! staging, part sends, journal resume.
use super::*;

impl Worker {
    /// Remote filename that doesn't collide — checks the projected
    /// namespace plus names already claimed this session, then
    /// appends " (n)" before the extension until it's free.
    fn unique_remote_name(&mut self, parent: &str, name: &str) -> String {
        let taken = |n: &str| {
            self.namespace.list(parent).iter().any(|e| e.name() == n)
                || self
                    .reserved_names
                    .contains(&format!("{parent}\0{n}"))
        };
        let pick = if taken(name) {
            let (stem, ext) = match name.rsplit_once('.') {
                Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
                _ => (name.to_string(), String::new()),
            };
            (1..1000)
                .map(|i| format!("{stem} ({i}){ext}"))
                .find(|c| !taken(c))
                .unwrap_or_else(|| name.to_string())
        } else {
            name.to_string()
        };
        self.reserved_names.insert(format!("{parent}\0{pick}"));
        pick
    }

    pub(super) async fn resume_uploads(&mut self) -> Result<()> {
        let channel_id = self
            .channel()
            .map(|p| p.id.bare_id_unchecked())
            .unwrap_or(0);
        let jobs: Vec<UploadJob> = self
            .journal
            .resumable_uploads(channel_id)
            .into_iter()
            .cloned()
            .collect();
        for job in jobs {
            let id = upload_transfer_id(&job.upload_uuid);
            let name = job.name.clone();
            if let Err(e) = self.spawn_cancellable(&id, &name, move |ctx| async move {
                Self::run_upload_task(&ctx, job).await
            }) {
                self.emit(Event::Error {
                    context: "upload",
                    message: format!("{e:#}"),
                });
            }
        }
        // Journaled downloads restart too — confirmed_bytes resumes
        // each one mid-part. Without this a dead app meant starting over.
        let jobs: Vec<crate::journal::DownloadJob> = self
            .journal
            .downloads
            .iter()
            .filter(|j| j.channel_id == channel_id)
            .cloned()
            .collect();
        for job in jobs {
            if !job.dest.parent().map(|p| p.exists()).unwrap_or(false) {
                self.journal.downloads.retain(|j| j.file_id != job.file_id);
                continue;
            }
            let id = download_transfer_id(&job.file_id);
            let name = job.name.clone();
            if let Err(e) = self.spawn_cancellable(&id, &name, move |ctx| async move {
                Self::run_download_task(&ctx, job).await
            }) {
                self.emit(Event::Error {
                    context: "download",
                    message: format!("{e:#}"),
                });
            }
        }
        self.save_journal();
        Ok(())
    }

    pub(super) async fn upload(&mut self, path: PathBuf, parent: String) -> Result<()> {
        let plaintext_size = std::fs::metadata(&path).context("stat upload")?.len() as i64;
        // TDrive rejects empty plaintext only when unencrypted — the TDE1
        // header makes the stored ciphertext non-empty.
        if plaintext_size == 0 && self.master_key.lock().unwrap().is_none() {
            return Err(anyhow!("empty files can't be uploaded"));
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unnamed".into());
        let name = self.unique_remote_name(&parent, &name);

        // Encryption is staged inside the task — even multi-GB encrypts
        // never block the command loop.
        let encrypted = self.master_key.lock().unwrap().is_some();
        let size = plaintext_size;

        if size > LARGE_FILE_MAX_BYTES {
            return Err(anyhow!("file exceeds the maximum upload size"));
        }

        let res: Result<()> = if size <= MAX_PART_BYTES {
            let id = upload_transfer_id(&uuid::Uuid::new_v4().to_string());
            let name2 = name.clone();
            let id2 = id.clone();
            self.spawn_cancellable(&id, &name, move |ctx| async move {
                Self::upload_single_task(
                    &ctx,
                    id2,
                    path,
                    name2,
                    parent,
                    size,
                    encrypted,
                    plaintext_size,
                )
                .await
            })?;
            Ok(())
        } else {
            let channel_id = self.channel()?.id.bare_id_unchecked();
            let part_count = (size + MAX_PART_BYTES - 1) / MAX_PART_BYTES;
            if part_count as usize > MAX_PARTS {
                return Err(anyhow!("file would exceed the {MAX_PARTS} part limit"));
            }
            let parts: Vec<PartReceipt> = (0..part_count as usize)
                .map(|i| {
                    let start = i as i64 * MAX_PART_BYTES;
                    PartReceipt {
                        index: i as i32,
                        msg_id: None,
                        size: (size - start).min(MAX_PART_BYTES),
                    }
                })
                .collect();
            let job = UploadJob {
                upload_uuid: uuid::Uuid::new_v4().to_string(),
                channel_id,
                local_path: path.clone(),
                source_path: path.clone(),
                name,
                parent,
                stored_size: size,
                plaintext_size,
                encrypted,
                parts,
                manifest_msg_id: None,
                started_at: ops::unix_now(),
            };
            // TDrive only journals resumable *unencrypted* uploads —
            // encrypted multipart resume would re-encrypt under a fresh
            // salt and produce permanently corrupt output.
            if !job.encrypted {
                self.journal.uploads.push(job.clone());
                self.save_journal();
            }
            let id = upload_transfer_id(&job.upload_uuid);
            let name = job.name.clone();
            self.spawn_cancellable(&id, &name, move |ctx| async move {
                Self::run_upload_task(&ctx, job).await
            })
        };
        res
    }

    /// Upload a local directory tree: mkdir ops for each subdir (the obj
    /// id is client-generated so we know parents before files go up),
    /// then a spawned upload task per file.
    pub(super) async fn upload_dir(&mut self, dir: PathBuf, parent: String) -> Result<()> {
        if !dir.is_dir() {
            return Err(anyhow!("not a directory"));
        }
        // Walk + mkdirs + enqueues run off the command loop — otherwise
        // every folder's mkdir awaits a Telegram RTT (and flood-wait
        // sleeps) serially on the loop, starving uploads in bursts.
        // Files stream into the upload queue as they're discovered, so
        // the semaphore always has work while the walk continues.
        let ctx = self.transfer_ctx(CancellationToken::new())?;
        tokio::spawn(async move {
            if let Err(e) = Self::upload_dir_task(&ctx, dir, parent).await {
                ctx.emit(Event::Error {
                    context: "upload dir",
                    message: format!("{e:#}"),
                });
            }
        });
        Ok(())
    }

    /// Background worker for UploadDir: files stream into the upload
    /// queue the moment they're discovered — parent dirs are referenced
    /// by uuid so landing before a mkdir is self-healing. mkdir ops send
    /// concurrently (bounded) so a deep tree doesn't trickle.
    pub(super) async fn upload_dir_task(
        ctx: &TransferCtx,
        dir: PathBuf,
        parent: String,
    ) -> Result<()> {
        // A dedicated sender drains mkdir ops off a channel — files never
        // wait on them since parents are uuids the projection resolves in
        // any order.
        let (mkdir_tx, mut mkdir_rx) = mpsc::unbounded_channel::<String>();
        let ctx2 = ctx.clone();
        let sender = tokio::spawn(async move {
            while let Some(header) = mkdir_rx.recv().await {
                if let Ok(id) = ctx2.send_op(&header).await {
                    ctx2.op_applied(header, id);
                }
            }
        });
        let push_mkdir = |obj: String, parent: String, name: String| {
            let _ = mkdir_tx.send(wire::format(&ops::mkdir_with_id(obj, parent, name)));
        };

        // Create the folder itself first — contents land inside it, not
        // scattered into the current directory.
        let root_obj = wire::new_folder_id();
        let name = dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "folder".into());
        push_mkdir(root_obj.clone(), parent, name);

        let mut queue = vec![(dir, root_obj)];
        while let Some((d, pid)) = queue.pop() {
            let mut entries: Vec<_> = std::fs::read_dir(&d)?.filter_map(|e| e.ok()).collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let p = e.path();
                if p.is_dir() {
                    let obj = wire::new_folder_id();
                    push_mkdir(
                        obj.clone(),
                        pid.clone(),
                        e.file_name().to_string_lossy().to_string(),
                    );
                    queue.push((p, obj));
                } else if p.is_file() {
                    // Back to the loop immediately — it registers +
                    // spawns the transfer without waiting on mkdirs.
                    ctx.journal(Command::Upload {
                        path: p,
                        parent: pid.clone(),
                    });
                }
            }
        }
        drop(mkdir_tx);
        let _ = sender.await;
        Ok(())
    }

    /// `7z a` the folder to staging, then upload the archive.
    pub(super) async fn upload_archive(&mut self, dir: PathBuf, parent: String) -> Result<()> {
        let name = dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "archive".into());
        let out = crate::paths::staging_dir()
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(format!("{name}.7z"));
        let out2 = out.clone();
        let dir2 = dir.clone();
        let status = tokio::task::spawn_blocking(move || {
            std::process::Command::new("7z")
                .arg("a")
                .arg("-t7z")
                .arg(&out2)
                .arg(&dir2)
                .status()
        })
        .await??;
        if !status.success() {
            return Err(anyhow!("7z archive failed (exit {status})"));
        }
        self.upload(out, parent).await
    }

    pub(super) async fn upload_single_task(
        ctx: &TransferCtx,
        id: String,
        path: PathBuf,
        name: String,
        parent: String,
        size: i64,
        encrypted: bool,
        plaintext_size: i64,
    ) -> Result<()> {
        let permit = ctx.upload_sem.acquire().await;
        ctx.update(&id, &name, 0, size, TransferState::Active);
        // Encrypt to staged ciphertext first when the vault is unlocked.
        let stored = if encrypted {
            let master = ctx.remote.master_key().context("drive locked")?;
            let ct_path = crate::paths::staging_dir()
                .unwrap_or_else(|_| std::env::temp_dir())
                .join(format!("tdrive-enc-{}.bin", uuid::Uuid::new_v4()));
            let pt = path.clone();
            let ct = ct_path.clone();
            let stored_len =
                tokio::task::spawn_blocking(move || crypto::encrypt_file(&master, &pt, &ct))
                    .await??;
            Some((ct_path, stored_len as i64))
        } else {
            None
        };
        let (send_path, send_size) = stored.clone().unwrap_or((path.clone(), size));
        let res = Self::upload_single_inner(
            ctx,
            &id,
            &send_path,
            &name,
            &parent,
            send_size,
            encrypted,
            plaintext_size,
        )
        .await;
        if let Some((ct, _)) = stored {
            let _ = std::fs::remove_file(ct);
        }
        match res {
            Ok((msg_id, header)) => {
                ctx.op_applied(header, msg_id);
                ctx.update(&id, &name, size, size, TransferState::Done);
                drop(permit);
                Self::publish_renditions(ctx, &path, &name, msg_id, msg_id, "", encrypted).await;
                Ok(())
            }
            Err(e) => {
                ctx.update(&id, &name, 0, size, TransferState::Failed);
                Err(e)
            }
        }
    }

    pub(super) async fn upload_single_inner(
        ctx: &TransferCtx,
        id: &str,
        path: &PathBuf,
        name: &str,
        parent: &str,
        size: i64,
        encrypted: bool,
        plaintext_size: i64,
    ) -> Result<(i64, String)> {
        let file = tokio::fs::File::open(path).await?;
        let progress = ProgressReader::new(file, {
            let tx = ctx.tx.clone();
            let id = id.to_string();
            let label = name.to_string();
            move |sent| {
                let _ = tx.try_send(Event::Transfer(TransferUpdate {
                    id: id.clone(),
                    label: label.clone(),
                    sent,
                    total: size,
                    state: TransferState::Active,
                }));
            }
        });
        let mut reader = progress;
        let uploaded = ctx
            .remote
            .client
            .upload_stream(&mut reader, size as usize, name.to_string())
            .await?;
        let caption = wire::format(&ops::file_upload(
            parent.to_string(),
            name.to_string(),
            size,
            encrypted,
            plaintext_size,
        ));
        let msg = ctx
            .remote
            .client
            .send_message(
                ctx.peer,
                InputMessage::new().document(uploaded).text(caption.clone()),
            )
            .await?;
        let id = msg.id() as i64;
        if id <= 0 {
            anyhow::bail!("send outcome unknown (msg id 0)");
        }
        Ok((id, caption))
    }

    /// Continue (or start) a multipart journal job until the manifest lands.
    /// The upload state machine — runs on a spawned task so the command
    /// loop stays responsive; journal/namespace writes flow back via
    /// Commands on `ctx.cmd`.
    pub(super) async fn run_upload_task(ctx: &TransferCtx, mut job: UploadJob) -> Result<()> {
        let permit = ctx.upload_sem.acquire().await;
        // Encrypt to staged ciphertext when the vault is unlocked — done
        // inside the task so even a multi-GB encrypt never blocks the loop.
        if job.encrypted && (!job.local_path.exists() || job.local_path == job.source_path) {
            let master = ctx
                .remote
                .master_key()
                .context("drive locked — cannot resume encrypted upload")?;
            let ct_path = crate::paths::staging_dir()
                .unwrap_or_else(|_| std::env::temp_dir())
                .join(format!("tdrive-enc-{}.bin", uuid::Uuid::new_v4()));
            let pt = job.source_path.clone();
            let ct = ct_path.clone();
            tokio::task::spawn_blocking(move || crypto::encrypt_file(&master, &pt, &ct)).await??;
            job.local_path = ct_path.clone();
            job.stored_size = std::fs::metadata(&ct_path)
                .map(|m| m.len() as i64)
                .unwrap_or(job.stored_size);
            for (i, p) in job.parts.iter_mut().enumerate() {
                let start = i as i64 * MAX_PART_BYTES;
                p.size = (job.stored_size - start).min(MAX_PART_BYTES);
            }
            ctx.journal(Command::UploadJobPatch {
                uuid: job.upload_uuid.clone(),
                local_path: ct_path,
                manifest_msg_id: None,
            });
        }

        let token = &ctx.token;
        let transfer_id = upload_transfer_id(&job.upload_uuid);
        let already: i64 = job
            .parts
            .iter()
            .filter(|p| p.msg_id.is_some())
            .map(|p| p.size)
            .sum();
        ctx.update(
            &transfer_id,
            &job.name,
            already,
            job.stored_size,
            TransferState::Active,
        );

        let pending: Vec<i32> = job.pending_parts();
        for idx in pending {
            if token.is_cancelled() {
                return Err(anyhow!("upload cancelled"));
            }
            let part = job.parts[idx as usize].clone();
            let offset = idx as i64 * MAX_PART_BYTES;
            let msg_id = Self::send_part(ctx, &job, part.index, offset, part.size).await?;
            job.parts[idx as usize].msg_id = Some(msg_id);
            ctx.journal(Command::UploadPartConfirmed {
                uuid: job.upload_uuid.clone(),
                pix: idx,
                msg_id,
            });
            let done: i64 = job
                .parts
                .iter()
                .filter(|p| p.msg_id.is_some())
                .map(|p| p.size)
                .sum();
            ctx.update(
                &transfer_id,
                &job.name,
                done,
                job.stored_size,
                TransferState::Active,
            );
        }

        // Manifest — the file's visibility boundary.
        let manifest = ops::file_manifest(
            job.upload_uuid.clone(),
            job.parent.clone(),
            job.name.clone(),
            job.parts.len() as i32,
            job.stored_size,
            job.started_at,
            job.encrypted,
            job.plaintext_size,
        );
        let header = wire::format(&manifest);
        let msg_id = ctx.send_op(&header).await?;
        ctx.op_applied(header, msg_id);
        ctx.journal(Command::UploadJobPatch {
            uuid: job.upload_uuid.clone(),
            local_path: job.local_path.clone(),
            manifest_msg_id: Some(msg_id),
        });
        ctx.journal(Command::TransferDone {
            id: transfer_id.clone(),
        });
        ctx.update(
            &transfer_id,
            &job.name,
            job.stored_size,
            job.stored_size,
            TransferState::Done,
        );
        // Free the queue slot — renditions are best-effort background
        // sends, not part of the upload's critical path.
        drop(permit);
        Self::publish_renditions(
            ctx,
            &job.source_path,
            &job.name,
            msg_id,
            0,
            &job.upload_uuid,
            job.encrypted,
        )
        .await;
        Ok(())
    }

    pub(super) async fn send_part(
        ctx: &TransferCtx,
        job: &UploadJob,
        index: i32,
        offset: i64,
        size: i64,
    ) -> Result<i64> {
        let file = tokio::fs::File::open(&job.local_path).await?;
        let mut limited = file;
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        limited
            .seek(std::io::SeekFrom::Start(offset as u64))
            .await?;
        let take = limited.take(size as u64);
        let progress = ProgressReader::new(take, {
            let tx = ctx.tx.clone();
            let id = upload_transfer_id(&job.upload_uuid);
            let label = job.name.clone();
            let total = job.stored_size;
            let base: i64 = job
                .parts
                .iter()
                .filter(|p| p.msg_id.is_some())
                .map(|p| p.size)
                .sum();
            move |sent| {
                let _ = tx.try_send(Event::Transfer(TransferUpdate {
                    id: id.clone(),
                    label: label.clone(),
                    sent: base + sent,
                    total,
                    state: TransferState::Active,
                }));
            }
        });
        let mut reader = progress;
        let part_name = part_attachment_name(&job.name, index, job.parts.len() as i32);
        let uploaded = ctx
            .remote
            .client
            .upload_stream(&mut reader, size as usize, part_name)
            .await?;
        let caption = wire::format(&ops::file_part(job.upload_uuid.clone(), index, size));
        let msg = ctx
            .remote
            .client
            .send_message(
                ctx.peer,
                InputMessage::new().document(uploaded).text(caption),
            )
            .await?;
        let id = msg.id() as i64;
        if id <= 0 {
            anyhow::bail!("send outcome unknown (msg id 0)");
        }
        Ok(id)
    }
}

/// TDrive's `partAttachmentName`: cosmetic attachment name for part docs.
pub(super) fn part_attachment_name(original: &str, index: i32, count: i32) -> String {
    if original.is_empty() {
        return format!("part-{index:05}");
    }
    if count <= 1 {
        return original.to_string();
    }
    format!("{original}.part{index}")
}
