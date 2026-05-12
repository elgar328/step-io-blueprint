//! Stage 4 — usage pruning against an external STEP file corpus.
//!
//! Pure transformation: variants.toml + corpus directory → usage.toml +
//! variants_pruned.toml + arenas_pruned.toml. The original variants.toml
//! / arenas.toml / pools.toml stay untouched — pruning produces a
//! parallel "view" used by downstream lowering when a slim, corpus-aware
//! IR is preferable to the full schema-faithful one.
//!
//! Pruning is fact-driven (entity instance counts from raw STEP files),
//! so there is no `prune_pending.toml`: every entity ends up in
//! `usage.toml` with a deterministic count, and `variants_pruned` /
//! `arenas_pruned` are computed deterministically from those counts.
//!
//! The cascading reclassification (P-2) walks a worklist fixpoint: each
//! pass marks more entities as unused (monotone increasing) or downgrades
//! a classification (one-way), so the loop is confluent and terminates.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::infer::arena::ArenaSpec;
use crate::infer::overrides::OverrideFile;
use crate::infer::variant::VariantSpec;

const VARIANTS_PENDING: &str = "variants_pending.toml";
const ARENAS_PENDING: &str = "arenas_pending.toml";
const FILE_VARIANTS: &str = "variants.toml";
const FILE_USAGE: &str = "usage.toml";
const FILE_VARIANTS_PRUNED: &str = "variants_pruned.toml";
const FILE_ARENAS_PRUNED: &str = "arenas_pruned.toml";
const FILE_ARENAS_OVERRIDES: &str = "arenas_overrides.toml";
const FILE_PRUNE_OVERRIDES: &str = "prune_overrides.toml";

#[derive(Debug, Default, Deserialize)]
struct PruneOverridesFile {
    #[serde(default)]
    keep: BTreeMap<String, KeepEntry>,
}

#[derive(Debug, Deserialize)]
struct KeepEntry {
    #[serde(default)]
    #[allow(dead_code)]
    reason: Option<String>,
}

