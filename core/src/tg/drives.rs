//! Drive lifecycle — listing, creating, opening channels and
//! shared-drive invite join/poll.
use super::*;

impl Worker {
    pub(super) async fn list_channels(&mut self) -> Result<()> {
        let client = self.client()?.clone();
        let mut dialogs = client.iter_dialogs();
        let mut out = Vec::new();
        while let Some(dialog) = dialogs.next().await? {
            // Min/incomplete entities yield None from to_ref — never call
            // Dialog::peer_ref (it unwraps and kills the worker thread).
            let (kind, title) = match dialog.peer() {
                Peer::Channel(ch) => {
                    if ch.raw.left {
                        continue;
                    }
                    match ch.kind() {
                        Some(ChannelKind::Broadcast) => {
                            (DriveKind::Personal, ch.title().to_string())
                        }
                        _ => continue,
                    }
                }
                Peer::Group(g) if g.is_megagroup() => {
                    (DriveKind::Shared, g.title().unwrap_or_default().to_string())
                }
                _ => continue,
            };
            let Ok(Some(peer_ref)) = dialog.peer().to_ref().await else {
                continue;
            };
            out.push(ChannelInfo {
                id: peer_ref.id.bare_id_unchecked(),
                access_hash: peer_ref.auth.hash(),
                title,
                kind,
            });
        }
        out.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
        self.emit(Event::Channels(out));
        Ok(())
    }

    pub(super) async fn create_channel(&mut self, title: String) -> Result<()> {
        self.create_channel_inner(title, false).await
    }

    /// `shared`: megagroup + exported invite link (TDrive shared drive).
    pub(super) async fn create_channel_inner(&mut self, title: String, shared: bool) -> Result<()> {
        let client = self.client()?.clone();
        let updates = client
            .invoke(&grammers_client::tl::functions::channels::CreateChannel {
                broadcast: !shared,
                megagroup: shared,
                for_import: false,
                forum: false,
                title: title.clone(),
                about: "TDrive storage".to_string(),
                geo_point: None,
                address: None,
                ttl_period: None,
            })
            .await?;
        let chats = chats_from(updates)?;
        for c in chats {
            self.open_channel(
                c.id,
                c.access_hash.unwrap_or(0),
                c.title,
                if shared {
                    DriveKind::Shared
                } else {
                    DriveKind::Personal
                },
            )
            .await?;
            if shared {
                self.export_invite().await?;
            }
            return Ok(());
        }
        Err(anyhow!("channel not found in create response"))
    }

    /// Export the invite link for a shared drive and surface it to the UI.
    pub(super) async fn export_invite(&mut self) -> Result<()> {
        use grammers_client::tl::enums::ExportedChatInvite;
        let invite = self
            .client()?
            .invoke(
                &grammers_client::tl::functions::messages::ExportChatInvite {
                    peer: self.channel()?.into(),
                    legacy_revoke_permanent: false,
                    request_needed: false,
                    expire_date: None,
                    usage_limit: None,
                    title: None,
                    subscription_pricing: None,
                },
            )
            .await?;
        if let ExportedChatInvite::ChatInviteExported(i) = invite {
            self.emit(Event::InviteLink { link: i.link });
        }
        Ok(())
    }

    pub(super) async fn open_channel(
        &mut self,
        id: i64,
        access_hash: i64,
        title: String,
        kind: DriveKind,
    ) -> Result<()> {
        let peer_id = PeerId::channel(id).context("bad channel id")?;
        *self.peer_cell.lock().unwrap() = Some(PeerRef {
            id: peer_id,
            auth: PeerAuth::from_hash(access_hash),
        });
        // Read-write WebDAV surface for this drive.
        if self.channel_id != id {
            // Different drive → different vault.
            *self.master_key.lock().unwrap() = None;
        }
        self.channel_id = id;
        self.channel_kind = kind;
        // Load the persisted projection so the first sync is incremental.
        self.namespace = Arc::new({
            let mut ns = crate::paths::namespace_path(id)
                .ok()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|s| serde_json::from_str::<crate::project::NamespaceSnapshot>(&s).ok())
                .map(crate::project::Namespace::restore_snapshot)
                .unwrap_or_default();
            ns.drop_zero_ids();
            ns
        });
        let pending = self.journal.resumable_uploads(id).len();
        self.emit(Event::ChannelOpened {
            info: ChannelInfo {
                id,
                access_hash,
                title,
                kind: self.channel_kind,
            },
            pending_uploads: pending,
        });
        self.sync().await?;
        self.emit_vault_state();

