//! Stage 2 — arena classification.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::express::Schema;
use crate::infer::overrides::{self, OverrideFile};
use crate::infer::variant::VariantSpec;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaSpec {
    pub arena: String,
}

const FILE_CONFIDENT: &str = "arenas.toml";
const FILE_OVERRIDES: &str = "arenas_overrides.toml";
const SECTION: &str = "group";

const VARIANT_CONFIDENT: &str = "variants.toml";
const VARIANT_PENDING: &str = "variants_pending.toml";

pub fn run(_schemas: &[Schema], allow_pending: bool) -> Result<(), String> {
    if !allow_pending && crate::infer::io::pending_exists(VARIANT_PENDING) {
        return Err(format!(
            "{VARIANT_PENDING} exists — variant stage has unresolved/review items.\n\
             Resolve in variants_overrides.toml or pass --allow-pending to proceed with partial input."
        ));
    }

    let variants: BTreeMap<String, VariantSpec> =
        crate::infer::io::read_confident(VARIANT_CONFIDENT, "entity")
            .map_err(|e| format!("read {VARIANT_CONFIDENT}: {e}"))?;
    if variants.is_empty() {
        return Err(format!(
            "{VARIANT_CONFIDENT} is empty or missing — run `infer variant` first."
        ));
    }

    let groups = compute_groups(&variants);

    let overrides_file: OverrideFile<ArenaSpec> =
        overrides::load(FILE_OVERRIDES).map_err(|e| format!("load overrides: {e}"))?;

    let known: BTreeSet<String> = groups.keys().cloned().collect();
    let errs = overrides::validate_known(&overrides_file, SECTION, &known, FILE_OVERRIDES);
    if !errs.is_empty() {
        return Err(errs.join("\n"));
    }

    // 1 group = 1 arena default; explicit overrides replace the default.
    let mut arenas: BTreeMap<String, ArenaSpec> = groups
        .keys()
        .map(|g| (g.clone(), ArenaSpec { arena: g.clone() }))
        .collect();
    for (g, spec) in &overrides_file.group {
        arenas.insert(g.clone(), spec.clone());
    }

    crate::infer::io::write_confident(FILE_CONFIDENT, SECTION, &arenas)
        .map_err(|e| format!("write {FILE_CONFIDENT}: {e}"))?;

    eprintln!(
        "infer arena: {} groups (overridden={})",
        arenas.len(),
        overrides_file.group.len(),
    );
    Ok(())
}

type Groups = BTreeMap<String, GroupInfo>;

/// Recompute the arena classification from a pruned variants map. Used by
/// the prune stage so it does not need to depend on the private group /
/// auto-decision helpers below — only this entry point and `ArenaSpec`
/// cross the module boundary.
///
/// The caller is responsible for filtering overrides whose target groups
/// disappeared during pruning; `validate_known` runs over `overrides` as
/// a final safety check and any stale entry produces an error.
pub fn recompute_for_pruned(
    variants: &BTreeMap<String, VariantSpec>,
    overrides: &OverrideFile<ArenaSpec>,
) -> Result<BTreeMap<String, ArenaSpec>, String> {
    let groups = compute_groups(variants);
    let known: BTreeSet<String> = groups.keys().cloned().collect();
    let errs = overrides::validate_known(overrides, SECTION, &known, FILE_OVERRIDES);
    if !errs.is_empty() {
        return Err(errs.join("\n"));
    }
    let mut out: BTreeMap<String, ArenaSpec> = groups
        .keys()
        .map(|g| (g.clone(), ArenaSpec { arena: g.clone() }))
        .collect();
    for (g, spec) in &overrides.group {
        out.insert(g.clone(), spec.clone());
    }
    Ok(out)
}

