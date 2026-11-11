//! Android path + config surface — replaces the desktop `config.rs`.
//! The Kotlin side passes the app's files dir at init; everything else
//! derives from it.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn init(dir: PathBuf) -> Result<()> {
    std::fs::create_dir_all(&dir)?;
    DATA_DIR.set(dir).map_err(|_| anyhow::anyhow!("data dir already set"))
}

fn data_dir() -> Result<PathBuf> {
    DATA_DIR
        .get()
        .cloned()
        .context("tdrive core not initialised — call init() first")
}

#[derive(Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub upload_limit: usize,
    #[serde(default)]
    pub download_dir: Option<PathBuf>,
}

pub fn default_upload_limit() -> usize {
    3
}

pub const MAX_UPLOAD_LIMIT: usize = 16;

impl AppConfig {
    pub fn path() -> Result<PathBuf> {
        Ok(data_dir()?.join("config.json"))
    }
    pub fn load() -> Result<Option<Self>> {
        let path = Self::path()?;
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn save(&self) -> Result<()> {
        atomic_json_write(&Self::path()?, self)
    }
}

pub fn atomic_json_write<T: serde::Serialize>(path: &std::path::Path, v: &T) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string(v)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn session_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("session.bin"))
}

pub fn namespace_path(channel_id: i64) -> Result<PathBuf> {
    Ok(data_dir()?.join(format!("namespace-{channel_id}.json")))
}

pub fn journal_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("journal.json"))
}

pub fn staging_dir() -> Result<PathBuf> {
    let d = data_dir()?.join("staging");
    std::fs::create_dir_all(&d)?;
    Ok(d)
}
