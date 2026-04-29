//! Stage 1 — variant classification.
//!
//! Pure function: schema → `BTreeMap<String, VariantSpec>`. Every entity
//! gets a deterministic decision from the structural rules below, applied
//! in a two-pass topological walk:
//!
//!   - **Pass 1 (leaf-to-root)** assigns markers and supertype kinds:
//!     `ComplexSupertype` (ANDOR), `EnumBase` (ABSTRACT/ONEOF + ≥ 2
//!     effective children), `MergedInto` (≤ 1 effective child wrapper).
//!   - **Pass 2 (root-to-leaf)** assigns the remaining entities:
//!     `NestedField` (sole-extending child of a SingleStruct parent),
//!     `InEnum` (a member of some enclosing `EnumBase`/`ComplexSupertype`),
//!     and `SingleStruct` as the fallback.
//!
//! "Effective" lookups treat `MergedInto` markers as transparent: an
//! `effective_parent` skips marker chains, and `effective_direct_children`
//! resolves direct children that are markers to their merge target.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::express::{AttrType, Schema, SupertypeDecl};
use crate::infer::refgraph::{self, RefTarget, UnifiedSchema};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VariantSpec {
    /// Concrete entity that owns its own IR struct.
    SingleStruct,

    /// Member of a polymorphic enum (`InEnum.enum_name` is the enclosing
    /// enum root).
    InEnum {
        enum_name: String,
    },

    /// Sole-extending child folded into the parent struct as an optional
    /// nested field (`Option<NestedStruct>` in IR).
    NestedField {
        into: String,
        as_field: String,
        added_attr_count: usize,
    },

    /// Polymorphic supertype declared with ABSTRACT or `SUPERTYPE OF
    /// (ONEOF ...)` and with at least two effective children. Owns the
    /// enum definition; no IR struct.
    EnumBase {
        enum_name: String,
    },

    /// Wrapper supertype with a single effective descendant. Folded into
    /// `target`; the optional `chain` records intermediate entity names
    /// collapsed by transitive cascading (excluding the entity itself and
    /// excluding `target`).
    MergedInto {
        target: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        chain: Vec<String>,
    },

    /// `SUPERTYPE OF (ONEOF (...) ANDOR mixin)` — needs both an enum body
    /// and a mixin field in IR. Children info preserved for the IR
    /// author.
    ComplexSupertype {
        mixin_pattern: String,
        oneof_children: Vec<String>,
        mixin_children: Vec<String>,
    },
}

const FILE_CONFIDENT: &str = "variants.toml";
const SECTION: &str = "entity";

pub fn run(schemas: &[Schema]) -> Result<(), String> {
    let unified = refgraph::build(schemas);
    let decisions = classify_all(&unified);

    crate::infer::io::write_confident(FILE_CONFIDENT, SECTION, &decisions)
        .map_err(|e| format!("write {FILE_CONFIDENT}: {e}"))?;

    let counts = KindCounts::from(&decisions);
    eprintln!(
        "infer variant: {} entities (single={} enum={} nested={} enum_base={} merged_into={} complex={})",
        decisions.len(),
        counts.single,
        counts.in_enum,
        counts.nested,
        counts.enum_base,
        counts.merged_into,
        counts.complex,
    );
    Ok(())
}

#[derive(Default)]
struct KindCounts {
    single: usize,
    in_enum: usize,
    nested: usize,
    enum_base: usize,
    merged_into: usize,
    complex: usize,
}

impl KindCounts {
    fn from(decisions: &BTreeMap<String, VariantSpec>) -> Self {
        let mut k = KindCounts::default();
        for spec in decisions.values() {
            match spec {
                VariantSpec::SingleStruct => k.single += 1,
                VariantSpec::InEnum { .. } => k.in_enum += 1,
                VariantSpec::NestedField { .. } => k.nested += 1,
                VariantSpec::EnumBase { .. } => k.enum_base += 1,
                VariantSpec::MergedInto { .. } => k.merged_into += 1,
                VariantSpec::ComplexSupertype { .. } => k.complex += 1,
            }
        }
        k
    }
}

