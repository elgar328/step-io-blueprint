//! Reshape stage — apply split / merge / recast abstractions to entities.toml.
//!
//! Reads entities.toml + splits.toml + merges.toml + recasts.toml,
//! validates that split sources / merge absorbs / recast targets exist
//! and have compatible variant kinds, then writes abstract_entities.toml
//! with the abstractions applied. Empty input files leave the output a
//! verbatim copy of entities.toml.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::infer::shape::EntitySummary;
use crate::infer::variant::VariantSpec;

const FILE_ENTITIES: &str = "entities.toml";
const FILE_SPLITS: &str = "splits.toml";
const FILE_MERGES: &str = "merges.toml";
const FILE_RECASTS: &str = "recasts.toml";
const FILE_ANCHORS: &str = "anchors.toml";
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

#[derive(Debug, Default, Deserialize)]
struct RecastsFile {
    #[serde(default)]
    recast: BTreeMap<String, RecastEntry>,
}

#[derive(Debug, Deserialize)]
struct RecastEntry {
    kind: String,
    #[serde(default)]
    enum_of: Option<String>,
    arena: String,
    entities: Vec<String>,
    #[serde(default)]
    reasons: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AnchorsFile {
    #[serde(default)]
    anchor: BTreeMap<String, AnchorEntry>,
}

#[derive(Debug, Deserialize)]
struct AnchorEntry {
    arena: String,
    kind: String,
    reasons: String,
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
    let recasts = load_recasts()?;
    let anchors = load_anchors()?;

    validate_splits(&splits, &entities);
    validate_merges(&merges, &entities);
    validate_recasts(&recasts, &entities);

    let mut abstract_entities = apply_splits_merges(&entities, &splits, &merges)?;
    validate_anchors(&anchors, &abstract_entities)?;
    let inserted_anchors = apply_anchors(&anchors, &mut abstract_entities);

    // Post-apply enum_of cross-ref: splits / merges / anchors are all
    // applied, so the only abstraction still pending is recasts itself.
    validate_recasts_enum_of(&recasts, &abstract_entities)?;
    apply_recasts(&mut abstract_entities, &recasts)?;
    let pruned_enum_bases = prune_empty_enum_bases(&mut abstract_entities)?;
    let collapsed_enum_bases = collapse_single_child_enum_bases(&mut abstract_entities)?;
    write_abstract_entities(&abstract_entities)?;

