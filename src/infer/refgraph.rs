//! ATTR cross-reference graph builder.
//!
//! Walks every ATTR of every entity in the schema union and emits an edge
//! per entity-to-entity reference. SELECT-based polymorphism is unfolded
//! (one edge per SELECT member). TYPE aliases are resolved transitively
//! to a fixpoint, so `attr : my_alias` where `my_alias = SELECT (a, b)`
//! produces edges to both `a` and `b`. Aggregation wrappers (LIST / SET /
//! BAG / ARRAY / OPTIONAL) are unwrapped without contributing edges of
//! their own.
//!
//! Used by all three stages: variant (polymorphic context counts), arena
//! (context fingerprint), pool (community detection).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::express::{AttrType, Schema, TypeDef};

/// One reference from an entity's ATTR to another entity (or to a
/// primitive / enumeration). Stored at full fidelity so callers can
/// aggregate as needed (variant counts polymorphic locations, pool sums
/// edges per arena, etc.).
#[derive(Debug, Clone)]
pub struct RefEdge {
    /// Owning entity (lowercase).
    pub from: String,
    /// ATTR name (lowercase).
    pub attr: String,
    /// Target. `Entity` is by far the most common; `Primitive` and
    /// `Enumeration` carried for completeness so callers can distinguish
    /// "no entity ref" from "ref to a primitive".
    pub target: RefTarget,
    /// True when this ATTR is wrapped in `OPTIONAL` (anywhere along the
    /// type chain). Useful for nullability-sensitive analysis.
    pub optional: bool,
    /// True when the ref came out of a SELECT unfolding. Flags
    /// polymorphic contexts (a single ATTR producing multiple edges).
    pub via_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RefTarget {
    /// Concrete entity name (lowercase). The most common target.
    Entity(String),
    /// Primitive type — no graph edge but recorded for completeness.
    Primitive(String),
    /// Enumeration value list — not polymorphic over entities.
    Enumeration(Vec<String>),
}

/// Schema union + ATTR ref graph derived from it.
///
/// Entities are unioned by name across the input schemas (attr superset).
/// Type aliases unioned similarly; conflicts (same name, different
/// aliased target) are recorded for the caller to surface as an
/// unresolved decision.
pub struct UnifiedSchema {
    /// Entity name → merged parents (union). Lowercase.
    pub entity_parents: BTreeMap<String, BTreeSet<String>>,
    /// Entity name → set of attr names declared anywhere across schemas.
    pub entity_attrs: BTreeMap<String, BTreeSet<String>>,
    /// All edges (one per concrete reference, after SELECT unfolding and
    /// TYPE alias resolution).
    pub edges: Vec<RefEdge>,
    /// (entity, attr) keys where the same attribute had different parsed
    /// types across schemas. Surfaced as classification-time signals.
    pub attr_conflicts: BTreeMap<(String, String), Vec<String>>,
    /// Type aliases that resolved differently across schemas (name →
    /// distinct aliased reprs).
    pub type_conflicts: BTreeMap<String, Vec<String>>,
}

/// Build the unified schema + ref graph from one or more parsed schemas.
pub fn build(schemas: &[Schema]) -> UnifiedSchema {
    // 1. Union entity definitions across schemas (parents + attrs).
    let mut entity_parents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut entity_attrs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    // Per-entity attr → list of distinct (schema_label, type repr string)
    // pairs to detect conflicts.
    let mut attr_types: HashMap<(String, String), Vec<(String, AttrType)>> = HashMap::new();

    for schema in schemas {
        for (name, ent) in &schema.entities {
            entity_parents
                .entry(name.clone())
                .or_default()
                .extend(ent.parents.iter().cloned());
            for spec in &ent.own_attrs {
                entity_attrs
                    .entry(name.clone())
                    .or_default()
                    .insert(spec.name.clone());
                attr_types
                    .entry((name.clone(), spec.name.clone()))
                    .or_default()
                    .push((schema.source_label.clone(), spec.ty.clone()));
            }
        }
    }

    // 2. Union TYPE aliases. Track conflicts.
    let mut types: HashMap<String, TypeDef> = HashMap::new();
    let mut type_conflicts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for schema in schemas {
        for (name, td) in &schema.types {
            match types.get(name) {
                None => {
                    types.insert(name.clone(), td.clone());
                }
                Some(existing) if shape_eq(&existing.aliased, &td.aliased) => {
                    // identical → keep
                }
                Some(_) => {
                    type_conflicts
                        .entry(name.clone())
                        .or_default()
                        .push(format!("{}: {:?}", schema.source_label, td.aliased));
                }
            }
        }
    }

    // 3. Build edges. For each (entity, attr) pair, pick the merged ATTR
    //    type. If schemas disagreed, record the conflict and pick the
    //    first parsed type (caller can surface; classification still
    //    proceeds best-effort).
    let mut attr_conflicts: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut edges: Vec<RefEdge> = Vec::new();

    for ((ent_name, attr_name), variants) in &attr_types {
        let canonical = &variants[0].1;
        let mut conflict_descriptions = Vec::new();
        for (label, ty) in &variants[1..] {
            if !shape_eq(canonical, ty) {
                conflict_descriptions.push(format!("{label}: {ty:?}"));
            }
        }
        if !conflict_descriptions.is_empty() {
            // Include the canonical too so the conflict is fully described.
            let mut all = vec![format!("{}: {:?}", variants[0].0, canonical)];
            all.extend(conflict_descriptions);
            attr_conflicts.insert((ent_name.clone(), attr_name.clone()), all);
        }

        emit_edges_for_type(
            ent_name,
            attr_name,
            canonical,
            false, // optional
            false, // via_select
            &types,
            &mut edges,
        );
    }

    UnifiedSchema {
        entity_parents,
        entity_attrs,
        edges,
        attr_conflicts,
        type_conflicts,
    }
}

/// Recursively emit edges for one ATTR type. Aggregations unwrap; SELECT
/// fans out (with `via_select = true`); Entity dispatches on whether the
/// name is a TYPE alias (resolve transitively, fixpoint protected) or a
/// real entity (terminal edge).
fn emit_edges_for_type(
    from: &str,
    attr: &str,
    ty: &AttrType,
    optional: bool,
    via_select: bool,
    types: &HashMap<String, TypeDef>,
    edges: &mut Vec<RefEdge>,
) {
    match ty {
        AttrType::Optional(inner) => emit_edges_for_type(
            from,
            attr,
            inner,
            true,
            via_select,
            types,
            edges,
        ),
        AttrType::List(inner)
        | AttrType::Set(inner)
        | AttrType::Bag(inner)
        | AttrType::Array(inner) => emit_edges_for_type(
            from,
            attr,
            inner,
            optional,
            via_select,
            types,
            edges,
        ),
        AttrType::Select(members) => {
            // Unfold each member as a separate edge. `via_select = true`
            // marks the polymorphic context.
            let mut visited = HashSet::new();
            visited.insert(attr.to_string()); // sentinel for fixpoint
            for member in members {
                resolve_named_to_edges(
                    from,
                    attr,
                    member,
                    optional,
                    true, // via_select
                    types,
                    edges,
                    &mut visited,
                );
            }
        }
        AttrType::Enumeration(values) => {
            edges.push(RefEdge {
                from: from.to_string(),
                attr: attr.to_string(),
                target: RefTarget::Enumeration(values.clone()),
                optional,
                via_select,
            });
        }
        AttrType::Primitive(p) => {
            edges.push(RefEdge {
                from: from.to_string(),
                attr: attr.to_string(),
                target: RefTarget::Primitive(p.clone()),
                optional,
                via_select,
            });
        }
        AttrType::Entity(name) => {
            let mut visited = HashSet::new();
            visited.insert(attr.to_string());
            resolve_named_to_edges(
                from,
                attr,
                name,
                optional,
                via_select,
                types,
                edges,
                &mut visited,
            );
        }
    }
}

/// Given a name that came out of `Entity(...)` (entity ref OR TYPE alias),
/// resolve through TYPE aliases transitively. Produces edges only for
/// terminal targets: real entities (or primitives if a TYPE alias chain
/// bottoms out at a primitive).
fn resolve_named_to_edges(
    from: &str,
    attr: &str,
    name: &str,
    optional: bool,
    via_select: bool,
    types: &HashMap<String, TypeDef>,
    edges: &mut Vec<RefEdge>,
    visited: &mut HashSet<String>,
) {
    if !visited.insert(name.to_string()) {
        // Cycle in TYPE chain — bail out silently. EXPRESS shouldn't
        // produce these but be defensive.
        return;
    }
    match types.get(name) {
        None => {
            // Not a TYPE alias → assume it's an entity name. Emit
            // terminal edge.
            edges.push(RefEdge {
                from: from.to_string(),
                attr: attr.to_string(),
                target: RefTarget::Entity(name.to_string()),
                optional,
                via_select,
            });
        }
        Some(td) => {
            // TYPE alias → recurse into its aliased repr. The alias might
            // be a SELECT (fans out), another alias (chain continues), a
            // primitive (terminal), or an aggregation wrapping any of
            // the above.
            resolve_aliased_to_edges(
                from,
                attr,
                &td.aliased,
                optional,
                via_select,
                types,
                edges,
                visited,
            );
        }
    }
}

fn resolve_aliased_to_edges(
    from: &str,
    attr: &str,
    aliased: &AttrType,
    optional: bool,
    via_select: bool,
    types: &HashMap<String, TypeDef>,
    edges: &mut Vec<RefEdge>,
    visited: &mut HashSet<String>,
) {
    match aliased {
        AttrType::Optional(inner) => resolve_aliased_to_edges(
            from, attr, inner, true, via_select, types, edges, visited,
        ),
        AttrType::List(inner)
        | AttrType::Set(inner)
        | AttrType::Bag(inner)
        | AttrType::Array(inner) => resolve_aliased_to_edges(
            from, attr, inner, optional, via_select, types, edges, visited,
        ),
        AttrType::Select(members) => {
            // Nested SELECT inside a TYPE alias → unfold further. Each
            // member counts as a polymorphic branch.
            for member in members {
                let mut local_visited = visited.clone();
                resolve_named_to_edges(
                    from,
                    attr,
                    member,
                    optional,
                    true, // via_select propagates
                    types,
                    edges,
                    &mut local_visited,
                );
            }
        }
        AttrType::Enumeration(values) => {
            edges.push(RefEdge {
                from: from.to_string(),
                attr: attr.to_string(),
                target: RefTarget::Enumeration(values.clone()),
                optional,
                via_select,
            });
        }
        AttrType::Primitive(p) => {
            edges.push(RefEdge {
                from: from.to_string(),
                attr: attr.to_string(),
                target: RefTarget::Primitive(p.clone()),
                optional,
                via_select,
            });
        }
        AttrType::Entity(name) => {
            resolve_named_to_edges(
                from, attr, name, optional, via_select, types, edges, visited,
            );
        }
    }
}

/// Structural equality on AttrType — used to detect cross-schema
/// disagreements without depending on Eq/Hash derives (AttrType has
/// Vec<String> inner content).
fn shape_eq(a: &AttrType, b: &AttrType) -> bool {
    match (a, b) {
        (AttrType::Entity(x), AttrType::Entity(y)) => x == y,
        (AttrType::Primitive(x), AttrType::Primitive(y)) => x == y,
        (AttrType::Optional(x), AttrType::Optional(y)) => shape_eq(x, y),
        (AttrType::List(x), AttrType::List(y)) => shape_eq(x, y),
        (AttrType::Set(x), AttrType::Set(y)) => shape_eq(x, y),
        (AttrType::Bag(x), AttrType::Bag(y)) => shape_eq(x, y),
        (AttrType::Array(x), AttrType::Array(y)) => shape_eq(x, y),
        (AttrType::Select(x), AttrType::Select(y)) => x == y,
        (AttrType::Enumeration(x), AttrType::Enumeration(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::express::{AttrSpec, EntitySchema, Schema, TypeDef};

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
        }
    }

    fn ty_alias(name: &str, aliased: AttrType) -> TypeDef {
        TypeDef {
            name: name.to_string(),
            aliased,
        }
    }

    #[test]
    fn entity_ref_produces_one_edge() {
        let s = schema(
            "test",
            vec![ent(
                "edge_curve",
                &[],
                vec![("curve_geometry", AttrType::Entity("curve".into()))],
            )],
            vec![],
        );
        let g = build(&[s]);
        assert_eq!(g.edges.len(), 1);
        let e = &g.edges[0];
        assert_eq!(e.from, "edge_curve");
        assert_eq!(e.attr, "curve_geometry");
        assert_eq!(e.target, RefTarget::Entity("curve".into()));
        assert!(!e.via_select);
        assert!(!e.optional);
    }

    #[test]
    fn optional_propagates_through_aggregation() {
        // OPTIONAL LIST OF cartesian_point
        let ty = AttrType::Optional(Box::new(AttrType::List(Box::new(AttrType::Entity(
            "cartesian_point".into(),
        )))));
        let s = schema(
            "test",
            vec![ent("foo", &[], vec![("pts", ty)])],
            vec![],
        );
        let g = build(&[s]);
        assert_eq!(g.edges.len(), 1);
        assert!(g.edges[0].optional);
        assert_eq!(g.edges[0].target, RefTarget::Entity("cartesian_point".into()));
    }

    #[test]
    fn select_unfolds_into_multiple_edges() {
        let ty = AttrType::Select(vec!["a".into(), "b".into(), "c".into()]);
        let s = schema(
            "test",
            vec![ent("foo", &[], vec![("kind", ty)])],
            vec![],
        );
        let g = build(&[s]);
        assert_eq!(g.edges.len(), 3);
        for e in &g.edges {
            assert!(e.via_select);
            assert!(matches!(&e.target, RefTarget::Entity(_)));
        }
        let mut targets: Vec<&str> = g
            .edges
            .iter()
            .filter_map(|e| match &e.target {
                RefTarget::Entity(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        targets.sort();
        assert_eq!(targets, vec!["a", "b", "c"]);
    }

    #[test]
    fn type_alias_to_select_unfolds_via_alias() {
        // TYPE shape_def = SELECT (sa, sar);
        // ENTITY foo; t : shape_def;
        let s = schema(
            "test",
            vec![ent(
                "foo",
                &[],
                vec![("t", AttrType::Entity("shape_def".into()))],
            )],
            vec![ty_alias(
                "shape_def",
                AttrType::Select(vec!["sa".into(), "sar".into()]),
            )],
        );
        let g = build(&[s]);
        assert_eq!(g.edges.len(), 2, "edges: {:?}", g.edges);
        for e in &g.edges {
            assert!(e.via_select);
        }
        let mut targets: Vec<&str> = g
            .edges
            .iter()
            .filter_map(|e| match &e.target {
                RefTarget::Entity(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        targets.sort();
        assert_eq!(targets, vec!["sa", "sar"]);
    }

    #[test]
    fn type_alias_chain_to_primitive() {
        // TYPE m1 = REAL; TYPE m2 = m1; ENTITY foo; v : m2;
        let s = schema(
            "test",
            vec![ent(
                "foo",
                &[],
                vec![("v", AttrType::Entity("m2".into()))],
            )],
            vec![
                ty_alias("m1", AttrType::Primitive("REAL".into())),
                ty_alias("m2", AttrType::Entity("m1".into())),
            ],
        );
        let g = build(&[s]);
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].target, RefTarget::Primitive("REAL".into()));
    }

    #[test]
    fn cross_schema_attr_superset() {
        // Schema A: foo has attr x. Schema B: foo has attrs x, y.
        // Union should contain both attrs.
        let a = schema(
            "a",
            vec![ent(
                "foo",
                &[],
                vec![("x", AttrType::Primitive("INTEGER".into()))],
            )],
            vec![],
        );
        let b = schema(
            "b",
            vec![ent(
                "foo",
                &[],
                vec![
                    ("x", AttrType::Primitive("INTEGER".into())),
                    ("y", AttrType::Primitive("REAL".into())),
                ],
            )],
            vec![],
        );
        let g = build(&[a, b]);
        let attrs: BTreeSet<_> = g.entity_attrs.get("foo").unwrap().iter().cloned().collect();
        assert_eq!(attrs.len(), 2);
        assert!(attrs.contains("x"));
        assert!(attrs.contains("y"));
        assert!(g.attr_conflicts.is_empty());
    }

    #[test]
    fn cross_schema_attr_conflict_recorded() {
        let a = schema(
            "a",
            vec![ent(
                "foo",
                &[],
                vec![("x", AttrType::Primitive("INTEGER".into()))],
            )],
            vec![],
        );
        let b = schema(
            "b",
            vec![ent(
                "foo",
                &[],
                vec![("x", AttrType::Primitive("REAL".into()))],
            )],
            vec![],
        );
        let g = build(&[a, b]);
        assert_eq!(g.attr_conflicts.len(), 1);
        let key = ("foo".to_string(), "x".to_string());
        assert!(g.attr_conflicts.contains_key(&key));
    }

    #[test]
    fn real_schemas_build_without_panic() {
        // Smoke: union of all 4 schemas should produce a non-trivial
        // graph with no panics.
        use crate::express::load_all_schemas;
        use std::path::Path;
        let schemas = load_all_schemas(Path::new("schemas"));
        assert_eq!(schemas.len(), 4);
        let g = build(&schemas);
        assert!(g.entity_parents.len() >= 700, "entities: {}", g.entity_parents.len());
        assert!(g.edges.len() >= 1000, "edges: {}", g.edges.len());
    }
}