pub fn classify_all(unified: &UnifiedSchema) -> BTreeMap<String, VariantSpec> {
    let descendants = build_descendant_index(unified);
    let polymorphic_targets = collect_polymorphic_targets(unified);
    let topo = topological_order(unified);
    let reverse_topo: Vec<String> = topo.iter().rev().cloned().collect();

    let mut decisions = pass1_supertype_kinds(unified, &descendants, &reverse_topo);
    pass2_remaining_kinds(unified, &descendants, &polymorphic_targets, &topo, &mut decisions);
    decisions
}

// --- Pass 1: supertype kinds (leaf-to-root) -------------------------------

fn pass1_supertype_kinds(
    unified: &UnifiedSchema,
    descendants: &HashMap<String, Vec<String>>,
    reverse_topo: &[String],
) -> BTreeMap<String, VariantSpec> {
    let mut decisions: BTreeMap<String, VariantSpec> = BTreeMap::new();

    for entity in reverse_topo {
        // Rule 1: ANDOR pattern → ComplexSupertype.
        if let Some(SupertypeDecl::OneOfAndOr {
            oneof_children,
            mixin_children,
        }) = unified.supertype_decls.get(entity)
        {
            decisions.insert(
                entity.clone(),
                VariantSpec::ComplexSupertype {
                    mixin_pattern: "andor".to_string(),
                    oneof_children: oneof_children.clone(),
                    mixin_children: mixin_children.clone(),
                },
            );
            continue;
        }

        let has_oneof = matches!(
            unified.supertype_decls.get(entity),
            Some(SupertypeDecl::OneOf { .. }) | Some(SupertypeDecl::OneOfAndOr { .. })
        );
        let is_abstract = unified.abstract_entities.contains(entity);

        let eff_children = effective_direct_children(entity, descendants, &decisions);

        // Rule 2: ABSTRACT/ONEOF + effective children ≥ 2 → EnumBase.
        if (is_abstract || has_oneof) && eff_children.len() >= 2 {
            decisions.insert(
                entity.clone(),
                VariantSpec::EnumBase {
                    enum_name: entity.clone(),
                },
            );
            continue;
        }

        // Rule 3 (a): ABSTRACT/ONEOF + effective children = 1 → MergedInto.
        if (is_abstract || has_oneof) && eff_children.len() == 1 {
            let target = eff_children.iter().next().cloned().unwrap();
            let chain = build_chain(entity, &target, descendants, &decisions);
            decisions.insert(
                entity.clone(),
                VariantSpec::MergedInto { target, chain },
            );
            continue;
        }

        // Rule 4 (b): own_attrs empty + effective children = 1 (no
        // ABSTRACT/ONEOF marker) → MergedInto.
        let own_attrs_empty = unified
            .entity_attrs
            .get(entity)
            .map_or(true, |s| s.is_empty());
        if own_attrs_empty && eff_children.len() == 1 {
            let target = eff_children.iter().next().cloned().unwrap();
            let chain = build_chain(entity, &target, descendants, &decisions);
            decisions.insert(
                entity.clone(),
                VariantSpec::MergedInto { target, chain },
            );
            continue;
        }

        // Falls through to pass 2.
    }

    decisions
}

/// `entity`'s direct children, with each `MergedInto` child resolved to
/// its transitive merge target. Duplicates removed.
fn effective_direct_children(
    entity: &str,
    descendants: &HashMap<String, Vec<String>>,
    decisions: &BTreeMap<String, VariantSpec>,
) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    if let Some(direct) = descendants.get(entity) {
        for child in direct {
            let resolved = resolve_merge_target(child, decisions);
            out.insert(resolved);
        }
    }
    out
}

/// Walk a `MergedInto` chain to its terminal IR-bearing entity. Falls
/// back to the input on cycle / missing decision (treats as terminal).
fn resolve_merge_target(
    start: &str,
    decisions: &BTreeMap<String, VariantSpec>,
) -> String {
    let mut current = start.to_string();
    let mut visited: HashSet<String> = HashSet::new();
    while visited.insert(current.clone()) {
        match decisions.get(&current) {
            Some(VariantSpec::MergedInto { target, .. }) => {
                current = target.clone();
            }
            _ => return current,
        }
    }
    current
}

