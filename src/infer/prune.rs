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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::infer::arena::ArenaSpec;
use crate::infer::overrides::OverrideFile;
use crate::infer::variant::VariantSpec;

const VARIANTS_PENDING: &str = "variants_pending.toml";
const ARENAS_PENDING: &str = "arenas_pending.toml";
const FILE_VARIANTS: &str = "variants.toml";
const FILE_ENUM_ROOT: &str = "variants_enum_root.toml";
const FILE_CORPUS_USAGE: &str = "corpus_usage.toml";
const FILE_USAGE: &str = "usage.toml";
const FILE_VARIANTS_PRUNED: &str = "variants_pruned.toml";
const FILE_ARENAS_PRUNED: &str = "arenas_pruned.toml";
const FILE_ARENAS_OVERRIDES: &str = "arenas_overrides.toml";
const FILE_PRUNE_OVERRIDES: &str = "prune_overrides.toml";
const FILE_POOLS: &str = "pools.toml";

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

#[derive(Debug, Deserialize)]
struct PoolEntry {
    pool: String,
}

#[derive(Debug, Default, Deserialize)]
struct PoolsFile {
    #[serde(default)]
    arena: BTreeMap<String, PoolEntry>,
}

/// Load `pools.toml` as an `arena -> pool` map. The flatten stage consults
/// it for the pool-boundary gate (do not absorb a middle node into a parent
/// of a different pool). A static manual input, like `prune_overrides.toml`.
fn load_pools() -> Result<BTreeMap<String, String>, String> {
    let path = Path::new("inferred").join(FILE_POOLS);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let body = fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    let file: PoolsFile = toml::from_str(&body).map_err(|e| format!("parse {path:?}: {e}"))?;
    Ok(file
        .arena
        .into_iter()
        .map(|(arena, entry)| (arena, entry.pool))
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    /// Total occurrences across every `.stp` / `.step` file under the
    /// supplied corpus path = `standalone_count + complex_part_count`.
    /// 0 means the entity did not appear in any sampled STEP file —
    /// candidate for pruning. Kept as the headline number for backward
    /// compatibility; consumers needing standalone-vs-complex must read
    /// the split fields below.
    pub instance_count: usize,

    /// Occurrences as a part of a complex MI instance — i.e. the
    /// `NAME(` token sat inside an `#N=( ... NAME(...) ... );` block.
    /// 0 means every occurrence was a standalone `#N=NAME(...)`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub complex_part_count: usize,

    /// Other entity names that appeared in the same complex-MI block
    /// as this entity, anywhere in the corpus. Empty when the entity
    /// is never seen inside a complex block. Order: sorted ascending
    /// so the catalog is deterministic. step-io reads this to know
    /// which leaf-set to bundle in a `#[step_entity_complex]` handler.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub co_instantiated_with: Vec<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

pub fn run(allow_pending: bool) -> Result<(), String> {
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

    // 1. Read variants.toml — primary classification input.
    let mut variants: BTreeMap<String, VariantSpec> =
        crate::infer::io::read_confident(FILE_VARIANTS, "entity")
            .map_err(|e| format!("read {FILE_VARIANTS}: {e}"))?;
    if variants.is_empty() {
        return Err(format!(
            "{FILE_VARIANTS} is empty or missing — run `infer variant` first."
        ));
    }

    let entity_names: Vec<String> = variants.keys().cloned().collect();

    // 2. Load the frozen corpus summary (generated by the corpus-usage bin in
    // step-io-reference-check, copied into inferred/). It lists every entity
    // name appearing in the corpus; reconstruct the standalone/complex split
    // for THIS schema's entities — peers outside the entity set are dropped
    // from co-instantiation, exactly as the former in-repo recognised scan did.
    let summary = load_corpus_summary()?;
    let entity_set: HashSet<&String> = variants.keys().collect();
    let mut total: HashMap<String, usize> = HashMap::new();
    let mut complex_part: HashMap<String, usize> = HashMap::new();
    let mut co_instantiated: HashMap<String, BTreeSet<String>> = HashMap::new();
    for name in &entity_names {
        let Some(rec) = summary.get(name) else {
            continue;
        };
        if rec.instance_count > 0 {
            total.insert(name.clone(), rec.instance_count);
        }
        if rec.complex_part_count > 0 {
            complex_part.insert(name.clone(), rec.complex_part_count);
        }
        let peers: BTreeSet<String> = rec
            .co_instantiated_with
            .iter()
            .filter(|p| entity_set.contains(p))
            .cloned()
            .collect();
        if !peers.is_empty() {
            co_instantiated.insert(name.clone(), peers);
        }
    }
    let entity_total = entity_names.len();
    let used = total.values().filter(|&&c| c > 0).count();
    eprintln!(
        "infer prune: {entity_total} entities (used={used} unused={})",
        entity_total - used
    );

    // 3. usage.toml — every entity, including count = 0.
    let usage: BTreeMap<String, UsageRecord> = entity_names
        .iter()
        .map(|n| {
            let total_n = total.get(n).copied().unwrap_or(0);
            let complex = complex_part.get(n).copied().unwrap_or(0);
            let coinst: Vec<String> = co_instantiated
                .get(n)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            (
                n.clone(),
                UsageRecord {
                    instance_count: total_n,
                    complex_part_count: complex,
                    co_instantiated_with: coinst,
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
    // Standalone counts (total minus complex-MI part occurrences) — a supertype
    // that only ever appears as a complex part (curve/surface/...) has
    // standalone 0 and must NOT be treated as directly instantiated.
    let standalone: HashMap<String, usize> = total
        .iter()
        .map(|(n, &t)| {
            let c = complex_part.get(n).copied().unwrap_or(0);
            (n.clone(), t.saturating_sub(c))
        })
        .collect();
    // Flatten instantiated middle nodes. A supertype that is directly
    // instantiated (standalone > 0) yet also sits inside another enum (has an
    // enclosing enum root) does not get its own enum level — it and its
    // children become flat InEnum members of their stable root. The
    // enclosing-root map comes from `variant` (prune lacks the parent graph).
    let enum_roots: BTreeMap<String, String> =
        crate::infer::io::read_confident(FILE_ENUM_ROOT, "enum_root").unwrap_or_default();
    // pools.toml (arena -> pool) drives the flatten's pool-boundary gate: a
    // middle node is not absorbed into a parent of a different pool.
    let pools = load_pools()?;
    let flattened = flatten_middle_nodes(&mut variants, &enum_roots, &standalone, &pools);
    if !flattened.is_empty() {
        let preview: Vec<String> = flattened
            .iter()
            .take(12)
            .map(|(e, r)| format!("{e}->{r}"))
            .collect();
        let suffix = if flattened.len() > 12 { ", ..." } else { "" };
        eprintln!(
            "infer prune: flattened {} instantiated middle node(s) to InEnum of stable root: {}{}",
            flattened.len(),
            preview.join(", "),
            suffix,
        );
    }

    let pruned_variants = prune_transitive_with_keep(&variants, &total, &standalone, &keep_set);
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

/// Read the frozen corpus summary (`inferred/corpus_usage.toml`). It is
/// generated by the `corpus-usage` bin in step-io-reference-check and copied
/// in manually — this repo no longer scans the corpus itself.
pub(crate) fn load_corpus_summary() -> Result<BTreeMap<String, UsageRecord>, String> {
    let path = Path::new("inferred").join(FILE_CORPUS_USAGE);
    if !path.exists() {
        return Err(format!(
            "{} not found — generate it with `cargo run --release --bin corpus-usage` \
             in step-io-reference-check and copy it into inferred/.",
            path.display()
        ));
    }
    let body = fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    let mut outer: BTreeMap<String, BTreeMap<String, UsageRecord>> =
        toml::from_str(&body).map_err(|e| format!("parse {path:?}: {e}"))?;
    let summary = outer.remove("entity").unwrap_or_default();
    if summary.is_empty() {
        return Err(format!(
            "{} has no [entity.*] records — regenerate it.",
            path.display()
        ));
    }
    Ok(summary)
}

/// Flatten instantiated middle nodes. A supertype that is directly
/// instantiated (standalone > 0) and also nested in another enum (has an
/// enclosing root) does not get its own enum level: it is demoted to a flat
/// `InEnum` member of its STABLE root — the nearest ancestor that is not
/// itself a flattening middle node — and any `InEnum` children that pointed
/// at a demoting node are re-pointed to the same stable root.
///
/// The schema alone cannot distinguish a flatten-worthy middle node (`group`:
/// directly instantiated) from a struct-less dispatch root (`edge`: never
/// instantiated) — both are non-abstract supertypes with own attrs and a
/// supertype parent. Only the corpus standalone count separates them, so this
/// runs here in prune (which has the corpus) rather than in variant.
/// Returns the `(entity, stable_root)` pairs that were demoted.
fn flatten_middle_nodes(
    variants: &mut BTreeMap<String, VariantSpec>,
    enum_roots: &BTreeMap<String, String>,
    standalone: &HashMap<String, usize>,
    pools: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    // Self-enum-root supertypes (EnumBase / ConcreteSupertype) that are
    // instantiated and have an enclosing root — the nodes that will demote.
    let mut demote_set: HashSet<String> = variants
        .iter()
        .filter(|(name, spec)| {
            matches!(
                spec,
                VariantSpec::EnumBase { .. } | VariantSpec::ConcreteSupertype
            ) && standalone.get(*name).copied().unwrap_or(0) > 0
                && enum_roots.contains_key(*name)
        })
        .map(|(n, _)| n.clone())
        .collect();
    if demote_set.is_empty() {
        return Vec::new();
    }

    // Pool-boundary gate: do not absorb a middle node into a parent of a
    // different pool. A node M that carries a pools.toml entry is pinned
    // (kept as its own arena) iff `pool(M) != pool(nearestEntriedAncestor(M))`,
    // where the nearest entried ancestor is the first enclosing-root ancestor
    // that itself has a pools.toml entry. This is a static structural fact, so
    // the decision is order-independent — no fixpoint needed. pools.toml is the
    // single control point: give a domain root a pool that differs from the
    // generic root it would flatten into, and it is preserved automatically.
    let nearest_entried_ancestor = |start: &str| -> Option<String> {
        let mut cur = enum_roots.get(start)?.clone();
        let mut visited: HashSet<String> = HashSet::new();
        loop {
            if pools.contains_key(&cur) {
                return Some(cur);
            }
            if !visited.insert(cur.clone()) {
                return None; // cycle guard
            }
            cur = enum_roots.get(&cur)?.clone();
        }
    };
    let pinned: Vec<String> = demote_set
        .iter()
        .filter(|m| {
            let key = m.as_str();
            let Some(m_pool) = pools.get(key) else {
                return false; // only entried nodes are pin candidates
            };
            nearest_entried_ancestor(key).is_some_and(|anc| pools.get(&anc) != Some(m_pool))
        })
        .cloned()
        .collect();
    for p in &pinned {
        demote_set.remove(p);
    }
    if !pinned.is_empty() {
        let mut preview = pinned.clone();
        preview.sort();
        eprintln!(
            "infer prune: pool-boundary gate kept {} middle node(s) as own arena: {}",
            pinned.len(),
            preview.join(", "),
        );
    }
    // Climb the enclosing-root chain while nodes keep demoting; return the
    // first non-demoting (stable) ancestor.
    let resolve = |start: &str| -> Option<String> {
        let mut cur = start.to_string();
        let mut visited: HashSet<String> = HashSet::new();
        while demote_set.contains(&cur) {
            if !visited.insert(cur.clone()) {
                return None; // cycle guard
            }
            cur = enum_roots.get(&cur)?.clone();
        }
        Some(cur)
    };

    let mut demoted: Vec<(String, String)> = Vec::new();
    let mut updates: Vec<(String, String)> = Vec::new();
    for (name, spec) in variants.iter() {
        match spec {
            VariantSpec::EnumBase { .. } | VariantSpec::ConcreteSupertype
                if demote_set.contains(name) =>
            {
                if let Some(root) = resolve(name) {
                    updates.push((name.clone(), root.clone()));
                    demoted.push((name.clone(), root));
                }
            }
            VariantSpec::InEnum { enum_name } if demote_set.contains(enum_name) => {
                if let Some(root) = resolve(enum_name) {
                    updates.push((name.clone(), root));
                }
            }
            _ => {}
        }
    }
    for (name, root) in updates {
        variants.insert(name, VariantSpec::InEnum { enum_name: root });
    }
    demoted.sort();
    demoted
}

/// Apply the P-2 transitive pruning rules to `variants` until a fixpoint
/// is reached. The result is a new `BTreeMap` with unused entities
/// removed and dependent classifications downgraded.
#[cfg(test)]
fn prune_transitive(
    variants: &BTreeMap<String, VariantSpec>,
    counts: &HashMap<String, usize>,
) -> BTreeMap<String, VariantSpec> {
    // Test fixtures model no complex-MI parts, so standalone == total.
    prune_transitive_with_keep(variants, counts, counts, &BTreeSet::new())
}

/// Same as `prune_transitive`, but honors `keep_overrides` — entities in
/// the set are never marked unused (neither by initial 0-instance scan,
/// Rule 2 enum-shrink, nor Rule 3-5 cascade). Used to preserve ABSTRACT
/// supertypes (curve / surface) that serve as IR polymorphism roots.
fn prune_transitive_with_keep(
    variants: &BTreeMap<String, VariantSpec>,
    counts: &HashMap<String, usize>,
    standalone: &HashMap<String, usize>,
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
            // Self-reference InEnum (variant.rs polymorphic-target fallback
            // assigns enum_name = self when a parent without a SUPERTYPE OF
            // clause is targeted by SUBTYPE OF children) acts as an enum
            // host just like EnumBase. Include it here so the parent is not
            // pruned to unused — otherwise Rule 3-5 cascades and drops the
            // live children too (Plan 3.38: pre_defined_curve_font /
            // pre_defined_symbol case).
            let is_supertype = matches!(
                spec,
                VariantSpec::EnumBase { .. }
                    | VariantSpec::ConcreteSupertype
                    | VariantSpec::ComplexSupertype { .. }
                    | VariantSpec::CompositeOneOf { .. }
            ) || matches!(
                spec,
                VariantSpec::InEnum { enum_name } if enum_name == name
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

    // Specialize self-live ConcreteSupertypes whose surviving InEnum
    // child set is empty. A child "survives" if its corpus count is
    // positive or it is kept by manual override; cascade-doomed
    // zero-corpus children do not block specialization. With no
    // surviving children, the supertype carries no polymorphism —
    // represent it as a plain SingleStruct so downstream stages do not
    // need a shape decision or an empty kind enum.
    let to_specialize: Vec<String> = pruned
        .iter()
        .filter(|(name, spec)| {
            if !matches!(spec, VariantSpec::ConcreteSupertype) {
                return false;
            }
            if counts.get(*name).copied().unwrap_or(0) == 0 {
                return false;
            }
            !pruned.iter().any(|(child, child_spec)| {
                matches!(
                    child_spec,
                    VariantSpec::InEnum { enum_name } if enum_name == *name
                ) && (counts.get(child).copied().unwrap_or(0) > 0 || keep_overrides.contains(child))
            })
        })
        .map(|(n, _)| n.clone())
        .collect();
    for name in &to_specialize {
        pruned.insert(name.clone(), VariantSpec::SingleStruct);
    }
    if !to_specialize.is_empty() {
        let preview: Vec<&str> = to_specialize.iter().take(8).map(|s| s.as_str()).collect();
        let suffix = if to_specialize.len() > 8 { ", ..." } else { "" };
        eprintln!(
            "infer prune: specialized {} childless ConcreteSupertype(s) to SingleStruct: {}{}",
            to_specialize.len(),
            preview.join(", "),
            suffix,
        );
    }

    // Corpus-recovery: an EnumBase that is itself directly instantiated as a
    // STANDALONE (non-complex) instance must retain a struct — EnumBase emits
    // no struct (naming.rs has_type=false/has_fields=false), so its direct
    // instances would have nowhere to live. This generalizes the per-entity
    // variants_overrides.toml ConcreteSupertype patches. The schema cannot
    // distinguish these from genuinely struct-less dispatch supertypes
    // (edge/face/surface) — both are non-abstract with own_attrs and
    // SUPERTYPE OF (ONEOF(...)); the discriminator is the STANDALONE corpus
    // count. NOTE: use `standalone`, not `counts` (= standalone + complex_part):
    // abstract roots like curve/surface/representation_item appear only as
    // complex-MI parts (e.g. `(CURVE() BOUNDED_CURVE() B_SPLINE_CURVE() ...)`),
    // so their `counts` are large but `standalone` is 0 — they must stay EnumBase.
    //
    //   standalone>0, no surviving InEnum child  -> SingleStruct (no dispatch)
    //   standalone>0, >=1 surviving InEnum child -> ConcreteSupertype (struct + dispatch)
    //   standalone==0                            -> leave EnumBase (pure dispatch root)
    let self_live_enum_bases: Vec<String> = pruned
        .iter()
        .filter(|(name, spec)| {
            matches!(spec, VariantSpec::EnumBase { .. })
                && standalone.get(*name).copied().unwrap_or(0) > 0
        })
        .map(|(n, _)| n.clone())
        .collect();
    for name in &self_live_enum_bases {
        let has_surviving_child = pruned.iter().any(|(child, child_spec)| {
            matches!(
                child_spec,
                VariantSpec::InEnum { enum_name } if enum_name == name
            ) && (counts.get(child).copied().unwrap_or(0) > 0 || keep_overrides.contains(child))
        });
        let new_spec = if has_surviving_child {
            VariantSpec::ConcreteSupertype
        } else {
            VariantSpec::SingleStruct
        };
        pruned.insert(name.clone(), new_spec);
    }
    if !self_live_enum_bases.is_empty() {
        let preview: Vec<&str> = self_live_enum_bases
            .iter()
            .take(8)
            .map(|s| s.as_str())
            .collect();
        let suffix = if self_live_enum_bases.len() > 8 {
            ", ..."
        } else {
            ""
        };
        eprintln!(
            "infer prune: recovered {} self-instantiated EnumBase(s) (-> ConcreteSupertype/SingleStruct): {}{}",
            self_live_enum_bases.len(),
            preview.join(", "),
            suffix,
        );
    }

    // Self-instantiated MergedInto recovery — the MergedInto branch of the same
    // flaw handled for EnumBase above. variant.rs's structural rules (3a/4b)
    // collapse a supertype with a single effective child into that child,
    // treating it as a pass-through wrapper. When the supertype is itself
    // directly instantiated (standalone > 0) it is a real concrete type, so
    // collapsing loses its standalone instances once the (typically 0-corpus)
    // merge target is pruned. Recover it like a live EnumBase.
    let self_live_merged: Vec<String> = pruned
        .iter()
        .filter(|(name, spec)| {
            matches!(spec, VariantSpec::MergedInto { .. })
                && standalone.get(*name).copied().unwrap_or(0) > 0
        })
        .map(|(n, _)| n.clone())
        .collect();
    for name in &self_live_merged {
        let has_surviving_child = pruned.iter().any(|(child, child_spec)| {
            matches!(
                child_spec,
                VariantSpec::InEnum { enum_name } if enum_name == name
            ) && (counts.get(child).copied().unwrap_or(0) > 0 || keep_overrides.contains(child))
        });
        let new_spec = if has_surviving_child {
            VariantSpec::ConcreteSupertype
        } else {
            VariantSpec::SingleStruct
        };
        pruned.insert(name.clone(), new_spec);
    }
    if !self_live_merged.is_empty() {
        let preview: Vec<&str> = self_live_merged
            .iter()
            .take(8)
            .map(|s| s.as_str())
            .collect();
        let suffix = if self_live_merged.len() > 8 {
            ", ..."
        } else {
            ""
        };
        eprintln!(
            "infer prune: recovered {} self-instantiated MergedInto(s) (-> ConcreteSupertype/SingleStruct): {}{}",
            self_live_merged.len(),
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
        .filter(|n| counts.get(*n).copied().unwrap_or(0) == 0 && !effective_keep.contains(*n))
        .cloned()
        .collect();

    loop {
        let mut changed = false;
        let snapshot: Vec<(String, VariantSpec)> =
            pruned.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

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
                VariantSpec::ConcreteSupertype if nc == 0 && !unused.contains(entity) => {
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
            if stale && !effective_keep.contains(entity) && unused.insert(entity.clone()) {
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
fn compute_pruned_group_names(pruned: &BTreeMap<String, VariantSpec>) -> BTreeSet<String> {
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
            eprintln!("warning: arenas_overrides [group.{k}] skipped — group removed by pruning");
        }
    }
    filtered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::variant::VariantSpec;

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
    fn prune_recovers_self_instantiated_merged_into() {
        // A supertype merged into its single 0-corpus child (variant.rs Rule
        // 3a/4b wrapper-collapse) must NOT be dropped when it is itself
        // directly instantiated (standalone > 0) — it is a real concrete type.
        // Recover it to SingleStruct; the 0-corpus merge target is pruned.
        let variants = variants_with(&[
            (
                "aoa",
                VariantSpec::MergedInto {
                    target: "dctpca".into(),
                    chain: vec![],
                },
            ),
            ("dctpca", VariantSpec::SingleStruct),
        ]);
        let counts: HashMap<String, usize> = [("aoa".to_string(), 116)].into_iter().collect();
        let pruned = prune_transitive(&variants, &counts);
        assert_eq!(
            pruned.get("aoa"),
            Some(&VariantSpec::SingleStruct),
            "self-instantiated MergedInto recovered to SingleStruct, not cascade-dropped"
        );
        assert!(
            !pruned.contains_key("dctpca"),
            "the 0-corpus merge target is still pruned"
        );
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
    fn prune_childless_concrete_supertype_specializes_to_single_struct() {
        let variants = variants_with(&[
            ("action", VariantSpec::ConcreteSupertype),
            (
                "executed_action",
                VariantSpec::InEnum {
                    enum_name: "action".into(),
                },
            ),
        ]);
        // Parent used, child not. The specialization step converts the
        // self-live but child-dead ConcreteSupertype to SingleStruct.
        let counts: HashMap<String, usize> = [("action".to_string(), 12)].into_iter().collect();
        let pruned = prune_transitive(&variants, &counts);
        assert!(matches!(
            pruned.get("action"),
            Some(VariantSpec::SingleStruct)
        ));
        assert!(!pruned.contains_key("executed_action"));
    }

    #[test]
    fn prune_concrete_supertype_with_live_child_unchanged() {
        let variants = variants_with(&[
            ("action", VariantSpec::ConcreteSupertype),
            (
                "executed_action",
                VariantSpec::InEnum {
                    enum_name: "action".into(),
                },
            ),
        ]);
        let counts: HashMap<String, usize> = [
            ("action".to_string(), 12),
            ("executed_action".to_string(), 4),
        ]
        .into_iter()
        .collect();
        let pruned = prune_transitive(&variants, &counts);
        assert!(matches!(
            pruned.get("action"),
            Some(VariantSpec::ConcreteSupertype)
        ));
        assert!(matches!(
            pruned.get("executed_action"),
            Some(VariantSpec::InEnum { .. })
        ));
    }

    #[test]
    fn prune_self_reference_in_enum_parent_kept_via_live_child() {
        // Plan 3.38: variant.rs polymorphic-target fallback classifies a
        // parent without a SUPERTYPE OF clause (but with SUBTYPE OF
        // children) as InEnum { enum_name = self }. auto_keep must
        // recognize this self-reference InEnum as a supertype so the
        // parent survives and Rule 3-5 cannot cascade-prune its live
        // child. Without the fix, the parent (corpus 0) is dropped and
        // the 100-corpus child cascades to unused.
        let variants = variants_with(&[
            (
                "parent_pdc",
                VariantSpec::InEnum {
                    enum_name: "parent_pdc".into(),
                },
            ),
            (
                "child_dpdc",
                VariantSpec::InEnum {
                    enum_name: "parent_pdc".into(),
                },
            ),
        ]);
        let counts: HashMap<String, usize> =
            [("child_dpdc".to_string(), 100)].into_iter().collect();
        let pruned = prune_transitive(&variants, &counts);
        assert!(pruned.contains_key("parent_pdc"));
        assert!(pruned.contains_key("child_dpdc"));
        // Parent's spec is unchanged (still self-reference InEnum).
        assert!(matches!(
            pruned.get("parent_pdc"),
            Some(VariantSpec::InEnum { enum_name }) if enum_name == "parent_pdc"
        ));
    }

    #[test]
    fn prune_self_reference_in_enum_drops_when_no_live_child() {
        // Regression guard for the auto_keep extension: when neither
        // the self-reference parent nor any child has corpus, both
        // entities must still be pruned (auto_keep requires self_live
        // OR has_live_child).
        let variants = variants_with(&[
            (
                "parent_pdc",
                VariantSpec::InEnum {
                    enum_name: "parent_pdc".into(),
                },
            ),
            (
                "child_dpdc",
                VariantSpec::InEnum {
                    enum_name: "parent_pdc".into(),
                },
            ),
        ]);
        let counts: HashMap<String, usize> = HashMap::new();
        let pruned = prune_transitive(&variants, &counts);
        assert!(!pruned.contains_key("parent_pdc"));
        assert!(!pruned.contains_key("child_dpdc"));
    }

    #[test]
    fn prune_concrete_supertype_with_only_dead_children_specializes() {
        let variants = variants_with(&[
            ("action", VariantSpec::ConcreteSupertype),
            (
                "executed_action",
                VariantSpec::InEnum {
                    enum_name: "action".into(),
                },
            ),
            (
                "planned_action",
                VariantSpec::InEnum {
                    enum_name: "action".into(),
                },
            ),
        ]);
        // Parent live, all children dead -> specialize to SingleStruct.
        let counts: HashMap<String, usize> = [("action".to_string(), 5)].into_iter().collect();
        let pruned = prune_transitive(&variants, &counts);
        assert!(matches!(
            pruned.get("action"),
            Some(VariantSpec::SingleStruct)
        ));
        assert!(!pruned.contains_key("executed_action"));
        assert!(!pruned.contains_key("planned_action"));
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
        let with_empty = prune_transitive_with_keep(&variants, &counts, &counts, &BTreeSet::new());
        assert_eq!(baseline, with_empty);
    }

    #[test]
    fn keep_preserves_zero_instance_entity() {
        // Initial 0-instance marking is bypassed for keep entries.
        let variants = variants_with(&[
            ("zero_used", VariantSpec::SingleStruct),
            ("seen", VariantSpec::SingleStruct),
        ]);
        let counts: HashMap<String, usize> = [("seen".to_string(), 3)].into_iter().collect();
        let pruned =
            prune_transitive_with_keep(&variants, &counts, &counts, &keep_set(&["zero_used"]));
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
            prune_transitive_with_keep(&variants, &counts, &counts, &keep_set(&["curve"]));
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
        let counts: HashMap<String, usize> = [("axis2_placement_3d".to_string(), 9_000_000)]
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

    // --- Corpus-recovery: self-instantiated EnumBase -> struct ---
    // An EnumBase emits no struct (naming.rs has_type=false), so an entity
    // that is directly instantiated standalone (#N=NAME(...)) but classified
    // EnumBase would lose its instances. The recovery rule keys on the
    // STANDALONE count (total - complex_part), not total. These tests pin
    // each branch and guard against the standalone/total confusion.

    #[test]
    fn corpus_recovery_self_live_no_live_child_becomes_single_struct() {
        // group case: 5793 standalone GROUP(...) instances, the only declared
        // child (change_group) has corpus 0 -> no surviving InEnum child ->
        // SingleStruct (no dispatch). The dead child is pruned.
        let variants = variants_with(&[
            (
                "group",
                VariantSpec::EnumBase {
                    enum_name: "group".into(),
                },
            ),
            (
                "change_group",
                VariantSpec::InEnum {
                    enum_name: "group".into(),
                },
            ),
        ]);
        let counts: HashMap<String, usize> = [("group".to_string(), 5793)].into_iter().collect();
        let standalone = counts.clone();
        let pruned = prune_transitive_with_keep(&variants, &counts, &standalone, &BTreeSet::new());
        assert!(matches!(
            pruned.get("group"),
            Some(VariantSpec::SingleStruct)
        ));
        assert!(!pruned.contains_key("change_group"));
    }

    #[test]
    fn corpus_recovery_self_live_with_live_child_becomes_concrete_supertype() {
        // shape_aspect case: 3320 standalone instances plus a live in_enum
        // child (datum_system) -> needs both a struct and dispatch ->
        // ConcreteSupertype, child stays InEnum.
        let variants = variants_with(&[
            (
                "shape_aspect",
                VariantSpec::EnumBase {
                    enum_name: "shape_aspect".into(),
                },
            ),
            (
                "datum_system",
                VariantSpec::InEnum {
                    enum_name: "shape_aspect".into(),
                },
            ),
        ]);
        let counts: HashMap<String, usize> = [
            ("shape_aspect".to_string(), 3320),
            ("datum_system".to_string(), 5),
        ]
        .into_iter()
        .collect();
        let standalone = counts.clone();
        let pruned = prune_transitive_with_keep(&variants, &counts, &standalone, &BTreeSet::new());
        assert!(matches!(
            pruned.get("shape_aspect"),
            Some(VariantSpec::ConcreteSupertype)
        ));
        assert!(matches!(
            pruned.get("datum_system"),
            Some(VariantSpec::InEnum { .. })
        ));
    }

    #[test]
    fn corpus_recovery_skips_abstract_supertype_zero_standalone() {
        // edge/face/placement case: the supertype is never instantiated
        // standalone (standalone 0) and is a pure dispatch root. Even with a
        // huge live child it must stay EnumBase — recovery does not fire.
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
        let counts: HashMap<String, usize> = [("axis2_placement_3d".to_string(), 9_000_000)]
            .into_iter()
            .collect();
        // placement absent from standalone -> 0.
        let standalone = counts.clone();
        let pruned = prune_transitive_with_keep(&variants, &counts, &standalone, &BTreeSet::new());
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
    fn corpus_recovery_self_live_single_child_not_collapsed() {
        // property_definition pin: a self-live EnumBase with a SINGLE live
        // child must become ConcreteSupertype, NOT collapse to its lone child
        // (the EnumBase nc==1 lone-child rule would have promoted the child to
        // SingleStruct and dropped the 13696 standalone PROPERTY_DEFINITION
        // instances). The child stays InEnum.
        let variants = variants_with(&[
            (
                "property_definition",
                VariantSpec::EnumBase {
                    enum_name: "property_definition".into(),
                },
            ),
            (
                "product_definition_shape",
                VariantSpec::InEnum {
                    enum_name: "property_definition".into(),
                },
            ),
        ]);
        let counts: HashMap<String, usize> = [
            ("property_definition".to_string(), 13_696),
            ("product_definition_shape".to_string(), 253_440),
        ]
        .into_iter()
        .collect();
        let standalone = counts.clone();
        let pruned = prune_transitive_with_keep(&variants, &counts, &standalone, &BTreeSet::new());
        assert!(matches!(
            pruned.get("property_definition"),
            Some(VariantSpec::ConcreteSupertype)
        ));
        assert!(matches!(
            pruned.get("product_definition_shape"),
            Some(VariantSpec::InEnum { .. })
        ));
    }

    #[test]
    fn corpus_recovery_complex_part_only_stays_enum_base() {
        // THE guard for this fix's core bug: curve/surface/representation_item
        // appear only as complex-MI parts (e.g. (CURVE() B_SPLINE_CURVE() ...))
        // with empty `()`. Their TOTAL count is large but STANDALONE is 0, so
        // they need no struct and must stay EnumBase. Keying recovery on total
        // instead of standalone would over-recover them into dead structs.
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
        // total: curve 330000 (all complex-part), line 50.
        let counts: HashMap<String, usize> =
            [("curve".to_string(), 330_000), ("line".to_string(), 50)]
                .into_iter()
                .collect();
        // standalone: curve 0 (never a standalone instance), line 50.
        let standalone: HashMap<String, usize> = [("line".to_string(), 50)].into_iter().collect();
        let pruned = prune_transitive_with_keep(&variants, &counts, &standalone, &BTreeSet::new());
        assert!(
            matches!(pruned.get("curve"), Some(VariantSpec::EnumBase { .. })),
            "curve appears only as a complex part (standalone 0) -> must stay EnumBase"
        );
        assert!(matches!(
            pruned.get("line"),
            Some(VariantSpec::InEnum { .. })
        ));
    }

    // --- Middle-node flatten: instantiated supertype nested in another enum
    // becomes a flat InEnum member of its stable root; its children re-point. ---

    fn roots(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn flatten_demotes_middle_node_and_reparents_children() {
        // point (abstract root) -> cartesian_point (instantiated middle node)
        // -> apll_point (child). cartesian_point and apll_point both flatten
        // to flat InEnum members of point; point itself is untouched.
        let mut variants = variants_with(&[
            (
                "point",
                VariantSpec::EnumBase {
                    enum_name: "point".into(),
                },
            ),
            (
                "cartesian_point",
                VariantSpec::EnumBase {
                    enum_name: "cartesian_point".into(),
                },
            ),
            (
                "apll_point",
                VariantSpec::InEnum {
                    enum_name: "cartesian_point".into(),
                },
            ),
        ]);
        let enum_roots = roots(&[
            ("cartesian_point", "point"),
            ("apll_point", "cartesian_point"),
        ]);
        let standalone: HashMap<String, usize> = [
            ("cartesian_point".to_string(), 100),
            ("apll_point".to_string(), 5),
        ]
        .into_iter()
        .collect();
        let demoted =
            flatten_middle_nodes(&mut variants, &enum_roots, &standalone, &BTreeMap::new());
        assert!(matches!(
            variants.get("cartesian_point"),
            Some(VariantSpec::InEnum { enum_name }) if enum_name == "point"
        ));
        assert!(matches!(
            variants.get("apll_point"),
            Some(VariantSpec::InEnum { enum_name }) if enum_name == "point"
        ));
        assert!(matches!(
            variants.get("point"),
            Some(VariantSpec::EnumBase { .. })
        ));
        assert_eq!(
            demoted,
            vec![("cartesian_point".to_string(), "point".to_string())]
        );
    }

    #[test]
    fn flatten_skips_top_supertype_without_ancestor() {
        // representation is instantiated and a supertype, but has NO enclosing
        // enum root (not in enum_roots) -> it is a top, must stay its own root.
        let mut variants = variants_with(&[
            ("representation", VariantSpec::ConcreteSupertype),
            (
                "shape_representation",
                VariantSpec::InEnum {
                    enum_name: "representation".into(),
                },
            ),
        ]);
        let enum_roots = roots(&[("shape_representation", "representation")]);
        let standalone: HashMap<String, usize> =
            [("representation".to_string(), 100)].into_iter().collect();
        let demoted =
            flatten_middle_nodes(&mut variants, &enum_roots, &standalone, &BTreeMap::new());
        assert!(matches!(
            variants.get("representation"),
            Some(VariantSpec::ConcreteSupertype)
        ));
        assert!(demoted.is_empty());
    }

    #[test]
    fn flatten_cascades_chain_to_stable_root() {
        // a (abstract) -> m1 -> m2 -> leaf, m1/m2 instantiated middle nodes.
        // All flatten to `a` (the first non-demoting ancestor).
        let mut variants = variants_with(&[
            (
                "a",
                VariantSpec::EnumBase {
                    enum_name: "a".into(),
                },
            ),
            (
                "m1",
                VariantSpec::EnumBase {
                    enum_name: "m1".into(),
                },
            ),
            ("m2", VariantSpec::ConcreteSupertype),
            (
                "leaf",
                VariantSpec::InEnum {
                    enum_name: "m2".into(),
                },
            ),
        ]);
        let enum_roots = roots(&[("m1", "a"), ("m2", "m1"), ("leaf", "m2")]);
        let standalone: HashMap<String, usize> = [
            ("m1".to_string(), 10),
            ("m2".to_string(), 5),
            ("leaf".to_string(), 2),
        ]
        .into_iter()
        .collect();
        flatten_middle_nodes(&mut variants, &enum_roots, &standalone, &BTreeMap::new());
        for e in ["m1", "m2", "leaf"] {
            assert!(
                matches!(variants.get(e), Some(VariantSpec::InEnum { enum_name }) if enum_name == "a"),
                "{e} should flatten to a, got {:?}",
                variants.get(e)
            );
        }
        assert!(matches!(
            variants.get("a"),
            Some(VariantSpec::EnumBase { .. })
        ));
    }

    #[test]
    fn flatten_leaves_uninstantiated_dispatch_root_untouched() {
        // edge regression: edge is a supertype with an enum-root ancestor but
        // is NEVER instantiated (standalone 0). It must stay a struct-less
        // EnumBase dispatch root — only corpus separates it from `group`.
        let mut variants = variants_with(&[
            (
                "edge",
                VariantSpec::EnumBase {
                    enum_name: "edge".into(),
                },
            ),
            (
                "edge_curve",
                VariantSpec::InEnum {
                    enum_name: "edge".into(),
                },
            ),
        ]);
        let enum_roots = roots(&[
            ("edge", "topological_representation_item"),
            ("edge_curve", "edge"),
        ]);
        // edge standalone 0 (absent); edge_curve instantiated.
        let standalone: HashMap<String, usize> =
            [("edge_curve".to_string(), 100)].into_iter().collect();
        let demoted =
            flatten_middle_nodes(&mut variants, &enum_roots, &standalone, &BTreeMap::new());
        assert!(matches!(
            variants.get("edge"),
            Some(VariantSpec::EnumBase { .. })
        ));
        assert!(matches!(
            variants.get("edge_curve"),
            Some(VariantSpec::InEnum { enum_name }) if enum_name == "edge"
        ));
        assert!(demoted.is_empty());
    }

    fn pools(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn flatten_pool_gate_pins_cross_pool_middle_node() {
        // styled_item (visualization) is an instantiated middle node under the
        // generic root representation_item (shape_rep). The pool-boundary gate
        // pins it (keeps its own arena); its styling child re-points to it, not
        // to representation_item.
        let mut variants = variants_with(&[
            (
                "representation_item",
                VariantSpec::EnumBase {
                    enum_name: "representation_item".into(),
                },
            ),
            ("styled_item", VariantSpec::ConcreteSupertype),
            (
                "over_riding_styled_item",
                VariantSpec::InEnum {
                    enum_name: "styled_item".into(),
                },
            ),
        ]);
        let enum_roots = roots(&[
            ("styled_item", "representation_item"),
            ("over_riding_styled_item", "styled_item"),
        ]);
        let standalone: HashMap<String, usize> = [
            ("styled_item".to_string(), 100),
            ("over_riding_styled_item".to_string(), 5),
        ]
        .into_iter()
        .collect();
        let pools = pools(&[
            ("representation_item", "shape_rep"),
            ("styled_item", "visualization"),
        ]);
        flatten_middle_nodes(&mut variants, &enum_roots, &standalone, &pools);
        // Pinned: kept ConcreteSupertype (own arena), NOT demoted.
        assert!(matches!(
            variants.get("styled_item"),
            Some(VariantSpec::ConcreteSupertype)
        ));
        // Child re-points to the pinned styled_item, staying in visualization.
        assert!(matches!(
            variants.get("over_riding_styled_item"),
            Some(VariantSpec::InEnum { enum_name }) if enum_name == "styled_item"
        ));
    }

    #[test]
    fn flatten_pool_gate_does_not_over_pin_same_pool_descendant() {
        // a (shape_rep) -> b (pmi) -> c (pmi), all entried & instantiated.
        // b crosses a pool boundary -> pin. c is the SAME pool as its nearest
        // entried ancestor b -> must NOT pin; it flattens into b. (Regression
        // guard: a naive "differs from far stable root" rule would over-pin c.)
        let mut variants = variants_with(&[
            (
                "a",
                VariantSpec::EnumBase {
                    enum_name: "a".into(),
                },
            ),
            ("b", VariantSpec::ConcreteSupertype),
            ("c", VariantSpec::ConcreteSupertype),
            (
                "leaf",
                VariantSpec::InEnum {
                    enum_name: "c".into(),
                },
            ),
        ]);
        let enum_roots = roots(&[("b", "a"), ("c", "b"), ("leaf", "c")]);
        let standalone: HashMap<String, usize> = [
            ("b".to_string(), 10),
            ("c".to_string(), 5),
            ("leaf".to_string(), 2),
        ]
        .into_iter()
        .collect();
        let pools = pools(&[("a", "shape_rep"), ("b", "pmi"), ("c", "pmi")]);
        flatten_middle_nodes(&mut variants, &enum_roots, &standalone, &pools);
        // b pins (pmi != shape_rep of nearest entried ancestor a).
        assert!(matches!(
            variants.get("b"),
            Some(VariantSpec::ConcreteSupertype)
        ));
        // c does NOT pin (pmi == pmi of nearest entried ancestor b) -> flattens into b.
        assert!(matches!(
            variants.get("c"),
            Some(VariantSpec::InEnum { enum_name }) if enum_name == "b"
        ));
        // leaf follows c into b.
        assert!(matches!(
            variants.get("leaf"),
            Some(VariantSpec::InEnum { enum_name }) if enum_name == "b"
        ));
    }
}
