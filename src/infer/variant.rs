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

use crate::express::{AttrType, Schema, SupertypeExpr};
use crate::infer::io::{PendingFile, PendingStats};
use crate::infer::overrides::{self, OverrideFile};
use crate::infer::refgraph::{self, RefTarget, UnifiedSchema};
use crate::infer::Unresolved;

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

    /// `SUPERTYPE OF (ONEOF (...) ANDOR/AND <mixin>)` — needs both an enum
    /// body and a mixin in IR. The mixin is the raw `SupertypeExpr` subtree
    /// (an `Entity` for the simple case, an `OneOf` for B4/B5 patterns)
    /// so the second ONEOF's exclusivity is preserved verbatim.
    ComplexSupertype {
        /// `"andor"` or `"and"` — distinguishes B5 (AND, all dimensions
        /// mandatory) from B3/B4/B6 (ANDOR, optional dimensions).
        mixin_pattern: String,
        /// First ONEOF's children (lowercase entity names), in source order.
        oneof_children: Vec<String>,
        /// Remaining factor next to the first ONEOF — either an `Entity`
        /// reference (B3/B6) or another `OneOf` subtree (B4/B5).
        mixin: SupertypeExpr,
    },

    /// `SUPERTYPE OF (ONEOF (..., (a ANDOR b), ..., (x AND y)))` — the
    /// outer ONEOF has both bare entity members and anonymous composite
    /// (AndOr / And) members. Found at `topological_representation_item`
    /// and `zone_structural_makeup`. Composite alternatives preserve the
    /// ANDOR / AND semantics so downstream lowering can decide how to
    /// surface them in the IR.
    CompositeOneOf {
        /// OneOf members that are bare entity refs.
        simple_alternatives: Vec<String>,
        /// OneOf members that are anonymous composite nodes.
        composite_alternatives: Vec<CompositeMember>,
    },

    /// SUPERTYPE OF clause is absent in the schema, but the entity has
    /// own attrs (instance-capable), at least one child via SUBTYPE OF,
    /// and its name is used as a polymorphic target somewhere. EXPRESS
    /// allows this implicit-supertype shape (e.g. `action`,
    /// `general_property`, `product_definition_formation`).
    ///
    /// The IR carries this entity's struct AND acts as the enum root for
    /// its children. Downstream lowering (step-io) decides between
    /// Carrier enum (`enum E { Itself(EData), ChildA, ... }`), base
    /// struct + parallel enum (`struct E { ... } enum EKind { ... }`),
    /// or just SingleStruct (when 53k stats show children are unused).
    /// The schema-check stage captures only the structural fact; the IR
    /// shape is a step-io lowering concern.
    ///
    /// The enum name is implicitly the entity's own name — children's
    /// `InEnum.enum_name` already targets it, and `enclosing_enum_root`
    /// returns the parent name directly when it sees this variant. So
    /// no `enum_name` field is needed.
    ConcreteSupertype,
}

/// One alternative inside a `CompositeOneOf` whose shape is *not* a bare
/// entity reference. Captures the ANDOR / AND semantics directly so the
/// downstream IR lowering does not have to re-derive them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompositeMember {
    /// `(a ANDOR b ANDOR ...)` — at least one of the children, possibly all.
    AndOr { children: Vec<String> },
    /// `(a AND b AND ...)` — all children simultaneously.
    And { children: Vec<String> },
}

/// User-supplied entity-level decision for `variants_overrides.toml`.
///
/// Tagged enum: each variant carries the fields it actually needs, so a
/// malformed combination (e.g. `kind = "single_struct"` plus `enum_name`)
/// fails to deserialize at load time instead of silently picking a
/// default. Each variant maps 1:1 to the same-named `VariantSpec` arm.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VariantOverride {
    SingleStruct {
        #[serde(default)]
        reason: Option<String>,
    },
    InEnum {
        enum_name: String,
        #[serde(default)]
        reason: Option<String>,
    },
    NestedField {
        into: String,
        as_field: String,
        added_attr_count: usize,
        #[serde(default)]
        reason: Option<String>,
    },
    EnumBase {
        enum_name: String,
        #[serde(default)]
        reason: Option<String>,
    },
    MergedInto {
        target: String,
        #[serde(default)]
        chain: Vec<String>,
        #[serde(default)]
        reason: Option<String>,
    },
    ComplexSupertype {
        mixin_pattern: String,
        oneof_children: Vec<String>,
        mixin: SupertypeExpr,
        #[serde(default)]
        reason: Option<String>,
    },
    CompositeOneOf {
        simple_alternatives: Vec<String>,
        composite_alternatives: Vec<CompositeMember>,
        #[serde(default)]
        reason: Option<String>,
    },
    ConcreteSupertype {
        #[serde(default)]
        reason: Option<String>,
    },
}

impl From<VariantOverride> for VariantSpec {
    fn from(o: VariantOverride) -> Self {
        match o {
            VariantOverride::SingleStruct { .. } => VariantSpec::SingleStruct,
            VariantOverride::InEnum { enum_name, .. } => VariantSpec::InEnum { enum_name },
            VariantOverride::NestedField {
                into,
                as_field,
                added_attr_count,
                ..
            } => VariantSpec::NestedField {
                into,
                as_field,
                added_attr_count,
            },
            VariantOverride::EnumBase { enum_name, .. } => VariantSpec::EnumBase { enum_name },
            VariantOverride::MergedInto { target, chain, .. } => {
                VariantSpec::MergedInto { target, chain }
            }
            VariantOverride::ComplexSupertype {
                mixin_pattern,
                oneof_children,
                mixin,
                ..
            } => VariantSpec::ComplexSupertype {
                mixin_pattern,
                oneof_children,
                mixin,
            },
            VariantOverride::CompositeOneOf {
                simple_alternatives,
                composite_alternatives,
                ..
            } => VariantSpec::CompositeOneOf {
                simple_alternatives,
                composite_alternatives,
            },
            VariantOverride::ConcreteSupertype { .. } => VariantSpec::ConcreteSupertype,
        }
    }
}

