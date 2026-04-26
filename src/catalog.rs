//! Group classification + catalog output.
//!
//! Reads `groups.toml` (entity → group rules), runs every entity in every
//! schema through the classifier, and emits two files:
//! - `ENTITY_CATALOG.md` — human review (group distribution, evidence per entity, 변경 제안)
//! - `entity_catalog.json` — machine-readable mapping for downstream
//!   tooling (트레잇 리팩토링 시 entity → group import).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::express::Schema;
use crate::inheritance::{ancestors, root_supertype};

#[derive(Debug, Deserialize)]
struct GroupsConfig {
    group: Vec<GroupRule>,
}

#[derive(Debug, Deserialize)]
struct GroupRule {
    name: String,
    description: String,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    root_supertypes: Vec<String>,
    #[serde(default)]
    exclude_root: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityRecord {
    pub name: String,
    pub group: String,
    pub confidence: Confidence,
    pub root_supertype: Option<String>,
    pub matched_pattern: Option<String>,
    pub schemas_present: Vec<String>,
    pub effective_attr_count: HashMap<String, usize>,
    pub step_io_supports: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Serialize)]
pub struct Catalog {
    pub schemas: Vec<String>,
    pub group_descriptions: BTreeMap<String, String>,
    pub entities: BTreeMap<String, EntityRecord>,
    pub groups: BTreeMap<String, GroupSummary>,
    pub parser_warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct GroupSummary {
    pub description: String,
    pub count: usize,
    pub step_io_count: usize,
    pub entities: Vec<String>,
}

pub fn build_catalog(
    schemas: &[Schema],
    groups_toml_path: &Path,
    step_io_entities: &BTreeSet<String>,
) -> Result<Catalog, String> {
    let toml_text = fs::read_to_string(groups_toml_path)
        .map_err(|e| format!("read {groups_toml_path:?}: {e}"))?;
    let config: GroupsConfig =
        toml::from_str(&toml_text).map_err(|e| format!("parse groups.toml: {e}"))?;

    // Aggregate every entity name across schemas.
    let mut all_entities: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for s in schemas {
        for name in s.entities.keys() {
            all_entities
                .entry(name.clone())
                .or_default()
                .insert(s.source_label.clone());
        }
    }

    // Classify each entity. Use the first schema that contains it for
    // root-supertype lookups (chains are typically identical across the
    // 4 STEP schemas).
    let schema_map: HashMap<&str, &Schema> = schemas
        .iter()
        .map(|s| (s.source_label.as_str(), s))
        .collect();

    let mut entities = BTreeMap::new();
    for (name, schemas_present) in &all_entities {
        let mut effective: HashMap<String, usize> = HashMap::new();
        let mut root: Option<String> = None;
        let mut anc: Vec<String> = Vec::new();
        for sl in schemas_present {
            if let Some(s) = schema_map.get(sl.as_str()) {
                if let Some(c) = crate::inheritance::effective_attr_count(name, s) {
                    effective.insert(sl.clone(), c);
                }
                if root.is_none() {
                    root = root_supertype(name, s);
                    anc = ancestors(name, s);
                }
            }
        }

        let (group, confidence, matched_pattern) =
            classify(name, root.as_deref(), &anc, &config);

        let step_io_supports = step_io_entities.contains(&name.to_uppercase());

        entities.insert(
            name.clone(),
            EntityRecord {
                name: name.clone(),
                group,
                confidence,
                root_supertype: root,
                matched_pattern,
                schemas_present: schemas_present.iter().cloned().collect(),
                effective_attr_count: effective,
                step_io_supports,
            },
        );
    }

    // Build group summaries.
    let mut groups: BTreeMap<String, GroupSummary> = BTreeMap::new();
    let descriptions: BTreeMap<String, String> = config
        .group
        .iter()
        .map(|g| (g.name.clone(), g.description.clone()))
        .collect();
    for rule in &config.group {
        groups.insert(
            rule.name.clone(),
            GroupSummary {
                description: rule.description.clone(),
                count: 0,
                step_io_count: 0,
                entities: Vec::new(),
            },
        );
    }
    // The classifier may emit "_unclassified" for entities matching nothing.
    groups.insert(
        "_unclassified".to_string(),
        GroupSummary {
            description: "no rule matched (manual review needed)".to_string(),
            count: 0,
            step_io_count: 0,
            entities: Vec::new(),
        },
    );
    for (name, rec) in &entities {
        let summary = groups
            .entry(rec.group.clone())
            .or_insert_with(|| GroupSummary {
                description: String::new(),
                count: 0,
                step_io_count: 0,
                entities: Vec::new(),
            });
        summary.count += 1;
        summary.entities.push(name.clone());
        if rec.step_io_supports {
            summary.step_io_count += 1;
        }
    }

    let parser_warnings = schemas
        .iter()
        .flat_map(|s| {
            s.parse_warnings
                .iter()
                .map(move |w| format!("[{}] {}", s.source_label, w))
        })
        .collect();

    Ok(Catalog {
        schemas: schemas.iter().map(|s| s.source_label.clone()).collect(),
        group_descriptions: descriptions,
        entities,
        groups,
        parser_warnings,
    })
}

fn classify(
    name: &str,
    root: Option<&str>,
    ancestors_list: &[String],
    config: &GroupsConfig,
) -> (String, Confidence, Option<String>) {
    let lower = name.to_lowercase();

    // Pass 1 — semantic priority: SUBTYPE root match (after exclude_root).
    let mut root_match: Option<&GroupRule> = None;
    for rule in &config.group {
        let claims_root = rule.root_supertypes.iter().any(|r| {
            // Matches if root is exactly this rule's root, or any ancestor is.
            root == Some(r.as_str()) || ancestors_list.iter().any(|a| a == r)
        });
        let excluded = rule.exclude_root.iter().any(|r| {
            ancestors_list.iter().any(|a| a == r) || root == Some(r.as_str())
        });
        if claims_root && !excluded {
            root_match = Some(rule);
            break;
        }
    }

    // Pass 2 — name pattern.
    let mut pattern_match: Option<(&GroupRule, String)> = None;
    for rule in &config.group {
        for pat in &rule.patterns {
            if glob_match(pat, &lower) {
                pattern_match = Some((rule, pat.clone()));
                break;
            }
        }
        if pattern_match.is_some() {
            break;
        }
    }

    match (root_match, pattern_match) {
        (Some(r), Some((p, pat))) if r.name == p.name => {
            // Both agree — high confidence.
            (r.name.clone(), Confidence::High, Some(pat))
        }
        (Some(r), Some((_, _))) => {
            // Root vs pattern conflict — root wins, low confidence.
            (r.name.clone(), Confidence::Low, None)
        }
        (Some(r), None) => (r.name.clone(), Confidence::Medium, None),
        (None, Some((p, pat))) => (p.name.clone(), Confidence::Medium, Some(pat)),
        (None, None) => ("_unclassified".to_string(), Confidence::Low, None),
    }
}

fn glob_match(pattern: &str, input: &str) -> bool {
    // Simple `*` glob — matches any chars (including empty). Returns true if
    // the entire pattern matches the entire input. No `?` / `[]` support.
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return input == parts[0];
    }
    let mut idx = 0;
    let total = parts.len();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !input[idx..].starts_with(part) {
                return false;
            }
            idx += part.len();
        } else if i == total - 1 {
            return input[idx..].ends_with(part);
        } else {
            match input[idx..].find(part) {
                Some(pos) => idx += pos + part.len(),
                None => return false,
            }
        }
    }
    true
}

