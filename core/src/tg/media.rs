//! Media pipeline — thumbnail/preview fetch and rendition publish.
use super::*;

/// TDR1 rendition envelope → bare JPEG, in place. Plain bodies pass
/// through unchanged.
fn strip_tdr1(path: &std::path::Path) -> std::io::Result<()> {
    let data = std::fs::read(path)?;
    if data.len() > 8 && &data[..4] == b"TDR1" {
        let n = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
        if 8 + n <= data.len() {
            std::fs::write(path, &data[8 + n..])?;
        }
    }
    Ok(())
}

impl Worker {
    /// Best-effort rendition publish — TDrive generates a 512px "thumb" and
    /// 1600px "preview" JPEG for image uploads, sent as `rend` part ops.
    /// Failures never invalidate the stored original.
    pub(super) async fn publish_renditions(
        ctx: &TransferCtx,
        source: &PathBuf,
        name: &str,
        file_msg_id: i64,
        content_msg_id: i64,
        upload_uuid: &str,
        encrypted: bool,
    ) {
        let owned_source;
        let source = if is_image_name(name) {
            source
        } else if is_video_name(name) {
            // TDrive renders video thumbs through ffmpeg; extract a frame
            // locally and feed the same rendition pipeline.
            match extract_video_frame(source).await {
                Some(p) => {
                    owned_source = p;
                    &owned_source
                }
                None => return,
            }
        } else {
            return;
        };
        for (kind, edge) in [
            (wire::RENDITION_THUMB, 512u32),
            (wire::RENDITION_PREVIEW, 1600u32),
        ] {
            if let Err(e) = Self::publish_rendition(
                ctx,
                source,
                file_msg_id,
                content_msg_id,
                upload_uuid,
                encrypted,
                kind,
                edge,
            )
            .await
            {
                ctx.emit(Event::Error {
                    context: "rendition",
                    message: format!("{e:#}"),
                });
            }
        }
    }

    pub(super) async fn publish_rendition(
        ctx: &TransferCtx,
        source: &PathBuf,
        file_msg_id: i64,
        content_msg_id: i64,
        upload_uuid: &str,
        encrypted: bool,
        kind: &str,
        edge: u32,
    ) -> Result<()> {
        let src = source.clone();
        let (jpeg, w, h) = tokio::task::spawn_blocking(move || {
            let img = image::open(&src).context("decode")?;
            let t = img.thumbnail(edge, edge);
            let mut cur = std::io::Cursor::new(Vec::new());
            t.write_to(&mut cur, image::ImageFormat::Jpeg)
                .context("jpeg encode")?;
            Ok::<_, anyhow::Error>((cur.into_inner(), t.width(), t.height()))
        })
        .await??;
        let pt_len = jpeg.len() as i64;
        let (body, stored) = if encrypted {
            let master = ctx.remote.master_key().context("drive locked")?;
            // TDrive wraps encrypted renditions in a TDR1 envelope binding
            // the descriptor (s=0) — Go's decryptRendition requires it.
            let desc = serde_json::json!({
                "c": ctx.peer.id.bare_id_unchecked(), "f": file_msg_id, "m": content_msg_id,
                "u": upload_uuid, "k": kind, "v": 1, "s": 0,
                "p": pt_len, "w": w, "h": h, "e": true,
            });
            let desc_bytes = serde_json::to_vec(&desc)?;
            let mut plain = Vec::with_capacity(8 + desc_bytes.len() + jpeg.len());
            plain.extend_from_slice(b"TDR1");
            plain.extend_from_slice(&(desc_bytes.len() as u32).to_be_bytes());
            plain.extend_from_slice(&desc_bytes);
            plain.extend_from_slice(&jpeg);
            let ct = tokio::task::spawn_blocking(move || crypto::encrypt_bytes(&master, &plain))
                .await??;
            let len = ct.len() as i64;
            (ct, len)
        } else {
            (jpeg, pt_len)
        };
        let rend = wire::Rendition {
            channel_id: ctx.peer.id.bare_id_unchecked(),
            file_msg_id,
            content_msg_id,
            upload_uuid: upload_uuid.to_string(),
            kind: kind.to_string(),
            version: 1,
            size: stored,
            plaintext_size: pt_len,
            width: w as i32,
            height: h as i32,
            encrypted,
        };
        let mut reader = std::io::Cursor::new(&body);
        let uploaded = ctx
            .remote
            .client
            .upload_stream(&mut reader, body.len(), "rendition.jpg".to_string())
            .await?;
        let caption = wire::format(&ops::rendition_part(stored, &rend));
        ctx.remote
            .client
            .send_message(
                ctx.peer,
                InputMessage::new().document(uploaded).text(caption),
            )
            .await?;
        Ok(())
    }

    /// Download a file's "thumb" rendition into the local cache and tell
    /// the UI where to read it.
    pub(super) fn spawn_thumbnail_fetch(&mut self, file_id: &str) -> Result<()> {
        self.spawn_rendition_fetch(file_id, FetchKind::Thumb)
    }

    /// Download a viewable rendition for the in-app viewer — "preview",
    /// then "thumbnail", then the raw body when the file is itself an image.
    pub(super) fn spawn_preview_fetch(&mut self, file_id: &str) -> Result<()> {
        self.spawn_rendition_fetch(file_id, FetchKind::Preview)
    }