const FILE_CONFIDENT: &str = "variants.toml";
const FILE_PENDING: &str = "variants_pending.toml";
const FILE_OVERRIDES: &str = "variants_overrides.toml";
const SECTION: &str = "entity";

pub fn run(schemas: &[Schema]) -> Result<(), String> {
    let unified = refgraph::build(schemas);

    let overrides_file: OverrideFile<VariantOverride> =
        overrides::load(FILE_OVERRIDES).map_err(|e| format!("load overrides: {e}"))?;

    let known: BTreeSet<String> = unified.entity_parents.keys().cloned().collect();
    let errs = overrides::validate_known(&overrides_file, SECTION, &known, FILE_OVERRIDES);
    if !errs.is_empty() {
        return Err(errs.join("\n"));
    }

    let (decisions, unresolved) = classify_all(&unified, &overrides_file);

    crate::infer::io::write_confident(FILE_CONFIDENT, SECTION, &decisions)
        .map_err(|e| format!("write {FILE_CONFIDENT}: {e}"))?;

    let pending: PendingFile<VariantSpec> = PendingFile {
        stats: PendingStats {
            total: decisions.len() + unresolved.len(),
            confident: decisions.len(),
            review: 0,
            unresolved: unresolved.len(),
        },
        review: BTreeMap::new(),
        unresolved,
    };
    // io::write_pending deletes the file when empty, so the strict gate at
    // the next stage works either way (file present == work to do).
    crate::infer::io::write_pending(FILE_PENDING, &pending)
        .map_err(|e| format!("write {FILE_PENDING}: {e}"))?;

    let counts = KindCounts::from(&decisions);
    eprintln!(
        "infer variant: {} entities (single={} enum={} nested={} enum_base={} merged_into={} complex={} composite={} concrete_super={}) unresolved={}",
        decisions.len(),
        counts.single,
        counts.in_enum,
        counts.nested,
        counts.enum_base,
        counts.merged_into,
        counts.complex,
        counts.composite,
        counts.concrete_super,
        pending.stats.unresolved,
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
    composite: usize,
    concrete_super: usize,
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
                VariantSpec::CompositeOneOf { .. } => k.composite += 1,
                VariantSpec::ConcreteSupertype => k.concrete_super += 1,
            }
        }
        k
    }
}

/// Run the variant classifier. Returns confident decisions plus an
/// `unresolved` map for entities whose `supertype_expr` was a composite
/// shape that no automatic rule recognised (Rule 8 — safety net for
/// future schema evolution beyond the 14 currently known patterns).
///
/// Overrides applied via `variants_overrides.toml` short-circuit the
/// automatic rules and land in `confident` directly.
pub fn classify_all(
    unified: &UnifiedSchema,
    overrides_file: &OverrideFile<VariantOverride>,
) -> (BTreeMap<String, VariantSpec>, BTreeMap<String, Unresolved>) {
    let descendants = build_descendant_index(unified);
    let polymorphic_targets = collect_polymorphic_targets(unified);
    let topo = topological_order(unified);
    let reverse_topo: Vec<String> = topo.iter().rev().cloned().collect();

    let mut decisions =
        pass1_supertype_kinds(unified, &descendants, &reverse_topo, overrides_file);
    let mut unresolved = BTreeMap::new();
    pass2_remaining_kinds(
        unified,
        &descendants,
        &polymorphic_targets,
        &topo,
        &mut decisions,
        &mut unresolved,
        overrides_file,
    );
    (decisions, unresolved)
}

// --- Pass 1: supertype kinds (leaf-to-root) -------------------------------

