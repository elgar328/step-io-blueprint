//! Stage 3 — pool classification.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::express::Schema;
use crate::infer::arena::ArenaSpec;
use crate::infer::io::{PendingFile, PendingStats};
use crate::infer::overrides::{self, OverrideFile};
use crate::infer::refgraph::{self, RefTarget, UnifiedSchema};
use crate::infer::variant::VariantSpec;
use crate::infer::{Bucket, Confidence, Decision, DecisionSource, InferResult, Unresolved};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSpec {
    pub pool: String,
}

const FILE_CONFIDENT: &str = "pools.toml";
const FILE_PENDING: &str = "pools_pending.toml";
const FILE_OVERRIDES: &str = "pools_overrides.toml";
const SECTION: &str = "arena";

const VARIANT_CONFIDENT: &str = "variants_pruned.toml";
const ARENA_CONFIDENT: &str = "arenas_pruned.toml";
const ARENA_PENDING: &str = "arenas_pending.toml";

pub fn run(schemas: &[Schema], allow_pending: bool) -> Result<(), String> {
    if !allow_pending && crate::infer::io::pending_exists(ARENA_PENDING) {
        return Err(format!(
            "{ARENA_PENDING} exists — arena stage has unresolved/review items.\n\
             Resolve in arenas_overrides.toml or pass --allow-pending."
        ));
    }

    let variants: BTreeMap<String, VariantSpec> =
        crate::infer::io::read_confident(VARIANT_CONFIDENT, "entity")
            .map_err(|e| format!("read {VARIANT_CONFIDENT}: {e}"))?;
    let arenas: BTreeMap<String, Decision<ArenaSpec>> =
        crate::infer::io::read_confident(ARENA_CONFIDENT, "group")
            .map_err(|e| format!("read {ARENA_CONFIDENT}: {e}"))?;
    if arenas.is_empty() {
        return Err(format!(
            "{ARENA_CONFIDENT} is empty — run `infer prune --corpus <path>` first."
        ));
    }

    let entity_to_group = compute_entity_to_group(&variants);
    let group_to_arena = compute_group_to_arena(&arenas);
    let entity_to_arena = compute_entity_to_arena(&entity_to_group, &group_to_arena);

    let unified = refgraph::build(schemas);
    let arena_edges = compute_arena_edges(&unified, &entity_to_arena);

    let arena_set: BTreeSet<String> = arenas.values().map(|d| d.data.arena.clone()).collect();
    let pool_assignment = compute_pools(&arena_set, &arena_edges);

    let overrides_file: OverrideFile<PoolSpec> =
        overrides::load(FILE_OVERRIDES).map_err(|e| format!("load overrides: {e}"))?;

    let mut errs = overrides::validate_known(&overrides_file, SECTION, &arena_set, FILE_OVERRIDES);
    errs.extend(overrides::validate_no_conflict(
        &overrides_file,
        SECTION,
        FILE_OVERRIDES,
    ));
    if !errs.is_empty() {
        return Err(errs.join("\n"));
    }

    let auto = compute_auto_decisions(&arena_set, &pool_assignment, &arena_edges);
    let result = merge_overrides(auto, &overrides_file)?;

    crate::infer::io::write_confident(FILE_CONFIDENT, SECTION, &result.confident)
        .map_err(|e| format!("write {FILE_CONFIDENT}: {e}"))?;

    let pending = PendingFile {
        stats: PendingStats {
            total: result.confident.len() + result.review.len() + result.unresolved.len(),
            confident: result.confident.len(),
            review: result.review.len(),
            unresolved: result.unresolved.len(),
        },
        review: result.review,
        unresolved: result.unresolved,
    };
    crate::infer::io::write_pending(FILE_PENDING, &pending)
        .map_err(|e| format!("write {FILE_PENDING}: {e}"))?;

    let pool_count = pool_assignment.values().collect::<BTreeSet<_>>().len();
    eprintln!(
        "infer pool: confident={} review={} unresolved={} (total={}, distinct pools={})",
        pending.stats.confident,
        pending.stats.review,
        pending.stats.unresolved,
        pending.stats.total,
        pool_count,
    );
    Ok(())
}