fn load_prune_overrides() -> Result<PruneOverridesFile, String> {
    let path = Path::new("inferred").join(FILE_PRUNE_OVERRIDES);
    if !path.exists() {
        return Ok(PruneOverridesFile::default());
    }
    let body = fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    toml::from_str(&body).map_err(|e| format!("parse {path:?}: {e}"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    /// Number of `\bENTITY_NAME\s*\(` matches across every `.stp` /
    /// `.step` file under the supplied corpus path. 0 means the entity
    /// did not appear in any sampled STEP file — candidate for pruning.
    pub instance_count: usize,
}

pub fn run(corpus_path: &Path, allow_pending: bool) -> Result<(), String> {
    // Strict gate: both upstream stages must be resolved before pruning
    // operates on their decisions.
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

    if !corpus_path.exists() {
        return Err(format!("corpus path does not exist: {corpus_path:?}"));
    }

    // 1. Read variants.toml — primary classification input.
    let variants: BTreeMap<String, VariantSpec> =
        crate::infer::io::read_confident(FILE_VARIANTS, "entity")
            .map_err(|e| format!("read {FILE_VARIANTS}: {e}"))?;
    if variants.is_empty() {
        return Err(format!(
            "{FILE_VARIANTS} is empty or missing — run `infer variant` first."
        ));
    }

    let entity_names: Vec<String> = variants.keys().cloned().collect();
    eprintln!(
        "infer prune: scanning {} for {} entities...",
        corpus_path.display(),
        entity_names.len()
    );

    // 2. Walk corpus + count instances.
    let counts = count_instances(corpus_path, &entity_names);
    let total = entity_names.len();
    let used = counts.values().filter(|&&c| c > 0).count();
    let unused = total - used;
    eprintln!("infer prune: {total} entities (used={used} unused={unused})");

    // 3. usage.toml — every entity, including count = 0.
    let usage: BTreeMap<String, UsageRecord> = entity_names
        .iter()
        .map(|n| {
            (
                n.clone(),
                UsageRecord {
                    instance_count: counts.get(n).copied().unwrap_or(0),
                },
            )
        })
        .collect();
    crate::infer::io::write_confident(FILE_USAGE, "entity", &usage)
        .map_err(|e| format!("write {FILE_USAGE}: {e}"))?;

    // 4. P-2 transitive prune of variants, honoring prune_overrides.toml
    // keep entries (preserve ABSTRACT supertypes that have 0 corpus
    // instances but are needed as IR polymorphism roots).
    let prune_overrides = load_prune_overrides()?;
    for entity in prune_overrides.keep.keys() {
        if !variants.contains_key(entity) {
            eprintln!(
                "warning: {FILE_PRUNE_OVERRIDES} [keep.{entity}] — entity not in {FILE_VARIANTS}"
            );
        }
    }
    let keep_set: BTreeSet<String> = prune_overrides.keep.keys().cloned().collect();
    let pruned_variants = prune_transitive_with_keep(&variants, &counts, &keep_set);
    eprintln!(
        "infer prune: variants_pruned has {} entities (vs {} original, {} kept by overrides)",
        pruned_variants.len(),
        variants.len(),
        keep_set.len(),
    );
    crate::infer::io::write_confident(FILE_VARIANTS_PRUNED, "entity", &pruned_variants)
        .map_err(|e| format!("write {FILE_VARIANTS_PRUNED}: {e}"))?;

    // 5. Recompute arenas from pruned variants. Filter overrides whose
    // groups disappeared during pruning — emit a warning per stale entry
    // instead of failing, since pruning naturally invalidates some
    // overrides.
    let arenas_overrides: OverrideFile<ArenaSpec> =
        crate::infer::overrides::load(FILE_ARENAS_OVERRIDES)
            .map_err(|e| format!("load {FILE_ARENAS_OVERRIDES}: {e}"))?;
    let valid_groups = compute_pruned_group_names(&pruned_variants);
    let filtered_overrides = filter_stale_overrides(&arenas_overrides, &valid_groups);
    let pruned_arenas =
        crate::infer::arena::recompute_for_pruned(&pruned_variants, &filtered_overrides)?;
    eprintln!(
        "infer prune: arenas_pruned has {} groups",
        pruned_arenas.len()
    );
    crate::infer::io::write_confident(FILE_ARENAS_PRUNED, "group", &pruned_arenas)
        .map_err(|e| format!("write {FILE_ARENAS_PRUNED}: {e}"))?;

    Ok(())
}

/// Walk `root` recursively, collecting `.stp` / `.step` files (case
/// insensitive). Permission errors and broken symlinks are silently
/// skipped — the corpus may legitimately contain unreadable entries.
fn walk_step_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(walk_step_files(&p));
        } else if matches!(
            p.extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_lowercase())
                .as_deref(),
            Some("stp" | "step")
        ) {
            out.push(p);
        }
    }
    out
}

/// Build a single regex matching every entity name (alternation, longest
/// first to defeat prefix shadowing) and run it against every file in
/// `corpus_path`. Returns `entity_name → instance_count`.
fn count_instances(
    corpus_path: &Path,
    entity_names: &[String],
) -> HashMap<String, usize> {
    if entity_names.is_empty() {
        return HashMap::new();
    }
    let mut alt: Vec<String> = entity_names.iter().map(|n| n.to_uppercase()).collect();
    alt.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let pattern = format!(
        r"\b({})\s*\(",
        alt.iter()
            .map(|s| regex::escape(s))
            .collect::<Vec<_>>()
            .join("|")
    );
    let re = Regex::new(&pattern).expect("entity-name alternation regex must compile");

    let mut counts: HashMap<String, usize> = HashMap::new();
    for path in walk_step_files(corpus_path) {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for cap in re.captures_iter(&text) {
            let name = cap[1].to_lowercase();
            *counts.entry(name).or_insert(0) += 1;
        }
    }
    counts
}

