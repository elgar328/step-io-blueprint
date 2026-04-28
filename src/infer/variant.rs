//! Stage 1 — variant classification.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::express::Schema;
use crate::infer::io::{ConfidentFile, PendingFile, PendingStats};
use crate::infer::overrides::{self, OverrideFile};
use crate::infer::refgraph::{self, RefTarget, UnifiedSchema};
use crate::infer::{Bucket, Confidence, Decision, DecisionSource, InferResult, Unresolved};

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
    },
}

const FILE_CONFIDENT: &str = "variants.toml";
const FILE_PENDING: &str = "variants_pending.toml";
const FILE_OVERRIDES: &str = "variants_overrides.toml";
const SECTION: &str = "entity";

pub fn run(schemas: &[Schema]) -> Result<(), String> {
    let unified = refgraph::build(schemas);
    let overrides_file: OverrideFile<VariantSpec> =
        overrides::load(FILE_OVERRIDES).map_err(|e| format!("load overrides: {e}"))?;

    let known: BTreeSet<String> = unified.entity_parents.keys().cloned().collect();
    let mut errs = overrides::validate_known(&overrides_file, SECTION, &known, FILE_OVERRIDES);
    errs.extend(overrides::validate_no_conflict(&overrides_file, SECTION, FILE_OVERRIDES));
    if !errs.is_empty() {
        return Err(errs.join("\n"));
    }

    let auto = compute_auto_decisions(&unified);
    let result = merge_overrides(auto, &overrides_file)?;

    let confident_outer = ConfidentFile {
        items: {
            let mut m = BTreeMap::new();
            m.insert(SECTION.to_string(), result.confident.clone());
            m
        },
    };
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

    eprintln!(
        "infer variant: confident={} review={} unresolved={} (total={})",
        pending.stats.confident,
        pending.stats.review,
        pending.stats.unresolved,
        pending.stats.total,
    );
    Ok(())
}

/// Auto decisions, before override merging. Each entity gets exactly one
/// `Decision<VariantSpec>` OR is recorded as unresolved.
struct AutoDecisions {
    entities: BTreeMap<String, AutoEntry>,
}

enum AutoEntry {
    Decided(Decision<VariantSpec>),
    Unresolved(Unresolved),
}

fn compute_auto_decisions(unified: &UnifiedSchema) -> AutoDecisions {
    let descendants = build_descendant_index(unified);
    let polymorphic_targets = collect_polymorphic_targets(unified);
    let conflict_keys: BTreeSet<&String> = unified
        .attr_conflicts
        .keys()
        .map(|(ent, _)| ent)
        .collect();

    let mut entities = BTreeMap::new();
    for entity in unified.entity_parents.keys() {
        if conflict_keys.contains(entity) {
            entities.insert(
                entity.clone(),
                AutoEntry::Unresolved(Unresolved {
                    reasons: vec![format!(
                        "cross-schema ATTR type conflict on this entity ({} disagreement(s))",
                        unified
                            .attr_conflicts
                            .keys()
                            .filter(|(e, _)| e == entity)
                            .count()
                    )],
                    override_example: format!(
                        "[entity.{entity}]\nkind = \"single_struct\"   # or in_enum / nested_field"
                    ),
                }),
            );
            continue;
        }

        let entry = classify_entity(entity, unified, &descendants, &polymorphic_targets);
        entities.insert(entity.clone(), entry);
    }
    AutoDecisions { entities }
}

fn classify_entity(
    entity: &str,
    unified: &UnifiedSchema,
    descendants: &HashMap<String, Vec<String>>,
    polymorphic_targets: &HashSet<String>,
) -> AutoEntry {
    if let Some(nested) = try_nested_field(entity, unified, descendants, polymorphic_targets) {
        return AutoEntry::Decided(nested);
    }

    let parents = unified
        .entity_parents
        .get(entity)
        .cloned()
        .unwrap_or_default();

    let enum_root = enclosing_enum_root(entity, unified, descendants, polymorphic_targets);
    if let Some(root) = enum_root {
        let conf = enum_candidate_confidence(&root, unified, descendants, polymorphic_targets);
        return AutoEntry::Decided(Decision {
            data: VariantSpec::InEnum {
                enum_name: root.clone(),
            },
            source: DecisionSource::Auto,
            confidence: conf,
            reasons: vec![format!(
                "polymorphic supertype {root:?}: {} concrete descendants, polymorphic context present",
                concrete_descendants(&root, unified, descendants).len()
            )],
        });
    }

    let mut reasons = vec!["no polymorphic supertype context".to_string()];
    if !parents.is_empty() {
        reasons.push(format!("parents: {}", parents.iter().cloned().collect::<Vec<_>>().join(", ")));
    }
    AutoEntry::Decided(Decision {
        data: VariantSpec::SingleStruct,
        source: DecisionSource::Auto,
        confidence: Confidence::new(0.95),
        reasons,
    })
}

