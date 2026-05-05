//! Reshape stage — apply split / merge abstractions to entities.toml.
//!
//! Reads entities.toml + splits.toml + merges.toml, validates that
//! split sources / merge absorbs exist and have compatible variant
//! kinds, then writes abstract_entities.toml with the abstractions
//! applied. Empty input files leave the output a verbatim copy of
//! entities.toml — Phase 1 infrastructure ships with no abstractions.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::infer::shape::EntitySummary;
use crate::infer::variant::VariantSpec;

const FILE_ENTITIES: &str = "entities.toml";
const FILE_SPLITS: &str = "splits.toml";
const FILE_MERGES: &str = "merges.toml";
const FILE_ABSTRACT_ENTITIES: &str = "abstract_entities.toml";

#[derive(Debug, Default, Deserialize)]
struct SplitsFile {
    #[serde(default)]
    split: BTreeMap<String, SplitEntry>,
}

#[derive(Debug, Deserialize)]
struct SplitEntry {
    context_signal: String,
    variants: Vec<SplitVariant>,
}

#[derive(Debug, Deserialize)]
struct SplitVariant {
    name: String,
    #[allow(dead_code)]
    suffix: String,
    arena: String,
}

#[derive(Debug, Default, Deserialize)]
struct MergesFile {
    #[serde(default)]
    merge: BTreeMap<String, MergeEntry>,
}

#[derive(Debug, Deserialize)]
struct MergeEntry {
    target_name: String,
    arena: String,
    absorbs: Vec<String>,
    #[serde(default)]
    fields_strategy: FieldsStrategy,
}

#[derive(Debug, Default, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum FieldsStrategy {
    #[default]
    Union,
    First,
}

pub fn run() -> Result<(), String> {
    let entities: BTreeMap<String, EntitySummary> =
        crate::infer::io::read_confident(FILE_ENTITIES, "entity")
            .map_err(|e| format!("read {FILE_ENTITIES}: {e}"))?;
    if entities.is_empty() {
        return Err(format!(
            "{FILE_ENTITIES} is empty or missing — run `infer shape` first."
        ));
    }
    let splits = load_splits()?;
    let merges = load_merges()?;

    validate_splits(&splits, &entities);
    validate_merges(&merges, &entities);

    let abstract_entities = apply_splits_merges(&entities, &splits, &merges)?;
    write_abstract_entities(&abstract_entities)?;

    eprintln!(
        "infer reshape: wrote {FILE_ABSTRACT_ENTITIES} ({} entities, {} splits, {} merges)",
        abstract_entities.len(),
        splits.split.len(),
        merges.merge.len()
    );
    Ok(())
}

fn load_splits() -> Result<SplitsFile, String> {
    let path = Path::new("inferred").join(FILE_SPLITS);
    if !path.exists() {
        return Ok(SplitsFile::default());
    }
    let body = fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    toml::from_str(&body).map_err(|e| format!("parse {path:?}: {e}"))
}

fn load_merges() -> Result<MergesFile, String> {
    let path = Path::new("inferred").join(FILE_MERGES);
    if !path.exists() {
        return Ok(MergesFile::default());
    }
    let body = fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    toml::from_str(&body).map_err(|e| format!("parse {path:?}: {e}"))
}

fn kind_str(spec: &VariantSpec) -> &'static str {
    match spec {
        VariantSpec::SingleStruct => "single_struct",
        VariantSpec::InEnum { .. } => "in_enum",
        VariantSpec::EnumBase { .. } => "enum_base",
        VariantSpec::ConcreteSupertype => "concrete_supertype",
        VariantSpec::ComplexSupertype { .. } => "complex_supertype",
        VariantSpec::CompositeOneOf { .. } => "composite_one_of",
        VariantSpec::NestedField { .. } => "nested_field",
        VariantSpec::MergedInto { .. } => "merged_into",
    }
}

fn validate_splits(splits: &SplitsFile, entities: &BTreeMap<String, EntitySummary>) {
    for (k, _) in &splits.split {
        match entities.get(k) {
            None => eprintln!(
                "warning: {FILE_SPLITS} [split.{k}] — entity not in {FILE_ENTITIES}"
            ),
            Some(s) => match &s.variant {
                VariantSpec::NestedField { .. } | VariantSpec::MergedInto { .. } => {
                    eprintln!(
                        "warning: {FILE_SPLITS} [split.{k}] — entity is {} (split unsupported on absorbed entities)",
                        kind_str(&s.variant)
                    );
                }
                _ => {}
            },
        }
    }
}

