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
    #[serde(default)]
    reasons: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SplitVariant {
    name: String,
    #[allow(dead_code)]
    suffix: String,
    arena: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    enum_of: Option<String>,
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
    #[serde(default)]
    reasons: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    enum_of: Option<String>,
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

fn variant_spec_from_override(
    base_spec: &VariantSpec,
    override_kind: Option<&str>,
    override_enum: Option<&str>,
    file_label: &str,
    diag_key: &str,
) -> Result<VariantSpec, String> {
    match (override_kind, override_enum) {
        (None, None) => Ok(base_spec.clone()),
        (None, Some(_)) => Err(format!(
            "{file_label} [{diag_key}]: enum_of set without kind"
        )),
        (Some("single_struct"), None) => Ok(VariantSpec::SingleStruct),
        (Some("single_struct"), Some(_)) => Err(format!(
            "{file_label} [{diag_key}]: enum_of incompatible with kind = \"single_struct\""
        )),
        (Some("in_enum"), Some(name)) => Ok(VariantSpec::InEnum {
            enum_name: name.to_string(),
        }),
        (Some("in_enum"), None) => Err(format!(
            "{file_label} [{diag_key}]: kind = \"in_enum\" requires enum_of"
        )),
        (Some(other), _) => Err(format!(
            "{file_label} [{diag_key}]: unsupported kind override '{other}' \
             (allowed: \"single_struct\", \"in_enum\")"
        )),
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
    //    metadata. The reasons rationale lives on the first variant only —
    //    the abstraction-decision property, not a per-variant property.
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

        // First variant — primary, carries reasons.
        let mut first_summary = base.clone();
        first_summary.arena = first.arena.clone();
        first_summary.variant = variant_spec_from_override(
            &base.variant,
            first.kind.as_deref(),
            first.enum_of.as_deref(),
            FILE_SPLITS,
            &format!("split.{source}.variants[0]"),
        )?;
        first_summary.reasons = entry.reasons.clone();
        out.insert(first.name.clone(), first_summary);

        // Remaining variants — derivatives, point back via split_from.
        // reasons stays None; readers follow split_from to find rationale.
        for (idx, variant) in variants_iter.enumerate() {
            let mut virt = base.clone();
            virt.arena = variant.arena.clone();
            virt.group = variant.arena.clone();
            virt.variant = variant_spec_from_override(
                &base.variant,
                variant.kind.as_deref(),
                variant.enum_of.as_deref(),
                FILE_SPLITS,
                &format!("split.{source}.variants[{}]", idx + 1),
            )?;
            virt.split_from = Some(source.clone());
            virt.split_context = Some(entry.context_signal.clone());
            out.insert(variant.name.clone(), virt);
        }
    }

    // 3. Apply merges: remove the absorbs and add the target as a single
    //    entity with merge_absorbs metadata. fields_union flag carries the
    //    strategy for downstream stages. reasons rationale rides on the
    //    target.
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
        // Merged entity defaults to SingleStruct; MergeEntry may override
        // to InEnum (member of a host enum) for N → 1 enum patterns.
        merged.variant = variant_spec_from_override(
            &VariantSpec::SingleStruct,
            entry.kind.as_deref(),
            entry.enum_of.as_deref(),
            FILE_MERGES,
            &format!("merge.{key}"),
        )?;
        merged.arena = entry.arena.clone();
        // group follows the variant: InEnum members share the host enum's
        // group (matches existing in_enum entities like advanced_face).
        merged.group = match &merged.variant {
            VariantSpec::InEnum { enum_name } => enum_name.clone(),
            _ => entry.target_name.clone(),
        };
        merged.merge_absorbs = entry.absorbs.clone();
        merged.fields_union = matches!(entry.fields_strategy, FieldsStrategy::Union);
        merged.reasons = entry.reasons.clone();
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
            reasons: None,
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
                            kind: None,
                            enum_of: None,
                        },
                        SplitVariant {
                            name: "cartesian_point_2d".into(),
                            suffix: "2d".into(),
                            arena: "cartesian_point_2d".into(),
                            kind: None,
                            enum_of: None,
                        },
                    ],
                    reasons: None,
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
                    reasons: None,
                    kind: None,
                    enum_of: None,
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
                    reasons: None,
                    kind: None,
                    enum_of: None,
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

    fn split_variant(
        name: &str,
        arena: &str,
        kind: Option<&str>,
        enum_of: Option<&str>,
    ) -> SplitVariant {
        SplitVariant {
            name: name.to_string(),
            suffix: name.to_string(),
            arena: arena.to_string(),
            kind: kind.map(str::to_string),
            enum_of: enum_of.map(str::to_string),
        }
    }

    fn direction_split_with(second: SplitVariant, reasons: Option<&str>) -> SplitsFile {
        SplitsFile {
            split: BTreeMap::from([(
                "direction".to_string(),
                SplitEntry {
                    context_signal: "is_2d".into(),
                    variants: vec![
                        split_variant("direction", "direction", None, None),
                        second,
                    ],
                    reasons: reasons.map(str::to_string),
                },
            )]),
        }
    }

    fn direction_seed() -> BTreeMap<String, EntitySummary> {
        let mut entities = BTreeMap::new();
        entities.insert(
            "direction".into(),
            summary(
                VariantSpec::InEnum {
                    enum_name: "geometric_representation_item".into(),
                },
                "direction",
            ),
        );
        entities
    }

    #[test]
    fn split_variant_kind_override_single_struct() {
        let entities = direction_seed();
        let splits = direction_split_with(
            split_variant("direction_2d", "direction_2d", Some("single_struct"), None),
            None,
        );
        let out = apply_splits_merges(&entities, &splits, &MergesFile::default()).unwrap();
        assert!(matches!(out["direction_2d"].variant, VariantSpec::SingleStruct));
        // First variant inherits source's InEnum (no override on it).
        assert!(matches!(
            out["direction"].variant,
            VariantSpec::InEnum { ref enum_name } if enum_name == "geometric_representation_item"
        ));
    }

    #[test]
    fn split_variant_kind_override_in_enum_with_enum_of() {
        let entities = direction_seed();
        let splits = direction_split_with(
            split_variant(
                "direction_2d",
                "direction_2d",
                Some("in_enum"),
                Some("planar_only"),
            ),
            None,
        );
        let out = apply_splits_merges(&entities, &splits, &MergesFile::default()).unwrap();
        match &out["direction_2d"].variant {
            VariantSpec::InEnum { enum_name } => assert_eq!(enum_name, "planar_only"),
            other => panic!("expected InEnum, got {other:?}"),
        }
    }

    #[test]
    fn split_variant_in_enum_missing_enum_of_errors() {
        let entities = direction_seed();
        let splits = direction_split_with(
            split_variant("direction_2d", "direction_2d", Some("in_enum"), None),
            None,
        );
        let err = apply_splits_merges(&entities, &splits, &MergesFile::default()).unwrap_err();
        assert!(err.contains("split.direction.variants[1]"));
        assert!(err.contains("requires enum_of"));
    }

    #[test]
    fn split_variant_orphan_enum_of_errors() {
        let entities = direction_seed();
        let splits = direction_split_with(
            split_variant("direction_2d", "direction_2d", None, Some("anything")),
            None,
        );
        let err = apply_splits_merges(&entities, &splits, &MergesFile::default()).unwrap_err();
        assert!(err.contains("split.direction.variants[1]"));
        assert!(err.contains("enum_of set without kind"));
    }

    #[test]
    fn split_variant_single_struct_with_enum_of_errors() {
        let entities = direction_seed();
        let splits = direction_split_with(
            split_variant(
                "direction_2d",
                "direction_2d",
                Some("single_struct"),
                Some("anything"),
            ),
            None,
        );
        let err = apply_splits_merges(&entities, &splits, &MergesFile::default()).unwrap_err();
        assert!(err.contains("split.direction.variants[1]"));
        assert!(err.contains("incompatible"));
    }

    #[test]
    fn split_variant_unsupported_kind_errors() {
        let entities = direction_seed();
        let splits = direction_split_with(
            split_variant("direction_2d", "direction_2d", Some("weird_kind"), None),
            None,
        );
        let err = apply_splits_merges(&entities, &splits, &MergesFile::default()).unwrap_err();
        assert!(err.contains("split.direction.variants[1]"));
        assert!(err.contains("unsupported kind override 'weird_kind'"));
    }

    #[test]
    fn split_reasons_lands_on_first_variant_only() {
        let entities = direction_seed();
        let splits = direction_split_with(
            split_variant("direction_2d", "direction_2d", Some("single_struct"), None),
            Some("2D direction lives outside the 3D enum"),
        );
        let out = apply_splits_merges(&entities, &splits, &MergesFile::default()).unwrap();
        assert_eq!(
            out["direction"].reasons.as_deref(),
            Some("2D direction lives outside the 3D enum")
        );
        // Virtual variant points back via split_from; reasons stays None.
        assert_eq!(out["direction_2d"].reasons, None);
        assert_eq!(out["direction_2d"].split_from.as_deref(), Some("direction"));
    }

    #[test]
    fn merge_reasons_lands_on_target() {
        let mut entities = BTreeMap::new();
        for name in ["b_spline_curve", "rational_b_spline_curve"] {
            entities.insert(name.into(), summary(VariantSpec::SingleStruct, name));
        }
        let merges = MergesFile {
            merge: BTreeMap::from([(
                "nurbs".to_string(),
                MergeEntry {
                    target_name: "nurbs_curve".into(),
                    arena: "nurbs_curve".into(),
                    absorbs: vec![
                        "b_spline_curve".into(),
                        "rational_b_spline_curve".into(),
                    ],
                    fields_strategy: FieldsStrategy::Union,
                    reasons: Some("Mathematical equivalence — unify under one type".into()),
                    kind: None,
                    enum_of: None,
                },
            )]),
        };
        let out = apply_splits_merges(&entities, &SplitsFile::default(), &merges).unwrap();
        assert_eq!(
            out["nurbs_curve"].reasons.as_deref(),
            Some("Mathematical equivalence — unify under one type")
        );
    }

    fn merge_with_override(
        target: &str,
        absorbs: Vec<&str>,
        kind: Option<&str>,
        enum_of: Option<&str>,
    ) -> MergesFile {
        MergesFile {
            merge: BTreeMap::from([(
                "m".to_string(),
                MergeEntry {
                    target_name: target.to_string(),
                    arena: target.to_string(),
                    absorbs: absorbs.into_iter().map(String::from).collect(),
                    fields_strategy: FieldsStrategy::Union,
                    reasons: None,
                    kind: kind.map(str::to_string),
                    enum_of: enum_of.map(str::to_string),
                },
            )]),
        }
    }

    fn merge_seed(absorbs: &[&str]) -> BTreeMap<String, EntitySummary> {
        let mut entities = BTreeMap::new();
        for name in absorbs {
            entities.insert(
                (*name).to_string(),
                summary(VariantSpec::SingleStruct, name),
            );
        }
        entities
    }

    #[test]
    fn merge_target_kind_override_single_struct() {
        let entities = merge_seed(&["a", "b"]);
        let merges = merge_with_override("ab", vec!["a", "b"], Some("single_struct"), None);
        let out = apply_splits_merges(&entities, &SplitsFile::default(), &merges).unwrap();
        assert!(matches!(out["ab"].variant, VariantSpec::SingleStruct));
        assert_eq!(out["ab"].group, "ab");
        assert_eq!(out["ab"].arena, "ab");
    }

    #[test]
    fn merge_target_kind_override_in_enum_with_enum_of() {
        let entities = merge_seed(&["a", "b"]);
        let merges = merge_with_override("ab", vec!["a", "b"], Some("in_enum"), Some("curve"));
        let out = apply_splits_merges(&entities, &SplitsFile::default(), &merges).unwrap();
        match &out["ab"].variant {
            VariantSpec::InEnum { enum_name } => assert_eq!(enum_name, "curve"),
            other => panic!("expected InEnum, got {other:?}"),
        }
        // group auto-follows enum_name (matches existing in_enum pattern).
        assert_eq!(out["ab"].group, "curve");
        // arena stays as MergeEntry.arena (user-controlled).
        assert_eq!(out["ab"].arena, "ab");
    }

    #[test]
    fn merge_target_in_enum_missing_enum_of_errors() {
        let entities = merge_seed(&["a", "b"]);
        let merges = merge_with_override("ab", vec!["a", "b"], Some("in_enum"), None);
        let err = apply_splits_merges(&entities, &SplitsFile::default(), &merges).unwrap_err();
        assert!(err.contains("merges.toml"));
        assert!(err.contains("merge.m"));
        assert!(err.contains("requires enum_of"));
    }

    #[test]
    fn merge_target_orphan_enum_of_errors() {
        let entities = merge_seed(&["a", "b"]);
        let merges = merge_with_override("ab", vec!["a", "b"], None, Some("curve"));
        let err = apply_splits_merges(&entities, &SplitsFile::default(), &merges).unwrap_err();
        assert!(err.contains("merges.toml"));
        assert!(err.contains("merge.m"));
        assert!(err.contains("enum_of set without kind"));
    }

    #[test]
    fn merge_target_single_struct_with_enum_of_errors() {
        let entities = merge_seed(&["a", "b"]);
        let merges = merge_with_override(
            "ab",
            vec!["a", "b"],
            Some("single_struct"),
            Some("curve"),
        );
        let err = apply_splits_merges(&entities, &SplitsFile::default(), &merges).unwrap_err();
        assert!(err.contains("merges.toml"));
        assert!(err.contains("merge.m"));
        assert!(err.contains("incompatible"));
    }

    #[test]
    fn merge_target_unsupported_kind_errors() {
        let entities = merge_seed(&["a", "b"]);
        let merges = merge_with_override("ab", vec!["a", "b"], Some("weird_kind"), None);
        let err = apply_splits_merges(&entities, &SplitsFile::default(), &merges).unwrap_err();
        assert!(err.contains("merges.toml"));
        assert!(err.contains("merge.m"));
        assert!(err.contains("unsupported kind override 'weird_kind'"));
    }
}
