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
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use backhand::{FilesystemReader, InnerNode, SquashfsFileReader};
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
    if !corpus_path.is_dir() {
        return Err(format!(
            "{} is not a directory (expected a directory containing .sqfs files)",
            corpus_path.display()
        ));
    }
    let containers = list_sqfs_containers(corpus_path);
    if containers.is_empty() {
        return Err(format!(
            "no *.sqfs containers found in {}",
            corpus_path.display()
        ));
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
        "infer prune: scanning {} ({} sqfs container(s)) for {} entities...",
        corpus_path.display(),
        containers.len(),
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

fn is_step_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("stp" | "step")
    )
}

/// Sorted list of `*.sqfs` files directly inside `root` (non-recursive).
fn list_sqfs_containers(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().and_then(|s| s.to_str()) == Some("sqfs")
        })
        .collect();
    paths.sort();
    paths
}

fn tally_entity_matches(re: &Regex, text: &str, counts: &mut HashMap<String, usize>) {
    for cap in re.captures_iter(text) {
        let name = cap[1].to_lowercase();
        *counts.entry(name).or_insert(0) += 1;
    }
}

/// Walk every `*.sqfs` container in `root` and invoke `cb` with each
/// STEP file's UTF-8 content. Open / parse failures on a container are
/// reported as warnings and skipped.
fn for_each_step_file_in_corpus<F>(root: &Path, mut cb: F)
where
    F: FnMut(&str),
{
    for sqfs_path in list_sqfs_containers(root) {
        let file = match fs::File::open(&sqfs_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("warning: open {}: {e}", sqfs_path.display());
                continue;
            }
        };
        let fs_reader = match FilesystemReader::from_reader(BufReader::new(file)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("warning: parse {}: {e}", sqfs_path.display());
                continue;
            }
        };
        let mut step_files: Vec<SquashfsFileReader> = Vec::new();
        for node in fs_reader.files() {
            if let InnerNode::File(file_reader) = &node.inner
                && is_step_path(&node.fullpath)
            {
                step_files.push(file_reader.clone());
            }
        }
        for sf in &step_files {
            let mut content = String::new();
            if fs_reader
                .file(sf)
                .reader()
                .read_to_string(&mut content)
                .is_err()
            {
                continue;
            }
            cb(&content);
        }
    }
}