fn validate_merges(merges: &MergesFile, entities: &BTreeMap<String, EntitySummary>) {
    for (k, m) in &merges.merge {
        for absorb in &m.absorbs {
            match entities.get(absorb) {
                None => eprintln!(
                    "warning: {FILE_MERGES} [merge.{k}] absorbs unknown entity {absorb}"
                ),
                Some(s) => match &s.variant {
                    VariantSpec::NestedField { .. } | VariantSpec::MergedInto { .. } => {
                        eprintln!(
                            "warning: {FILE_MERGES} [merge.{k}] absorbs {absorb} which is already {} (no IR struct)",
                            kind_str(&s.variant)
                        );
                    }
                    _ => {}
                },
            }
        }
    }
}

fn apply_splits_merges(
    entities: &BTreeMap<String, EntitySummary>,
    splits: &SplitsFile,
    merges: &MergesFile,
) -> Result<BTreeMap<String, EntitySummary>, String> {
    // 1. Start from a clone of the input entities.
    let mut out: BTreeMap<String, EntitySummary> = entities.clone();

    // 2. Apply splits: for each [split.<source>], the first variant takes
    //    over the source entity's slot (with its arena adjusted), and the
    //    remaining variants are added as virtual entities with split_from
    //    metadata.
    for (source, entry) in &splits.split {
        let Some(base) = out.remove(source) else {
            continue; // already warned in validate_splits
        };
        let mut variants_iter = entry.variants.iter();
        let first = match variants_iter.next() {
            Some(v) => v,
            None => {
                out.insert(source.clone(), base);
                continue;
            }
        };

        // First variant: keep the source name, adjust arena.
        let mut first_summary = base.clone();
        first_summary.arena = first.arena.clone();
        out.insert(first.name.clone(), first_summary);

        // Remaining variants: virtual entities with split_from metadata.
        for variant in variants_iter {
            let mut virt = base.clone();
            virt.arena = variant.arena.clone();
            virt.group = variant.arena.clone();
            virt.split_from = Some(source.clone());
            virt.split_context = Some(entry.context_signal.clone());
            out.insert(variant.name.clone(), virt);
        }
    }

    // 3. Apply merges: remove the absorbs and add the target as a single
    //    entity with merge_absorbs metadata. fields_union flag carries the
    //    strategy for downstream stages.
    for (key, entry) in &merges.merge {
        // Pick a "base" template to seed the merged entity from one of
        // the absorbs (first that exists). If none exist, skip.
        let mut base: Option<EntitySummary> = None;
        for absorb in &entry.absorbs {
            if let Some(s) = out.remove(absorb) {
                if base.is_none() {
                    base = Some(s);
                }
            }
        }
        let Some(mut merged) = base else {
            continue;
        };
        // Merged entity becomes a plain SingleStruct in the abstraction.
        merged.variant = VariantSpec::SingleStruct;
        merged.arena = entry.arena.clone();
        merged.group = entry.target_name.clone();
        merged.merge_absorbs = entry.absorbs.clone();
        merged.fields_union = matches!(entry.fields_strategy, FieldsStrategy::Union);
        let _ = key; // entries keyed for readability; target_name carries the name.
        out.insert(entry.target_name.clone(), merged);
    }

    Ok(out)
}