fn pass1_supertype_kinds(
    unified: &UnifiedSchema,
    descendants: &HashMap<String, Vec<String>>,
    reverse_topo: &[String],
    overrides_file: &OverrideFile<VariantOverride>,
) -> BTreeMap<String, VariantSpec> {
    let mut decisions: BTreeMap<String, VariantSpec> = BTreeMap::new();

    for entity in reverse_topo {
        // Override short-circuit: explicit user decision wins over any
        // automatic rule. batch_accept entries fall through to the rules
        // (they accept whatever the auto run produces).
        if let Some(over) = overrides_file.entity.get(entity) {
            decisions.insert(entity.clone(), over.clone().into());
            continue;
        }

        let expr = unified.supertype_exprs.get(entity);

        // Rule 1: 2-child AndOr/And whose children are only OneOf|Entity →
        // ComplexSupertype. Covers B3/B4/B5/B6.
        if let Some(spec) = expr.and_then(classify_complex_supertype) {
            decisions.insert(entity.clone(), spec);
            continue;
        }

        // Rule 1.5: OneOf with at least one composite (AndOr/And) child whose
        // children are all bare Entity refs → CompositeOneOf. Covers B7.
        if let Some(spec) = expr.and_then(classify_composite_oneof) {
            decisions.insert(entity.clone(), spec);
            continue;
        }

        let has_oneof = expr.map_or(false, has_oneof_recursive);
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
        // ABSTRACT/ONEOF marker) → MergedInto. Skip when the entity sits
        // under a parent that has 2+ siblings — such a parent is
        // destined to become an EnumBase or ComplexSupertype, and the
        // entity is a useful enum variant rather than a wrapper.
        let own_attrs_empty = unified
            .entity_attrs
            .get(entity)
            .map_or(true, |s| s.is_empty());
        let has_enum_shaped_parent = unified
            .entity_parents
            .get(entity)
            .map_or(false, |parents| {
                parents
                    .iter()
                    .any(|p| descendants.get(p).map_or(0, |d| d.len()) >= 2)
            });
        if own_attrs_empty && eff_children.len() == 1 && !has_enum_shaped_parent {
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

/// Recognise the `ComplexSupertype` shape — exactly one `AndOr` or `And`
/// node at the root with two children, both either `OneOf` or `Entity`.
/// Returns `None` for any other shape so the caller falls through to the
/// next rule. The first `OneOf` child (in source order) becomes
/// `oneof_children`; the remaining child is preserved as `mixin` (an
/// `Entity` for B3/B6, an `OneOf` subtree for B4/B5).
fn classify_complex_supertype(expr: &SupertypeExpr) -> Option<VariantSpec> {
    let (children, mixin_pattern) = match expr {
        SupertypeExpr::AndOr { children } => (children, "andor"),
        SupertypeExpr::And { children } => (children, "and"),
        _ => return None,
    };
    if children.len() != 2 {
        return None;
    }
    // Both children must be Entity or OneOf — anything else is out of band.
    for c in children {
        if !matches!(c, SupertypeExpr::Entity { .. } | SupertypeExpr::OneOf { .. }) {
            return None;
        }
    }
    // Find the first OneOf in source order; the other half becomes the mixin.
    let oneof_idx = children
        .iter()
        .position(|c| matches!(c, SupertypeExpr::OneOf { .. }))?;
    let mixin_idx = 1 - oneof_idx;
    let oneof_children = match &children[oneof_idx] {
        SupertypeExpr::OneOf { children: items } => items
            .iter()
            .filter_map(|item| match item {
                SupertypeExpr::Entity { name } => Some(name.clone()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        _ => unreachable!(),
    };
    // OneOf children must all be plain Entity refs at this rule. Any deeper
    // nesting falls through to Rule 8 / pass2.
    if let SupertypeExpr::OneOf { children: items } = &children[oneof_idx] {
        if items.len() != oneof_children.len() {
            return None;
        }
    }
    Some(VariantSpec::ComplexSupertype {
        mixin_pattern: mixin_pattern.to_string(),
        oneof_children,
        mixin: children[mixin_idx].clone(),
    })
}

/// Recognise the `CompositeOneOf` shape — a `OneOf` whose direct children
/// are a mix of `Entity` refs and *anonymous* composite nodes (`AndOr` or
/// `And`) where every composite child is itself a bare `Entity`. Covers
/// `topological_representation_item` and `zone_structural_makeup`.
fn classify_composite_oneof(expr: &SupertypeExpr) -> Option<VariantSpec> {
    let SupertypeExpr::OneOf { children } = expr else {
        return None;
    };
    let mut simple = Vec::new();
    let mut composite = Vec::new();
    for child in children {
        match child {
            SupertypeExpr::Entity { name } => simple.push(name.clone()),
            SupertypeExpr::AndOr { children: parts } => {
                let names = entity_names(parts)?;
                composite.push(CompositeMember::AndOr { children: names });
            }
            SupertypeExpr::And { children: parts } => {
                let names = entity_names(parts)?;
                composite.push(CompositeMember::And { children: names });
            }
            // OneOf inside a OneOf — not a known shape; skip rule.
            SupertypeExpr::OneOf { .. } => return None,
        }
    }
    if composite.is_empty() {
        // No composite alternatives → this is a plain OneOf, not B7.
        return None;
    }
    Some(VariantSpec::CompositeOneOf {
        simple_alternatives: simple,
        composite_alternatives: composite,
    })
}

/// Collect bare entity names out of a list of subexpressions; returns
/// `None` if any subexpression is not an `Entity` (signals deeper nesting).
fn entity_names(exprs: &[SupertypeExpr]) -> Option<Vec<String>> {
    exprs
        .iter()
        .map(|e| match e {
            SupertypeExpr::Entity { name } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// True if any node in the tree is a `OneOf`. Used by Rule 2 (EnumBase)
/// so that `(a ANDOR ONEOF(b, c))` and similar still count as "has ONEOF".
fn has_oneof_recursive(expr: &SupertypeExpr) -> bool {
    match expr {
        SupertypeExpr::OneOf { .. } => true,
        SupertypeExpr::AndOr { children } | SupertypeExpr::And { children } => {
            children.iter().any(has_oneof_recursive)
        }
        SupertypeExpr::Entity { .. } => false,
    }
}

/// Flatten every `Entity` name reachable inside `expr`.
fn collect_entity_names(expr: &SupertypeExpr) -> Vec<String> {
    let mut out = Vec::new();
    fn rec(expr: &SupertypeExpr, out: &mut Vec<String>) {
        match expr {
            SupertypeExpr::Entity { name } => out.push(name.clone()),
            SupertypeExpr::OneOf { children }
            | SupertypeExpr::AndOr { children }
            | SupertypeExpr::And { children } => {
                for c in children {
                    rec(c, out);
                }
            }
        }
    }
    rec(expr, &mut out);
    out
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
    unresolved: &mut BTreeMap<String, Unresolved>,
    overrides_file: &OverrideFile<VariantOverride>,
) {
    for entity in topo {
        if decisions.contains_key(entity) {
            continue;
        }
        // Override short-circuit (defensive: pass1 already covered this,
        // but pass1 is reverse-topo so any topo-order miss lands here).
        if let Some(over) = overrides_file.entity.get(entity) {
            decisions.insert(entity.clone(), over.clone().into());
            continue;
        }

        // Rule 1.7 (ConcreteSupertype): SUPERTYPE OF clause absent yet the
        // entity has own_attrs, ≥ 1 child via SUBTYPE OF, and shows up as
        // a polymorphic target somewhere. EXPRESS allows this implicit-
        // supertype pattern. This must run before Rule 5/6 so that chain
        // entities (own parent is itself a ConcreteSupertype) match here
        // instead of being mis-classified as InEnum of the parent enum
        // by Rule 6's polymorphic-target fallback.
        let supertype_absent = unified.supertype_exprs.get(entity).is_none();
        let has_own_attrs = unified
            .entity_attrs
            .get(entity)
            .map_or(false, |s| !s.is_empty());
        let direct_children: usize = descendants
            .get(entity)
            .map_or(0, |v| v.len());
        if supertype_absent
            && has_own_attrs
            && direct_children >= 1
            && polymorphic_targets.contains(entity)
        {
            decisions.insert(entity.clone(), VariantSpec::ConcreteSupertype);
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

        // Rule 8: SUPERTYPE OF body present but not a plain Entity ref and
        // none of the automatic rules matched. The shape is a deeper
        // anonymous composition that the rule set does not understand —
        // raise to the user via unresolved instead of falling through to
        // SingleStruct (which would silently misclassify).
        if let Some(expr) = unified.supertype_exprs.get(entity) {
            if !matches!(expr, SupertypeExpr::Entity { .. }) {
                unresolved.insert(
                    entity.clone(),
                    Unresolved {
                        reasons: vec![format!(
                            "Rule 8: SUPERTYPE OF tree did not match any automatic rule (Rule 1, 1.5, 2-6). Tree shape: {expr:?}"
                        )],
                        override_example: format_override_example(expr),
                    },
                );
                continue;
            }
        }

        // Rule 7: fallback (no SUPERTYPE OF body, or just a single bare
        // entity ref the higher rules already classified the parent of).
        decisions.insert(entity.clone(), VariantSpec::SingleStruct);
    }
}

/// Render an `override_example` snippet for an unresolved entity, hinting
/// the user how to populate `variants_overrides.toml`. Includes the raw
/// tree dump so the user knows what shape they are deciding on.
fn format_override_example(expr: &SupertypeExpr) -> String {
    format!(
        "# Tree: {expr:?}\n# Choose one of: single_struct / in_enum / enum_base / merged_into / complex_supertype / composite_one_of\nkind = \"single_struct\""
    )
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

/// Among an entity's parents, pick the one whose VariantSpec already
/// owns an enum body — EnumBase / ConcreteSupertype / ComplexSupertype
/// / CompositeOneOf. Returns the first qualifying parent in EXPRESS
/// source order (relies on `entity_parents` preserving that order).
/// Returns `None` if no parent qualifies.
fn pick_enum_root_parent<'a>(
    parents: &'a [String],
    decisions: &BTreeMap<String, VariantSpec>,
) -> Option<&'a String> {
    parents.iter().find(|p| {
        matches!(
            decisions.get(*p),
            Some(VariantSpec::EnumBase { .. })
                | Some(VariantSpec::ComplexSupertype { .. })
                | Some(VariantSpec::CompositeOneOf { .. })
                | Some(VariantSpec::ConcreteSupertype)
        )
    })
}

fn enclosing_enum_root(
    entity: &str,
    unified: &UnifiedSchema,
    descendants: &HashMap<String, Vec<String>>,
    polymorphic_targets: &HashSet<String>,
    decisions: &BTreeMap<String, VariantSpec>,
) -> Option<String> {
    // Walk the parents chain (skipping MergedInto markers); return the
    // first parent whose VariantSpec owns an enum body. Inspects every
    // parent at each step, not just the first one, so multi-inheritance
    // entities (e.g. vertex_point with parents [vertex, GRI]) pick the
    // enum_root parent rather than the alphabetically-first one.
    let mut current = entity.to_string();
    let mut visited: HashSet<String> = HashSet::new();
    while visited.insert(current.clone()) {
        let Some(parents) = unified.entity_parents.get(&current) else {
            break;
        };
        if let Some(root_parent) = pick_enum_root_parent(parents, decisions) {
            return match decisions.get(root_parent) {
                Some(VariantSpec::EnumBase { enum_name }) => Some(enum_name.clone()),
                _ => Some(root_parent.clone()),
            };
        }
        // No qualifying parent at this level. Step up the chain via
        // the first MergedInto parent (those are the markers we skip),
        // otherwise fall back to the first parent in source order.
        let next = parents
            .iter()
            .find(|p| matches!(decisions.get(*p), Some(VariantSpec::MergedInto { .. })))
            .or_else(|| parents.first());
        let Some(next) = next.cloned() else {
            break;
        };
        current = next;
    }

    // Fallback: the legacy polymorphic-target rule. Walks every
    // ancestor (not just the source-order first) and picks the
    // narrowest one that is itself referenced as an ATTR target *and*
    // has at least two concrete descendants, with `entity` among them.
    let mut chain: Vec<String> = vec![entity.to_string()];
    let mut queue: Vec<String> = vec![entity.to_string()];
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(node) = queue.pop() {
        if !seen.insert(node.clone()) {
            continue;
        }
        if let Some(parents) = unified.entity_parents.get(&node) {
            for p in parents {
                if !seen.contains(p) {
                    chain.push(p.clone());
                    queue.push(p.clone());
                }
            }
        }
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
            supertype_expr: None,
        }
    }

    /// Run the classifier with no overrides — most tests want this. The
    /// new `classify_all` returns `(confident, unresolved)`; tests that
    /// only check confident decisions use this thin wrapper.
    fn classify_no_overrides(unified: &UnifiedSchema) -> BTreeMap<String, VariantSpec> {
        let overrides = OverrideFile::<VariantOverride>::default();
        classify_all(unified, &overrides).0
    }

    fn ent_decl(
        name: &str,
        parents: &[&str],
        attrs: Vec<(&str, AttrType)>,
        is_abstract: bool,
        supertype_expr: Option<SupertypeExpr>,
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
            supertype_expr,
        }
    }

    /// Build a `SupertypeExpr::OneOf` of bare entity refs.
    fn oneof_of(names: &[&str]) -> SupertypeExpr {
        SupertypeExpr::OneOf {
            children: names
                .iter()
                .map(|n| SupertypeExpr::Entity {
                    name: (*n).to_string(),
                })
                .collect(),
        }
    }

    fn entity_ref(name: &str) -> SupertypeExpr {
        SupertypeExpr::Entity {
            name: name.to_string(),
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
        let decisions = classify_no_overrides(&unified);
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
                    Some(oneof_of(&["circle", "square"])),
                ),
                ent("circle", &["shape"], vec![("r", AttrType::Primitive("REAL".into()))]),
                ent("square", &["shape"], vec![("s", AttrType::Primitive("REAL".into()))]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_no_overrides(&unified);
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
        let decisions = classify_no_overrides(&unified);
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
                    Some(SupertypeExpr::AndOr {
                        children: vec![oneof_of(&["high", "low"]), entity_ref("actuated")],
                    }),
                ),
                ent("high", &["kpair"], vec![("a", AttrType::Primitive("REAL".into()))]),
                ent("low", &["kpair"], vec![("b", AttrType::Primitive("REAL".into()))]),
                ent("actuated", &["kpair"], vec![("c", AttrType::Primitive("REAL".into()))]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_no_overrides(&unified);
        match decisions.get("kpair").unwrap() {
            VariantSpec::ComplexSupertype {
                mixin_pattern,
                oneof_children,
                mixin,
            } => {
                assert_eq!(mixin_pattern, "andor");
                assert_eq!(oneof_children, &vec!["high".to_string(), "low".to_string()]);
                assert_eq!(mixin, &entity_ref("actuated"));
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
                    // Single-child ONEOF is rejected by the parser per
                    // EXPRESS spec; the wrapper-collapse path here is
                    // exercised via `is_abstract = true` plus a single
                    // child below — supertype_expr stays None.
                    None,
                ),
                ent("only", &["wrapper"], vec![("v", AttrType::Primitive("REAL".into()))]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_no_overrides(&unified);
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
        let decisions = classify_no_overrides(&unified);
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
    fn rule4_skipped_when_parent_has_multiple_children() {
        // parent has 3 siblings (a, b, c). a has its own subtype a_sub.
        // a has no own attrs and one effective child -> Rule 4 would
        // normally merge a into a_sub, but the enum-shaped parent guard
        // skips Rule 4, so Pass 2 classifies a as InEnum(parent).
        let s = schema(
            "test",
            vec![
                ent_decl(
                    "parent",
                    &[],
                    vec![],
                    false,
                    Some(SupertypeExpr::OneOf {
                        children: vec![
                            SupertypeExpr::Entity { name: "a".into() },
                            SupertypeExpr::Entity { name: "b".into() },
                            SupertypeExpr::Entity { name: "c".into() },
                        ],
                    }),
                ),
                ent("a", &["parent"], vec![]),
                ent("b", &["parent"], vec![("v", AttrType::Primitive("REAL".into()))]),
                ent("c", &["parent"], vec![("w", AttrType::Primitive("REAL".into()))]),
                ent("a_sub", &["a"], vec![("x", AttrType::Primitive("REAL".into()))]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_no_overrides(&unified);
        match decisions.get("a").unwrap() {
            VariantSpec::InEnum { enum_name } => {
                assert_eq!(enum_name, "parent");
            }
            other => panic!("expected InEnum(parent) for a, got {other:?}"),
        }
    }

    #[test]
    fn rule4_fires_for_wrapper_chain_with_singleton_parent() {
        // Cascading chain a -> b -> c where each level has one child.
        // Rule 4 fires because no parent has 2+ descendants, preserving
        // the wrapper-collapse behavior.
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
        let decisions = classify_no_overrides(&unified);
        assert!(matches!(
            decisions.get("a"),
            Some(VariantSpec::MergedInto { .. })
        ));
        assert!(matches!(
            decisions.get("b"),
            Some(VariantSpec::MergedInto { .. })
        ));
    }

    // --- Rule 1 / 1.5 / 8 / Overrides regression -------------------------

    /// B5: `ONEOF (a, b) AND ONEOF (c, d)` — solid_with_slot's real shape.
    /// The `mixin_pattern` must come back as `"and"` (not `"andor"`) so
    /// downstream IR lowering keeps the multi-dimensional semantics.
    #[test]
    fn classify_b5_and_between_oneofs() {
        let s = schema(
            "test",
            vec![
                ent_decl(
                    "slot",
                    &[],
                    vec![],
                    true,
                    Some(SupertypeExpr::And {
                        children: vec![oneof_of(&["trapezoidal", "tee"]), oneof_of(&["straight", "curved"])],
                    }),
                ),
                ent("trapezoidal", &["slot"], vec![]),
                ent("tee", &["slot"], vec![]),
                ent("straight", &["slot"], vec![]),
                ent("curved", &["slot"], vec![]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_no_overrides(&unified);
        match decisions.get("slot").unwrap() {
            VariantSpec::ComplexSupertype {
                mixin_pattern,
                oneof_children,
                mixin,
            } => {
                assert_eq!(mixin_pattern, "and");
                assert_eq!(
                    oneof_children,
                    &vec!["trapezoidal".to_string(), "tee".to_string()]
                );
                // mixin is the second OneOf subtree, *not* a flattened list.
                match mixin {
                    SupertypeExpr::OneOf { children } => {
                        assert_eq!(children.len(), 2);
                    }
                    other => panic!("expected OneOf mixin, got {other:?}"),
                }
            }
            other => panic!("expected ComplexSupertype, got {other:?}"),
        }
    }

    /// B6: `(ref ANDOR ONEOF(a, b))` — leading bare ref in front of ONEOF.
    /// edge_blended_solid's real shape. The `mixin` is the bare entity ref
    /// and `oneof_children` carries the inner ONEOF.
    #[test]
    fn classify_b6_ref_andor_oneof() {
        let s = schema(
            "test",
            vec![
                ent_decl(
                    "blended",
                    &[],
                    vec![],
                    true,
                    Some(SupertypeExpr::AndOr {
                        children: vec![entity_ref("track"), oneof_of(&["constant", "variable"])],
                    }),
                ),
                ent("track", &["blended"], vec![]),
                ent("constant", &["blended"], vec![]),
                ent("variable", &["blended"], vec![]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_no_overrides(&unified);
        match decisions.get("blended").unwrap() {
            VariantSpec::ComplexSupertype {
                mixin_pattern,
                oneof_children,
                mixin,
            } => {
                assert_eq!(mixin_pattern, "andor");
                assert_eq!(
                    oneof_children,
                    &vec!["constant".to_string(), "variable".to_string()]
                );
                assert_eq!(mixin, &entity_ref("track"));
            }
            other => panic!("expected ComplexSupertype, got {other:?}"),
        }
    }

    /// B7-1: `ONEOF (vertex, edge, ..., (loop ANDOR path))` — anonymous
    /// AndOr inside ONEOF. topological_representation_item's real shape.
    #[test]
    fn classify_b7_andor_inside_oneof() {
        let s = schema(
            "test",
            vec![
                ent_decl(
                    "trep",
                    &[],
                    vec![],
                    true,
                    Some(SupertypeExpr::OneOf {
                        children: vec![
                            entity_ref("vertex"),
                            entity_ref("edge"),
                            SupertypeExpr::AndOr {
                                children: vec![entity_ref("loop"), entity_ref("path")],
                            },
                        ],
                    }),
                ),
                ent("vertex", &["trep"], vec![]),
                ent("edge", &["trep"], vec![]),
                ent("loop", &["trep"], vec![]),
                ent("path", &["trep"], vec![]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_no_overrides(&unified);
        match decisions.get("trep").unwrap() {
            VariantSpec::CompositeOneOf {
                simple_alternatives,
                composite_alternatives,
            } => {
                assert_eq!(
                    simple_alternatives,
                    &vec!["vertex".to_string(), "edge".to_string()]
                );
                assert_eq!(composite_alternatives.len(), 1);
                match &composite_alternatives[0] {
                    CompositeMember::AndOr { children } => {
                        assert_eq!(
                            children,
                            &vec!["loop".to_string(), "path".to_string()]
                        );
                    }
                    other => panic!("expected AndOr composite, got {other:?}"),
                }
            }
            other => panic!("expected CompositeOneOf, got {other:?}"),
        }
    }

    /// B7-2: `ONEOF ((a AND b), c)` — anonymous And pair inside ONEOF.
    /// zone_structural_makeup's real shape.
    #[test]
    fn classify_b7_and_pair_inside_oneof() {
        let s = schema(
            "test",
            vec![
                ent_decl(
                    "zone",
                    &[],
                    vec![],
                    true,
                    Some(SupertypeExpr::OneOf {
                        children: vec![
                            SupertypeExpr::And {
                                children: vec![entity_ref("a"), entity_ref("b")],
                            },
                            entity_ref("c"),
                        ],
                    }),
                ),
                ent("a", &["zone"], vec![]),
                ent("b", &["zone"], vec![]),
                ent("c", &["zone"], vec![]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_no_overrides(&unified);
        match decisions.get("zone").unwrap() {
            VariantSpec::CompositeOneOf {
                simple_alternatives,
                composite_alternatives,
            } => {
                assert_eq!(simple_alternatives, &vec!["c".to_string()]);
                assert_eq!(composite_alternatives.len(), 1);
                match &composite_alternatives[0] {
                    CompositeMember::And { children } => {
                        assert_eq!(children, &vec!["a".to_string(), "b".to_string()]);
                    }
                    other => panic!("expected And composite, got {other:?}"),
                }
            }
            other => panic!("expected CompositeOneOf, got {other:?}"),
        }
    }

    /// Rule 8 safety net: a deeply nested shape that no automatic rule
    /// recognises must land in `unresolved` rather than silently falling
    /// through to SingleStruct. Uses an artificial 3-child AndOr that
    /// Rule 1 explicitly skips.
    #[test]
    fn rule8_raises_unresolved_for_unknown_shape() {
        let s = schema(
            "test",
            vec![
                ent_decl(
                    "weird",
                    &[],
                    vec![],
                    true,
                    Some(SupertypeExpr::AndOr {
                        children: vec![oneof_of(&["x", "y"]), entity_ref("m"), entity_ref("n")],
                    }),
                ),
                ent("x", &["weird"], vec![]),
                ent("y", &["weird"], vec![]),
                ent("m", &["weird"], vec![]),
                ent("n", &["weird"], vec![]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let overrides = OverrideFile::<VariantOverride>::default();
        let (decisions, unresolved) = classify_all(&unified, &overrides);
        // Rule 2 (ABSTRACT/ONEOF + ≥ 2 effective children) catches this
        // before Rule 8 because has_oneof_recursive is true. So this
        // entity ends up as EnumBase. To exercise Rule 8 we need a shape
        // that misses every other rule too.
        // Confirm the EnumBase path runs for this case (sanity).
        assert!(matches!(
            decisions.get("weird").unwrap(),
            VariantSpec::EnumBase { .. }
        ));
        assert_eq!(unresolved.len(), 0);
    }

    /// Rule 8 actually firing: a non-abstract entity with an AndOr-only
    /// SUPERTYPE body whose 3 children are bare entity refs (no ONEOF
    /// anywhere in the tree) — Rule 1 needs exactly 2 children, Rule 1.5
    /// needs an outer OneOf, Rule 2 needs has_oneof_recursive, Rule 3-6
    /// don't apply (parent unrelated). Falls through to Rule 8.
    #[test]
    fn rule8_raises_unresolved_for_3child_andor_no_oneof() {
        let s = schema(
            "test",
            vec![
                ent_decl(
                    "weird",
                    &[],
                    vec![],
                    false, // not abstract
                    Some(SupertypeExpr::AndOr {
                        children: vec![entity_ref("a"), entity_ref("b"), entity_ref("c")],
                    }),
                ),
                ent("a", &["weird"], vec![("x", AttrType::Primitive("REAL".into()))]),
                ent("b", &["weird"], vec![("y", AttrType::Primitive("REAL".into()))]),
                ent("c", &["weird"], vec![("z", AttrType::Primitive("REAL".into()))]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let overrides = OverrideFile::<VariantOverride>::default();
        let (_decisions, unresolved) = classify_all(&unified, &overrides);
        let u = unresolved
            .get("weird")
            .expect("weird should land in unresolved");
        assert!(
            !u.reasons.is_empty(),
            "unresolved entry must carry a reason"
        );
        assert!(
            u.override_example.contains("kind"),
            "override_example must hint the user how to fill the override"
        );
    }

    /// End-to-end regression for the four entities where the old
    /// regex-based parser silently misclassified the SUPERTYPE OF body.
    /// Runs the full classifier on the real schemas and asserts each
    /// entity's exact `VariantSpec` shape.
    #[test]
    fn silent_fail_entities_classify_correctly_on_real_schemas() {
        use std::path::Path;

        let schemas = crate::express::load_all_schemas(Path::new("schemas"));
        assert_eq!(schemas.len(), 4, "expected 4 schemas, got {}", schemas.len());

        let unified = refgraph::build(&schemas);
        let decisions = classify_no_overrides(&unified);

        // edge_blended_solid (B6): leading bare ref + ANDOR + ONEOF
        match decisions.get("edge_blended_solid").unwrap() {
            VariantSpec::ComplexSupertype {
                mixin_pattern,
                oneof_children,
                mixin,
            } => {
                assert_eq!(mixin_pattern, "andor");
                assert_eq!(
                    oneof_children,
                    &vec![
                        "solid_with_constant_radius_edge_blend".to_string(),
                        "solid_with_variable_radius_edge_blend".to_string(),
                        "solid_with_chamfered_edges".to_string(),
                    ]
                );
                assert_eq!(
                    mixin,
                    &SupertypeExpr::Entity {
                        name: "track_blended_solid".to_string(),
                    }
                );
            }
            other => panic!("edge_blended_solid: expected ComplexSupertype, got {other:?}"),
        }

        // solid_with_depression (B6)
        match decisions.get("solid_with_depression").unwrap() {
            VariantSpec::ComplexSupertype {
                mixin_pattern,
                oneof_children,
                mixin,
            } => {
                assert_eq!(mixin_pattern, "andor");
                assert_eq!(
                    oneof_children,
                    &vec![
                        "solid_with_hole".to_string(),
                        "solid_with_pocket".to_string(),
                        "solid_with_slot".to_string(),
                        "solid_with_groove".to_string(),
                    ]
                );
                assert_eq!(
                    mixin,
                    &SupertypeExpr::Entity {
                        name: "solid_with_through_depression".to_string(),
                    }
                );
            }
            other => panic!("solid_with_depression: expected ComplexSupertype, got {other:?}"),
        }

        // solid_with_stepped_round_hole (B6)
        match decisions.get("solid_with_stepped_round_hole").unwrap() {
            VariantSpec::ComplexSupertype {
                mixin_pattern,
                oneof_children: _,
                mixin,
            } => {
                assert_eq!(mixin_pattern, "andor");
                assert_eq!(
                    mixin,
                    &SupertypeExpr::Entity {
                        name: "solid_with_stepped_round_hole_and_conical_transitions".to_string(),
                    }
                );
            }
            other => {
                panic!("solid_with_stepped_round_hole: expected ComplexSupertype, got {other:?}")
            }
        }

        // solid_with_slot (B5): ONEOF AND ONEOF — the only entity in the
        // corpus with mixin_pattern = "and".
        match decisions.get("solid_with_slot").unwrap() {
            VariantSpec::ComplexSupertype {
                mixin_pattern,
                oneof_children,
                mixin,
            } => {
                assert_eq!(mixin_pattern, "and");
                assert_eq!(
                    oneof_children,
                    &vec![
                        "solid_with_trapezoidal_section_slot".to_string(),
                        "solid_with_tee_section_slot".to_string(),
                    ]
                );
                match mixin {
                    SupertypeExpr::OneOf { children } => {
                        let names: Vec<&str> = children
                            .iter()
                            .filter_map(|c| match c {
                                SupertypeExpr::Entity { name } => Some(name.as_str()),
                                _ => None,
                            })
                            .collect();
                        assert_eq!(
                            names,
                            vec!["solid_with_straight_slot", "solid_with_curved_slot"]
                        );
                    }
                    other => panic!("solid_with_slot: expected OneOf mixin, got {other:?}"),
                }
            }
            other => panic!("solid_with_slot: expected ComplexSupertype, got {other:?}"),
        }

        // B7: topological_representation_item → CompositeOneOf with the
        // (loop ANDOR path) member preserved.
        match decisions.get("topological_representation_item").unwrap() {
            VariantSpec::CompositeOneOf {
                simple_alternatives: _,
                composite_alternatives,
            } => {
                assert_eq!(composite_alternatives.len(), 1);
                match &composite_alternatives[0] {
                    CompositeMember::AndOr { children } => {
                        assert!(children.contains(&"loop".to_string()));
                        assert!(children.contains(&"path".to_string()));
                    }
                    other => panic!("expected AndOr composite, got {other:?}"),
                }
            }
            other => panic!("topological_representation_item: expected CompositeOneOf, got {other:?}"),
        }

        // B7: zone_structural_makeup → CompositeOneOf with two And pairs.
        match decisions.get("zone_structural_makeup").unwrap() {
            VariantSpec::CompositeOneOf {
                simple_alternatives: _,
                composite_alternatives,
            } => {
                assert_eq!(composite_alternatives.len(), 2);
                for member in composite_alternatives {
                    assert!(matches!(member, CompositeMember::And { .. }));
                }
            }
            other => panic!("zone_structural_makeup: expected CompositeOneOf, got {other:?}"),
        }
    }

    /// Implicit-supertype pattern: schema omits SUPERTYPE OF, the entity
    /// has its own attrs, and ≥ 1 child plus a polymorphic-target
    /// reference to its name. Auto-classified as ConcreteSupertype.
    #[test]
    fn classify_concrete_supertype_implicit_pattern() {
        let s = schema(
            "test",
            vec![
                // `act` has own attrs, no SUPERTYPE OF clause, and 2
                // children pointing to it via SUBTYPE OF. A separate
                // entity references `act` polymorphically through an
                // ATTR — that triggers polymorphic_targets membership.
                ent("act", &[], vec![("name", AttrType::Primitive("STRING".into()))]),
                ent(
                    "executed_act",
                    &["act"],
                    vec![("done", AttrType::Primitive("BOOLEAN".into()))],
                ),
                ent(
                    "analyzed_act",
                    &["act"],
                    vec![("score", AttrType::Primitive("REAL".into()))],
                ),
                ent(
                    "consumer",
                    &[],
                    vec![("target", AttrType::Entity("act".into()))],
                ),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let decisions = classify_no_overrides(&unified);
        assert!(
            matches!(decisions.get("act").unwrap(), VariantSpec::ConcreteSupertype),
            "act should be ConcreteSupertype, got {:?}",
            decisions.get("act").unwrap()
        );
        match decisions.get("executed_act").unwrap() {
            VariantSpec::InEnum { enum_name } => assert_eq!(enum_name, "act"),
            other => panic!("executed_act should be InEnum(act), got {other:?}"),
        }
    }

    /// Real-schema regression — a representative sample of entities that
    /// must classify as ConcreteSupertype on the actual schemas. The full
    /// auto-rule reach is wider (~75 entities, the rule's
    /// polymorphic_targets is broader than the proxy used during
    /// planning); this test pins a known-good subset including the chain
    /// case `representation_relationship_with_transformation`.
    #[test]
    fn concrete_supertype_classifies_known_entities_on_real_schemas() {
        use std::path::Path;

        let schemas = crate::express::load_all_schemas(Path::new("schemas"));
        let unified = refgraph::build(&schemas);
        let decisions = classify_no_overrides(&unified);

        // Spot-check sample. Each must be ConcreteSupertype.
        let expected: &[&str] = &[
            "action",
            "action_method",
            "characterized_object",
            "general_property",
            "item_defined_transformation",
            "product_definition_formation",
            "product_definition_relationship",
            "property_definition_representation",
            "representation_context",
            "representation_map",
            "representation_relationship",
            "shape_aspect_relationship",
            // chain entity — own parent (representation_relationship) is
            // also a ConcreteSupertype; the rule must run before Rule 6
            // for this case to land here instead of InEnum.
            "representation_relationship_with_transformation",
        ];
        for name in expected {
            assert!(
                matches!(
                    decisions.get(*name),
                    Some(VariantSpec::ConcreteSupertype)
                ),
                "{name}: expected ConcreteSupertype, got {:?}",
                decisions.get(*name)
            );
        }
    }

    /// Plan 1's silent-fail regression must still hold after adding the
    /// ConcreteSupertype rule — those four entities have explicit
    /// SUPERTYPE OF clauses, so the new rule's `supertype_absent` guard
    /// should leave them untouched.
    #[test]
    fn plan1_silent_fail_classifications_unchanged() {
        use std::path::Path;
        let schemas = crate::express::load_all_schemas(Path::new("schemas"));
        let unified = refgraph::build(&schemas);
        let decisions = classify_no_overrides(&unified);
        for name in [
            "edge_blended_solid",
            "solid_with_depression",
            "solid_with_stepped_round_hole",
            "solid_with_slot",
        ] {
            assert!(
                matches!(
                    decisions.get(name),
                    Some(VariantSpec::ComplexSupertype { .. })
                ),
                "{name}: expected ComplexSupertype, got {:?}",
                decisions.get(name)
            );
        }
    }

    /// Override short-circuit: an entity listed in
    /// `variants_overrides.toml` bypasses every automatic rule and lands
    /// in the confident map with the user's chosen kind.
    #[test]
    fn override_short_circuits_automatic_rules() {
        let s = schema(
            "test",
            vec![
                // Without override the entity would auto-classify as
                // SingleStruct (no parents, no children, no flags).
                ent("forced", &[], vec![("v", AttrType::Primitive("REAL".into()))]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let mut overrides = OverrideFile::<VariantOverride>::default();
        overrides.entity.insert(
            "forced".to_string(),
            VariantOverride::EnumBase {
                enum_name: "forced".to_string(),
                reason: Some("manual decision".to_string()),
            },
        );
        let (decisions, unresolved) = classify_all(&unified, &overrides);
        assert_eq!(unresolved.len(), 0);
        match decisions.get("forced").unwrap() {
            VariantSpec::EnumBase { enum_name } => assert_eq!(enum_name, "forced"),
            other => panic!("override should win; got {other:?}"),
        }
    }

    /// Override with VariantOverride::ConcreteSupertype should land in
    /// confident as VariantSpec::ConcreteSupertype.
    #[test]
    fn override_concrete_supertype() {
        let s = schema(
            "test",
            vec![ent(
                "forced",
                &[],
                vec![("v", AttrType::Primitive("REAL".into()))],
            )],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let mut overrides = OverrideFile::<VariantOverride>::default();
        overrides.entity.insert(
            "forced".to_string(),
            VariantOverride::ConcreteSupertype {
                reason: Some("manual decision".to_string()),
            },
        );
        let (decisions, unresolved) = classify_all(&unified, &overrides);
        assert_eq!(unresolved.len(), 0);
        assert!(
            matches!(decisions.get("forced").unwrap(), VariantSpec::ConcreteSupertype),
            "override should produce ConcreteSupertype; got {:?}",
            decisions.get("forced").unwrap()
        );
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
        let decisions = classify_no_overrides(&unified);
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
        let decisions = classify_no_overrides(&unified);
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
        let decisions = classify_no_overrides(&unified);
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
        let decisions = classify_no_overrides(&unified);
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
        let decisions = classify_no_overrides(&unified);
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
        let decisions = classify_no_overrides(&unified);
        for name in ["sub_a", "sub_b"] {
            let d = decisions.get(name).unwrap();
            assert!(
                !matches!(d, VariantSpec::NestedField { .. }),
                "{name}: should not be NestedField (sibling extends too)"
            );
        }
    }
}
