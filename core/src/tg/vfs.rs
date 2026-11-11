//! Namespace mutations — every user-visible write op TDrive emits,
//! folded through `send_and_project` so local state matches the wire.
use super::*;

impl Worker {
    pub(super) async fn mkdir(&mut self, parent: String, name: String) -> Result<()> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(anyhow!("folder name required"));
        }
        let op = ops::mkdir(parent, name);
        self.send_and_project(&op).await
    }

    pub(super) async fn rename(&mut self, obj: String, name: String) -> Result<()> {
        let op = ops::rename(obj, name);
        self.send_and_project(&op).await
    }

    pub(super) async fn move_obj(&mut self, obj: String, parent: String) -> Result<()> {
        let op = ops::move_obj(obj, parent);
        self.send_and_project(&op).await
    }

    /// Move a live object to the trash — same `trash` op TDrive emits, with
    /// the deterministic op id and 30-day retention window.
    pub(super) async fn delete(&mut self, obj: String) -> Result<()> {
        let Some(d) = self.namespace.dirent(&obj) else {
            return Err(anyhow!("no such object"));
        };
        let (revision, tombstoned) = (d.revision, d.tombstoned);
        if tombstoned {
            return Err(anyhow!("item is already in the trash"));
        }
        let op = ops::trash(obj, revision);
        self.send_and_project(&op).await
    }

    /// Permanently purge a trashed object: `harddel` marker plus physical
    /// deletion of the file's Telegram body messages.
    pub(super) async fn purge(&mut self, obj: String) -> Result<()> {
        let Some(d) = self.namespace.dirent(&obj) else {
            return Err(anyhow!("no such object"));
        };
        let (revision, tombstoned) = (d.revision, d.tombstoned);
        if !tombstoned {
            return Err(anyhow!("only trashed items can be purged"));
        }
        // Collect body messages before the marker lands — purging a
        // folder must delete descendant media too; tombstoning alone
        // only hides them.
        let bodies = self.namespace.delete_body_msg_ids(&obj);
        let op = ops::hard_delete(obj, revision);
        self.send_and_project(&op).await?;
        if let Err(e) = self.remote()?.delete_bodies(&bodies).await {
            self.emit(Event::Error {
                context: "purge",
                message: format!("body delete: {e:#}"),
            });
        }
        Ok(())
    }

    pub(super) async fn empty_trash(&mut self) -> Result<()> {
        let ids: Vec<String> = self.namespace.trash.keys().cloned().collect();
        for obj in ids {
            if let Err(e) = self.purge(obj.clone()).await {
                self.emit(Event::Error {
                    context: "empty trash",
                    message: format!("{obj}: {e:#}"),
                });
            }
        }
        Ok(())
    }

    /// Restore from the trash — TDrive's rule: land at the original parent
    /// (root if it no longer exists) under a conflict-free sibling name.
    pub(super) async fn restore(&mut self, obj: String) -> Result<()> {
        let entry = self
            .namespace
            .trash
            .get(&obj)
            .cloned()
            .context("item is not in the trash")?;
        let Some(d) = self.namespace.dirent(&obj) else {
            return Err(anyhow!("no such object"));
        };
        let (revision, tombstoned) = (d.revision, d.tombstoned);
        if !tombstoned {
            return Err(anyhow!("item is not in the trash"));
        }
        let parent = if !entry.original_parent.is_empty()
            && self
                .namespace
                .folders
                .get(&entry.original_parent)
                .map(|d| !d.tombstoned)
                .unwrap_or(false)
        {
            entry.original_parent.clone()
        } else {
            String::new()
        };
        let name =
            self.namespace
                .next_free_sibling_name(&parent, &entry.original_name, entry.is_folder);
        let op = ops::restore(obj, parent, name, revision);
        self.send_and_project(&op).await
    }

    /// Adopt a raw channel attachment into the namespace with a `meta` op —
    /// the same op TDrive emits when claiming an unop'd document.
    pub(super) async fn adopt(&mut self, msg_id: i64, parent: String) -> Result<()> {
        let e = self
            .namespace
            .unadopted
            .iter()
            .find(|e| e.msg_id == msg_id)
            .cloned()
            .context("no adoptable attachment with that id")?;
        let (name, size, sender, date) = (e.name, e.size, e.sender_id, e.date);
        // TDrive's rule: only the uploader can manage a raw attachment.
        if self.actor_id != 0 && sender != self.actor_id {
            return Err(anyhow!("only the uploader can manage this file"));
        }
        let op = ops::meta(msg_id, parent, name, size, date);
        self.send_and_project(&op).await
    }
}
