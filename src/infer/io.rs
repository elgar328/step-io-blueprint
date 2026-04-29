//! TOML read/write for the `inferred/` directory.
//!
//! Two files per stage:
//! - `<stage>.toml`        — confident decisions only, fed to next stage.
//! - `<stage>_pending.toml` — review + unresolved + stats. Deleted when
//!   empty so file presence acts as the "more work to do" signal.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;

use super::{Decision, Unresolved};

const INFERRED_DIR: &str = "inferred";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingStats {
    pub total: usize,
    pub confident: usize,
    pub review: usize,
    pub unresolved: usize,
}

/// On-disk shape of `<stage>_pending.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingFile<T> {
    pub stats: PendingStats,
    /// `entity_or_group_name → Decision<T>` for review entries.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub review: BTreeMap<String, Decision<T>>,
    /// `entity_or_group_name → Unresolved` for unresolved entries.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unresolved: BTreeMap<String, Unresolved>,
}

impl<T> PendingFile<T> {
    pub fn is_empty(&self) -> bool {
        self.review.is_empty() && self.unresolved.is_empty()
    }
}

fn inferred_path(filename: &str) -> PathBuf {
    Path::new(INFERRED_DIR).join(filename)
}

fn ensure_dir() -> io::Result<()> {
    fs::create_dir_all(INFERRED_DIR)
}

pub fn write_confident<V: Serialize>(
    stage_filename: &str,
    section: &str,
    decisions: &BTreeMap<String, V>,
) -> io::Result<()> {
    ensure_dir()?;
    let mut outer: BTreeMap<String, &BTreeMap<String, V>> = BTreeMap::new();
    outer.insert(section.to_string(), decisions);
    let body = toml::to_string_pretty(&outer)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(inferred_path(stage_filename), body)
}

pub fn read_confident<V: DeserializeOwned>(
    stage_filename: &str,
    section: &str,
) -> io::Result<BTreeMap<String, V>> {
    let path = inferred_path(stage_filename);
    let body = fs::read_to_string(&path)?;
    let mut outer: BTreeMap<String, BTreeMap<String, V>> = toml::from_str(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{path:?}: {e}")))?;
    Ok(outer.remove(section).unwrap_or_default())
}

pub fn write_pending<T: Serialize>(
    pending_filename: &str,
    file: &PendingFile<T>,
) -> io::Result<()> {
    let path = inferred_path(pending_filename);
    if file.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        return Ok(());
    }
    ensure_dir()?;
    let body = toml::to_string_pretty(file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(path, body)
}

/// Returns true if `<stage>_pending.toml` exists. Used by the strict gate
/// before a downstream stage starts.
pub fn pending_exists(pending_filename: &str) -> bool {
    inferred_path(pending_filename).exists()
}
