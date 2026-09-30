use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{ir::FileIr, link::ImportResolution};

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
pub const FRONTEND_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+oxc-0.152.0.flow-ir-7");

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub frontend_version: String,
    pub snapshot_id: String,
    pub project_name: String,
    pub config_hash: String,
    pub files: Vec<FileIr>,
    pub resolutions: Vec<ImportResolution>,
}

impl Snapshot {
    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
    }

    pub fn read(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let snapshot: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to decode {}", path.display()))?;
        Ok(snapshot)
    }
}