        // Resume interrupted downloads for this drive.
        let pending_downloads: Vec<DownloadJob> = self
            .journal
            .downloads
            .iter()
            .filter(|j| j.channel_id == id)
            .cloned()
            .collect();
        for job in pending_downloads {
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
        Ok(())
    }

    pub(super) async fn join_invite(&mut self, link: &str) -> Result<()> {
        // t.me/+hash or t.me/joinchat/hash
        let hash = link
            .trim()
            .rsplit('/')
            .next()
            .unwrap_or("")
            .trim_start_matches('+')
            .to_string();
        if hash.is_empty() {
            return Err(anyhow!("not an invite link"));
        }
        let client = self.client()?.clone();
        let result = client
            .invoke(
                &grammers_client::tl::functions::messages::ImportChatInvite { hash: hash.clone() },
            )
            .await;
        use grammers_client::tl::enums::messages::ChatInviteJoinResult;
        let ok = match result {
            Ok(ChatInviteJoinResult::Ok(ok)) => ok,
            Err(e) if format!("{e}").contains("INVITE_REQUEST_SENT") => {
                // Join-request links: poll CheckChatInvite off the command
                // loop — a 120s inline wait would starve every other op.
                // Approval comes back as Command::JoinApproved.
                let client = self.client()?.clone();
                let cmd = self.cmd_tx.clone();
                let tx = self.tx.clone();
                let hash = hash.clone();
                let _ = tx.try_send(Event::Error {
                    context: "join",
                    message: "waiting for admin approval…".to_string(),
                });
                tokio::spawn(async move {
                    use grammers_client::tl::enums::{Chat, ChatInvite};
                    for _ in 0..24 {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        let inv = client
                            .invoke(&grammers_client::tl::functions::messages::CheckChatInvite {
                                hash: hash.clone(),
                            })
                            .await;
                        if let Ok(ChatInvite::Already(already)) = inv {
                            if let Chat::Channel(c) = already.chat {
                                let _ = cmd.send(Command::JoinApproved {
                                    id: c.id,
                                    access_hash: c.access_hash.unwrap_or(0),
                                    title: c.title,
                                    kind: if c.megagroup {
                                        DriveKind::Shared
                                    } else {
                                        DriveKind::Personal
                                    },
                                });
                                return;
                            }
                        }
                    }
                    let _ = tx.try_send(Event::Error {
                        context: "join",
                        message: "join request still pending".to_string(),
                    });
                });
                return Ok(());
            }
            _ => return Err(anyhow!("invite requires an interactive join")),
        };
        for c in chats_from(ok.updates)? {
            self.open_channel(
                c.id,
                c.access_hash.unwrap_or(0),
                c.title,
                if c.megagroup {
                    DriveKind::Shared
                } else {
                    DriveKind::Personal
                },
            )
            .await?;
            self.list_channels().await?;
            return Ok(());
        }
        Err(anyhow!("invite did not resolve to a channel"))
    }
}

/// Pull the channel list out of an updates envelope and keep only real
/// channels — both create and import land the drive the same way.
fn chats_from(
    updates: grammers_client::tl::enums::Updates,
) -> Result<Vec<grammers_client::tl::types::Channel>> {
    use grammers_client::tl::enums::{Chat, Updates};
    let chats = match updates {
        Updates::Combined(u) => u.chats,
        Updates::Updates(u) => u.chats,
        _ => anyhow::bail!("unexpected updates response"),
    };
    Ok(chats
        .into_iter()
        .filter_map(|c| match c {
            Chat::Channel(c) => Some(c),
            _ => None,
        })
        .collect())
}
