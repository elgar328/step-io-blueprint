//! Stage 1 — variant classification.
//!
//! Pure function: schema → `BTreeMap<String, VariantSpec>`. No confidence,
//! no bucket, no override mechanism. Every entity gets a deterministic
//! decision from the structural rules below.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::express::{AttrType, Schema};
use crate::infer::refgraph::{self, RefTarget, UnifiedSchema};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VariantSpec {
    SingleStruct,
    InEnum {
        enum_name: String,
    },
    NestedField {
        into: String,
        as_field: String,
        added_attr_count: usize,
    },
}

const FILE_CONFIDENT: &str = "variants.toml";
const SECTION: &str = "entity";

pub fn run(schemas: &[Schema]) -> Result<(), String> {
    let unified = refgraph::build(schemas);
    let decisions = classify_all(&unified);

    crate::infer::io::write_confident(FILE_CONFIDENT, SECTION, &decisions)
        .map_err(|e| format!("write {FILE_CONFIDENT}: {e}"))?;

    let (single, in_enum, nested) = count_kinds(&decisions);
    eprintln!(
        "infer variant: {} entities (single={} enum={} nested={})",
        decisions.len(),
        single,
        in_enum,
        nested,
    );
    Ok(())
}

fn count_kinds(decisions: &BTreeMap<String, VariantSpec>) -> (usize, usize, usize) {
    let mut single = 0;
    let mut in_enum = 0;
    let mut nested = 0;
    for spec in decisions.values() {
        match spec {
            VariantSpec::SingleStruct => single += 1,
            VariantSpec::InEnum { .. } => in_enum += 1,
            VariantSpec::NestedField { .. } => nested += 1,
        }
    }
    (single, in_enum, nested)
}

pub fn classify_all(unified: &UnifiedSchema) -> BTreeMap<String, VariantSpec> {
    let descendants = build_descendant_index(unified);
    let polymorphic_targets = collect_polymorphic_targets(unified);

    let mut out = BTreeMap::new();
    for entity in unified.entity_parents.keys() {
        let spec = classify_entity(entity, unified, &descendants, &polymorphic_targets);
        out.insert(entity.clone(), spec);
    }
    out
}

fn classify_entity(
    entity: &str,
    unified: &UnifiedSchema,
    descendants: &HashMap<String, Vec<String>>,
    polymorphic_targets: &HashSet<String>,
) -> VariantSpec {
    if let Some(nested) = try_nested_field(entity, unified, descendants, polymorphic_targets) {
        return nested;
    }
    if let Some(root) = enclosing_enum_root(entity, unified, descendants, polymorphic_targets) {
        return VariantSpec::InEnum { enum_name: root };
    }
    VariantSpec::SingleStruct
}

fn try_nested_field(
    entity: &str,
    unified: &UnifiedSchema,
    descendants: &HashMap<String, Vec<String>>,
    polymorphic_targets: &HashSet<String>,
) -> Option<VariantSpec> {
    let parents = unified.entity_parents.get(entity)?;
    if parents.len() != 1 {
        return None;
    }
    let parent = parents.iter().next()?.clone();

    if polymorphic_targets.contains(entity) {
        return None;
    }

    let own_attrs = unified.entity_attrs.get(entity).cloned().unwrap_or_default();
    let parent_attrs = unified.entity_attrs.get(&parent).cloned().unwrap_or_default();
    let extra: std::collections::BTreeSet<String> =
        own_attrs.difference(&parent_attrs).cloned().collect();
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

    let extending_siblings = concrete_descendants(&parent, unified, descendants)
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
        into: parent,
        as_field,
        added_attr_count: added_count,
    })
}

fn enclosing_enum_root(
    entity: &str,
    unified: &UnifiedSchema,
    descendants: &HashMap<String, Vec<String>>,
    polymorphic_targets: &HashSet<String>,
) -> Option<String> {
    let mut chain: Vec<String> = vec![entity.to_string()];
    let mut current = entity.to_string();
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current.clone()) {
            break;
        }
        let parents = unified
            .entity_parents
            .get(&current)
            .cloned()
            .unwrap_or_default();
        let Some(parent) = parents.iter().next().cloned() else {
            break;
        };
        chain.push(parent.clone());
        current = parent;
    }

    for candidate in chain {
        if !polymorphic_targets.contains(&candidate) {
            continue;
        }
        let concrete = concrete_descendants(&candidate, unified, descendants);
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

fn concrete_descendants(
    root: &str,
    unified: &UnifiedSchema,
    descendants: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let _ = unified;
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
