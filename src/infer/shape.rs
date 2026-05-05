//! ConcreteSupertype IR shape decision (manual input + validation).
//!
//! Pure validation: compares the ConcreteSupertype set in
//! `variants_pruned.toml` against the entries in `shapes.toml`. Missing
//! required entries → Err stops the run; extra entries → warning,
//! ignored. No output file — the input file itself is the step-io
//! codegen input.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::infer::arena::{compute_entity_to_group, ArenaSpec};
use crate::infer::prune::UsageRecord;
use crate::infer::variant::VariantSpec;

const VARIANTS_PENDING: &str = "variants_pending.toml";
const ARENAS_PENDING: &str = "arenas_pending.toml";
const FILE_VARIANTS_PRUNED: &str = "variants_pruned.toml";
const FILE_CS_SHAPES: &str = "shapes.toml";

/// Serialized as a plain string (`"carrier"` / `"base_parallel"`) so it
/// fits inline in entity tables without producing a nested sub-table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcreteSupertypeShape {
    /// `enum E { Itself(EData), ChildA(...), ... }` — parent and children
    /// are equal-rank variants.
    Carrier,
    /// `struct E { /* parent attrs */ } enum EKind { ... }` — parent
    /// struct is primary, kind enum is the auxiliary axis.
    BaseParallel,
}

/// One entry in `shapes.toml`. The user writes `[entity.<name>] shape =
/// "..."`; this struct deserializes that table form into the inner enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct ShapeEntry {
    shape: ConcreteSupertypeShape,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConcreteSupertypeShapesFile {
    #[serde(default)]
    entity: BTreeMap<String, ShapeEntry>,
}

/// Per-entity row of the unified `entities.toml` view. Aggregates every
/// classification decision so downstream stages (naming, pool) take a
/// single file as input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntitySummary {
    #[serde(flatten)]
    pub variant: VariantSpec,
    pub group: String,
    pub arena: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<ConcreteSupertypeShape>,
    pub instance_count: usize,

    // Reshape stage metadata — shape stage leaves these at default,
    // reshape fills them when applying splits / merges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split_context: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merge_absorbs: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub fields_union: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

pub fn run(allow_pending: bool) -> Result<(), String> {
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

    let pruned: BTreeMap<String, VariantSpec> =
        crate::infer::io::read_confident(FILE_VARIANTS_PRUNED, "entity")
            .map_err(|e| format!("read {FILE_VARIANTS_PRUNED}: {e}"))?;
    if pruned.is_empty() {
        return Err(format!(
            "{FILE_VARIANTS_PRUNED} is empty or missing — run `infer prune` first."
        ));
    }
    let required: BTreeSet<String> = pruned
        .iter()
        .filter(|(_, v)| matches!(v, VariantSpec::ConcreteSupertype))
        .map(|(k, _)| k.clone())
        .collect();

    let path = Path::new("inferred").join(FILE_CS_SHAPES);
    if !path.exists() {
        return Err(missing_file_message(&required));
    }
    let body = fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    let file: ConcreteSupertypeShapesFile =
        toml::from_str(&body).map_err(|e| format!("parse {path:?}: {e}"))?;
    let provided: BTreeMap<String, ConcreteSupertypeShape> = file
        .entity
        .iter()
        .map(|(k, v)| (k.clone(), v.shape))
        .collect();

    match validate(&required, &provided) {
        Validation::Ok { carrier, base_parallel, extras } => {
            for e in &extras {
                eprintln!(
                    "warning: {FILE_CS_SHAPES} [entity.{e}] is not a ConcreteSupertype \
                     in {FILE_VARIANTS_PRUNED} — ignored"
                );
            }
            eprintln!(
                "infer shape: {} ConcreteSupertype entities (carrier={carrier} base_parallel={base_parallel})",
                required.len()
            );

            let arenas: BTreeMap<String, ArenaSpec> =
                crate::infer::io::read_confident("arenas_pruned.toml", "group")
                    .map_err(|e| format!("read arenas_pruned.toml: {e}"))?;
            let usage: BTreeMap<String, UsageRecord> =
                crate::infer::io::read_confident("usage.toml", "entity")
                    .map_err(|e| format!("read usage.toml: {e}"))?;
            let entities = compile_entities(&pruned, &arenas, &provided, &usage)?;
            write_entities_toml(&entities)?;
            eprintln!(
                "infer shape: wrote entities.toml ({} entities)",
                entities.len()
            );
            Ok(())
        }
        Validation::Missing(missing) => Err(missing_entries_message(&missing)),
    }
}

