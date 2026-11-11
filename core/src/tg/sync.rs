//! Namespace sync — full channel scan, watermark, live-sync debounce.
use super::*;
use crate::wire::{OpType, OpTypeOrUnknown};
use std::collections::{HashMap, HashSet};

impl Worker {
    /// History scan of the open drive. A full scan runs when the namespace
    /// is empty; later refreshes apply only messages past the watermark.
    pub(super) async fn sync(&mut self) -> Result<()> {
        self.emit(Event::SyncState { active: true });
        let r = self.sync_inner().await;
        self.emit(Event::SyncState { active: false });
        r
    }

    pub(super) async fn sync_inner(&mut self) -> Result<()> {
        let client = self.client()?.clone();
        let peer = self.channel()?;
        let watermark = self.namespace.last_msg_id;
        let mut ns = (*self.namespace).clone();
        // Incremental when the watermark exists (offset past the last
        // seen id); a zero watermark means empty-or-unsynced → full scan.
        let mut iter = client.iter_messages(peer).reverse(true);
        if watermark > 0 {
            iter = client
                .iter_messages(peer)
                .offset_id(watermark as i32)
                .reverse(true);
        }
        while let Some(msg) = iter.next().await? {
            if !ns.apply_message(msg.id() as i64, msg.text()) {
                if let Some(Media::Document(d)) = msg.media() {
                    ns.record_unadopted(
                        msg.id() as i64,
                        d.name().unwrap_or("unnamed").to_string(),
                        d.size().unwrap_or(0) as i64,
                        msg.sender_id().map(|p| p.bare_id_unchecked()).unwrap_or(0),
                        msg.date().as_second(),
                    );
                }
            }
        }
        self.namespace = Arc::new(ns);
        self.save_namespace();
        self.push_namespace();

        // Retention sweep: physically purge entries past purge_after, like
        // TDrive's expired-trash sweep. Failures are surfaced, not fatal.
        let now = ops::unix_now();
        let expired: Vec<String> = self
            .namespace
            .trash
            .values()
            .filter(|e| e.purge_after > 0 && e.purge_after <= now)
            .map(|e| e.object_id.clone())
            .collect();
        for obj in expired {
            if let Err(e) = self.purge(obj).await {
                self.emit(Event::Error {
                    context: "trash purge",
                    message: format!("{e:#}"),
                });
            }
        }
        Ok(())
    }

    /// Debounced incremental refresh triggered by incoming updates.
    pub(super) async fn live_sync(&mut self) -> Result<()> {
        self.live_sync_queued
            .store(false, std::sync::atomic::Ordering::Relaxed);
        // A zero watermark just means empty-or-unsynced — full scan is the
        // correct response, not a permanent wedge.
        if self.peer_cell.lock().unwrap().is_none() {
            return Ok(());
        }
        let now = std::time::Instant::now();
        if now.duration_since(self.last_live_sync) < std::time::Duration::from_millis(1500) {
            return Ok(());
        }
        self.last_live_sync = now;
        self.sync().await
    }
    /// Scan the whole op log and stage a deletion plan for messages that
    /// can no longer affect the projection: ops on hard-deleted objects
    /// (the object's whole lifecycle — commit, parts, trash, harddel —
    /// is dead) plus the body messages they bound. Live objects' ops are
    /// never eligible, so replaying what remains projects identically.
    pub(super) async fn optimize_scan(&mut self) -> Result<()> {
        let client = self.client()?.clone();
        let peer = self.channel()?;
        self.emit(Event::Status {
            message: "optimizing — scanning the op log…".into(),
        });
        let mut dead_seeds: HashSet<String> = HashSet::new();
        let mut parent_of: HashMap<String, String> = HashMap::new();
        // (msg_id, obj, bound body id)
        let mut op_msgs: Vec<(i64, String, i64)> = Vec::new();
        let mut scanned = 0usize;
        let mut iter = client.iter_messages(peer).reverse(true);
        while let Some(msg) = iter.next().await? {
            scanned += 1;
            let Ok(op) = wire::parse(msg.text()) else {
                continue;
            };
            let obj = op.obj.clone();
            if !op.parent.is_empty() {
                parent_of.insert(obj.clone(), op.parent.clone());
            }
            if op.op_type == OpTypeOrUnknown::Known(OpType::HardDeleteTree) {
                dead_seeds.insert(obj.clone());
            }
            let body = if op.content_msg_id > 0 {
                op.content_msg_id
            } else {
                0
            };
            op_msgs.push((msg.id() as i64, obj, body));
        }
        // Descendants of a dead folder are dead too — a folder's harddel
        // removes its whole subtree, so every op under it is dead weight.
        let mut dead = dead_seeds;
        loop {
            let grew = dead.len();
            let add: Vec<String> = parent_of
                .iter()
                .filter(|(obj, parent)| !dead.contains(*obj) && dead.contains(*parent))
                .map(|(obj, _)| obj.clone())
                .collect();
            dead.extend(add);
            if dead.len() == grew {
                break;
            }
        }
        let mut ids: Vec<i64> = Vec::new();
        for (mid, obj, body) in op_msgs {
            if dead.contains(&obj) {
                ids.push(mid);
                if body > 0 {
                    ids.push(body);
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        let dead = ids.len();
        self.pending_optimize = ids;
        self.emit(Event::OptimizePlan { dead, scanned });
        Ok(())
    }

    /// Delete the staged plan in batches of 100 (Telegram's per-call cap).
    pub(super) async fn optimize_apply(&mut self) -> Result<()> {
        let ids = std::mem::take(&mut self.pending_optimize);
        if ids.is_empty() {
            return Err(anyhow!("nothing staged — run optimize first"));
        }
        let remote = self.remote()?;
        let total = ids.len();
        for (done, chunk) in ids.chunks(100).enumerate() {
            remote.delete_bodies(chunk).await?;
            self.emit(Event::Status {
                message: format!(
                    "optimizing — deleted {}/{}",
                    done * 100 + chunk.len(),
                    total
                ),
            });
        }
        self.emit(Event::Status {
            message: format!("optimized — removed {total} obsolete messages"),
        });
        Ok(())
    }
}