fn try_nested_field(
    entity: &str,
    unified: &UnifiedSchema,
    descendants: &HashMap<String, Vec<String>>,
    polymorphic_targets: &HashSet<String>,
) -> Option<Decision<VariantSpec>> {
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
    let extra: BTreeSet<String> = own_attrs.difference(&parent_attrs).cloned().collect();
    let added_count = extra.len();
    if added_count == 0 || added_count > 3 {
        return None;
    }

    let extending_siblings = concrete_descendants(&parent, unified, descendants)
        .iter()
        .filter(|s| {
            let s_attrs = unified.entity_attrs.get(*s).cloned().unwrap_or_default();
            !s_attrs.difference(&parent_attrs).next().is_none()
        })
        .count();
    if extending_siblings != 1 {
        return None;
    }

    let base = match added_count {
        1 => 0.90,
        2 => 0.70,
        3 => 0.55,
        _ => unreachable!(),
    };

    let as_field = if added_count == 1 {
        extra.iter().next().cloned().unwrap()
    } else {
        synthesize_field_name(entity, &parent)
    };

    Some(Decision {
        data: VariantSpec::NestedField {
            into: parent,
            as_field,
        },
        source: DecisionSource::Auto,
        confidence: Confidence::new(base),
        reasons: vec![format!(
            "subtype with {} added attr(s), no own polymorphic context, sole extending sibling",
            added_count
        )],
    })
}

fn synthesize_field_name(entity: &str, parent: &str) -> String {
    let stripped = entity
        .strip_prefix(parent)
        .or_else(|| entity.strip_suffix(parent))
        .unwrap_or(entity)
        .trim_matches('_');
    if stripped.is_empty() {
        entity.to_string()
    } else {
        stripped.to_string()
    }
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

    let mut best: Option<String> = None;
    for candidate in chain {
        if !polymorphic_targets.contains(&candidate) {
            continue;
        }
        let concrete = concrete_descendants(&candidate, unified, descendants);
        if concrete.len() < 2 {
            continue;
        }
        if !concrete.iter().any(|d| d == entity)
            && unified
                .entity_attrs
                .get(entity)
                .is_some()
        {
            continue;
        }
        best = Some(candidate);
        break;
    }
    best
}

fn enum_candidate_confidence(
    root: &str,
    unified: &UnifiedSchema,
    descendants: &HashMap<String, Vec<String>>,
    polymorphic_targets: &HashSet<String>,
) -> Confidence {
    let poly_count = polymorphic_context_count(root, unified);
    let descendants = concrete_descendants(root, unified, descendants);
    let n = descendants.len();

    let p_score = match poly_count {
        0 => 0.0,
        1 => 0.5,
        2..=3 => 0.8,
        _ => 1.0,
    };
    let v_score = match n {
        0 | 1 => 0.0,
        2 => 0.4,
        3..=15 => 1.0,
        16..=30 => 0.8,
        31..=50 => 0.5,
        _ => 0.3,
    };
    let multi_parent_descendants = descendants
        .iter()
        .filter(|d| {
            unified
                .entity_parents
                .get(*d)
                .map_or(false, |p| p.len() > 1)
        })
        .count();
    let s_score = if descendants.is_empty() {
        1.0
    } else {
        1.0 - (multi_parent_descendants as f32 / descendants.len() as f32)
    };

    Confidence::new(0.4 * p_score + 0.3 * v_score + 0.3 * s_score)
}

fn polymorphic_context_count(root: &str, unified: &UnifiedSchema) -> usize {
    let mut locations: HashSet<(String, String)> = HashSet::new();
    for edge in &unified.edges {
        if let RefTarget::Entity(target) = &edge.target {
            if target == root {
                locations.insert((edge.from.clone(), edge.attr.clone()));
            }
        }
    }
    locations.len()
}

