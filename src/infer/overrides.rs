//! `<stage>_overrides.toml` loader and merger.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverrideFile<T> {
    /// Stage-specific top-level table (`entity`, `group`, or `arena`).
    /// Keyed by unit name (entity, group, arena).
    #[serde(default = "BTreeMap::new")]
    pub entity: BTreeMap<String, T>,
    #[serde(default = "BTreeMap::new")]
    pub group: BTreeMap<String, T>,
    #[serde(default = "BTreeMap::new")]
    pub arena: BTreeMap<String, T>,
}

impl<T> Default for OverrideFile<T> {
    fn default() -> Self {
        Self {
            entity: BTreeMap::new(),
            group: BTreeMap::new(),
            arena: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
pub struct OverrideError {
    pub messages: Vec<String>,
}

impl std::fmt::Display for OverrideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for m in &self.messages {
            writeln!(f, "{m}")?;
        }
        Ok(())
    }
}

impl std::error::Error for OverrideError {}

pub fn load<T: DeserializeOwned>(filename: &str) -> Result<OverrideFile<T>, String> {
    let path = Path::new("inferred").join(filename);
    if !path.exists() {
        return Ok(OverrideFile::default());
    }
    let body = fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    toml::from_str(&body).map_err(|e| format!("parse {path:?}: {e}"))
}

/// Validate the override file against the universe of known unit names.
/// Returns errors for stale references (entity/group/arena keys not
/// present in `known`).
pub fn validate_known<T>(
    over: &OverrideFile<T>,
    section: &str,
    known: &BTreeSet<String>,
    filename: &str,
) -> Vec<String> {
    let mut errs = Vec::new();
    let table: &BTreeMap<String, T> = match section {
        "entity" => &over.entity,
        "group" => &over.group,
        "arena" => &over.arena,
        _ => return errs,
    };
    for key in table.keys() {
        if !known.contains(key) {
            errs.push(format!(
                "{filename}: [{section}.{key}] references unknown {section} (not present in any schema). Remove or fix the key."
            ));
        }
    }
    errs
}