fn compute_entity_to_group(
    variants: &BTreeMap<String, VariantSpec>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (entity, spec) in variants {
        let group = match spec {
            VariantSpec::SingleStruct => entity.clone(),
            VariantSpec::InEnum { enum_name } => enum_name.clone(),
            VariantSpec::NestedField { into, .. } => into.clone(),
            VariantSpec::EnumBase { enum_name } => enum_name.clone(),
            VariantSpec::ComplexSupertype { .. } => entity.clone(),
            VariantSpec::CompositeOneOf { .. } => entity.clone(),
            VariantSpec::ConcreteSupertype => entity.clone(),
            VariantSpec::MergedInto { target, .. } => target.clone(),
        };
        out.insert(entity.clone(), group);
    }
    out
}

fn compute_group_to_arena(
    arenas: &BTreeMap<String, Decision<ArenaSpec>>,
) -> HashMap<String, String> {
    arenas
        .iter()
        .map(|(g, d)| (g.clone(), d.data.arena.clone()))
        .collect()
}

fn compute_entity_to_arena(
    entity_to_group: &HashMap<String, String>,
    group_to_arena: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (entity, group) in entity_to_group {
        if let Some(arena) = group_to_arena.get(group) {
            out.insert(entity.clone(), arena.clone());
        }
    }
    out
}

fn compute_arena_edges(
    unified: &UnifiedSchema,
    entity_to_arena: &HashMap<String, String>,
) -> HashMap<(String, String), usize> {
    let mut out: HashMap<(String, String), usize> = HashMap::new();
    for edge in &unified.edges {
        let Some(from_arena) = entity_to_arena.get(&edge.from).cloned() else {
            continue;
        };
        let to_arena = match &edge.target {
            RefTarget::Entity(t) => match entity_to_arena.get(t).cloned() {
                Some(a) => a,
                None => continue,
            },
            _ => continue,
        };
        if from_arena == to_arena {
            continue;
        }
        *out.entry((from_arena, to_arena)).or_insert(0) += 1;
    }
    out
}

fn compute_pools(
    arenas: &BTreeSet<String>,
    edges: &HashMap<(String, String), usize>,
) -> HashMap<String, String> {
    let mut parent: HashMap<String, String> = arenas.iter().map(|a| (a.clone(), a.clone())).collect();

    fn find(parent: &mut HashMap<String, String>, x: &str) -> String {
        let p = parent.get(x).cloned().unwrap_or_else(|| x.to_string());
        if p == x {
            return p;
        }
        let r = find(parent, &p);
        parent.insert(x.to_string(), r.clone());
        r
    }
    fn union(parent: &mut HashMap<String, String>, a: &str, b: &str) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            let (canon, other) = if ra < rb { (ra, rb) } else { (rb, ra) };
            parent.insert(other, canon);
        }
    }

    let mut sorted_edges: Vec<&(String, String)> = edges.keys().collect();
    sorted_edges.sort();
    for (from, to) in sorted_edges {
        union(&mut parent, from, to);
    }

    let mut out = HashMap::new();
    let names: Vec<String> = parent.keys().cloned().collect();
    for name in names {
        let root = find(&mut parent, &name);
        out.insert(name, root);
    }
    out
}

struct AutoDecisions {
    arenas: BTreeMap<String, AutoEntry>,
}

enum AutoEntry {
    Decided(Decision<PoolSpec>),
    #[allow(dead_code)]
    Unresolved(Unresolved),
}

fn compute_auto_decisions(
    arenas: &BTreeSet<String>,
    pool_assignment: &HashMap<String, String>,
    arena_edges: &HashMap<(String, String), usize>,
) -> AutoDecisions {
    let mut out: BTreeMap<String, AutoEntry> = BTreeMap::new();
    for arena in arenas {
        let pool = pool_assignment
            .get(arena)
            .cloned()
            .unwrap_or_else(|| arena.clone());
        let conf = arena_pool_confidence(arena, &pool, pool_assignment, arena_edges);
        out.insert(
            arena.clone(),
            AutoEntry::Decided(Decision {
                data: PoolSpec { pool: pool.clone() },
                source: DecisionSource::Auto,
                confidence: conf,
                reasons: vec![format!("connected component pool {pool:?}")],
            }),
        );
    }
    AutoDecisions { arenas: out }
}

