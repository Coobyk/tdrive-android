//! Desktop surfaces (FUSE/DAV/stream) are absent on Android — VfsOp
//! writes arrive only if a future SAF-backed file provider needs them.
use super::*;

impl Worker {
    pub(super) async fn vfs_op(&mut self, op: VfsOp) -> Result<()> {
        let _ = op;
        Err(anyhow!("mounts are not supported on Android"))
    }
}