/// Intermediate `MergedInto` nodes between `entity` and `target`
/// (exclusive of both ends). For `a → b → c` with both a and b merged
/// into c: chain for a is `["b"]`, chain for b is `[]`.
fn build_chain(
    entity: &str,
    target: &str,
    descendants: &HashMap<String, Vec<String>>,
    decisions: &BTreeMap<String, VariantSpec>,
) -> Vec<String> {
    let direct = match descendants.get(entity) {
        Some(d) => d,
        None => return Vec::new(),
    };
    let immediate_child = direct.iter().next().cloned();
    let Some(start) = immediate_child else {
        return Vec::new();
    };
    let mut chain: Vec<String> = Vec::new();
    let mut current = start;
    let mut visited: HashSet<String> = HashSet::new();
    while visited.insert(current.clone()) {
        if current == target {
            return chain;
        }
        match decisions.get(&current) {
            Some(VariantSpec::MergedInto { target: t, .. }) => {
                chain.push(current.clone());
                current = t.clone();
            }
            _ => return chain,
        }
    }
    chain
}

// --- Pass 2: remaining kinds (root-to-leaf) -------------------------------

fn pass2_remaining_kinds(
    unified: &UnifiedSchema,
    descendants: &HashMap<String, Vec<String>>,
    polymorphic_targets: &HashSet<String>,
    topo: &[String],
    decisions: &mut BTreeMap<String, VariantSpec>,
) {
    for entity in topo {
        if decisions.contains_key(entity) {
            continue;
        }

        let eff_parent = effective_parent(entity, unified, decisions);

        // Rule 5: NestedField — only when effective parent is a SingleStruct.
        if let Some(parent) = &eff_parent {
            if matches!(decisions.get(parent), Some(VariantSpec::SingleStruct)) {
                if let Some(nested) =
                    try_nested_field(entity, parent, unified, descendants, polymorphic_targets)
                {
                    decisions.insert(entity.clone(), nested);
                    continue;
                }
            }
        }

        // Rule 6: InEnum — first EnumBase/ComplexSupertype on the
        // effective parent chain, falling back to polymorphic-target
        // detection for concrete supertypes that lack an explicit
        // ABSTRACT/ONEOF marker.
        if let Some(root) =
            enclosing_enum_root(entity, unified, descendants, polymorphic_targets, decisions)
        {
            decisions.insert(
                entity.clone(),
                VariantSpec::InEnum { enum_name: root },
            );
            continue;
        }

        // Rule 7: fallback.
        decisions.insert(entity.clone(), VariantSpec::SingleStruct);
    }
}

/// First non-marker ancestor on the parents-chain. `EnumBase` /
/// `ComplexSupertype` count as ancestors (chain stops there).
fn effective_parent(
    entity: &str,
    unified: &UnifiedSchema,
    decisions: &BTreeMap<String, VariantSpec>,
) -> Option<String> {
    let mut current = entity.to_string();
    let mut visited: HashSet<String> = HashSet::new();
    loop {
        if !visited.insert(current.clone()) {
            return None;
        }
        let parent = unified
            .entity_parents
            .get(&current)
            .and_then(|ps| ps.iter().next())
            .cloned()?;
        match decisions.get(&parent) {
            Some(VariantSpec::MergedInto { .. }) => {
                current = parent;
                continue;
            }
            _ => return Some(parent),
        }
    }
}

fn try_nested_field(
    entity: &str,
    parent: &str,
    unified: &UnifiedSchema,
    descendants: &HashMap<String, Vec<String>>,
    polymorphic_targets: &HashSet<String>,
) -> Option<VariantSpec> {
    let parents = unified.entity_parents.get(entity)?;
    if parents.len() != 1 {
        return None;
    }

    if polymorphic_targets.contains(entity) {
        return None;
    }

    let own_attrs = unified.entity_attrs.get(entity).cloned().unwrap_or_default();
    let parent_attrs = unified
        .entity_attrs
        .get(parent)
        .cloned()
        .unwrap_or_default();
    let extra: BTreeSet<String> = own_attrs.difference(&parent_attrs).cloned().collect();
    let added_count = extra.len();
    if added_count == 0 || added_count > 3 {
        return None;
    }

    let entity_attr_types = unified.entity_attr_types.get(entity);
    let all_extra_optional = extra.iter().all(|attr| {
        entity_attr_types
            .and_then(|m| m.get(attr))
            .map(|ty| matches!(ty, AttrType::Optional(_)))
            .unwrap_or(false)
    });
    if all_extra_optional {
        return None;
    }

    let extending_siblings = concrete_descendants(parent, descendants)
        .iter()
        .filter(|s| {
            let s_attrs = unified.entity_attrs.get(*s).cloned().unwrap_or_default();
            s_attrs.difference(&parent_attrs).next().is_some()
        })
        .count();
    if extending_siblings != 1 {
        return None;
    }

    let as_field = if added_count == 1 {
        extra.iter().next().cloned().unwrap()
    } else {
        format!("{entity}_ext")
    };

    Some(VariantSpec::NestedField {
        into: parent.to_string(),
        as_field,
        added_attr_count: added_count,
    })
}