fn compile_entities(
    variants: &BTreeMap<String, VariantSpec>,
    arenas: &BTreeMap<String, ArenaSpec>,
    shapes: &BTreeMap<String, ConcreteSupertypeShape>,
    usage: &BTreeMap<String, UsageRecord>,
) -> Result<BTreeMap<String, EntitySummary>, String> {
    let entity_to_group = compute_entity_to_group(variants);

    let mut out = BTreeMap::new();
    for (entity, variant) in variants {
        let group = entity_to_group
            .get(entity)
            .ok_or_else(|| format!("entity {entity} has no group"))?
            .clone();
        let arena = arenas
            .get(&group)
            .ok_or_else(|| format!("group {group} missing in arenas_pruned.toml"))?
            .arena
            .clone();
        let shape = shapes.get(entity).copied();
        let instance_count = usage.get(entity).map(|u| u.instance_count).unwrap_or(0);
        out.insert(
            entity.clone(),
            EntitySummary {
                variant: variant.clone(),
                group,
                arena,
                shape,
                instance_count,
                split_from: None,
                split_context: None,
                merge_absorbs: Vec::new(),
                fields_union: false,
            },
        );
    }
    Ok(out)
}

fn write_entities_toml(
    entities: &BTreeMap<String, EntitySummary>,
) -> Result<(), String> {
    let mut outer: BTreeMap<&str, &BTreeMap<String, EntitySummary>> = BTreeMap::new();
    outer.insert("entity", entities);
    let body = toml::to_string_pretty(&outer)
        .map_err(|e| format!("serialize entities.toml: {e}"))?;
    let header = "# Generated by `infer shape`. Do not edit manually.\n\
                  # Inputs: variants_pruned.toml + arenas_pruned.toml + shapes.toml + usage.toml\n\n";
    fs::write(
        Path::new("inferred").join("entities.toml"),
        format!("{header}{body}"),
    )
    .map_err(|e| format!("write entities.toml: {e}"))
}

#[derive(Debug)]
enum Validation {
    Ok {
        carrier: usize,
        base_parallel: usize,
        extras: Vec<String>,
    },
    Missing(Vec<String>),
}

fn validate(
    required: &BTreeSet<String>,
    provided: &BTreeMap<String, ConcreteSupertypeShape>,
) -> Validation {
    let provided_keys: BTreeSet<&String> = provided.keys().collect();
    let required_refs: BTreeSet<&String> = required.iter().collect();

    let missing: Vec<String> = required_refs
        .difference(&provided_keys)
        .map(|s| (*s).clone())
        .collect();
    if !missing.is_empty() {
        return Validation::Missing(missing);
    }

    let extras: Vec<String> = provided_keys
        .difference(&required_refs)
        .map(|s| (*s).clone())
        .collect();

    let (mut carrier, mut base_parallel) = (0usize, 0usize);
    for (k, v) in provided {
        if !required.contains(k) {
            continue;
        }
        match v {
            ConcreteSupertypeShape::Carrier => carrier += 1,
            ConcreteSupertypeShape::BaseParallel => base_parallel += 1,
        }
    }
    Validation::Ok {
        carrier,
        base_parallel,
        extras,
    }
}

fn missing_file_message(required: &BTreeSet<String>) -> String {
    let list = required
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n  ");
    format!(
        "{FILE_CS_SHAPES} not found — required ConcreteSupertype entities ({}):\n  {list}\n\
         Add `[entity.<name>] shape = \"carrier\" | \"base_parallel\"` for each.",
        required.len()
    )
}

