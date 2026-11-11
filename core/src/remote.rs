//! The live drive connection shared by every surface — WebDAV, FUSE, the
//! streaming server and background transfer tasks all used to thread the
//! same (client, peer, master, namespace) tuple by hand. `Remote` is that
//! tuple as a type: the cells are updated on channel switch and vault
//! lock/unlock, so mounted surfaces always follow the current drive.
//!
//! Transfer tasks snapshot `peer` at spawn instead — an upload finishes
//! on the drive it started on.

use crate::crypto::{self, MASTER_KEY_LEN};
use crate::project::Namespace;
use anyhow::{Context, Result};
use grammers_client::Client;
use grammers_client::message::InputMessage;
use grammers_client::session::types::PeerRef;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Remote {
    pub client: Client,
    peer: Arc<Mutex<Option<PeerRef>>>,
    master: Arc<Mutex<Option<[u8; MASTER_KEY_LEN]>>>,
    ns: Arc<Mutex<Arc<Namespace>>>,
}

/// Telegram download granularity — resume offsets and range reads must
/// agree on the chunk size or skip math drifts.
pub const BODY_CHUNK: i32 = 512 * 1024;

impl Remote {
    pub fn new(
        client: Client,
        peer: Arc<Mutex<Option<PeerRef>>>,
        master: Arc<Mutex<Option<[u8; MASTER_KEY_LEN]>>>,
        ns: Arc<Mutex<Arc<Namespace>>>,
    ) -> Self {
        Self {
            client,
            peer,
            master,
            ns,
        }
    }

    /// Current drive peer.
    pub fn peer(&self) -> Result<PeerRef> {
        self.peer.lock().unwrap().context("no drive open")
    }

    pub fn master_key(&self) -> Option<[u8; MASTER_KEY_LEN]> {
        *self.master.lock().unwrap()
    }

    pub fn namespace(&self) -> Arc<Namespace> {
        self.ns.lock().unwrap().clone()
    }

    /// Send a TDX1 op to `peer` and return the message id, recovering it
    /// by scan when grammers reports `Empty{id:0}`.
    pub async fn send_op(&self, peer: PeerRef, header: &str) -> Result<i64> {
        let msg = self
            .client
            .send_message(peer, InputMessage::new().text(header))
            .await?;
        let id = msg.id() as i64;
        if id > 0 {
            return Ok(id);
        }
        Ok(recover_msg_id(&self.client, peer, header).await)
    }

    /// Send a file body (uploaded bytes + TDX1 caption) and return its
    /// message id, recovering it by caption scan when grammers reports
    /// `Empty{id:0}`. Single-body files and parts both bind their commit
    /// op to this id — a 0 here would corrupt the commit.
    pub async fn send_doc_op(
        &self,
        peer: PeerRef,
        uploaded: grammers_client::media::Uploaded,
        caption: &str,
    ) -> Result<i64> {
        let msg = self
            .client
            .send_message(
                peer,
                InputMessage::new()
                    .document(uploaded)
                    .text(caption.to_string()),
            )
            .await?;
        let id = msg.id() as i64;
        if id > 0 {
            return Ok(id);
        }
        Ok(recover_msg_id(&self.client, peer, caption).await)
    }

    /// Delete body messages, honoring Telegram's 100-ids-per-call cap.
    pub async fn delete_bodies(&self, ids: &[i64]) -> Result<()> {
        let peer = self.peer()?;
        for chunk in ids.chunks(100) {
            let ids: Vec<i32> = chunk.iter().map(|&m| m as i32).collect();
            self.client.delete_messages(peer, &ids).await?;
        }
        Ok(())
    }

    /// Assemble plaintext `[start, end)` from the stored body parts
    /// (`(msg_id, stored size)` pairs in read order).
    pub async fn read_range(
        &self,
        parts: &[(i64, i64)],
        start: u64,
        end: u64,
        encrypted: bool,
    ) -> Result<Vec<u8>> {
        let peer = self.peer()?;
        if encrypted {
            let key = self.master_key().context("drive locked")?;
            // The TDE1 header sits at the start of the stored stream.
            let header = self
                .stored_read(&peer, parts, 0, crypto::HEADER_LEN as u64)
                .await?;
            let dec = crypto::RangeDecryptor::new(&key, &header)?;
            let (cs, ce) = dec.ct_range(start, end);
            let ct = self.stored_read(&peer, parts, cs, ce).await?;
            return dec.decrypt_range(start, end, &ct);
        }
        self.stored_read(&peer, parts, start, end).await
    }

    /// Concatenate `[start, end)` from the stored stream spanning parts.
    async fn stored_read(
        &self,
        peer: &PeerRef,
        parts: &[(i64, i64)],
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut base = 0u64;
        for (msg_id, size) in parts {
            let lo = base;
            let hi = base + *size as u64;
            base = hi;
            if start >= hi || end <= lo {
                continue;
            }
            let seg = self
                .part_read(peer, *msg_id, start.max(lo) - lo, end.min(hi) - lo)
                .await?;
            out.extend_from_slice(&seg);
        }
        Ok(out)
    }

    /// Fetch `[start, end)` bytes of one stored message body via
    /// chunk-aligned downloads (`skip_chunks` only skips whole chunks).
    async fn part_read(
        &self,
        peer: &PeerRef,
        msg_id: i64,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>> {
        const CHUNK: u64 = BODY_CHUNK as u64;
        let msgs = self
            .client
            .get_messages_by_id(*peer, &[msg_id as i32])
            .await?;
        let media = msgs
            .into_iter()
            .flatten()
            .next()
            .and_then(|m| m.media())
            .context("body missing")?;
        let skip = (start / CHUNK) as i32;
        let mut it = self
            .client
            .iter_download(&media)
            .chunk_size(CHUNK as i32)
            .skip_chunks(skip);
        let mut buf = Vec::new();
        while let Some(b) = it.next().await? {
            buf.extend_from_slice(&b);
            if skip as u64 * CHUNK + buf.len() as u64 >= end {
                break;
            }
        }
        let rel = (start % CHUNK) as usize;
        let take = (end - start) as usize;
        // A body shorter than its declared size must not panic — the
        // slice would otherwise kill the FUSE dispatch thread.
        if rel >= buf.len() {
            return Ok(Vec::new());
        }
        Ok(buf[rel..(rel + take).min(buf.len())].to_vec())
    }
}

/// Recover the real message id after `send_message` reports `Empty{id:0}`
/// — the op reached the channel, so the newest message carrying our exact
/// text is the one just sent. Returns 0 when recovery fails.
async fn recover_msg_id(client: &Client, peer: PeerRef, header: &str) -> i64 {
    let mut iter = client.iter_messages(peer);
    for _ in 0..20 {
        let m = match iter.next().await {
            Ok(Some(m)) => m,
            _ => break,
        };
        // Empty caption can't be matched on text — the newest message is
        // the one just sent.
        if header.is_empty() || m.text() == header {
            return m.id() as i64;
        }
    }
    0
}