    eprintln!(
        "infer reshape: wrote {FILE_ABSTRACT_ENTITIES} ({} entities, {} splits, {} merges, {} recasts, {} anchors, {} empty enum_bases pruned, {} single-child enum_bases collapsed)",
        abstract_entities.len(),
        splits.split.len(),
        merges.merge.len(),
        recasts.recast.len(),
        inserted_anchors.len(),
        pruned_enum_bases.len(),
        collapsed_enum_bases.len()
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

fn load_recasts() -> Result<RecastsFile, String> {
    let path = Path::new("inferred").join(FILE_RECASTS);
    if !path.exists() {
        return Ok(RecastsFile::default());
    }
    let body = fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    toml::from_str(&body).map_err(|e| format!("parse {path:?}: {e}"))
}

fn load_anchors() -> Result<AnchorsFile, String> {
    let path = Path::new("inferred").join(FILE_ANCHORS);
    if !path.exists() {
        return Ok(AnchorsFile::default());
    }
    let body = fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    toml::from_str(&body).map_err(|e| format!("parse {path:?}: {e}"))
}

fn validate_anchors(
    anchors: &AnchorsFile,
    entities: &BTreeMap<String, EntitySummary>,
) -> Result<(), String> {
    for (name, entry) in &anchors.anchor {
        if entities.contains_key(name) {
            return Err(format!(
                "{FILE_ANCHORS} [anchor.{name}] collides with existing entity \
                 (schema entity, split product, or merge target)"
            ));
        }
        if entry.kind != "enum_base" {
            return Err(format!(
                "{FILE_ANCHORS} [anchor.{name}] kind={:?} unsupported \
                 (only \"enum_base\")",
                entry.kind
            ));
        }
    }
    Ok(())
}

fn apply_anchors(
    anchors: &AnchorsFile,
    out: &mut BTreeMap<String, EntitySummary>,
) -> Vec<String> {
    let mut inserted = Vec::new();
    for (name, entry) in &anchors.anchor {
        out.insert(
            name.clone(),
            EntitySummary {
                variant: VariantSpec::EnumBase {
                    enum_name: name.clone(),
                },
                group: name.clone(),
                arena: entry.arena.clone(),
                shape: None,
                instance_count: 0,
                complex_part_count: 0,
                co_instantiated_with: Vec::new(),
                split_from: None,
                split_context: None,
                merge_absorbs: Vec::new(),
                fields_union: false,
                reasons: Some(entry.reasons.clone()),
            },
        );
        inserted.push(name.clone());
    }
    inserted
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
    for k in splits.split.keys() {
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

fn validate_recasts(recasts: &RecastsFile, entities: &BTreeMap<String, EntitySummary>) {
    for (k, entry) in &recasts.recast {
        for entity_name in &entry.entities {
            match entities.get(entity_name) {
                None => eprintln!(
                    "warning: {FILE_RECASTS} [recast.{k}] entity {entity_name} not in {FILE_ENTITIES}"
                ),
                Some(s) => match &s.variant {
                    VariantSpec::NestedField { .. } | VariantSpec::MergedInto { .. } => {
                        eprintln!(
                            "warning: {FILE_RECASTS} [recast.{k}] entity {entity_name} is {} (recast unsupported on absorbed)",
                            kind_str(&s.variant)
                        );
                    }
                    _ => {}
                },
            }
        }
    }
}

/// Post-apply check: each recast's `enum_of` target must resolve to an
/// existing EnumBase or ConcreteSupertype in the abstract-entities map
/// (schema entity, split product, or anchor).
fn validate_recasts_enum_of(
    recasts: &RecastsFile,
    abstract_entities: &BTreeMap<String, EntitySummary>,
) -> Result<(), String> {
    for (label, entry) in &recasts.recast {
        if entry.kind != "in_enum" {
            continue;
        }
        let target = entry.enum_of.as_ref().ok_or_else(|| {
            format!("{FILE_RECASTS} [recast.{label}] kind=in_enum requires enum_of")
        })?;
        let target_entity = abstract_entities.get(target).ok_or_else(|| {
            format!(
                "{FILE_RECASTS} [recast.{label}] enum_of={target:?} not in entities \
                 (schema entity, split product, or anchor). \
                 Declare it in {FILE_ANCHORS} if intentional."
            )
        })?;
        match &target_entity.variant {
            VariantSpec::EnumBase { .. } | VariantSpec::ConcreteSupertype => {}
            other => {
                return Err(format!(
                    "{FILE_RECASTS} [recast.{label}] enum_of={target:?} resolves to {}, \
                     not enum_base or concrete_supertype",
                    kind_str(other)
                ));
            }
        }
    }
    Ok(())
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
            if let Some(s) = out.remove(absorb)
                && base.is_none() {
                    base = Some(s);
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

fn apply_recasts(
    out: &mut BTreeMap<String, EntitySummary>,
    recasts: &RecastsFile,
) -> Result<(), String> {
    for (key, entry) in &recasts.recast {
        let target_variant = variant_spec_from_override(
            &VariantSpec::SingleStruct,
            Some(&entry.kind),
            entry.enum_of.as_deref(),
            FILE_RECASTS,
            &format!("recast.{key}"),
        )?;
        for entity_name in &entry.entities {
            let Some(s) = out.get_mut(entity_name) else {
                eprintln!(
                    "warning: {FILE_RECASTS} [recast.{key}] entity {entity_name} not in abstract_entities (already split / merged?)"
                );
                continue;
            };
            s.variant = target_variant.clone();
            s.arena = entry.arena.clone();
            s.group = match &target_variant {
                VariantSpec::InEnum { enum_name } => enum_name.clone(),
                _ => entry.arena.clone(),
            };
            s.reasons = entry.reasons.clone();
        }
    }
    Ok(())
}

/// Drop enum_bases that lost all in_enum children to splits / merges /
/// recasts. Mirrors prune.rs Rule 2's shrink-driven removal, applied
/// at the abstract-entities stage instead of the prune stage.
///
/// Errs if any remaining entity references the empty enum_base via
/// NestedField.into / MergedInto.target — that combination is
/// unnatural (a referenced enum_base should carry variants) and likely
/// a classification bug. User must fix before pipeline continues.
fn prune_empty_enum_bases(
    out: &mut BTreeMap<String, EntitySummary>,
) -> Result<Vec<String>, String> {
    let mut to_remove: Vec<String> = Vec::new();
    for (name, summary) in out.iter() {
        if !matches!(summary.variant, VariantSpec::EnumBase { .. }) {
            continue;
        }
        let has_live_child = out.values().any(|s| {
            matches!(&s.variant, VariantSpec::InEnum { enum_name }
                if enum_name == name)
        });
        if has_live_child {
            continue;
        }
        let dangling: Vec<String> = out
            .iter()
            .filter_map(|(n, s)| match &s.variant {
                VariantSpec::NestedField { into, .. } if into == name => Some(n.clone()),
                VariantSpec::MergedInto { target, .. } if target == name => Some(n.clone()),
                _ => None,
            })
            .collect();
        if !dangling.is_empty() {
            return Err(format!(
                "empty enum_base {name} has dangling NestedField/MergedInto \
                 references: {dangling:?}. Fix the classification before continuing."
            ));
        }
        to_remove.push(name.clone());
    }
    for name in &to_remove {
        out.remove(name);
    }
    Ok(to_remove)
}

/// Collapse enum_bases that have exactly one live in_enum child:
/// promote the lone child to SingleStruct and drop the enum_base.
/// Mirrors prune.rs Rule 2's nc==1 path, applied at the abstract-
/// entities stage.
///
/// Errs on dangling NestedField/MergedInto references — same policy
/// as prune_empty_enum_bases.
fn collapse_single_child_enum_bases(
    out: &mut BTreeMap<String, EntitySummary>,
) -> Result<Vec<String>, String> {
    let mut to_collapse: Vec<(String, String)> = Vec::new();
    for (name, summary) in out.iter() {
        if !matches!(summary.variant, VariantSpec::EnumBase { .. }) {
            continue;
        }
        let live_children: Vec<String> = out
            .iter()
            .filter_map(|(n, s)| match &s.variant {
                VariantSpec::InEnum { enum_name } if enum_name == name => Some(n.clone()),
                _ => None,
            })
            .collect();
        if live_children.len() != 1 {
            continue;
        }
        let dangling: Vec<String> = out
            .iter()
            .filter_map(|(n, s)| match &s.variant {
                VariantSpec::NestedField { into, .. } if into == name => Some(n.clone()),
                VariantSpec::MergedInto { target, .. } if target == name => Some(n.clone()),
                _ => None,
            })
            .collect();
        if !dangling.is_empty() {
            return Err(format!(
                "single-child enum_base {name} has dangling NestedField/MergedInto \
                 references: {dangling:?}. Fix the classification before continuing."
            ));
        }
        to_collapse.push((name.clone(), live_children.into_iter().next().unwrap()));
    }
    let removed: Vec<String> = to_collapse.iter().map(|(eb, _)| eb.clone()).collect();
    for (enum_base, child) in to_collapse {
        if let Some(child_entity) = out.get_mut(&child) {
            child_entity.variant = VariantSpec::SingleStruct;
        }
        out.remove(&enum_base);
    }
    Ok(removed)
}

fn write_abstract_entities(
    entities: &BTreeMap<String, EntitySummary>,
) -> Result<(), String> {
    let mut outer: BTreeMap<&str, &BTreeMap<String, EntitySummary>> = BTreeMap::new();
    outer.insert("entity", entities);
    let body = toml::to_string_pretty(&outer)
        .map_err(|e| format!("serialize {FILE_ABSTRACT_ENTITIES}: {e}"))?;
    let header = "# Generated by `infer reshape`. Do not edit manually.\n\
                  # Inputs: entities.toml + splits.toml + merges.toml + recasts.toml + anchors.toml + schemas/*.exp\n\n";
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
            complex_part_count: 0,
            co_instantiated_with: Vec::new(),
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
        for v in out.values() {
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

    fn recast_with(
        key: &str,
        kind: &str,
        enum_of: Option<&str>,
        arena: &str,
        entities: Vec<&str>,
        reasons: Option<&str>,
    ) -> RecastsFile {
        RecastsFile {
            recast: BTreeMap::from([(
                key.to_string(),
                RecastEntry {
                    kind: kind.to_string(),
                    enum_of: enum_of.map(str::to_string),
                    arena: arena.to_string(),
                    entities: entities.into_iter().map(String::from).collect(),
                    reasons: reasons.map(str::to_string),
                },
            )]),
        }
    }

    #[test]
    fn empty_recasts_leaves_entities_unchanged() {
        let mut out = BTreeMap::new();
        out.insert("line".into(), summary(VariantSpec::SingleStruct, "line"));
        let before = out.clone();
        apply_recasts(&mut out, &RecastsFile::default()).unwrap();
        assert_eq!(out, before);
    }

    #[test]
    fn recast_to_in_enum_updates_variant_group_arena_reasons() {
        let mut out = BTreeMap::new();
        out.insert("line".into(), summary(VariantSpec::SingleStruct, "line"));
        out.insert("circle".into(), summary(VariantSpec::SingleStruct, "circle"));
        let recasts = recast_with(
            "curve_unification",
            "in_enum",
            Some("curve"),
            "curve",
            vec!["line", "circle"],
            Some("Unify under Curve enum"),
        );
        apply_recasts(&mut out, &recasts).unwrap();
        for name in ["line", "circle"] {
            match &out[name].variant {
                VariantSpec::InEnum { enum_name } => assert_eq!(enum_name, "curve"),
                other => panic!("expected InEnum for {name}, got {other:?}"),
            }
            assert_eq!(out[name].arena, "curve");
            assert_eq!(out[name].group, "curve");
            assert_eq!(out[name].reasons.as_deref(), Some("Unify under Curve enum"));
        }
    }

    #[test]
    fn recast_to_single_struct_updates_variant_and_group_to_arena() {
        let mut out = BTreeMap::new();
        out.insert(
            "x".into(),
            summary(
                VariantSpec::InEnum {
                    enum_name: "old_enum".into(),
                },
                "x",
            ),
        );
        let recasts = recast_with(
            "promote",
            "single_struct",
            None,
            "standalone",
            vec!["x"],
            None,
        );
        apply_recasts(&mut out, &recasts).unwrap();
        assert!(matches!(out["x"].variant, VariantSpec::SingleStruct));
        assert_eq!(out["x"].arena, "standalone");
        // group falls back to arena when variant is not InEnum.
        assert_eq!(out["x"].group, "standalone");
    }

    #[test]
    fn recast_unknown_entity_warns_but_others_apply() {
        let mut out = BTreeMap::new();
        out.insert("line".into(), summary(VariantSpec::SingleStruct, "line"));
        let recasts = recast_with(
            "curve_unification",
            "in_enum",
            Some("curve"),
            "curve",
            vec!["line", "ghost_entity"],
            None,
        );
        apply_recasts(&mut out, &recasts).unwrap();
        // Known entity reclassified, unknown was warned and skipped.
        match &out["line"].variant {
            VariantSpec::InEnum { enum_name } => assert_eq!(enum_name, "curve"),
            other => panic!("expected InEnum, got {other:?}"),
        }
        assert!(!out.contains_key("ghost_entity"));
    }

    #[test]
    fn recast_invalid_kind_errors_with_file_label() {
        let mut out = BTreeMap::new();
        out.insert("line".into(), summary(VariantSpec::SingleStruct, "line"));
        let recasts = recast_with(
            "bad",
            "weird_kind",
            None,
            "curve",
            vec!["line"],
            None,
        );
        let err = apply_recasts(&mut out, &recasts).unwrap_err();
        assert!(err.contains("recasts.toml"));
        assert!(err.contains("recast.bad"));
        assert!(err.contains("unsupported kind override 'weird_kind'"));
    }

    #[test]
    fn prune_empty_enum_bases_removes_zero_child() {
        let mut out = BTreeMap::new();
        out.insert(
            "bounded_curve".into(),
            summary(
                VariantSpec::EnumBase {
                    enum_name: "bounded_curve".into(),
                },
                "bounded_curve",
            ),
        );
        let removed = prune_empty_enum_bases(&mut out).unwrap();
        assert_eq!(removed, vec!["bounded_curve".to_string()]);
        assert!(!out.contains_key("bounded_curve"));
    }

    #[test]
    fn prune_empty_enum_bases_keeps_one_child() {
        let mut out = BTreeMap::new();
        out.insert(
            "elementary_surface".into(),
            summary(
                VariantSpec::EnumBase {
                    enum_name: "elementary_surface".into(),
                },
                "elementary_surface",
            ),
        );
        out.insert(
            "degenerate_toroidal_surface".into(),
            summary(
                VariantSpec::InEnum {
                    enum_name: "elementary_surface".into(),
                },
                "elementary_surface",
            ),
        );
        let removed = prune_empty_enum_bases(&mut out).unwrap();
        assert!(removed.is_empty());
        assert!(out.contains_key("elementary_surface"));
        assert!(out.contains_key("degenerate_toroidal_surface"));
    }

    #[test]
    fn prune_empty_enum_bases_after_recast() {
        let mut entities = BTreeMap::new();
        entities.insert(
            "conic".into(),
            summary(
                VariantSpec::EnumBase {
                    enum_name: "conic".into(),
                },
                "conic",
            ),
        );
        entities.insert(
            "circle".into(),
            summary(
                VariantSpec::InEnum {
                    enum_name: "conic".into(),
                },
                "conic",
            ),
        );
        let recasts = recast_with(
            "curve_unification",
            "in_enum",
            Some("curve"),
            "curve",
            vec!["circle"],
            None,
        );
        let mut out = entities.clone();
        apply_recasts(&mut out, &recasts).unwrap();
        // After recast: conic's only child (circle) is now in_enum curve.
        let removed = prune_empty_enum_bases(&mut out).unwrap();
        assert_eq!(removed, vec!["conic".to_string()]);
        assert!(!out.contains_key("conic"));
        match &out["circle"].variant {
            VariantSpec::InEnum { enum_name } => assert_eq!(enum_name, "curve"),
            other => panic!("expected InEnum curve, got {other:?}"),
        }
    }

    #[test]
    fn prune_empty_enum_bases_errs_on_dangling_refs() {
        let mut out = BTreeMap::new();
        out.insert(
            "bounded_curve".into(),
            summary(
                VariantSpec::EnumBase {
                    enum_name: "bounded_curve".into(),
                },
                "bounded_curve",
            ),
        );
        // Y has NestedField pointing at bounded_curve — Err expected.
        out.insert(
            "y".into(),
            summary(
                VariantSpec::NestedField {
                    into: "bounded_curve".into(),
                    as_field: "bc".into(),
                    added_attr_count: 0,
                },
                "y",
            ),
        );
        let err = prune_empty_enum_bases(&mut out).unwrap_err();
        assert!(err.contains("dangling"));
        assert!(err.contains("bounded_curve"));
        assert!(out.contains_key("bounded_curve"));
        assert!(out.contains_key("y"));
    }

    #[test]
    fn collapse_single_child_enum_bases_promotes_lone_child() {
        let mut out = BTreeMap::new();
        out.insert(
            "elementary_surface".into(),
            summary(
                VariantSpec::EnumBase {
                    enum_name: "elementary_surface".into(),
                },
                "elementary_surface",
            ),
        );
        out.insert(
            "degenerate_toroidal_surface".into(),
            summary(
                VariantSpec::InEnum {
                    enum_name: "elementary_surface".into(),
                },
                "elementary_surface",
            ),
        );
        let removed = collapse_single_child_enum_bases(&mut out).unwrap();
        assert_eq!(removed, vec!["elementary_surface".to_string()]);
        assert!(!out.contains_key("elementary_surface"));
        assert!(matches!(
            out["degenerate_toroidal_surface"].variant,
            VariantSpec::SingleStruct
        ));
    }

    #[test]
    fn collapse_single_child_enum_bases_keeps_two_children() {
        let mut out = BTreeMap::new();
        out.insert(
            "conic".into(),
            summary(
                VariantSpec::EnumBase {
                    enum_name: "conic".into(),
                },
                "conic",
            ),
        );
        for child in ["circle", "ellipse"] {
            out.insert(
                child.into(),
                summary(
                    VariantSpec::InEnum {
                        enum_name: "conic".into(),
                    },
                    "conic",
                ),
            );
        }
        let removed = collapse_single_child_enum_bases(&mut out).unwrap();
        assert!(removed.is_empty());
        assert!(out.contains_key("conic"));
        assert!(matches!(
            out["circle"].variant,
            VariantSpec::InEnum { .. }
        ));
    }

    #[test]
    fn collapse_single_child_enum_bases_errs_on_dangling_refs() {
        let mut out = BTreeMap::new();
        out.insert(
            "elementary_surface".into(),
            summary(
                VariantSpec::EnumBase {
                    enum_name: "elementary_surface".into(),
                },
                "elementary_surface",
            ),
        );
        out.insert(
            "degenerate_toroidal_surface".into(),
            summary(
                VariantSpec::InEnum {
                    enum_name: "elementary_surface".into(),
                },
                "elementary_surface",
            ),
        );
        // Y references elementary_surface via MergedInto — Err expected.
        out.insert(
            "y".into(),
            summary(
                VariantSpec::MergedInto {
                    target: "elementary_surface".into(),
                    chain: vec![],
                },
                "y",
            ),
        );
        let err = collapse_single_child_enum_bases(&mut out).unwrap_err();
        assert!(err.contains("dangling"));
        assert!(err.contains("elementary_surface"));
        assert!(out.contains_key("elementary_surface"));
        assert!(matches!(
            out["degenerate_toroidal_surface"].variant,
            VariantSpec::InEnum { .. }
        ));
    }

    fn anchor_entry(arena: &str, kind: &str, reasons: &str) -> AnchorEntry {
        AnchorEntry {
            arena: arena.to_string(),
            kind: kind.to_string(),
            reasons: reasons.to_string(),
        }
    }

    #[test]
    fn validate_anchors_rejects_collision_with_existing_entity() {
        let mut entities = BTreeMap::new();
        entities.insert(
            "curve".into(),
            summary(VariantSpec::ConcreteSupertype, "curve"),
        );
        let anchors = AnchorsFile {
            anchor: BTreeMap::from([(
                "curve".to_string(),
                anchor_entry("curve", "enum_base", "should collide"),
            )]),
        };
        let err = validate_anchors(&anchors, &entities).unwrap_err();
        assert!(err.contains("collides"));
        assert!(err.contains("curve"));
    }

    #[test]
    fn validate_anchors_rejects_non_enum_base_kind() {
        let entities = BTreeMap::new();
        let anchors = AnchorsFile {
            anchor: BTreeMap::from([(
                "foo".to_string(),
                anchor_entry("curve", "single_struct", "wrong kind"),
            )]),
        };
        let err = validate_anchors(&anchors, &entities).unwrap_err();
        assert!(err.contains("unsupported"));
        assert!(err.contains("single_struct"));
    }

    #[test]
    fn apply_anchors_inserts_enum_base_with_all_fields() {
        let mut out: BTreeMap<String, EntitySummary> = BTreeMap::new();
        let anchors = AnchorsFile {
            anchor: BTreeMap::from([(
                "surface_trace_curve".to_string(),
                anchor_entry("curve", "enum_base", "reason text"),
            )]),
        };
        let inserted = apply_anchors(&anchors, &mut out);
        assert_eq!(inserted, vec!["surface_trace_curve".to_string()]);
        let entity = out.get("surface_trace_curve").expect("anchor inserted");
        match &entity.variant {
            VariantSpec::EnumBase { enum_name } => {
                assert_eq!(enum_name, "surface_trace_curve");
            }
            other => panic!("expected EnumBase, got {other:?}"),
        }
        assert_eq!(entity.group, "surface_trace_curve");
        assert_eq!(entity.arena, "curve");
        assert_eq!(entity.shape, None);
        assert_eq!(entity.instance_count, 0);
        assert_eq!(entity.split_from, None);
        assert_eq!(entity.split_context, None);
        assert!(entity.merge_absorbs.is_empty());
        assert!(!entity.fields_union);
        assert_eq!(entity.reasons.as_deref(), Some("reason text"));
    }

    #[test]
    fn validate_recasts_enum_of_errs_on_missing_target() {
        let abstract_entities: BTreeMap<String, EntitySummary> = BTreeMap::new();
        let recasts = RecastsFile {
            recast: BTreeMap::from([(
                "ghost_unification".to_string(),
                RecastEntry {
                    kind: "in_enum".to_string(),
                    enum_of: Some("ghost".to_string()),
                    arena: "curve".to_string(),
                    entities: vec![],
                    reasons: None,
                },
            )]),
        };
        let err = validate_recasts_enum_of(&recasts, &abstract_entities).unwrap_err();
        assert!(err.contains("ghost"));
        assert!(err.contains("anchors.toml"));
    }

    #[test]
    fn validate_recasts_enum_of_accepts_anchor_target() {
        let mut abstract_entities: BTreeMap<String, EntitySummary> = BTreeMap::new();
        let anchors = AnchorsFile {
            anchor: BTreeMap::from([(
                "surface_trace_curve".to_string(),
                anchor_entry("curve", "enum_base", "anchor reason"),
            )]),
        };
        apply_anchors(&anchors, &mut abstract_entities);
        let recasts = RecastsFile {
            recast: BTreeMap::from([(
                "surface_trace_unification".to_string(),
                RecastEntry {
                    kind: "in_enum".to_string(),
                    enum_of: Some("surface_trace_curve".to_string()),
                    arena: "curve".to_string(),
                    entities: vec![],
                    reasons: None,
                },
            )]),
        };
        validate_recasts_enum_of(&recasts, &abstract_entities).unwrap();
    }
}