fn write_abstract_entities(
    entities: &BTreeMap<String, EntitySummary>,
) -> Result<(), String> {
    let mut outer: BTreeMap<&str, &BTreeMap<String, EntitySummary>> = BTreeMap::new();
    outer.insert("entity", entities);
    let body = toml::to_string_pretty(&outer)
        .map_err(|e| format!("serialize {FILE_ABSTRACT_ENTITIES}: {e}"))?;
    let header = "# Generated by `infer reshape`. Do not edit manually.\n\
                  # Inputs: entities.toml + splits.toml + merges.toml + schemas/*.exp\n\n";
    fs::write(
        Path::new("inferred").join(FILE_ABSTRACT_ENTITIES),
        format!("{header}{body}"),
    )
    .map_err(|e| format!("write {FILE_ABSTRACT_ENTITIES}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::shape::ConcreteSupertypeShape;

    fn summary(variant: VariantSpec, arena: &str) -> EntitySummary {
        EntitySummary {
            variant,
            group: arena.to_string(),
            arena: arena.to_string(),
            shape: None,
            instance_count: 0,
            split_from: None,
            split_context: None,
            merge_absorbs: Vec::new(),
            fields_union: false,
        }
    }

    #[test]
    fn empty_inputs_yield_verbatim_copy() {
        let mut entities = BTreeMap::new();
        entities.insert("a".into(), summary(VariantSpec::SingleStruct, "a"));
        entities.insert("b".into(), summary(VariantSpec::SingleStruct, "b"));
        let splits = SplitsFile::default();
        let merges = MergesFile::default();
        let out = apply_splits_merges(&entities, &splits, &merges).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.contains_key("a"));
        assert!(out.contains_key("b"));
        for (_, v) in &out {
            assert!(v.split_from.is_none());
            assert!(v.merge_absorbs.is_empty());
        }
    }

    #[test]
    fn split_one_entity_into_two() {
        let mut entities = BTreeMap::new();
        entities.insert(
            "cartesian_point".into(),
            summary(VariantSpec::SingleStruct, "cartesian_point"),
        );
        let splits = SplitsFile {
            split: BTreeMap::from([(
                "cartesian_point".to_string(),
                SplitEntry {
                    context_signal: "is_2d".into(),
                    variants: vec![
                        SplitVariant {
                            name: "cartesian_point".into(),
                            suffix: "3d".into(),
                            arena: "cartesian_point".into(),
                        },
                        SplitVariant {
                            name: "cartesian_point_2d".into(),
                            suffix: "2d".into(),
                            arena: "cartesian_point_2d".into(),
                        },
                    ],
                },
            )]),
        };
        let merges = MergesFile::default();
        let out = apply_splits_merges(&entities, &splits, &merges).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out["cartesian_point"].split_from.is_none());
        assert_eq!(
            out["cartesian_point_2d"].split_from.as_deref(),
            Some("cartesian_point")
        );
        assert_eq!(
            out["cartesian_point_2d"].split_context.as_deref(),
            Some("is_2d")
        );
        assert_eq!(out["cartesian_point_2d"].arena, "cartesian_point_2d");
    }

    #[test]
    fn merge_three_entities_into_one() {
        let mut entities = BTreeMap::new();
        for name in ["b_spline_curve", "rational_b_spline_curve", "quasi_uniform_curve"] {
            entities.insert(name.into(), summary(VariantSpec::SingleStruct, name));
        }
        let splits = SplitsFile::default();
        let merges = MergesFile {
            merge: BTreeMap::from([(
                "nurbs".to_string(),
                MergeEntry {
                    target_name: "nurbs_curve".into(),
                    arena: "nurbs_curve".into(),
                    absorbs: vec![
                        "b_spline_curve".into(),
                        "rational_b_spline_curve".into(),
                        "quasi_uniform_curve".into(),
                    ],
                    fields_strategy: FieldsStrategy::Union,
                },
            )]),
        };
        let out = apply_splits_merges(&entities, &splits, &merges).unwrap();
        assert_eq!(out.len(), 1);
        let nurbs = &out["nurbs_curve"];
        assert_eq!(nurbs.merge_absorbs.len(), 3);
        assert!(nurbs.fields_union);
        assert_eq!(nurbs.arena, "nurbs_curve");
    }

    #[test]
    fn merge_first_strategy_clears_union_flag() {
        let mut entities = BTreeMap::new();
        entities.insert("a".into(), summary(VariantSpec::SingleStruct, "a"));
        entities.insert("b".into(), summary(VariantSpec::SingleStruct, "b"));
        let splits = SplitsFile::default();
        let merges = MergesFile {
            merge: BTreeMap::from([(
                "m".to_string(),
                MergeEntry {
                    target_name: "ab".into(),
                    arena: "ab".into(),
                    absorbs: vec!["a".into(), "b".into()],
                    fields_strategy: FieldsStrategy::First,
                },
            )]),
        };
        let out = apply_splits_merges(&entities, &splits, &merges).unwrap();
        assert!(!out["ab"].fields_union);
    }

    #[test]
    fn meta_fields_round_trip_through_toml() {
        let mut entities = BTreeMap::new();
        let mut s = summary(VariantSpec::SingleStruct, "cp_2d");
        s.split_from = Some("cartesian_point".into());
        s.split_context = Some("is_2d".into());
        entities.insert("cartesian_point_2d".into(), s);

        let mut outer: BTreeMap<&str, &BTreeMap<String, EntitySummary>> = BTreeMap::new();
        outer.insert("entity", &entities);
        let body = toml::to_string_pretty(&outer).unwrap();
        assert!(body.contains("split_from = \"cartesian_point\""));
        assert!(body.contains("split_context = \"is_2d\""));

        #[derive(Deserialize)]
        struct Outer {
            entity: BTreeMap<String, EntitySummary>,
        }
        let parsed: Outer = toml::from_str(&body).unwrap();
        assert_eq!(parsed.entity, entities);
    }

    #[test]
    fn shape_passes_through_split() {
        let mut entities = BTreeMap::new();
        let mut s = summary(VariantSpec::ConcreteSupertype, "face_bound");
        s.shape = Some(ConcreteSupertypeShape::Carrier);
        entities.insert("face_bound".into(), s);
        let splits = SplitsFile::default();
        let merges = MergesFile::default();
        let out = apply_splits_merges(&entities, &splits, &merges).unwrap();
        assert_eq!(out["face_bound"].shape, Some(ConcreteSupertypeShape::Carrier));
    }
}