/// Apply the P-2 transitive pruning rules to `variants` until a fixpoint
/// is reached. The result is a new `BTreeMap` with unused entities
/// removed and dependent classifications downgraded.
#[cfg(test)]
fn prune_transitive(
    variants: &BTreeMap<String, VariantSpec>,
    counts: &HashMap<String, usize>,
) -> BTreeMap<String, VariantSpec> {
    prune_transitive_with_keep(variants, counts, &BTreeSet::new())
}

/// Same as `prune_transitive`, but honors `keep_overrides` — entities in
/// the set are never marked unused (neither by initial 0-instance scan,
/// Rule 2 enum-shrink, nor Rule 3-5 cascade). Used to preserve ABSTRACT
/// supertypes (curve / surface) that serve as IR polymorphism roots.
fn prune_transitive_with_keep(
    variants: &BTreeMap<String, VariantSpec>,
    counts: &HashMap<String, usize>,
    keep_overrides: &BTreeSet<String>,
) -> BTreeMap<String, VariantSpec> {
    let mut pruned: BTreeMap<String, VariantSpec> = variants.clone();
    let mut unused: BTreeSet<String> = pruned
        .keys()
        .filter(|n| {
            counts.get(*n).copied().unwrap_or(0) == 0 && !keep_overrides.contains(*n)
        })
        .cloned()
        .collect();

    loop {
        let mut changed = false;
        let snapshot: Vec<(String, VariantSpec)> = pruned
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Rule 2: enum-shaped supertypes whose live member set has shrunk.
        // Rule order matters — must run before the Rule 1 sweep below so
        // EnumBase entries (which may already sit in `unused`) can still
        // direct their lone surviving child to SingleStruct before being
        // physically removed.
        for (entity, spec) in &snapshot {
            // keep_overrides preserves enum_base entities — skip Rule 2's
            // shrink-driven removal so they survive even with 0 or 1 live
            // children.
            if keep_overrides.contains(entity) {
                continue;
            }
            let nc = count_live_children(entity, &pruned, &unused);
            match spec {
                VariantSpec::EnumBase { .. } if nc == 0 => {
                    if unused.insert(entity.clone()) {
                        changed = true;
                    }
                }
                VariantSpec::EnumBase { .. } if nc == 1 => {
                    let lone_child = pruned.iter().find_map(|(c, s)| {
                        if unused.contains(c) {
                            return None;
                        }
                        match s {
                            VariantSpec::InEnum { enum_name } if enum_name == entity => {
                                Some(c.clone())
                            }
                            _ => None,
                        }
                    });
                    if let Some(child) = lone_child {
                        if !matches!(pruned.get(&child), Some(VariantSpec::SingleStruct)) {
                            pruned.insert(child, VariantSpec::SingleStruct);
                            changed = true;
                        }
                        if unused.insert(entity.clone()) {
                            changed = true;
                        }
                    }
                }
                VariantSpec::ConcreteSupertype
                    if nc == 0 && !unused.contains(entity) =>
                {
                    // Self is still instance-capable (own attrs); fall
                    // back to SingleStruct rather than deleting it.
                    pruned.insert(entity.clone(), VariantSpec::SingleStruct);
                    changed = true;
                }
                _ => {}
            }
        }

        // Rule 3-5: classifications referencing a parent that has gone
        // away (already unused or absent) become unused themselves. Read
        // the *current* pruned spec, not the snapshot — Rule 2 may have
        // just reclassified an entity (e.g. lone-child InEnum →
        // SingleStruct), and the new shape no longer references a parent.
        let entity_keys: Vec<String> = pruned.keys().cloned().collect();
        for entity in &entity_keys {
            if unused.contains(entity) {
                continue;
            }
            let Some(spec) = pruned.get(entity) else {
                continue;
            };
            let stale = match spec {
                VariantSpec::InEnum { enum_name } => parent_gone(enum_name, &pruned, &unused),
                VariantSpec::MergedInto { target, .. } => parent_gone(target, &pruned, &unused),
                VariantSpec::NestedField { into, .. } => parent_gone(into, &pruned, &unused),
                _ => false,
            };
            if stale
                && !keep_overrides.contains(entity)
                && unused.insert(entity.clone())
            {
                changed = true;
            }
        }

        // Rule 1: physically drop everything marked unused. Done last so
        // Rule 2 / 3-5 can read the original entries during this iter.
        let drop_now: Vec<String> = unused.iter().cloned().collect();
        for u in &drop_now {
            if pruned.remove(u).is_some() {
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }

    pruned
}

/// Count InEnum children pointing at `entity` that are still live (i.e.
/// not already marked unused).
fn count_live_children(
    entity: &str,
    pruned: &BTreeMap<String, VariantSpec>,
    unused: &BTreeSet<String>,
) -> usize {
    pruned
        .iter()
        .filter(|(name, spec)| {
            !unused.contains(*name)
                && matches!(spec, VariantSpec::InEnum { enum_name } if enum_name == entity)
        })
        .count()
}

fn parent_gone(
    parent: &str,
    pruned: &BTreeMap<String, VariantSpec>,
    unused: &BTreeSet<String>,
) -> bool {
    !pruned.contains_key(parent) || unused.contains(parent)
}

/// Names of the groups present in the pruned classification — feeds the
/// stale-override filter so `arenas_overrides.toml` entries pointing at
/// groups erased by pruning can be skipped with a warning. Mirrors the
/// group-key derivation in `arena::compute_groups` without exposing the
/// internal `Groups` type.
fn compute_pruned_group_names(
    pruned: &BTreeMap<String, VariantSpec>,
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for (entity, spec) in pruned {
        match spec {
            VariantSpec::SingleStruct => {
                names.insert(entity.clone());
            }
            VariantSpec::InEnum { enum_name } | VariantSpec::EnumBase { enum_name } => {
                names.insert(enum_name.clone());
            }
            VariantSpec::ConcreteSupertype
            | VariantSpec::ComplexSupertype { .. }
            | VariantSpec::CompositeOneOf { .. } => {
                names.insert(entity.clone());
            }
            VariantSpec::NestedField { .. } | VariantSpec::MergedInto { .. } => {}
        }
    }
    names
}

/// Drop overrides whose target group is no longer present after pruning.
/// Emits a warning per dropped entry so the user knows to review.
fn filter_stale_overrides(
    overrides: &OverrideFile<ArenaSpec>,
    valid_groups: &BTreeSet<String>,
) -> OverrideFile<ArenaSpec> {
    let mut filtered = OverrideFile::<ArenaSpec>::default();
    for (k, v) in &overrides.group {
        if valid_groups.contains(k) {
            filtered.group.insert(k.clone(), v.clone());
        } else {
            eprintln!(
                "warning: arenas_overrides [group.{k}] skipped — group removed by pruning"
            );
        }
    }
    filtered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::variant::VariantSpec;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_step(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn count_instances_simple() {
        let dir = TempDir::new().unwrap();
        write_step(
            dir.path(),
            "a.stp",
            "ISO-10303-21;\n\
             #1 = CARTESIAN_POINT('', (0, 0, 0));\n\
             #2 = CARTESIAN_POINT('', (1, 1, 1));\n\
             #3 = LINE('', #1, #4);\n\
             END-ISO-10303-21;\n",
        );
        write_step(
            dir.path(),
            "b.step",
            "#10 = CARTESIAN_POINT('', (2, 2, 2));\n",
        );
        let names = vec!["cartesian_point".to_string(), "line".to_string()];
        let counts = count_instances(dir.path(), &names);
        assert_eq!(counts.get("cartesian_point").copied(), Some(3));
        assert_eq!(counts.get("line").copied(), Some(1));
    }

    #[test]
    fn count_instances_empty_corpus() {
        let dir = TempDir::new().unwrap();
        let names = vec!["cartesian_point".to_string()];
        let counts = count_instances(dir.path(), &names);
        assert!(counts.is_empty() || counts.get("cartesian_point").copied() == Some(0));
    }

    #[test]
    fn count_instances_recursive_walk() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        write_step(&sub, "deep.stp", "#1 = LINE('', #2, #3);\n");
        let names = vec!["line".to_string()];
        let counts = count_instances(dir.path(), &names);
        assert_eq!(counts.get("line").copied(), Some(1));
    }

    #[test]
    fn count_instances_skips_non_step_files() {
        let dir = TempDir::new().unwrap();
        write_step(dir.path(), "data.txt", "#1 = LINE('', #2, #3);\n");
        let names = vec!["line".to_string()];
        let counts = count_instances(dir.path(), &names);
        assert!(counts.get("line").copied().unwrap_or(0) == 0);
    }

    fn variants_with(pairs: &[(&str, VariantSpec)]) -> BTreeMap<String, VariantSpec> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn prune_drops_unused_singletons() {
        let variants = variants_with(&[
            ("used", VariantSpec::SingleStruct),
            ("dead", VariantSpec::SingleStruct),
        ]);
        let counts: HashMap<String, usize> = [("used".to_string(), 5)].into_iter().collect();
        let pruned = prune_transitive(&variants, &counts);
        assert!(pruned.contains_key("used"));
        assert!(!pruned.contains_key("dead"));
    }

    #[test]
    fn prune_enum_with_zero_remaining_members_disappears() {
        let variants = variants_with(&[
            (
                "shape",
                VariantSpec::EnumBase {
                    enum_name: "shape".into(),
                },
            ),
            (
                "circle",
                VariantSpec::InEnum {
                    enum_name: "shape".into(),
                },
            ),
            (
                "square",
                VariantSpec::InEnum {
                    enum_name: "shape".into(),
                },
            ),
        ]);
        // No instance counts for any of them.
        let counts: HashMap<String, usize> = HashMap::new();
        let pruned = prune_transitive(&variants, &counts);
        assert!(!pruned.contains_key("shape"));
        assert!(!pruned.contains_key("circle"));
        assert!(!pruned.contains_key("square"));
    }

    #[test]
    fn prune_enum_with_one_remaining_member_collapses_to_single_struct() {
        let variants = variants_with(&[
            (
                "shape",
                VariantSpec::EnumBase {
                    enum_name: "shape".into(),
                },
            ),
            (
                "circle",
                VariantSpec::InEnum {
                    enum_name: "shape".into(),
                },
            ),
            (
                "square",
                VariantSpec::InEnum {
                    enum_name: "shape".into(),
                },
            ),
        ]);
        let counts: HashMap<String, usize> = [("circle".to_string(), 7)].into_iter().collect();
        let pruned = prune_transitive(&variants, &counts);
        // EnumBase removed, square (unused) removed, lone child reclassified.
        assert!(!pruned.contains_key("shape"));
        assert!(!pruned.contains_key("square"));
        assert!(matches!(
            pruned.get("circle"),
            Some(VariantSpec::SingleStruct)
        ));
    }

    #[test]
    fn prune_concrete_supertype_with_no_children_becomes_single_struct() {
        let variants = variants_with(&[
            ("action", VariantSpec::ConcreteSupertype),
            (
                "executed_action",
                VariantSpec::InEnum {
                    enum_name: "action".into(),
                },
            ),
        ]);
        // Parent used, child not.
        let counts: HashMap<String, usize> = [("action".to_string(), 12)].into_iter().collect();
        let pruned = prune_transitive(&variants, &counts);
        assert!(matches!(
            pruned.get("action"),
            Some(VariantSpec::SingleStruct)
        ));
        assert!(!pruned.contains_key("executed_action"));
    }

    #[test]
    fn prune_in_enum_loses_parent_and_disappears() {
        let variants = variants_with(&[
            (
                "shape",
                VariantSpec::EnumBase {
                    enum_name: "shape".into(),
                },
            ),
            (
                "circle",
                VariantSpec::InEnum {
                    enum_name: "shape".into(),
                },
            ),
            (
                "square",
                VariantSpec::InEnum {
                    enum_name: "shape".into(),
                },
            ),
        ]);
        // No usage at all → entire group disappears (cascade chain).
        let counts: HashMap<String, usize> = HashMap::new();
        let pruned = prune_transitive(&variants, &counts);
        assert!(pruned.is_empty());
    }

    fn keep_set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn prune_overrides_empty_matches_wrapper() {
        // Regression gate: empty keep set must match the no-override
        // wrapper exactly. Same input, same fixpoint.
        let variants = variants_with(&[
            ("used", VariantSpec::SingleStruct),
            ("dead", VariantSpec::SingleStruct),
        ]);
        let counts: HashMap<String, usize> = [("used".to_string(), 5)].into_iter().collect();
        let baseline = prune_transitive(&variants, &counts);
        let with_empty = prune_transitive_with_keep(&variants, &counts, &BTreeSet::new());
        assert_eq!(baseline, with_empty);
    }

    #[test]
    fn keep_preserves_zero_instance_entity() {
        // Initial 0-instance marking is bypassed for keep entries.
        let variants = variants_with(&[
            ("zero_used", VariantSpec::SingleStruct),
            ("seen", VariantSpec::SingleStruct),
        ]);
        let counts: HashMap<String, usize> =
            [("seen".to_string(), 3)].into_iter().collect();
        let pruned =
            prune_transitive_with_keep(&variants, &counts, &keep_set(&["zero_used"]));
        assert!(pruned.contains_key("zero_used"));
        assert!(pruned.contains_key("seen"));
    }

    #[test]
    fn keep_breaks_cascade_for_enum_base() {
        // The point of prune_overrides for Curve / Surface unification:
        // an abstract enum_base with 0 instances would normally trigger
        // cascade pruning of its in_enum children — even those with
        // high usage. keep on the parent breaks that cascade.
        let variants = variants_with(&[
            (
                "curve",
                VariantSpec::EnumBase {
                    enum_name: "curve".into(),
                },
            ),
            (
                "line",
                VariantSpec::InEnum {
                    enum_name: "curve".into(),
                },
            ),
            (
                "ray",
                VariantSpec::InEnum {
                    enum_name: "curve".into(),
                },
            ),
        ]);
        // curve has 0 corpus instances (abstract); line + ray have many.
        let counts: HashMap<String, usize> =
            [("line".to_string(), 1000), ("ray".to_string(), 500)]
                .into_iter()
                .collect();

        // Without keep: cascade would normally NOT trigger here since
        // curve has 2 live children (Rule 2's nc==0/1 don't fire). curve
        // gets initial-marked unused (0 instance) → children cascade.
        let without_keep = prune_transitive(&variants, &counts);
        assert!(!without_keep.contains_key("curve"));
        assert!(!without_keep.contains_key("line"));
        assert!(!without_keep.contains_key("ray"));

        // With keep.curve: curve survives initial marking → children
        // don't cascade.
        let with_keep =
            prune_transitive_with_keep(&variants, &counts, &keep_set(&["curve"]));
        assert!(with_keep.contains_key("curve"));
        assert!(with_keep.contains_key("line"));
        assert!(with_keep.contains_key("ray"));
    }

    #[test]
    fn keep_preserves_enum_base_with_lone_child() {
        // Rule 2's "lone child" branch normally dissolves an enum_base
        // with 1 live child into SingleStruct + drops the parent. keep
        // on the parent skips this dissolution so it remains as a
        // future polymorphism root for downstream recasts.
        let variants = variants_with(&[
            (
                "curve",
                VariantSpec::EnumBase {
                    enum_name: "curve".into(),
                },
            ),
            (
                "line",
                VariantSpec::InEnum {
                    enum_name: "curve".into(),
                },
            ),
        ]);
        let counts: HashMap<String, usize> =
            [("line".to_string(), 7)].into_iter().collect();

        let without_keep = prune_transitive(&variants, &counts);
        assert!(!without_keep.contains_key("curve"));
        assert!(matches!(
            without_keep.get("line"),
            Some(VariantSpec::SingleStruct)
        ));

        let with_keep =
            prune_transitive_with_keep(&variants, &counts, &keep_set(&["curve"]));
        assert!(matches!(
            with_keep.get("curve"),
            Some(VariantSpec::EnumBase { .. })
        ));
        assert!(matches!(
            with_keep.get("line"),
            Some(VariantSpec::InEnum { .. })
        ));
    }
}
