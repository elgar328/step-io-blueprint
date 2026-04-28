//! Stage 2 — arena classification.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::express::Schema;
use crate::infer::io::{PendingFile, PendingStats};
use crate::infer::overrides::{self, OverrideFile};
use crate::infer::variant::VariantSpec;
use crate::infer::{Bucket, Confidence, Decision, DecisionSource, InferResult, Unresolved};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaSpec {
    pub arena: String,
}

const FILE_CONFIDENT: &str = "arenas.toml";
const FILE_PENDING: &str = "arenas_pending.toml";
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

    let variants: BTreeMap<String, Decision<VariantSpec>> =
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
    let mut errs = overrides::validate_known(&overrides_file, SECTION, &known, FILE_OVERRIDES);
    errs.extend(overrides::validate_no_conflict(
        &overrides_file,
        SECTION,
        FILE_OVERRIDES,
    ));
    if !errs.is_empty() {
        return Err(errs.join("\n"));
    }

    let auto = compute_auto_decisions(&groups);
    let result = merge_overrides(auto, &overrides_file)?;

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
        "infer arena: confident={} review={} unresolved={} (total={})",
        pending.stats.confident,
        pending.stats.review,
        pending.stats.unresolved,
        pending.stats.total,
    );
    Ok(())
}

type Groups = BTreeMap<String, GroupInfo>;

#[derive(Debug, Clone)]
struct GroupInfo {
    members: Vec<String>,
    is_enum: bool,
}

fn compute_groups(variants: &BTreeMap<String, Decision<VariantSpec>>) -> Groups {
    let mut groups: Groups = BTreeMap::new();
    for (entity, dec) in variants {
        match &dec.data {
            VariantSpec::SingleStruct => {
                groups
                    .entry(entity.clone())
                    .or_insert_with(|| GroupInfo {
                        members: vec![entity.clone()],
                        is_enum: false,
                    });
            }
            VariantSpec::InEnum { enum_name } => {
                let entry = groups.entry(enum_name.clone()).or_insert_with(|| GroupInfo {
                    members: Vec::new(),
                    is_enum: true,
                });
                entry.members.push(entity.clone());
            }
            VariantSpec::NestedField { .. } => {
                // absorbed into parent's group; not a group of its own
            }
        }
    }
    for g in groups.values_mut() {
        g.members.sort();
    }
    groups
}

struct AutoDecisions {
    groups: BTreeMap<String, AutoEntry>,
}

enum AutoEntry {
    Decided(Decision<ArenaSpec>),
    #[allow(dead_code)]
    Unresolved(Unresolved),
}

fn compute_auto_decisions(groups: &Groups) -> AutoDecisions {
    let mut out: BTreeMap<String, AutoEntry> = BTreeMap::new();
    for (name, info) in groups {
        let conf = if info.is_enum {
            Confidence::new(0.9)
        } else {
            Confidence::new(0.95)
        };
        out.insert(
            name.clone(),
            AutoEntry::Decided(Decision {
                data: ArenaSpec {
                    arena: name.clone(),
                },
                source: DecisionSource::Auto,
                confidence: conf,
                reasons: vec![format!(
                    "default 1 group = 1 arena ({} member(s))",
                    info.members.len()
                )],
            }),
        );
    }
    AutoDecisions { groups: out }
}

fn merge_overrides(
    auto: AutoDecisions,
    overrides_file: &OverrideFile<ArenaSpec>,
) -> Result<InferResult<ArenaSpec>, String> {
    let mut confident = BTreeMap::new();
    let mut review = BTreeMap::new();
    let mut unresolved = BTreeMap::new();
    let mut errors = Vec::new();

    let accept_set: BTreeSet<&String> = overrides_file.batch_accept.entries.iter().collect();

    for (key, entry) in auto.groups {
        if let Some(override_spec) = overrides_file.group.get(&key) {
            let prior_conf = match &entry {
                AutoEntry::Decided(d) => d.confidence,
                AutoEntry::Unresolved(_) => Confidence::new(1.0),
            };
            confident.insert(
                key,
                Decision {
                    data: override_spec.clone(),
                    source: DecisionSource::Override,
                    confidence: prior_conf,
                    reasons: Vec::new(),
                },
            );
            continue;
        }

        if accept_set.contains(&key) {
            match entry {
                AutoEntry::Decided(d) => match d.bucket() {
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
                },
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
                    unresolved.insert(
                        key,
                        Unresolved {
                            reasons: d.reasons,
                            override_example: "arena = \"some_arena_name\"".to_string(),
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

    fn variant_dec(spec: VariantSpec) -> Decision<VariantSpec> {
        Decision {
            data: spec,
            source: DecisionSource::Auto,
            confidence: Confidence::new(0.9),
            reasons: Vec::new(),
        }
    }

    #[test]
    fn single_struct_becomes_own_group() {
        let mut variants = BTreeMap::new();
        variants.insert("foo".into(), variant_dec(VariantSpec::SingleStruct));
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
                variant_dec(VariantSpec::InEnum {
                    enum_name: "surface".into(),
                }),
            );
        }
        let groups = compute_groups(&variants);
        assert_eq!(groups.len(), 1);
        assert!(groups["surface"].is_enum);
        assert_eq!(
            groups["surface"].members,
            vec!["cylinder".to_string(), "plane".to_string(), "sphere".to_string()]
        );
    }

    #[test]
    fn nested_field_does_not_create_group() {
        let mut variants = BTreeMap::new();
        variants.insert(
            "rational_b_spline".into(),
            variant_dec(VariantSpec::NestedField {
                into: "b_spline".into(),
                as_field: "weights".into(),
            }),
        );
        variants.insert("b_spline".into(), variant_dec(VariantSpec::SingleStruct));
        let groups = compute_groups(&variants);
        assert_eq!(groups.len(), 1);
        assert!(groups.contains_key("b_spline"));
        assert!(!groups.contains_key("rational_b_spline"));
    }

    #[test]
    fn auto_default_arena_named_after_group() {
        let mut groups: Groups = BTreeMap::new();
        groups.insert(
            "surface".into(),
            GroupInfo {
                members: vec!["plane".into(), "cylinder".into()],
                is_enum: true,
            },
        );
        let auto = compute_auto_decisions(&groups);
        let d = match auto.groups.get("surface").unwrap() {
            AutoEntry::Decided(d) => d,
            _ => panic!("expected decided"),
        };
        assert_eq!(d.data.arena, "surface");
        assert!(d.confidence.0 >= 0.8);
    }
}