fn missing_entries_message(missing: &[String]) -> String {
    let list = missing.join("\n  ");
    format!(
        "{FILE_CS_SHAPES} missing {} required ConcreteSupertype entries:\n  {list}\n\
         Add `[entity.<name>] shape = \"carrier\" | \"base_parallel\"` for each.",
        missing.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required_set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn provided_map(
        pairs: &[(&str, ConcreteSupertypeShape)],
    ) -> BTreeMap<String, ConcreteSupertypeShape> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn validate_complete_match_counts_shapes() {
        let required = required_set(&["face_bound", "styled_item"]);
        let provided = provided_map(&[
            ("face_bound", ConcreteSupertypeShape::Carrier),
            ("styled_item", ConcreteSupertypeShape::BaseParallel),
        ]);
        match validate(&required, &provided) {
            Validation::Ok {
                carrier,
                base_parallel,
                extras,
            } => {
                assert_eq!(carrier, 1);
                assert_eq!(base_parallel, 1);
                assert!(extras.is_empty());
            }
            Validation::Missing(_) => panic!("expected Ok"),
        }
    }

    #[test]
    fn validate_missing_entry_returns_missing_list() {
        let required = required_set(&["face_bound", "styled_item"]);
        let provided = provided_map(&[("face_bound", ConcreteSupertypeShape::Carrier)]);
        match validate(&required, &provided) {
            Validation::Missing(missing) => {
                assert_eq!(missing, vec!["styled_item".to_string()]);
            }
            Validation::Ok { .. } => panic!("expected Missing"),
        }
    }

    #[test]
    fn validate_extra_entry_passes_with_warning_payload() {
        let required = required_set(&["face_bound"]);
        let provided = provided_map(&[
            ("face_bound", ConcreteSupertypeShape::Carrier),
            ("cartesian_point", ConcreteSupertypeShape::Carrier),
        ]);
        match validate(&required, &provided) {
            Validation::Ok {
                carrier,
                base_parallel,
                extras,
            } => {
                assert_eq!(carrier, 1);
                assert_eq!(base_parallel, 0);
                assert_eq!(extras, vec!["cartesian_point".to_string()]);
            }
            Validation::Missing(_) => panic!("expected Ok"),
        }
    }

    #[test]
    fn missing_file_message_lists_required() {
        let required = required_set(&["face_bound", "styled_item"]);
        let msg = missing_file_message(&required);
        assert!(msg.contains("face_bound"));
        assert!(msg.contains("styled_item"));
        assert!(msg.contains("required ConcreteSupertype entities (2)"));
    }

    #[test]
    fn missing_entries_message_lists_missing() {
        let msg = missing_entries_message(&["styled_item".into()]);
        assert!(msg.contains("styled_item"));
        assert!(msg.contains("missing 1 required"));
    }

    #[test]
    fn parses_toml_with_tagged_shape() {
        let body = r#"
[entity.face_bound]
shape = "carrier"

[entity.styled_item]
shape = "base_parallel"
"#;
        let file: ConcreteSupertypeShapesFile = toml::from_str(body).unwrap();
        assert_eq!(
            file.entity.get("face_bound").map(|e| e.shape),
            Some(ConcreteSupertypeShape::Carrier)
        );
        assert_eq!(
            file.entity.get("styled_item").map(|e| e.shape),
            Some(ConcreteSupertypeShape::BaseParallel)
        );
    }

    fn variants_with(pairs: &[(&str, VariantSpec)]) -> BTreeMap<String, VariantSpec> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn arenas_with(pairs: &[(&str, &str)]) -> BTreeMap<String, ArenaSpec> {
        pairs
            .iter()
            .map(|(g, a)| {
                (
                    g.to_string(),
                    ArenaSpec {
                        arena: a.to_string(),
                    },
                )
            })
            .collect()
    }

    fn usage_with(pairs: &[(&str, usize)]) -> BTreeMap<String, UsageRecord> {
        pairs
            .iter()
            .map(|(k, n)| {
                (
                    k.to_string(),
                    UsageRecord {
                        instance_count: *n,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn compile_entities_basic() {
        let variants = variants_with(&[
            ("cartesian_point", VariantSpec::SingleStruct),
            (
                "line",
                VariantSpec::InEnum {
                    enum_name: "curve".into(),
                },
            ),
            (
                "curve",
                VariantSpec::EnumBase {
                    enum_name: "curve".into(),
                },
            ),
            ("face_bound", VariantSpec::ConcreteSupertype),
        ]);
        let arenas = arenas_with(&[
            ("cartesian_point", "cartesian_point"),
            ("curve", "curve"),
            ("face_bound", "face_bound"),
        ]);
        let shapes: BTreeMap<String, ConcreteSupertypeShape> =
            [("face_bound".to_string(), ConcreteSupertypeShape::Carrier)]
                .into_iter()
                .collect();
        let usage = usage_with(&[
            ("cartesian_point", 100),
            ("line", 50),
            ("curve", 0),
            ("face_bound", 7),
        ]);

        let out = compile_entities(&variants, &arenas, &shapes, &usage).unwrap();

        assert_eq!(out["cartesian_point"].group, "cartesian_point");
        assert_eq!(out["cartesian_point"].arena, "cartesian_point");
        assert_eq!(out["cartesian_point"].instance_count, 100);
        assert!(out["cartesian_point"].shape.is_none());

        assert_eq!(out["line"].group, "curve");
        assert_eq!(out["line"].arena, "curve");

        assert_eq!(out["curve"].group, "curve");
        assert_eq!(out["curve"].arena, "curve");

        assert_eq!(out["face_bound"].group, "face_bound");
        assert_eq!(
            out["face_bound"].shape,
            Some(ConcreteSupertypeShape::Carrier)
        );
    }

    #[test]
    fn compile_entities_nested_field_inherits_parent_group() {
        let variants = variants_with(&[
            ("parent", VariantSpec::SingleStruct),
            (
                "child",
                VariantSpec::NestedField {
                    into: "parent".into(),
                    as_field: "child".into(),
                    added_attr_count: 1,
                },
            ),
        ]);
        let arenas = arenas_with(&[("parent", "parent")]);
        let shapes = BTreeMap::new();
        let usage = usage_with(&[("parent", 1), ("child", 0)]);

        let out = compile_entities(&variants, &arenas, &shapes, &usage).unwrap();

        assert_eq!(out["child"].group, "parent");
        assert_eq!(out["child"].arena, "parent");
    }

    #[test]
    fn compile_entities_merged_into_follows_chain() {
        let variants = variants_with(&[
            ("a", VariantSpec::SingleStruct),
            (
                "b",
                VariantSpec::MergedInto {
                    target: "a".into(),
                    chain: vec![],
                },
            ),
            (
                "c",
                VariantSpec::MergedInto {
                    target: "b".into(),
                    chain: vec![],
                },
            ),
        ]);
        let arenas = arenas_with(&[("a", "a")]);
        let shapes = BTreeMap::new();
        let usage = BTreeMap::new();

        let out = compile_entities(&variants, &arenas, &shapes, &usage).unwrap();

        assert_eq!(out["b"].group, "a");
        assert_eq!(out["c"].group, "a");
        assert_eq!(out["c"].arena, "a");
    }

    #[test]
    fn entity_summary_toml_roundtrip() {
        let variants = variants_with(&[
            ("cartesian_point", VariantSpec::SingleStruct),
            (
                "line",
                VariantSpec::InEnum {
                    enum_name: "curve".into(),
                },
            ),
            (
                "curve",
                VariantSpec::EnumBase {
                    enum_name: "curve".into(),
                },
            ),
            ("face_bound", VariantSpec::ConcreteSupertype),
            (
                "child",
                VariantSpec::NestedField {
                    into: "cartesian_point".into(),
                    as_field: "extra".into(),
                    added_attr_count: 2,
                },
            ),
        ]);
        let arenas = arenas_with(&[
            ("cartesian_point", "cartesian_point"),
            ("curve", "curve"),
            ("face_bound", "face_bound"),
        ]);
        let shapes: BTreeMap<String, ConcreteSupertypeShape> = [(
            "face_bound".to_string(),
            ConcreteSupertypeShape::BaseParallel,
        )]
        .into_iter()
        .collect();
        let usage = usage_with(&[("cartesian_point", 5)]);

        let entities = compile_entities(&variants, &arenas, &shapes, &usage).unwrap();

        // Serialize to TOML, then deserialize back. Catches any
        // flatten + tagged enum incompatibility in toml-rs.
        let mut outer: BTreeMap<&str, &BTreeMap<String, EntitySummary>> = BTreeMap::new();
        outer.insert("entity", &entities);
        let body = toml::to_string_pretty(&outer).unwrap();

        #[derive(Deserialize)]
        struct Outer {
            entity: BTreeMap<String, EntitySummary>,
        }
        let parsed: Outer = toml::from_str(&body).unwrap();

        assert_eq!(parsed.entity, entities);
    }
}