pub fn write_markdown(catalog: &Catalog, path: &Path) -> std::io::Result<()> {
    let mut out = String::new();
    out.push_str("# Entity catalog\n\n");
    out.push_str(&format!(
        "Schemas loaded: {} ({})\n\n",
        catalog.schemas.len(),
        catalog.schemas.join(", ")
    ));
    out.push_str(&format!(
        "Total unique entities: **{}**\n\n",
        catalog.entities.len()
    ));
    let step_io_total = catalog
        .entities
        .values()
        .filter(|e| e.step_io_supports)
        .count();
    out.push_str(&format!(
        "step-io processes: **{}** entities\n\n",
        step_io_total
    ));

    // Group distribution.
    out.push_str("## Group distribution\n\n");
    out.push_str("| group | description | count | step-io |\n");
    out.push_str("|---|---|---:|---:|\n");
    for (gname, summary) in &catalog.groups {
        if summary.count == 0 && gname != "_unclassified" {
            continue;
        }
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            gname, summary.description, summary.count, summary.step_io_count
        ));
    }
    out.push('\n');

    // Group details + entities.
    out.push_str("## Entities by group\n\n");
    for (gname, summary) in &catalog.groups {
        if summary.count == 0 {
            continue;
        }
        out.push_str(&format!(
            "### `{}` — {} ({} entities, {} step-io)\n\n",
            gname, summary.description, summary.count, summary.step_io_count
        ));
        for entity_name in &summary.entities {
            let rec = catalog.entities.get(entity_name).unwrap();
            let conf = match rec.confidence {
                Confidence::High => "H",
                Confidence::Medium => "M",
                Confidence::Low => "L",
            };
            let marker = if rec.step_io_supports { "✓" } else { " " };
            out.push_str(&format!(
                "- {} [{}] `{}` (root: {}, schemas: {})\n",
                marker,
                conf,
                rec.name,
                rec.root_supertype.as_deref().unwrap_or("?"),
                rec.schemas_present.join("/"),
            ));
        }
        out.push('\n');
    }

    // Parser warnings.
    if !catalog.parser_warnings.is_empty() {
        out.push_str("## Parser warnings\n\n");
        for w in &catalog.parser_warnings {
            out.push_str(&format!("- {w}\n"));
        }
        out.push('\n');
    }

    fs::write(path, out)
}

pub fn write_json(catalog: &Catalog, path: &Path) -> std::io::Result<()> {
    let s = serde_json::to_string_pretty(catalog)?;
    fs::write(path, s)
}