/// Build an entity → group reverse index. Used by the shape stage to
/// compile the unified `entities.toml` view: every entity (including
/// those `compute_groups` skips because they have no IR struct of their
/// own — `NestedField` and `MergedInto`) needs a group / arena to point
/// at.
///
/// `NestedField` inherits its parent's group (`into`); `MergedInto`
/// inherits its target's group, transitively if the target is itself
/// merged. The fixpoint loop handles arbitrary chains regardless of
/// iteration order.
pub(crate) fn compute_entity_to_group(
    variants: &BTreeMap<String, VariantSpec>,
) -> BTreeMap<String, String> {
    let groups = compute_groups(variants);
    let mut out: BTreeMap<String, String> = BTreeMap::new();

    // 1. Members emitted by compute_groups (SingleStruct / InEnum members
    //    / ConcreteSupertype / Complex / Composite). EnumBase only seeds
    //    the group key — its own entity is mapped separately below.
    for (group_name, info) in &groups {
        for member in &info.members {
            out.insert(member.clone(), group_name.clone());
        }
    }

    // 2. EnumBase: not a member of its group, but it is itself an entity
    //    that needs a group mapping. The enum_name is the group key.
    //    ComplexSupertype / CompositeOneOf seed their own group with
    //    `or_insert_with` so children that happen to alphabetize before
    //    them can leave the parent's self entry off the members list —
    //    map the parent explicitly here.
    for (entity, spec) in variants {
        match spec {
            VariantSpec::EnumBase { enum_name } => {
                out.entry(entity.clone())
                    .or_insert_with(|| enum_name.clone());
            }
            VariantSpec::ComplexSupertype { .. } | VariantSpec::CompositeOneOf { .. } => {
                out.entry(entity.clone()).or_insert_with(|| entity.clone());
            }
            _ => {}
        }
    }

    // 3. NestedField → parent's group; MergedInto → target's group. Both
    //    can chain (a NestedField parent might itself be merged), so a
    //    fixpoint loop handles arbitrary chains. The bound is the worst-
    //    case chain length (variants.len() + 1) — any cycle would prevent
    //    progress and be cut off here, leaving the offending entity
    //    unmapped (compile_entities surfaces that as an error).
    for _ in 0..variants.len() + 1 {
        let mut changed = false;
        for (entity, spec) in variants {
            if out.contains_key(entity) {
                continue;
            }
            let parent = match spec {
                VariantSpec::NestedField { into, .. } => Some(into),
                VariantSpec::MergedInto { target, .. } => Some(target),
                _ => None,
            };
            if let Some(p) = parent
                && let Some(g) = out.get(p).cloned()
            {
                out.insert(entity.clone(), g);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    out
}

#[derive(Debug, Clone)]
struct GroupInfo {
    members: Vec<String>,
    is_enum: bool,
}

fn compute_groups(variants: &BTreeMap<String, VariantSpec>) -> Groups {
    let mut groups: Groups = BTreeMap::new();
    for (entity, spec) in variants {
        match spec {
            VariantSpec::SingleStruct => {
                groups.entry(entity.clone()).or_insert_with(|| GroupInfo {
                    members: vec![entity.clone()],
                    is_enum: false,
                });
            }
            VariantSpec::InEnum { enum_name } => {
                let entry = groups
                    .entry(enum_name.clone())
                    .or_insert_with(|| GroupInfo {
                        members: Vec::new(),
                        is_enum: true,
                    });
                entry.members.push(entity.clone());
            }
            VariantSpec::EnumBase { enum_name } => {
                // Establishes the enum group key but doesn't appear as a
                // member (the base entity has no IR struct of its own).
                groups
                    .entry(enum_name.clone())
                    .or_insert_with(|| GroupInfo {
                        members: Vec::new(),
                        is_enum: true,
                    });
            }
            VariantSpec::ComplexSupertype { .. } | VariantSpec::CompositeOneOf { .. } => {
                // Complex / composite supertype carries its own struct +
                // nested enum + mixin (or composite alternatives) in IR;
                // treated as its own non-enum group here.
                groups.entry(entity.clone()).or_insert_with(|| GroupInfo {
                    members: vec![entity.clone()],
                    is_enum: false,
                });
            }
            VariantSpec::ConcreteSupertype => {
                // Implicit supertype: the entity is both a concrete struct
                // and the enum root for its children. Register as an enum
                // group named after itself, and include itself in members.
                // Children carry InEnum { enum_name: <this entity> } and
                // join the same group automatically.
                let entry = groups.entry(entity.clone()).or_insert_with(|| GroupInfo {
                    members: Vec::new(),
                    is_enum: true,
                });
                entry.members.push(entity.clone());
            }
            VariantSpec::NestedField { .. } | VariantSpec::MergedInto { .. } => {
                // No IR struct → not a group of its own.
            }
        }
    }
    for g in groups.values_mut() {
        g.members.sort();
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_struct_becomes_own_group() {
        let mut variants = BTreeMap::new();
        variants.insert("foo".into(), VariantSpec::SingleStruct);
        let groups = compute_groups(&variants);
        assert_eq!(groups.len(), 1);
        assert!(!groups["foo"].is_enum);
    }

    #[test]
    fn enum_members_collected_into_one_group() {
        let mut variants = BTreeMap::new();
        for v in ["plane", "cylinder", "sphere"] {
            variants.insert(
                v.into(),
                VariantSpec::InEnum {
                    enum_name: "surface".into(),
                },
            );
        }
        let groups = compute_groups(&variants);
        assert_eq!(groups.len(), 1);
        assert!(groups["surface"].is_enum);
        assert_eq!(
            groups["surface"].members,
            vec![
                "cylinder".to_string(),
                "plane".to_string(),
                "sphere".to_string()
            ]
        );
    }

    #[test]
    fn nested_field_does_not_create_group() {
        let mut variants = BTreeMap::new();
        variants.insert(
            "rational_b_spline".into(),
            VariantSpec::NestedField {
                into: "b_spline".into(),
                as_field: "weights".into(),
                added_attr_count: 1,
            },
        );
        variants.insert("b_spline".into(), VariantSpec::SingleStruct);
        let groups = compute_groups(&variants);
        assert_eq!(groups.len(), 1);
        assert!(groups.contains_key("b_spline"));
        assert!(!groups.contains_key("rational_b_spline"));
    }
}