fn enclosing_enum_root(
    entity: &str,
    unified: &UnifiedSchema,
    descendants: &HashMap<String, Vec<String>>,
    polymorphic_targets: &HashSet<String>,
    decisions: &BTreeMap<String, VariantSpec>,
) -> Option<String> {
    // Walk the parents chain (skipping markers); return the first
    // EnumBase / ComplexSupertype.
    let mut current = entity.to_string();
    let mut visited: HashSet<String> = HashSet::new();
    while visited.insert(current.clone()) {
        let parent = unified
            .entity_parents
            .get(&current)
            .and_then(|ps| ps.iter().next())
            .cloned();
        let Some(parent) = parent else {
            break;
        };
        match decisions.get(&parent) {
            Some(VariantSpec::EnumBase { enum_name }) => return Some(enum_name.clone()),
            Some(VariantSpec::ComplexSupertype { .. }) => return Some(parent),
            Some(VariantSpec::MergedInto { .. }) => {
                current = parent;
                continue;
            }
            _ => {
                current = parent;
                continue;
            }
        }
    }

    // Fallback: the legacy polymorphic-target rule. Picks the narrowest
    // ancestor that is itself referenced as an ATTR target *and* has at
    // least two concrete descendants, with `entity` among them.
    let mut chain: Vec<String> = vec![entity.to_string()];
    let mut current = entity.to_string();
    let mut visited: HashSet<String> = HashSet::new();
    loop {
        if !visited.insert(current.clone()) {
            break;
        }
        let Some(parent) = unified
            .entity_parents
            .get(&current)
            .and_then(|ps| ps.iter().next())
            .cloned()
        else {
            break;
        };
        chain.push(parent.clone());
        current = parent;
    }
    for candidate in chain {
        if !polymorphic_targets.contains(&candidate) {
            continue;
        }
        let concrete = concrete_descendants(&candidate, descendants);
        if concrete.len() < 2 {
            continue;
        }
        if !concrete.iter().any(|d| d == entity) && unified.entity_attrs.get(entity).is_some() {
            continue;
        }
        return Some(candidate);
    }
    None
}

// --- shared helpers --------------------------------------------------------

fn concrete_descendants(
    root: &str,
    descendants: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_string()];
    let mut visited = HashSet::new();
    while let Some(name) = stack.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        if let Some(children) = descendants.get(&name) {
            for c in children {
                stack.push(c.clone());
                out.push(c.clone());
            }
        }
    }
    out
}

fn build_descendant_index(unified: &UnifiedSchema) -> HashMap<String, Vec<String>> {
    let mut idx: HashMap<String, Vec<String>> = HashMap::new();
    for (child, parents) in &unified.entity_parents {
        for p in parents {
            idx.entry(p.clone()).or_default().push(child.clone());
        }
    }
    idx
}

fn collect_polymorphic_targets(unified: &UnifiedSchema) -> HashSet<String> {
    let mut out = HashSet::new();
    for edge in &unified.edges {
        if let RefTarget::Entity(target) = &edge.target {
            out.insert(target.clone());
        }
    }
    out
}