/// Build a single regex matching every entity name (alternation, longest
/// first to defeat prefix shadowing) and run it against every STEP file
/// inside every `*.sqfs` container in `corpus_path`. Returns
/// `entity_name → instance_count`.
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
    for_each_step_file_in_corpus(corpus_path, |text| {
        tally_entity_matches(&re, text, &mut counts);
    });
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

    // Auto-keep supertypes that own at least one live child. Without
    // this, an abstract supertype with corpus count 0 gets marked
    // unused below, and Rule 3-5 then cascades and prunes every live
    // child too (e.g. placement -> axis2_placement_3d with 9M
    // instances would disappear).
    let auto_keep: BTreeSet<String> = pruned
        .iter()
        .filter_map(|(name, spec)| {
            let is_supertype = matches!(
                spec,
                VariantSpec::EnumBase { .. }
                    | VariantSpec::ConcreteSupertype
                    | VariantSpec::ComplexSupertype { .. }
                    | VariantSpec::CompositeOneOf { .. }
            );
            if !is_supertype {
                return None;
            }
            let self_live = counts.get(name).copied().unwrap_or(0) > 0;
            let has_live_child = pruned.iter().any(|(child, child_spec)| {
                let points_back = match child_spec {
                    VariantSpec::InEnum { enum_name } => enum_name == name,
                    _ => false,
                };
                points_back && counts.get(child).copied().unwrap_or(0) > 0
            });
            if self_live || has_live_child {
                Some(name.clone())
            } else {
                None
            }
        })
        .collect();

    if !auto_keep.is_empty() {
        let preview: Vec<&str> = auto_keep.iter().take(8).map(|s| s.as_str()).collect();
        let suffix = if auto_keep.len() > 8 { ", ..." } else { "" };
        eprintln!(
            "infer prune: auto-kept {} supertype(s) with live children: {}{}",
            auto_keep.len(),
            preview.join(", "),
            suffix,
        );
    }

    let effective_keep: BTreeSet<String> = keep_overrides
        .iter()
        .chain(auto_keep.iter())
        .cloned()
        .collect();

    let mut unused: BTreeSet<String> = pruned
        .keys()
        .filter(|n| {
            counts.get(*n).copied().unwrap_or(0) == 0 && !effective_keep.contains(*n)
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
            // effective_keep preserves enum_base entities — skip Rule 2's
            // shrink-driven removal so they survive even with 0 or 1 live
            // children. Includes both manual prune_overrides and the
            // auto-keep set computed above.
            if effective_keep.contains(entity) {
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
                && !effective_keep.contains(entity)
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
    use tempfile::TempDir;

    #[test]
    fn tally_entity_matches_simple() {
        let names = ["cartesian_point", "line"];
        let mut alt: Vec<String> = names.iter().map(|n| n.to_uppercase()).collect();
        alt.sort_by_key(|s| std::cmp::Reverse(s.len()));
        let pattern = format!(
            r"\b({})\s*\(",
            alt.iter()
                .map(|s| regex::escape(s))
                .collect::<Vec<_>>()
                .join("|")
        );
        let re = Regex::new(&pattern).unwrap();
        let text = "ISO-10303-21;\n\
                    #1 = CARTESIAN_POINT('', (0, 0, 0));\n\
                    #2 = CARTESIAN_POINT('', (1, 1, 1));\n\
                    #3 = LINE('', #1, #4);\n\
                    END-ISO-10303-21;\n";
        let mut counts: HashMap<String, usize> = HashMap::new();
        tally_entity_matches(&re, text, &mut counts);
        assert_eq!(counts.get("cartesian_point").copied(), Some(2));
        assert_eq!(counts.get("line").copied(), Some(1));
    }

    #[test]
    fn list_sqfs_containers_empty_dir() {
        let dir = TempDir::new().unwrap();
        assert!(list_sqfs_containers(dir.path()).is_empty());
    }

    #[test]
    fn count_instances_empty_corpus() {
        let dir = TempDir::new().unwrap();
        let names = vec!["cartesian_point".to_string()];
        let counts = count_instances(dir.path(), &names);
        assert!(counts.is_empty() || counts.get("cartesian_point").copied() == Some(0));
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
        // Auto-keep rule: shape has a live child (circle, corpus=7), so
        // it survives the initial unused sweep. Cascade therefore does
        // not fire, and Rule 2's 1-child collapse is also skipped (the
        // skip applies to anything in effective_keep). circle stays
        // InEnum; only square (corpus 0) gets dropped.
        assert!(pruned.contains_key("shape"));
        assert!(!pruned.contains_key("square"));
        assert!(matches!(
            pruned.get("circle"),
            Some(VariantSpec::InEnum { .. })
        ));
    }

    #[test]
    fn prune_concrete_supertype_with_no_children_stays_concrete_supertype() {
        let variants = variants_with(&[
            ("action", VariantSpec::ConcreteSupertype),
            (
                "executed_action",
                VariantSpec::InEnum {
                    enum_name: "action".into(),
                },
            ),
        ]);
        // Parent used, child not. With Plan 3.23's auto-keep rule, a
        // ConcreteSupertype with corpus > 0 is auto-kept, so the lone
        // dead child is dropped but the parent stays as
        // ConcreteSupertype (it doesn't dissolve to SingleStruct).
        // Collapsing a 0-child ConcreteSupertype to SingleStruct is a
        // separate concern tracked for a future plan.
        let counts: HashMap<String, usize> = [("action".to_string(), 12)].into_iter().collect();
        let pruned = prune_transitive(&variants, &counts);
        assert!(matches!(
            pruned.get("action"),
            Some(VariantSpec::ConcreteSupertype)
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
        // All three have 0 corpus instances (blueprint-only supertype +
        // never-used variants — the curve / surface case where manual
        // keep is the only way to preserve them).
        let counts: HashMap<String, usize> = HashMap::new();

        // Without keep: curve gets initial-marked unused (0 instance),
        // and Rule 3-5 cascades through the InEnum children.
        let without_keep = prune_transitive(&variants, &counts);
        assert!(!without_keep.contains_key("curve"));
        assert!(!without_keep.contains_key("line"));
        assert!(!without_keep.contains_key("ray"));

        // With keep.curve: curve survives initial marking. line / ray
        // are still 0-instance, so they get marked unused on their own
        // (not via cascade). Manual keep preserves the supertype only;
        // preserving the variants too is the auto-keep rule's job and
        // requires corpus > 0 on at least one child.
        let with_keep =
            prune_transitive_with_keep(&variants, &counts, &keep_set(&["curve"]));
        assert!(with_keep.contains_key("curve"));
        assert!(!with_keep.contains_key("line"));
        assert!(!with_keep.contains_key("ray"));
    }

    // Removed `keep_preserves_enum_base_with_lone_child`: the
    // "lone-child collapse skipped by manual keep" scenario only
    // matters when the lone child has corpus > 0, but in that case the
    // auto-keep rule fires on its own and supersedes manual keep. The
    // remaining manual-keep responsibility (preserving a blueprint-only
    // supertype with no live children) is covered by
    // `keep_breaks_cascade_for_enum_base`, and the live-child case is
    // covered by `auto_keep_preserves_supertype_with_live_child`.

    #[test]
    fn auto_keep_preserves_supertype_with_live_child() {
        // The real-world case (placement / axis2_placement_3d): the
        // supertype is abstract (corpus 0) but one of its variants has
        // a huge live count. Without auto-keep, both would disappear.
        let variants = variants_with(&[
            (
                "placement",
                VariantSpec::EnumBase {
                    enum_name: "placement".into(),
                },
            ),
            (
                "axis2_placement_3d",
                VariantSpec::InEnum {
                    enum_name: "placement".into(),
                },
            ),
        ]);
        let counts: HashMap<String, usize> =
            [("axis2_placement_3d".to_string(), 9_000_000)]
                .into_iter()
                .collect();

        // No manual keep — auto-keep rule alone must preserve placement
        // and break the cascade.
        let pruned = prune_transitive(&variants, &counts);
        assert!(matches!(
            pruned.get("placement"),
            Some(VariantSpec::EnumBase { .. })
        ));
        assert!(matches!(
            pruned.get("axis2_placement_3d"),
            Some(VariantSpec::InEnum { .. })
        ));
    }

    #[test]
    fn auto_keep_does_not_fire_when_all_children_unused() {
        // Blueprint-only supertype: every child has corpus 0. Auto-keep
        // must stay off; manual keep is still required. This is the
        // curve / surface case.
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
        let counts: HashMap<String, usize> = HashMap::new();

        let pruned = prune_transitive(&variants, &counts);
        assert!(!pruned.contains_key("curve"));
        assert!(!pruned.contains_key("line"));
    }
}
