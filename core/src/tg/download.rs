//! Download engine — chunked body fetch and journal-backed resume.
use super::*;
use crate::remote::BODY_CHUNK;

/// The in-flight staging path for `dest` — the `.tdrive-part` suffix
/// marks a partial body; cancel cleans it up, success renames it away.
pub(super) fn staging_path(dest: &std::path::Path) -> std::path::PathBuf {
    dest.with_extension(format!(
        "{}.tdrive-part",
        dest.extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default()
    ))
}

impl Worker {
    pub(super) async fn download(&mut self, file_id: String, dest_dir: PathBuf) -> Result<()> {
        let file = self
            .namespace
            .files
            .get(&file_id)
            .cloned()
            .context("file not found")?;
        if file.encrypted && self.master_key.lock().unwrap().is_none() {
            return Err(anyhow!("drive is locked — unlock it to download"));
        }
        let body_ids = self.namespace.body_msg_ids(&file);
        if body_ids.is_empty() {
            return Err(anyhow!("file body messages are missing"));
        }
        let channel_id = self.channel()?.id.bare_id_unchecked();
        let dest = if dest_dir.is_dir() {
            unique_dest(dest_dir.join(&file.name))
        } else {
            dest_dir
        };
        let job = DownloadJob {
            channel_id,
            file_id: file.id.clone(),
            name: file.name.clone(),
            body_msg_ids: body_ids,
            total_size: file.size,
            dest,
            confirmed_bytes: 0,
            encrypted: file.encrypted,
        };
        let id = download_transfer_id(&job.file_id);
        self.journal.downloads.push(job.clone());
        self.save_journal();
        let name = job.name.clone();
        self.spawn_cancellable(&id, &name, move |ctx| async move {
            Self::run_download_task(&ctx, job).await
        })?;
        Ok(())
    }

    /// Download state machine — spawned task; journal/namespace writes
    /// flow back through Commands on `ctx.cmd`.
    pub(super) async fn run_download_task(ctx: &TransferCtx, job: DownloadJob) -> Result<()> {
        let client = ctx.remote.client.clone();
        let peer = ctx.peer;
        let transfer_id = download_transfer_id(&job.file_id);
        ctx.update(
            &transfer_id,
            &job.name,
            job.confirmed_bytes,
            job.total_size,
            TransferState::Active,
        );

        let staging = staging_path(&job.dest);
        let mut out = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&staging)
            .await?;
        use tokio::io::{AsyncSeekExt, AsyncWriteExt};

        // Fetch every body message once to learn each part's size and
        // assemble byte offsets — needed to map confirmed_bytes to a
        // (part, chunk) resume point.
        let ids: Vec<i32> = job.body_msg_ids.iter().map(|&id| id as i32).collect();
        let msgs = client.get_messages_by_id(peer, &ids).await?;
        let mut parts: Vec<(i64, Media)> = Vec::with_capacity(msgs.len()); // (offset, media)
        let mut offset = 0i64;
        for (m, &msg_id) in msgs.iter().zip(&job.body_msg_ids) {
            let Some(msg) = m else {
                return Err(anyhow!("body message {msg_id} is gone"));
            };
            let Some(media) = msg.media() else {
                return Err(anyhow!("body message {msg_id} has no media"));
            };
            parts.push((offset, media));
            offset += media_size(&parts.last().unwrap().1) as i64;
        }

        let token = &ctx.token;
        let mut written = job.confirmed_bytes;
        for (part_offset, media) in parts {
            let doc_size = media_size(&media) as i64;
            let part_end = part_offset + doc_size;
            if written >= part_end {
                continue; // this part is fully confirmed
            }
            // Resume mid-part at a whole-chunk boundary; bytes past the last
            // confirmed chunk are re-fetched.
            let within = (written - part_offset).max(0);
            let skip = (within / BODY_CHUNK as i64) as i32;
            written = part_offset + skip as i64 * BODY_CHUNK as i64;
            out.set_len(written as u64).await?;
            out.seek(std::io::SeekFrom::Start(written as u64)).await?;
            ctx.journal(Command::DlConfirmed {
                file_id: job.file_id.clone(),
                written,
            });

            let mut dl = client.iter_download(&media).chunk_size(BODY_CHUNK);
            if skip > 0 {
                dl = dl.skip_chunks(skip);
            }
            let mut since_ckpt = 0u32;
            while let Some(chunk) = dl.next().await? {
                if token.is_cancelled() {
                    ctx.journal(Command::DlConfirmed {
                        file_id: job.file_id.clone(),
                        written,
                    });
                    return Err(anyhow!("download cancelled"));
                }
                out.write_all(&chunk).await?;
                written += chunk.len() as i64;
                since_ckpt += 1;
                if since_ckpt >= 8 {
                    // TDrive checkpoints every 8 blocks — per-chunk fsync +
                    // full-journal rewrite is pathologically slow.
                    since_ckpt = 0;
                    out.sync_data().await?;
                    ctx.journal(Command::DlConfirmed {
                        file_id: job.file_id.clone(),
                        written,
                    });
                    ctx.update(
                        &transfer_id,
                        &job.name,
                        written,
                        job.total_size,
                        TransferState::Active,
                    );
                }
            }
            out.sync_data().await?;
        }
        drop(out);
        if job.encrypted {
            let master = ctx
                .remote
                .master_key()
                .context("drive locked mid-download")?;
            let staging2 = staging.clone();
            let dest2 = job.dest.clone();
            tokio::task::spawn_blocking(move || crypto::decrypt_file(&master, &staging2, &dest2))
                .await??;
            let _ = std::fs::remove_file(&staging);
        } else {
            std::fs::rename(&staging, &job.dest)?;
        }
        ctx.journal(Command::TransferDone {
            id: transfer_id.clone(),
        });
        ctx.update(
            &transfer_id,
            &job.name,
            written,
            job.total_size,
            TransferState::Done,
        );
        Ok(())
    }
}

fn unique_dest(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let parent = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    for i in 1..1000 {
        let cand = parent.join(format!("{stem} ({i}){ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    path
}