/// Forward topological order: parents before children. Cycle-safe — any
/// strongly-connected nodes are emitted in arbitrary order at the end.
fn topological_order(unified: &UnifiedSchema) -> Vec<String> {
    let mut indeg: BTreeMap<String, usize> = BTreeMap::new();
    for entity in unified.entity_parents.keys() {
        indeg.entry(entity.clone()).or_insert(0);
    }
    for parents in unified.entity_parents.values() {
        for parent in parents {
            indeg.entry(parent.clone()).or_insert(0);
        }
    }
    for (child, parents) in &unified.entity_parents {
        for parent in parents {
            if indeg.contains_key(parent) {
                *indeg.entry(child.clone()).or_insert(0) += 1;
            }
        }
    }

    let mut queue: Vec<String> = indeg
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(k, _)| k.clone())
        .collect();
    queue.sort();

    let mut out = Vec::with_capacity(indeg.len());
    let mut head = 0;
    while head < queue.len() {
        let cur = queue[head].clone();
        head += 1;
        out.push(cur.clone());
        // Find children of `cur`.
        for (child, parents) in &unified.entity_parents {
            if parents.contains(&cur) {
                if let Some(n) = indeg.get_mut(child) {
                    if *n > 0 {
                        *n -= 1;
                        if *n == 0 {
                            queue.push(child.clone());
                        }
                    }
                }
            }
        }
    }

    if out.len() < indeg.len() {
        // Cycle: append remaining entities deterministically so the
        // pipeline still completes.
        let placed: HashSet<String> = out.iter().cloned().collect();
        let mut leftover: Vec<String> = indeg
            .keys()
            .filter(|k| !placed.contains(*k))
            .cloned()
            .collect();
        leftover.sort();
        eprintln!(
            "warning: inheritance graph has a cycle ({} entities left over) — appended in arbitrary order",
            leftover.len()
        );
        out.extend(leftover);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::express::{AttrSpec, AttrType, EntitySchema, Schema, TypeDef};
    use std::collections::HashMap;

    fn schema(label: &str, ents: Vec<EntitySchema>, types: Vec<TypeDef>) -> Schema {
        let mut entities = HashMap::new();
        for e in ents {
            entities.insert(e.name.clone(), e);
        }
        let mut t = HashMap::new();
        for td in types {
            t.insert(td.name.clone(), td);
        }
        Schema {
            source_label: label.to_string(),
            entities,
            types: t,
            parse_warnings: Vec::new(),
        }
    }

    fn ent(name: &str, parents: &[&str], attrs: Vec<(&str, AttrType)>) -> EntitySchema {
        EntitySchema {
            name: name.to_string(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            own_attrs: attrs
                .into_iter()
                .map(|(n, ty)| AttrSpec {
                    name: n.to_string(),
                    ty,
                })
                .collect(),
            is_abstract: false,
            supertype_decl: None,
        }
    }

    fn ent_decl(
        name: &str,
        parents: &[&str],
        attrs: Vec<(&str, AttrType)>,
        is_abstract: bool,
        supertype_decl: Option<SupertypeDecl>,
    ) -> EntitySchema {
        EntitySchema {
            name: name.to_string(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            own_attrs: attrs
                .into_iter()
                .map(|(n, ty)| AttrSpec {
                    name: n.to_string(),
                    ty,
                })
                .collect(),
            is_abstract,
            supertype_decl,
        }
    }

    #[test]
    fn enum_candidate_via_polymorphic_select() {
        let s = schema(
            "test",
            vec![
                ent("surface", &[], vec![]),
                ent("plane", &["surface"], vec![]),
                ent("cylinder", &["surface"], vec![]),
                ent(
                    "user",
                    &[],
                    vec![("geom", AttrType::Entity("surface".into()))],
                ),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_all(&unified);
        match decisions.get("plane").unwrap() {
            VariantSpec::InEnum { enum_name } => assert_eq!(enum_name, "surface"),
            other => panic!("expected InEnum, got {other:?}"),
        }
    }

    #[test]
    fn enum_base_when_oneof_declared() {
        let s = schema(
            "test",
            vec![
                ent_decl(
                    "shape",
                    &[],
                    vec![],
                    false,
                    Some(SupertypeDecl::OneOf {
                        children: vec!["circle".into(), "square".into()],
                    }),
                ),
                ent("circle", &["shape"], vec![("r", AttrType::Primitive("REAL".into()))]),
                ent("square", &["shape"], vec![("s", AttrType::Primitive("REAL".into()))]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_all(&unified);
        match decisions.get("shape").unwrap() {
            VariantSpec::EnumBase { enum_name } => assert_eq!(enum_name, "shape"),
            other => panic!("expected EnumBase, got {other:?}"),
        }
        match decisions.get("circle").unwrap() {
            VariantSpec::InEnum { enum_name } => assert_eq!(enum_name, "shape"),
            other => panic!("expected InEnum, got {other:?}"),
        }
    }

    #[test]
    fn enum_base_when_abstract_only() {
        let s = schema(
            "test",
            vec![
                ent_decl("base", &[], vec![], true, None),
                ent("a", &["base"], vec![("x", AttrType::Primitive("REAL".into()))]),
                ent("b", &["base"], vec![("y", AttrType::Primitive("REAL".into()))]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_all(&unified);
        assert!(matches!(
            decisions.get("base").unwrap(),
            VariantSpec::EnumBase { .. }
        ));
    }

    #[test]
    fn complex_supertype_for_andor() {
        let s = schema(
            "test",
            vec![
                ent_decl(
                    "kpair",
                    &[],
                    vec![("j", AttrType::Primitive("REAL".into()))],
                    true,
                    Some(SupertypeDecl::OneOfAndOr {
                        oneof_children: vec!["high".into(), "low".into()],
                        mixin_children: vec!["actuated".into()],
                    }),
                ),
                ent("high", &["kpair"], vec![("a", AttrType::Primitive("REAL".into()))]),
                ent("low", &["kpair"], vec![("b", AttrType::Primitive("REAL".into()))]),
                ent("actuated", &["kpair"], vec![("c", AttrType::Primitive("REAL".into()))]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_all(&unified);
        match decisions.get("kpair").unwrap() {
            VariantSpec::ComplexSupertype {
                mixin_pattern,
                oneof_children,
                mixin_children,
            } => {
                assert_eq!(mixin_pattern, "andor");
                assert_eq!(oneof_children, &vec!["high".to_string(), "low".to_string()]);
                assert_eq!(mixin_children, &vec!["actuated".to_string()]);
            }
            other => panic!("expected ComplexSupertype, got {other:?}"),
        }
    }

    #[test]
    fn merged_into_when_oneof_with_single_child() {
        let s = schema(
            "test",
            vec![
                ent_decl(
                    "wrapper",
                    &[],
                    vec![],
                    true,
                    Some(SupertypeDecl::OneOf {
                        children: vec!["only".into()],
                    }),
                ),
                ent("only", &["wrapper"], vec![("v", AttrType::Primitive("REAL".into()))]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_all(&unified);
        match decisions.get("wrapper").unwrap() {
            VariantSpec::MergedInto { target, chain } => {
                assert_eq!(target, "only");
                assert!(chain.is_empty());
            }
            other => panic!("expected MergedInto, got {other:?}"),
        }
    }

    #[test]
    fn merged_into_cascading_chain() {
        let s = schema(
            "test",
            vec![
                ent("a", &[], vec![]),
                ent("b", &["a"], vec![]),
                ent("c", &["b"], vec![("v", AttrType::Primitive("REAL".into()))]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_all(&unified);
        match decisions.get("a").unwrap() {
            VariantSpec::MergedInto { target, chain } => {
                assert_eq!(target, "c");
                assert_eq!(chain, &vec!["b".to_string()]);
            }
            other => panic!("expected MergedInto for a, got {other:?}"),
        }
        match decisions.get("b").unwrap() {
            VariantSpec::MergedInto { target, chain } => {
                assert_eq!(target, "c");
                assert!(chain.is_empty());
            }
            other => panic!("expected MergedInto for b, got {other:?}"),
        }
        assert!(matches!(decisions.get("c").unwrap(), VariantSpec::SingleStruct));
    }

    #[test]
    fn nested_field_for_unique_extending_subtype() {
        let s = schema(
            "test",
            vec![
                ent("base", &[], vec![("x", AttrType::Primitive("INTEGER".into()))]),
                ent(
                    "ext",
                    &["base"],
                    vec![
                        ("x", AttrType::Primitive("INTEGER".into())),
                        ("y", AttrType::Primitive("REAL".into())),
                    ],
                ),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_all(&unified);
        match decisions.get("ext").unwrap() {
            VariantSpec::NestedField {
                into,
                as_field,
                added_attr_count,
            } => {
                assert_eq!(into, "base");
                assert_eq!(as_field, "y");
                assert_eq!(*added_attr_count, 1);
            }
            other => panic!("expected NestedField, got {other:?}"),
        }
    }

    #[test]
    fn nested_field_with_two_added_attrs_carries_count() {
        let s = schema(
            "test",
            vec![
                ent("base", &[], vec![("x", AttrType::Primitive("INTEGER".into()))]),
                ent(
                    "ext",
                    &["base"],
                    vec![
                        ("x", AttrType::Primitive("INTEGER".into())),
                        ("a", AttrType::Primitive("REAL".into())),
                        ("b", AttrType::Primitive("STRING".into())),
                    ],
                ),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_all(&unified);
        match decisions.get("ext").unwrap() {
            VariantSpec::NestedField {
                into,
                as_field,
                added_attr_count,
            } => {
                assert_eq!(into, "base");
                assert_eq!(as_field, "ext_ext");
                assert_eq!(*added_attr_count, 2);
            }
            other => panic!("expected NestedField, got {other:?}"),
        }
    }

    #[test]
    fn nested_field_rejected_when_all_extra_optional() {
        let s = schema(
            "test",
            vec![
                ent("base", &[], vec![("x", AttrType::Primitive("INTEGER".into()))]),
                ent(
                    "ext",
                    &["base"],
                    vec![
                        ("x", AttrType::Primitive("INTEGER".into())),
                        (
                            "a",
                            AttrType::Optional(Box::new(AttrType::Primitive("REAL".into()))),
                        ),
                        (
                            "b",
                            AttrType::Optional(Box::new(AttrType::Primitive("STRING".into()))),
                        ),
                    ],
                ),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_all(&unified);
        let ext = decisions.get("ext").unwrap();
        assert!(
            !matches!(ext, VariantSpec::NestedField { .. }),
            "ext should not be NestedField (all extras OPTIONAL), got {ext:?}"
        );
    }

    #[test]
    fn nested_field_kept_when_some_extra_non_optional() {
        let s = schema(
            "test",
            vec![
                ent("base", &[], vec![("x", AttrType::Primitive("INTEGER".into()))]),
                ent(
                    "ext",
                    &["base"],
                    vec![
                        ("x", AttrType::Primitive("INTEGER".into())),
                        (
                            "a",
                            AttrType::Optional(Box::new(AttrType::Primitive("REAL".into()))),
                        ),
                        ("b", AttrType::Primitive("STRING".into())),
                    ],
                ),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_all(&unified);
        match decisions.get("ext").unwrap() {
            VariantSpec::NestedField {
                added_attr_count, ..
            } => assert_eq!(*added_attr_count, 2),
            other => panic!("expected NestedField, got {other:?}"),
        }
    }

    #[test]
    fn single_struct_for_isolated_entity() {
        let s = schema(
            "test",
            vec![ent(
                "foo",
                &[],
                vec![("x", AttrType::Primitive("INTEGER".into()))],
            )],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_all(&unified);
        assert!(matches!(
            decisions.get("foo").unwrap(),
            VariantSpec::SingleStruct
        ));
    }

    #[test]
    fn sibling_with_different_extra_attr_is_not_nested() {
        let s = schema(
            "test",
            vec![
                ent("base", &[], vec![("x", AttrType::Primitive("INTEGER".into()))]),
                ent(
                    "sub_a",
                    &["base"],
                    vec![
                        ("x", AttrType::Primitive("INTEGER".into())),
                        ("a", AttrType::Primitive("REAL".into())),
                    ],
                ),
                ent(
                    "sub_b",
                    &["base"],
                    vec![
                        ("x", AttrType::Primitive("INTEGER".into())),
                        ("b", AttrType::Primitive("STRING".into())),
                    ],
                ),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_all(&unified);
        for name in ["sub_a", "sub_b"] {
            let d = decisions.get(name).unwrap();
            assert!(
                !matches!(d, VariantSpec::NestedField { .. }),
                "{name}: should not be NestedField (sibling extends too)"
            );
        }
    }
}
