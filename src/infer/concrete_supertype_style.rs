//! ConcreteSupertype IR style decision (manual input + validation).
//!
//! Pure validation: compares the ConcreteSupertype set in
//! `variants_pruned.toml` against the entries in
//! `concrete_supertype_styles.toml`. Missing required entries → Err
//! stops the run; extra entries → warning, ignored. No output file —
//! the input file itself is the step-io codegen input.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::infer::variant::VariantSpec;

const VARIANTS_PENDING: &str = "variants_pending.toml";
const ARENAS_PENDING: &str = "arenas_pending.toml";
const FILE_VARIANTS_PRUNED: &str = "variants_pruned.toml";
const FILE_CS_STYLES: &str = "concrete_supertype_styles.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "style", rename_all = "snake_case")]
pub enum ConcreteSupertypeStyle {
    /// `enum E { Itself(EData), ChildA(...), ... }` — parent and children
    /// are equal-rank variants.
    Carrier,
    /// `struct E { /* parent attrs */ } enum EKind { ... }` — parent
    /// struct is primary, kind enum is the auxiliary axis.
    BaseParallel,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConcreteSupertypeStylesFile {
    #[serde(default)]
    entity: BTreeMap<String, ConcreteSupertypeStyle>,
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

    let path = Path::new("inferred").join(FILE_CS_STYLES);
    if !path.exists() {
        return Err(missing_file_message(&required));
    }
    let body = fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    let file: ConcreteSupertypeStylesFile =
        toml::from_str(&body).map_err(|e| format!("parse {path:?}: {e}"))?;

    match validate(&required, &file.entity) {
        Validation::Ok { carrier, base_parallel, extras } => {
            for e in &extras {
                eprintln!(
                    "warning: {FILE_CS_STYLES} [entity.{e}] is not a ConcreteSupertype \
                     in {FILE_VARIANTS_PRUNED} — ignored"
                );
            }
            eprintln!(
                "infer concrete_supertype_style: {} ConcreteSupertype entities (carrier={carrier} base_parallel={base_parallel})",
                required.len()
            );
            Ok(())
        }
        Validation::Missing(missing) => Err(missing_entries_message(&missing)),
    }
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
    provided: &BTreeMap<String, ConcreteSupertypeStyle>,
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
            ConcreteSupertypeStyle::Carrier => carrier += 1,
            ConcreteSupertypeStyle::BaseParallel => base_parallel += 1,
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
        "{FILE_CS_STYLES} not found — required ConcreteSupertype entities ({}):\n  {list}\n\
         Add `[entity.<name>] style = \"carrier\" | \"base_parallel\"` for each.",
        required.len()
    )
}

fn missing_entries_message(missing: &[String]) -> String {
    let list = missing.join("\n  ");
    format!(
        "{FILE_CS_STYLES} missing {} required ConcreteSupertype entries:\n  {list}\n\
         Add `[entity.<name>] style = \"carrier\" | \"base_parallel\"` for each.",
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
        pairs: &[(&str, ConcreteSupertypeStyle)],
    ) -> BTreeMap<String, ConcreteSupertypeStyle> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn validate_complete_match_counts_styles() {
        let required = required_set(&["face_bound", "styled_item"]);
        let provided = provided_map(&[
            ("face_bound", ConcreteSupertypeStyle::Carrier),
            ("styled_item", ConcreteSupertypeStyle::BaseParallel),
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
        let provided = provided_map(&[("face_bound", ConcreteSupertypeStyle::Carrier)]);
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
            ("face_bound", ConcreteSupertypeStyle::Carrier),
            ("cartesian_point", ConcreteSupertypeStyle::Carrier),
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
    fn parses_toml_with_tagged_style() {
        let body = r#"
[entity.face_bound]
style = "carrier"

[entity.styled_item]
style = "base_parallel"
"#;
        let file: ConcreteSupertypeStylesFile = toml::from_str(body).unwrap();
        assert_eq!(
            file.entity.get("face_bound"),
            Some(&ConcreteSupertypeStyle::Carrier)
        );
        assert_eq!(
            file.entity.get("styled_item"),
            Some(&ConcreteSupertypeStyle::BaseParallel)
        );
    }
}