fn concrete_descendants(
    root: &str,
    unified: &UnifiedSchema,
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

fn merge_overrides(
    auto: AutoDecisions,
    overrides_file: &OverrideFile<VariantSpec>,
) -> Result<InferResult<VariantSpec>, String> {
    let mut confident = BTreeMap::new();
    let mut review = BTreeMap::new();
    let mut unresolved = BTreeMap::new();
    let mut errors = Vec::new();

    let accept_set: BTreeSet<&String> = overrides_file.batch_accept.entries.iter().collect();

    for (key, entry) in auto.entities {
        if let Some(override_spec) = overrides_file.entity.get(&key) {
            let prior_conf = match &entry {
                AutoEntry::Decided(d) => d.confidence,
                AutoEntry::Unresolved(_) => Confidence::new(1.0),
            };
            let dec = Decision {
                data: override_spec.clone(),
                source: DecisionSource::Override,
                confidence: prior_conf,
                reasons: Vec::new(),
            };
            confident.insert(key, dec);
            continue;
        }

        if accept_set.contains(&key) {
            match entry {
                AutoEntry::Decided(d) => {
                    let bucket = d.bucket();
                    match bucket {
                        Bucket::Confident => {
                            errors.push(format!(
                                "{FILE_OVERRIDES}: batch_accept.entries lists {key:?}, but it's already in the confident bucket. Remove the entry."
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
                                "{FILE_OVERRIDES}: batch_accept.entries lists {key:?}, but it has no auto decision (unresolved). Use an explicit override instead."
                            ));
                        }
                    }
                }
                AutoEntry::Unresolved(_) => {
                    errors.push(format!(
                        "{FILE_OVERRIDES}: batch_accept.entries lists {key:?}, but it has no auto decision (unresolved). Use an explicit override instead."
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
                    let reasons = d.reasons;
                    unresolved.insert(
                        key,
                        Unresolved {
                            reasons,
                            override_example: format!(
                                "kind = \"single_struct\"   # or in_enum / nested_field"
                            ),
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
        // ENTITY surface; (abstract-ish, polymorphic via SELECT)
        // ENTITY plane SUBTYPE OF (surface);
        // ENTITY cylinder SUBTYPE OF (surface);
        // ENTITY user; geom : surface;
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
        let auto = compute_auto_decisions(&unified);

        let plane = match auto.entities.get("plane").unwrap() {
            AutoEntry::Decided(d) => d,
            _ => panic!("expected decided"),
        };
        match &plane.data {
            VariantSpec::InEnum { enum_name } => assert_eq!(enum_name, "surface"),
            other => panic!("expected InEnum, got {other:?}"),
        }
    }

    #[test]
    fn nested_field_for_unique_extending_subtype() {
        // ENTITY base; x : INTEGER;
        // ENTITY ext SUBTYPE OF (base); y : REAL;
        // (no polymorphic context on ext, ext is the sole extending sibling)
        let s = schema(
            "test",
            vec![
                ent("base", &[], vec![("x", AttrType::Primitive("INTEGER".into()))]),
                ent("ext", &["base"], vec![
                    ("x", AttrType::Primitive("INTEGER".into())),
                    ("y", AttrType::Primitive("REAL".into())),
                ]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let auto = compute_auto_decisions(&unified);
        let ext = match auto.entities.get("ext").unwrap() {
            AutoEntry::Decided(d) => d,
            _ => panic!("expected decided"),
        };
        match &ext.data {
            VariantSpec::NestedField { into, as_field } => {
                assert_eq!(into, "base");
                assert_eq!(as_field, "y");
            }
            other => panic!("expected NestedField, got {other:?}"),
        }
    }

    #[test]
    fn single_struct_for_isolated_entity() {
        let s = schema(
            "test",
            vec![ent("foo", &[], vec![("x", AttrType::Primitive("INTEGER".into()))])],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let auto = compute_auto_decisions(&unified);
        let foo = match auto.entities.get("foo").unwrap() {
            AutoEntry::Decided(d) => d,
            _ => panic!("expected decided"),
        };
        assert!(matches!(&foo.data, VariantSpec::SingleStruct));
    }

    #[test]
    fn sibling_with_different_extra_attr_is_not_nested() {
        // base / sub_a (adds a) / sub_b (adds b). Two extending siblings →
        // nested_field rule should reject both.
        let s = schema(
            "test",
            vec![
                ent("base", &[], vec![("x", AttrType::Primitive("INTEGER".into()))]),
                ent("sub_a", &["base"], vec![
                    ("x", AttrType::Primitive("INTEGER".into())),
                    ("a", AttrType::Primitive("REAL".into())),
                ]),
                ent("sub_b", &["base"], vec![
                    ("x", AttrType::Primitive("INTEGER".into())),
                    ("b", AttrType::Primitive("STRING".into())),
                ]),
            ],
            vec![],
        );
        let unified = refgraph::build(&[s]);
        let auto = compute_auto_decisions(&unified);
        for name in ["sub_a", "sub_b"] {
            let d = match auto.entities.get(name).unwrap() {
                AutoEntry::Decided(d) => d,
                _ => panic!("{name}: expected decided"),
            };
            assert!(
                !matches!(&d.data, VariantSpec::NestedField { .. }),
                "{name}: should not be NestedField (sibling extends too)"
            );
        }
    }
}