fn arena_pool_confidence(
    arena: &str,
    pool: &str,
    pool_assignment: &HashMap<String, String>,
    arena_edges: &HashMap<(String, String), usize>,
) -> Confidence {
    let mut same = 0usize;
    let mut cross = 0usize;
    for ((from, to), count) in arena_edges {
        if from != arena && to != arena {
            continue;
        }
        let same_pool = pool_assignment.get(from) == Some(&pool.to_string())
            && pool_assignment.get(to) == Some(&pool.to_string());
        if same_pool {
            same += count;
        } else {
            cross += count;
        }
    }
    let total = same + cross;
    let ratio = if total == 0 {
        1.0
    } else {
        same as f32 / total as f32
    };
    Confidence::new(0.6 * ratio + 0.4 * ratio)
}

fn merge_overrides(
    auto: AutoDecisions,
    overrides_file: &OverrideFile<PoolSpec>,
) -> Result<InferResult<PoolSpec>, String> {
    let mut confident = BTreeMap::new();
    let mut review = BTreeMap::new();
    let mut unresolved = BTreeMap::new();
    let mut errors = Vec::new();
    let accept_set: BTreeSet<&String> = overrides_file.batch_accept.entries.iter().collect();

    for (key, entry) in auto.arenas {
        if let Some(spec) = overrides_file.arena.get(&key) {
            let prior_conf = match &entry {
                AutoEntry::Decided(d) => d.confidence,
                AutoEntry::Unresolved(_) => Confidence::new(1.0),
            };
            confident.insert(
                key,
                Decision {
                    data: spec.clone(),
                    source: DecisionSource::Override,
                    confidence: prior_conf,
                    reasons: Vec::new(),
                },
            );
            continue;
        }
        if accept_set.contains(&key) {
            match entry {
                AutoEntry::Decided(d) => match d.bucket() {
                    Bucket::Confident => {
                        errors.push(format!(
                            "{FILE_OVERRIDES}: batch_accept.entries lists {key:?}, but already confident."
                        ));
                    }
                    Bucket::Review => {
                        confident.insert(
                            key,
                            Decision {
                                data: d.data,
                                source: DecisionSource::Accepted,
                                confidence: d.confidence,
                                reasons: Vec::new(),
                            },
                        );
                    }
                    Bucket::Unresolved => {
                        errors.push(format!(
                            "{FILE_OVERRIDES}: batch_accept.entries lists {key:?}, but unresolved. Use explicit override."
                        ));
                    }
                },
                AutoEntry::Unresolved(_) => {
                    errors.push(format!(
                        "{FILE_OVERRIDES}: batch_accept.entries lists {key:?}, but unresolved."
                    ));
                }
            }
            continue;
        }
        match entry {
            AutoEntry::Decided(d) => match d.bucket() {
                Bucket::Confident => {
                    confident.insert(key, d);
                }
                Bucket::Review => {
                    review.insert(key, d);
                }
                Bucket::Unresolved => {
                    unresolved.insert(
                        key,
                        Unresolved {
                            reasons: d.reasons,
                            override_example: "pool = \"some_pool_name\"".to_string(),
                        },
                    );
                }
            },
            AutoEntry::Unresolved(u) => {
                unresolved.insert(key, u);
            }
        }
    }

    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }

    Ok(InferResult {
        confident,
        review,
        unresolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connected_components_form_pools() {
        let arenas: BTreeSet<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut edges = HashMap::new();
        edges.insert(("a".to_string(), "b".to_string()), 1);
        edges.insert(("c".to_string(), "d".to_string()), 1);
        let assign = compute_pools(&arenas, &edges);
        assert_eq!(assign["a"], assign["b"]);
        assert_eq!(assign["c"], assign["d"]);
        assert_ne!(assign["a"], assign["c"]);
    }

    #[test]
    fn isolated_arena_is_its_own_pool() {
        let arenas: BTreeSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let edges = HashMap::new();
        let assign = compute_pools(&arenas, &edges);
        assert_eq!(assign["a"], "a");
        assert_eq!(assign["b"], "b");
    }

    #[test]
    fn pool_assignment_is_deterministic() {
        let arenas: BTreeSet<String> = ["a", "b", "c"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut edges = HashMap::new();
        edges.insert(("a".to_string(), "b".to_string()), 1);
        edges.insert(("b".to_string(), "c".to_string()), 1);
        let a1 = compute_pools(&arenas, &edges);
        let a2 = compute_pools(&arenas, &edges);
        assert_eq!(a1, a2);
        // canonical = lexicographic min in component
        assert_eq!(a1["a"], "a");
        assert_eq!(a1["b"], "a");
        assert_eq!(a1["c"], "a");
    }
}