    /// One rendition fetch: resolve the source body, fetch the media off
    /// the command loop (bounded by `thumb_sem`), decrypt + TDR1-strip
    /// renditions, cache by body id, emit Ready/Failed.
    fn spawn_rendition_fetch(&mut self, file_id: &str, kind: FetchKind) -> Result<()> {
        // (body msg id, is_rendition, encrypted)
        let source = match kind {
            FetchKind::Thumb => self
                .namespace
                .renditions
                .get(file_id)
                .and_then(|m| m.get(wire::RENDITION_THUMB))
                .map(|(id, r)| (*id, true, r.encrypted)),
            FetchKind::Preview => {
                let file = self.namespace.files.get(file_id).cloned();
                self.namespace
                    .renditions
                    .get(file_id)
                    .and_then(|m| {
                        m.get(wire::RENDITION_PREVIEW)
                            .or_else(|| m.get(wire::RENDITION_THUMB))
                            .map(|(id, r)| (*id, true, r.encrypted))
                    })
                    .or_else(|| {
                        file.and_then(|f| {
                            if !is_image_name(&f.name) {
                                return None;
                            }
                            Some((
                                *self.namespace.body_msg_ids(&f).first()?,
                                false,
                                f.encrypted,
                            ))
                        })
                    })
            }
        };
        let Some((body_id, is_rendition, encrypted)) = source else {
            return match kind {
                FetchKind::Thumb => {
                    self.emit(kind.failed(file_id));
                    Ok(())
                }
                FetchKind::Preview => Err(anyhow!("no previewable body")),
            };
        };
        let (dir, ext) = match kind {
            FetchKind::Thumb => ("tdrive-rs/thumbs", "bin"),
            FetchKind::Preview => ("tdrive-rs/previews", "img"),
        };
        let dir = crate::paths::staging_dir()?.join(dir);
        std::fs::create_dir_all(&dir)?;
        let out = dir.join(format!("{body_id}.{ext}"));
        if out.exists() {
            self.emit(kind.ready(file_id, out));
            return Ok(());
        }
        if encrypted && self.master_key.lock().unwrap().is_none() {
            return match kind {
                FetchKind::Thumb => Ok(()),
                FetchKind::Preview => Err(anyhow!("unlock the drive to view encrypted files")),
            };
        }
        let client = self.client()?.clone();
        let peer = self.channel()?;
        let master = self.master_key.clone();
        let sem = self.thumb_sem.clone();
        let tx = self.tx.clone();
        let file_id = file_id.to_string();
        tokio::spawn(async move {
            let _permit = sem.acquire().await;
            let key = *master.lock().unwrap();
            let ok = (async {
                let msgs = client
                    .get_messages_by_id(peer, &[body_id as i32])
                    .await
                    .ok()?;
                let media = msgs.into_iter().flatten().next()?.media()?;
                if encrypted {
                    let staging = dir.join(format!("{body_id}.part"));
                    client.download_media(&media, &staging).await.ok()?;
                    let st = staging.clone();
                    let o = out.clone();
                    tokio::task::spawn_blocking(move || -> Result<()> {
                        crypto::decrypt_file(&key.ok_or_else(|| anyhow!("locked"))?, &st, &o)?;
                        if is_rendition {
                            strip_tdr1(&o)?;
                        }
                        Ok(())
                    })
                    .await
                    .ok()?
                    .ok()?;
                    let _ = std::fs::remove_file(&staging);
                } else {
                    client.download_media(&media, &out).await.ok()?;
                }
                Some(())
            })
            .await
            .is_some();
            let _ = tx
                .send(if ok {
                    kind.ready(&file_id, out)
                } else {
                    kind.failed(&file_id)
                })
                .await;
        });
        Ok(())
    }
}

/// Which rendition surface a fetch targets — picks the source order,
/// cache dir, and the Ready/Failed events.
#[derive(Clone, Copy)]
enum FetchKind {
    Thumb,
    Preview,
}

impl FetchKind {
    fn ready(&self, file_id: &str, path: std::path::PathBuf) -> Event {
        match self {
            Self::Thumb => Event::ThumbnailReady {
                file_id: file_id.to_string(),
                path,
            },
            Self::Preview => Event::PreviewReady {
                file_id: file_id.to_string(),
                path,
            },
        }
    }

    fn failed(&self, file_id: &str) -> Event {
        match self {
            Self::Thumb => Event::ThumbnailFailed {
                file_id: file_id.to_string(),
            },
            Self::Preview => Event::PreviewFailed {
                file_id: file_id.to_string(),
            },
        }
    }
}

/// thumbnail approach — best-effort, returns None without ffmpeg).
pub(super) async fn extract_video_frame(source: &PathBuf) -> Option<PathBuf> {
    let out = std::env::temp_dir().join(format!("tdrive-vf-{}.jpg", uuid::Uuid::new_v4()));
    let src = source.clone();
    let out2 = out.clone();
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new("ffmpeg")
            .args(["-i"])
            .arg(&src)
            .args(["-vf", "thumbnail", "-frames:v", "1", "-q:v", "5", "-y"])
            .arg(&out2)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    })
    .await
    .ok()?
    .ok()?;
    (status.success() && out.exists()).then_some(out)
}

pub(crate) fn is_video_name(name: &str) -> bool {
    matches!(
        name.rsplit('.')
            .next()
            .unwrap_or("")
            .to_lowercase()
            .as_str(),
        "mp4" | "mkv" | "webm" | "mov" | "m4v" | "avi" | "ts"
    )
}

pub(crate) fn is_image_name(name: &str) -> bool {
    matches!(
        name.rsplit('.')
            .next()
            .unwrap_or("")
            .to_lowercase()
            .as_str(),
        "jpg" | "jpeg" | "png" | "webp" | "gif"
    )
}
