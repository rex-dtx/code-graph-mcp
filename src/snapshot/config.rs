//! `.code-graph.toml` parsing.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, Default)]
pub struct CodeGraphConfig {
    #[serde(default)]
    pub snapshot: SnapshotConfig,
}

#[derive(Debug, Deserialize, Default)]
pub struct SnapshotConfig {
    pub url: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

pub fn load_config(root: &Path) -> Result<CodeGraphConfig> {
    let path = root.join(".code-graph.toml");
    if !path.exists() {
        return Ok(CodeGraphConfig::default());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
}
