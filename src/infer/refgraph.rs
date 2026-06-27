//! Schema-union builder for the faithful exporters.
//!
//! Unions the parsed AP schemas by entity name and derives the facts the
//! exporters need: merged parents (`entity_parents`), the abstract-entity set
//! (`abstract_entities`), and cross-schema attribute-type conflicts
//! (`attr_conflicts`). Attributes are merged per entity; when schemas disagree
//! on an attribute's type the conflict is recorded (the first-parsed type wins).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::express::{AttrType, Schema};

/// Schema union derived from the input schemas.
///
/// Entities are unioned by name across schemas (attribute superset); only the
/// facts the exporters read are retained.
pub struct UnifiedSchema {
    /// Entity name → merged parents (union), in the source order of the first
    /// schema declaring each parent.
    pub entity_parents: BTreeMap<String, Vec<String>>,
    /// (entity, attr) keys where the same attribute had a shape-different parsed
    /// type across schemas. Surfaced as classification-time signals.
    pub attr_conflicts: BTreeMap<(String, String), Vec<String>>,
    /// Entities whose SUPERTYPE block carries `ABSTRACT` in at least one schema.
    pub abstract_entities: BTreeSet<String>,
}

/// Build the unified schema from one or more parsed schemas.
pub fn build(schemas: &[Schema]) -> UnifiedSchema {
    // 1. Union entity definitions across schemas (parents, abstract flag, and
    //    per-attr parsed types for conflict detection).
    let mut entity_parents: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut abstract_entities: BTreeSet<String> = BTreeSet::new();
    // Per-entity attr → list of (schema_label, type) to detect conflicts.
    let mut attr_types: HashMap<(String, String), Vec<(String, AttrType)>> = HashMap::new();

    for schema in schemas {
        for (name, ent) in &schema.entities {
            let parents_vec = entity_parents.entry(name.clone()).or_default();
            for p in &ent.parents {
                if !parents_vec.contains(p) {
                    parents_vec.push(p.clone());
                }
            }
            for spec in &ent.own_attrs {
                attr_types
                    .entry((name.clone(), spec.name.clone()))
                    .or_default()
                    .push((schema.source_label.clone(), spec.ty.clone()));
            }
            if ent.is_abstract {
                abstract_entities.insert(name.clone());
            }
        }
    }

    // 2. Record cross-schema attr-type conflicts. The canonical (first-parsed)
    //    type wins; differing shapes are listed for the caller to surface.
    let mut attr_conflicts: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
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
    }

    UnifiedSchema {
        entity_parents,
        attr_conflicts,
        abstract_entities,
    }
}

/// Structural equality on AttrType — used to detect cross-schema disagreements
/// without depending on Eq/Hash derives (AttrType has Vec<String> content).
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
            redeclared_attrs: Vec::new(),
            is_abstract: false,
            supertype_expr: None,
            derived_attrs: Vec::new(),
        }
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
        // Smoke: union of all 6 schemas should produce a non-trivial graph
        // with no panics.
        use crate::express::load_all_schemas;
        use std::path::Path;
        let schemas = load_all_schemas(Path::new("schemas"));
        assert_eq!(schemas.len(), 6);
        let g = build(&schemas);
        assert!(
            g.entity_parents.len() >= 700,
            "entities: {}",
            g.entity_parents.len()
        );
    }
}
