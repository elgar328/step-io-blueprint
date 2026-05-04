//! Pool grouping (manual input + validation).
//!
//! Pure validation: compares the arena set in `arenas_pruned.toml`
//! against the entries in `pools.toml`. Missing required entries → Err
//! stops the run; extra entries → warning, ignored. No output file —
//! the input file itself is the step-io codegen input.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::infer::arena::ArenaSpec;
use crate::infer::Decision;

const VARIANTS_PENDING: &str = "variants_pending.toml";
const ARENAS_PENDING: &str = "arenas_pending.toml";
const FILE_ARENAS_PRUNED: &str = "arenas_pruned.toml";
const FILE_POOLS: &str = "pools.toml";

/// One entry in `pools.toml`. The user writes `[arena.<name>] pool =
/// "..."`; this struct deserializes that table form into the inner
/// pool name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PoolEntry {
    pool: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PoolsFile {
    #[serde(default)]
    arena: BTreeMap<String, PoolEntry>,
}

pub fn run(allow_pending: bool) -> Result<(), String> {
    if !allow_pending && crate::infer::io::pending_exists(VARIANTS_PENDING) {
        return Err(format!(
            "{VARIANTS_PENDING} exists — variant stage has unresolved items.\n\
             Resolve in variants_overrides.toml or pass --allow-pending."
        ));
    }
    if !allow_pending && crate::infer::io::pending_exists(ARENAS_PENDING) {
        return Err(format!(
            "{ARENAS_PENDING} exists — arena stage has unresolved/review items.\n\
             Resolve in arenas_overrides.toml or pass --allow-pending."
        ));
    }

    let arenas: BTreeMap<String, Decision<ArenaSpec>> =
        crate::infer::io::read_confident(FILE_ARENAS_PRUNED, "group")
            .map_err(|e| format!("read {FILE_ARENAS_PRUNED}: {e}"))?;
    if arenas.is_empty() {
        return Err(format!(
            "{FILE_ARENAS_PRUNED} is empty or missing — run `infer prune --corpus <path>` first."
        ));
    }
    let required: BTreeSet<String> = arenas.values().map(|d| d.data.arena.clone()).collect();

    let path = Path::new("inferred").join(FILE_POOLS);
    if !path.exists() {
        return Err(missing_file_message(&required));
    }
    let body = fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    let file: PoolsFile =
        toml::from_str(&body).map_err(|e| format!("parse {path:?}: {e}"))?;

    match validate(&required, &file.arena) {
        Validation::Ok {
            distinct_pools,
            extras,
        } => {
            for e in &extras {
                eprintln!(
                    "warning: {FILE_POOLS} [arena.{e}] is not an arena in {FILE_ARENAS_PRUNED} — ignored"
                );
            }
            eprintln!(
                "infer pool: {} arenas grouped into {distinct_pools} distinct pools",
                required.len()
            );
            Ok(())
        }
        Validation::Missing(missing) => Err(missing_entries_message(&missing)),
    }
}

#[derive(Debug)]
enum Validation {
    Ok {
        distinct_pools: usize,
        extras: Vec<String>,
    },
    Missing(Vec<String>),
}

fn validate(
    required: &BTreeSet<String>,
    provided: &BTreeMap<String, PoolEntry>,
) -> Validation {
    let provided_keys: BTreeSet<&String> = provided.keys().collect();
    let required_refs: BTreeSet<&String> = required.iter().collect();

    let missing: Vec<String> = required_refs
        .difference(&provided_keys)
        .map(|s| (*s).clone())
        .collect();
    if !missing.is_empty() {
        return Validation::Missing(missing);
    }

    let extras: Vec<String> = provided_keys
        .difference(&required_refs)
        .map(|s| (*s).clone())
        .collect();

    let pool_set: BTreeSet<&String> = provided
        .iter()
        .filter(|(k, _)| required.contains(*k))
        .map(|(_, v)| &v.pool)
        .collect();

    Validation::Ok {
        distinct_pools: pool_set.len(),
        extras,
    }
}

fn missing_file_message(required: &BTreeSet<String>) -> String {
    let list = required
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n  ");
    format!(
        "{FILE_POOLS} not found — required arenas ({}):\n  {list}\n\
         Add `[arena.<name>] pool = \"<pool_name>\"` for each.",
        required.len()
    )
}

fn missing_entries_message(missing: &[String]) -> String {
    let list = missing.join("\n  ");
    format!(
        "{FILE_POOLS} missing {} required arena entries:\n  {list}\n\
         Add `[arena.<name>] pool = \"<pool_name>\"` for each.",
        missing.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required_set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn provided_map(pairs: &[(&str, &str)]) -> BTreeMap<String, PoolEntry> {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    PoolEntry {
                        pool: v.to_string(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn validate_complete_match_counts_pools() {
        let required = required_set(&["cartesian_point", "line", "face_bound"]);
        let provided = provided_map(&[
            ("cartesian_point", "geometry"),
            ("line", "geometry"),
            ("face_bound", "topology"),
        ]);
        match validate(&required, &provided) {
            Validation::Ok {
                distinct_pools,
                extras,
            } => {
                assert_eq!(distinct_pools, 2);
                assert!(extras.is_empty());
            }
            Validation::Missing(_) => panic!("expected Ok"),
        }
    }

    #[test]
    fn validate_missing_entry_returns_missing_list() {
        let required = required_set(&["cartesian_point", "line"]);
        let provided = provided_map(&[("cartesian_point", "geometry")]);
        match validate(&required, &provided) {
            Validation::Missing(missing) => {
                assert_eq!(missing, vec!["line".to_string()]);
            }
            Validation::Ok { .. } => panic!("expected Missing"),
        }
    }

    #[test]
    fn validate_extra_entry_passes_with_warning_payload() {
        let required = required_set(&["cartesian_point"]);
        let provided = provided_map(&[
            ("cartesian_point", "geometry"),
            ("ghost_arena", "junk"),
        ]);
        match validate(&required, &provided) {
            Validation::Ok {
                distinct_pools,
                extras,
            } => {
                assert_eq!(distinct_pools, 1);
                assert_eq!(extras, vec!["ghost_arena".to_string()]);
            }
            Validation::Missing(_) => panic!("expected Ok"),
        }
    }

    #[test]
    fn missing_file_message_lists_required() {
        let required = required_set(&["cartesian_point", "line"]);
        let msg = missing_file_message(&required);
        assert!(msg.contains("cartesian_point"));
        assert!(msg.contains("line"));
        assert!(msg.contains("required arenas (2)"));
    }

    #[test]
    fn missing_entries_message_lists_missing() {
        let msg = missing_entries_message(&["line".into()]);
        assert!(msg.contains("line"));
        assert!(msg.contains("missing 1 required"));
    }

    #[test]
    fn parses_toml_with_pool_field() {
        let body = r#"
[arena.cartesian_point]
pool = "geometry"

[arena.face_bound]
pool = "topology"
"#;
        let file: PoolsFile = toml::from_str(body).unwrap();
        assert_eq!(
            file.arena.get("cartesian_point").map(|e| e.pool.as_str()),
            Some("geometry")
        );
        assert_eq!(
            file.arena.get("face_bound").map(|e| e.pool.as_str()),
            Some("topology")
        );
    }
}
