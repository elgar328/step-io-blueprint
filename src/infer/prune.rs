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
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use backhand::{FilesystemReader, InnerNode, SquashfsFileReader};
use serde::{Deserialize, Serialize};

use crate::infer::arena::ArenaSpec;
use crate::infer::overrides::OverrideFile;
use crate::infer::variant::VariantSpec;

const VARIANTS_PENDING: &str = "variants_pending.toml";
const ARENAS_PENDING: &str = "arenas_pending.toml";
const FILE_VARIANTS: &str = "variants.toml";
const FILE_ENUM_ROOT: &str = "variants_enum_root.toml";
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

    // 2. Walk corpus + count instances. Splits each token into
    // standalone vs complex-part and gathers per-entity leaf-set.
    let tally = count_instances(corpus_path, &entity_names);
    let total = entity_names.len();
    let used = tally.total.values().filter(|&&c| c > 0).count();
    let unused = total - used;
    eprintln!("infer prune: {total} entities (used={used} unused={unused})");

    // 3. usage.toml — every entity, including count = 0.
    let usage: BTreeMap<String, UsageRecord> = entity_names
        .iter()
        .map(|n| {
            let total_n = tally.total.get(n).copied().unwrap_or(0);
            let complex = tally.complex_part.get(n).copied().unwrap_or(0);
            let coinst: Vec<String> = tally
                .co_instantiated_with
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
    let standalone: HashMap<String, usize> = tally
        .total
        .iter()
        .map(|(n, &t)| {
            let c = tally.complex_part.get(n).copied().unwrap_or(0);
            (n.clone(), t.saturating_sub(c))
        })
        .collect();
    // Phase 0 (detection only): instantiated middle nodes — supertypes that
    // are directly instantiated (standalone > 0) yet also sit inside another
    // enum (have an enclosing enum root). These are the entities the flatten
    // rule will demote from their own enum root to a flat InEnum member of
    // the stable root. Read the enclosing-root map emitted by `variant`.
    let enum_roots: BTreeMap<String, String> =
        crate::infer::io::read_confident(FILE_ENUM_ROOT, "enum_root").unwrap_or_default();
    let middle_nodes: Vec<&String> = variants
        .iter()
        .filter(|(name, spec)| {
            matches!(
                spec,
                VariantSpec::EnumBase { .. } | VariantSpec::ConcreteSupertype
            ) && standalone.get(*name).copied().unwrap_or(0) > 0
                && enum_roots.contains_key(*name)
        })
        .map(|(n, _)| n)
        .collect();
    if !middle_nodes.is_empty() {
        let preview: Vec<String> = middle_nodes
            .iter()
            .take(12)
            .map(|n| format!("{n}->{}", enum_roots.get(*n).map(String::as_str).unwrap_or("?")))
            .collect();
        let suffix = if middle_nodes.len() > 12 { ", ..." } else { "" };
        eprintln!(
            "infer prune: detected {} instantiated middle node(s) (flatten candidates): {}{}",
            middle_nodes.len(),
            preview.join(", "),
            suffix,
        );
    }

    let pruned_variants =
        prune_transitive_with_keep(&variants, &tally.total, &standalone, &keep_set);
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

/// Per-corpus tally produced by `scan_step_text`. `total` lets callers
/// keep using a single number for "used at all"; consumers needing the
/// split read `complex_part` and `co_instantiated_with` separately.
#[derive(Debug, Default)]
struct ScanTally {
    total: HashMap<String, usize>,
    complex_part: HashMap<String, usize>,
    co_instantiated_with: HashMap<String, BTreeSet<String>>,
}

impl ScanTally {
    fn bump_total(&mut self, name: &str) {
        *self.total.entry(name.to_string()).or_insert(0) += 1;
    }
    fn bump_complex(&mut self, name: &str) {
        *self.complex_part.entry(name.to_string()).or_insert(0) += 1;
    }
    fn record_complex_group(&mut self, leaves: &[String]) {
        // Each leaf records every other leaf in the same complex block.
        for (i, leaf) in leaves.iter().enumerate() {
            let entry = self
                .co_instantiated_with
                .entry(leaf.clone())
                .or_default();
            for (j, other) in leaves.iter().enumerate() {
                if i != j {
                    entry.insert(other.clone());
                }
            }
        }
    }
}

/// Scan one STEP P21 file. For each instance declaration `#N=<body>;`:
///   - If `<body>` starts with `(`, treat it as a complex MI block and
///     count every `NAME\s*\(` token inside as a `complex_part` for
///     that NAME; also gather the set of NAMEs as a leaf-set.
///   - Otherwise it's a standalone instance whose head NAME counts
///     once as standalone.
/// `total` is always bumped (standalone + complex_part). The scanner
/// respects P21 string literals (`'...'`, with `''` as an escaped
/// quote) so parentheses inside strings do not perturb the paren-depth
/// state machine.
fn scan_step_text(text: &str, recognised: &HashSet<String>, tally: &mut ScanTally) {
    let bytes = text.as_bytes();
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        // Find next instance start: '#' digit+ optional ws '='.
        if bytes[i] != b'#' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < len && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j == i + 1 {
            i = j;
            continue; // '#' not followed by digits — not an instance ref.
        }
        // Skip ws then expect '='.
        while j < len && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if j >= len || bytes[j] != b'=' {
            i = j;
            continue;
        }
        j += 1;
        // Skip ws / newlines after '='.
        while j < len && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= len {
            break;
        }
        if bytes[j] == b'(' {
            // Complex MI block. Walk to the matching ')' at depth 0,
            // tallying every `NAME\s*\(` token along the way.
            let (consumed, leaves) = walk_complex_block(&bytes[j..], recognised);
            for name in &leaves {
                tally.bump_total(name);
                tally.bump_complex(name);
            }
            tally.record_complex_group(&leaves);
            i = j + consumed;
        } else if bytes[j].is_ascii_alphabetic() || bytes[j] == b'_' {
            // Standalone instance — its head NAME is `[j..k)` where k
            // is the first non-identifier byte.
            let name_start = j;
            while j < len
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
            {
                j += 1;
            }
            let name = std::str::from_utf8(&bytes[name_start..j])
                .unwrap_or("")
                .to_ascii_lowercase();
            if recognised.contains(&name) {
                tally.bump_total(&name);
                // standalone — no complex_part bump.
            }
            i = j;
        } else {
            i = j + 1;
        }
    }
}

/// Walk forward starting at a `(` (the opening of a complex MI block).
/// Returns `(bytes_consumed, leaf_names_in_order_seen)` where consumed
/// includes the closing `)` (or end-of-text on malformed input — a best-
/// effort safety net). Skips parentheses that appear inside P21 string
/// literals.
fn walk_complex_block(bytes: &[u8], recognised: &HashSet<String>) -> (usize, Vec<String>) {
    let mut depth: i32 = 0;
    let mut i = 0;
    let len = bytes.len();
    let mut in_string = false;
    let mut leaves: Vec<String> = Vec::new();
    let mut leaf_set: BTreeSet<String> = BTreeSet::new();
    while i < len {
        let c = bytes[i];
        if in_string {
            if c == b'\'' {
                // `''` is an escaped single-quote inside the literal.
                if i + 1 < len && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => {
                in_string = true;
                i += 1;
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return (i, leaves_dedup(leaves, leaf_set));
                }
            }
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                // Candidate NAME — read identifier, then check '('.
                let start = i;
                while i < len
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                {
                    i += 1;
                }
                // Skip ws before '('.
                let mut k = i;
                while k < len && bytes[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k < len && bytes[k] == b'(' {
                    let name = std::str::from_utf8(&bytes[start..i])
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if recognised.contains(&name) && leaf_set.insert(name.clone()) {
                        leaves.push(name);
                    }
                }
                // Don't advance past the '(' here — the outer loop will
                // see it and increment depth.
            }
            _ => i += 1,
        }
    }
    (i, leaves_dedup(leaves, leaf_set))
}

fn leaves_dedup(leaves: Vec<String>, _seen: BTreeSet<String>) -> Vec<String> {
    // `_seen` already enforces uniqueness during insertion; `leaves`
    // preserves first-seen order. Returned as-is.
    leaves
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

/// Walk every STEP file under `corpus_path` and tally each entity in
/// `entity_names`. Returns a `ScanTally` with split standalone/complex
/// counts and a corpus-wide co-instantiation catalogue. The split is
/// driven by the STEP P21 instance grammar — an instance whose body
/// starts with `(` is a complex MI block; everything inside it is a
/// complex_part. Everything else is a standalone occurrence.
fn count_instances(corpus_path: &Path, entity_names: &[String]) -> ScanTally {
    let mut tally = ScanTally::default();
    if entity_names.is_empty() {
        return tally;
    }
    let recognised: HashSet<String> = entity_names.iter().cloned().collect();
    for_each_step_file_in_corpus(corpus_path, |text| {
        scan_step_text(text, &recognised, &mut tally);
    });
    tally
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
                ) && (counts.get(child).copied().unwrap_or(0) > 0
                    || keep_overrides.contains(child))
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
            ) && (counts.get(child).copied().unwrap_or(0) > 0
                || keep_overrides.contains(child))
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
        let suffix = if self_live_enum_bases.len() > 8 { ", ..." } else { "" };
        eprintln!(
            "infer prune: recovered {} self-instantiated EnumBase(s) (-> ConcreteSupertype/SingleStruct): {}{}",
            self_live_enum_bases.len(),
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

    fn recognised(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn scan_standalone_only() {
        let r = recognised(&["cartesian_point", "line"]);
        let text = "#1 = CARTESIAN_POINT('', (0, 0, 0));\n\
                    #2 = CARTESIAN_POINT('', (1, 1, 1));\n\
                    #3 = LINE('', #1, #4);\n";
        let mut t = ScanTally::default();
        scan_step_text(text, &r, &mut t);
        assert_eq!(t.total.get("cartesian_point").copied(), Some(2));
        assert_eq!(t.total.get("line").copied(), Some(1));
        // Standalone-only — complex_part stays 0.
        assert!(t.complex_part.is_empty());
        assert!(t.co_instantiated_with.is_empty());
    }

    #[test]
    fn scan_complex_block_one_line() {
        let r = recognised(&["geometric_tolerance", "position_tolerance", "geometric_tolerance_with_modifiers"]);
        // A real-shape complex MI instance on a single line.
        let text = "#16940=(GEOMETRIC_TOLERANCE('Position.4','',#16938,#16890)\
                   GEOMETRIC_TOLERANCE_WITH_MODIFIERS((.STATISTICAL_TOLERANCE.))\
                   POSITION_TOLERANCE()) ;\n";
        let mut t = ScanTally::default();
        scan_step_text(text, &r, &mut t);
        // All 3 leaves counted once each, all as complex-part.
        assert_eq!(t.total.get("geometric_tolerance").copied(), Some(1));
        assert_eq!(t.total.get("position_tolerance").copied(), Some(1));
        assert_eq!(t.total.get("geometric_tolerance_with_modifiers").copied(), Some(1));
        assert_eq!(t.complex_part.get("geometric_tolerance").copied(), Some(1));
        assert_eq!(t.complex_part.get("position_tolerance").copied(), Some(1));
        assert_eq!(t.complex_part.get("geometric_tolerance_with_modifiers").copied(), Some(1));
        // Standalone count is total - complex_part, so 0 for each.
        // co_instantiated_with: each leaf records the other two.
        let pos = t.co_instantiated_with.get("position_tolerance").unwrap();
        assert!(pos.contains("geometric_tolerance"));
        assert!(pos.contains("geometric_tolerance_with_modifiers"));
        assert!(!pos.contains("position_tolerance"));
    }

    #[test]
    fn scan_complex_block_multiline() {
        let r = recognised(&["flatness_tolerance", "geometric_tolerance", "geometric_tolerance_with_defined_unit"]);
        let text = "#37=(\n\
                    FLATNESS_TOLERANCE()\n\
                    GEOMETRIC_TOLERANCE('Flatness.1','',#232,#1113)\n\
                    GEOMETRIC_TOLERANCE_WITH_DEFINED_UNIT(#233)\n\
                    );\n";
        let mut t = ScanTally::default();
        scan_step_text(text, &r, &mut t);
        assert_eq!(t.complex_part.get("flatness_tolerance").copied(), Some(1));
        assert_eq!(t.complex_part.get("geometric_tolerance").copied(), Some(1));
        assert_eq!(t.complex_part.get("geometric_tolerance_with_defined_unit").copied(), Some(1));
    }

    #[test]
    fn scan_mixed_standalone_and_complex() {
        let r = recognised(&["cartesian_point", "position_tolerance", "geometric_tolerance"]);
        let text = "#1=CARTESIAN_POINT('',(0,0,0));\n\
                    #2=CARTESIAN_POINT('',(1,1,1));\n\
                    #99=(GEOMETRIC_TOLERANCE('p','',#1,#2)POSITION_TOLERANCE());\n";
        let mut t = ScanTally::default();
        scan_step_text(text, &r, &mut t);
        // cartesian_point: 2 standalone, 0 complex-part.
        assert_eq!(t.total.get("cartesian_point").copied(), Some(2));
        assert_eq!(t.complex_part.get("cartesian_point").copied().unwrap_or(0), 0);
        // geometric_tolerance + position_tolerance: each 1 complex-part.
        assert_eq!(t.complex_part.get("geometric_tolerance").copied(), Some(1));
        assert_eq!(t.complex_part.get("position_tolerance").copied(), Some(1));
    }

    #[test]
    fn scan_paren_inside_string_literal() {
        // A single-quoted string containing `)` must not close the
        // complex block early.
        let r = recognised(&["foo", "bar"]);
        let text = "#1=(FOO('has ) inside','also ('') doubled')BAR());\n";
        let mut t = ScanTally::default();
        scan_step_text(text, &r, &mut t);
        assert_eq!(t.complex_part.get("foo").copied(), Some(1));
        assert_eq!(
            t.complex_part.get("bar").copied(),
            Some(1),
            "BAR must still be tallied — string-literal escape must not bleed out"
        );
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
        let tally = count_instances(dir.path(), &names);
        assert!(tally.total.is_empty());
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
        let with_empty =
            prune_transitive_with_keep(&variants, &counts, &counts, &BTreeSet::new());
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
        let pruned =
            prune_transitive_with_keep(&variants, &counts, &standalone, &BTreeSet::new());
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
        let pruned =
            prune_transitive_with_keep(&variants, &counts, &standalone, &BTreeSet::new());
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
        let counts: HashMap<String, usize> =
            [("axis2_placement_3d".to_string(), 9_000_000)]
                .into_iter()
                .collect();
        // placement absent from standalone -> 0.
        let standalone = counts.clone();
        let pruned =
            prune_transitive_with_keep(&variants, &counts, &standalone, &BTreeSet::new());
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
        let pruned =
            prune_transitive_with_keep(&variants, &counts, &standalone, &BTreeSet::new());
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
        let counts: HashMap<String, usize> = [
            ("curve".to_string(), 330_000),
            ("line".to_string(), 50),
        ]
        .into_iter()
        .collect();
        // standalone: curve 0 (never a standalone instance), line 50.
        let standalone: HashMap<String, usize> =
            [("line".to_string(), 50)].into_iter().collect();
        let pruned =
            prune_transitive_with_keep(&variants, &counts, &standalone, &BTreeSet::new());
        assert!(
            matches!(pruned.get("curve"), Some(VariantSpec::EnumBase { .. })),
            "curve appears only as a complex part (standalone 0) -> must stay EnumBase"
        );
        assert!(matches!(
            pruned.get("line"),
            Some(VariantSpec::InEnum { .. })
        ));
    }
}
