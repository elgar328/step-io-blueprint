//! Profile export stage — emit `profiles/<target>.toml`, the per-target
//! **output SchemaProfile** for schema-conditioned writing in step-io.
//!
//! The writer needs to know, *per output target*, which entities are legal and
//! what each entity's attributes are. This stage emits one profile per curated
//! output target (see [`TARGETS`]). Currently the sole target is `ap242e4`
//! (AP242 ed4, ISO 10303-242:2025 IS — step-io's only public output schema); the
//! table is a slice so more IS-edition targets can be added without other change.
//!
//! Each profile is built from that target's **single** [`Schema`]
//! (`schema.entities` verbatim, no union or newest-AP-pick), so an entity's
//! presence = legal in the target, absence = illegal (step-io's projection drops
//! it). Attribute repr (`ty_repr`) and the redeclaration signal filter come from
//! `common`; step-io flattens inheritance from `parents` (attrs are never
//! pre-flattened here).
//!
//! `[meta]` (FILE_SCHEMA descriptor + APPLICATION_PROTOCOL_DEFINITION) is not
//! derivable from the `.exp` (those are Part21 header constructs), so it is
//! hand-supplied per target (see [`TARGETS`]).
//! `attr_conflicts` is intentionally omitted — it is a cross-schema-disagreement
//! signal with no meaning inside a single-schema profile.

use std::collections::{BTreeMap, HashMap};
use std::fs;

use serde::Serialize;

use crate::export::common::{oneof_groups, redeclaration_owner, ty_repr};
use crate::export::where_types::{self, Count};
use crate::express::{EntitySchema, Schema};

const OUT_DIR: &str = "profiles";
/// What became of each `TYPEOF`-mentioning `WHERE` rule of the target.
const WHERE_REPORT: &str = "inferred/where_type_report.txt";

/// A curated output target. `label_match` is matched against
/// [`Schema::source_label`] with `starts_with` (labels are the clean schema file
/// stems, e.g. `ap242e4`). FILE_SCHEMA / APD values are hand-supplied IS
/// descriptors (the `.exp` carries no Part21 header data).
struct Target {
    /// Output file name (`profiles/<out_name>.toml`).
    out_name: &'static str,
    /// `source_label` prefix identifying the source schema (unique per target).
    label_match: &'static str,
    /// `FILE_SCHEMA` descriptor strings (AP203 ed1 needs two; the long forms
    /// are single).
    file_schema: &'static [&'static str],
    /// `APPLICATION_PROTOCOL_DEFINITION(status, application, year, …)`.
    apd_status: &'static str,
    apd_name: &'static str,
    apd_year: i64,
    /// `APPLICATION_CONTEXT.application` description string (the AC the APD
    /// references; distinct from `apd_name`).
    apd_desc: &'static str,
}

/// The single curated output target: AP242 ed4, the only public output schema
/// (step-io reads AP203/AP214/AP242 but always writes AP242 ed4). The
/// FILE_SCHEMA version is that of the ISO/TS 10303-442 edition holding ed4's
/// schema (ed7, 2025); the APD year is ed4's.
const TARGETS: &[Target] = &[Target {
    out_name: "ap242e4",
    label_match: "ap242e4",
    file_schema: &["AP242_MANAGED_MODEL_BASED_3D_ENGINEERING_MIM_LF { 1 0 10303 442 7 1 4 }"],
    apd_status: "international standard",
    apd_name: "ap242_managed_model_based_3d_engineering_mim_lf",
    apd_year: 2025,
    apd_desc: "managed model based 3d engineering",
}];

#[derive(Serialize)]
struct ProfileAttr {
    name: String,
    ty: String,
    /// For a redeclaration, the entity that declares the attribute.
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
}

/// One legal entity in a target's output profile (own attrs + parents;
/// inheritance flattened by step-io) minus
/// `attr_conflicts`. Field order matters for toml (scalars before the
/// array-of-tables `own_attrs`).
#[derive(Serialize)]
struct ProfileEntity {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    parents: Vec<String>,
    is_abstract: bool,
    own_attrs: Vec<ProfileAttr>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    redeclared_attrs: Vec<ProfileAttr>,
    /// Inherited like redeclarations: they hold for every subtype.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    type_rules: Vec<ProfileTypeRule>,
}

/// An attribute type restriction a `WHERE` rule makes: the attribute (or
/// each element of it) may reference only `admits`.
#[derive(Serialize)]
struct ProfileTypeRule {
    name: String,
    /// The entity declaring the attribute.
    owner: String,
    /// The rule label.
    rule: String,
    /// The single type a positive rule names, else `<entity>_<attr>`.
    type_name: String,
    admits: Vec<String>,
    /// How many of `names` the value's `TYPEOF` holds: `at_least_one`,
    /// `exactly_one` or `zero`.
    count: &'static str,
    /// The type names the rule lists; a complex value's `TYPEOF` is the
    /// union over its parts.
    names: Vec<String>,
    /// The rule holds only where SELF's `TYPEOF` holds all of these.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    when_self: Vec<String>,
    /// The rule does not hold where SELF's `TYPEOF` holds any of these.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unless_self: Vec<String>,
    /// The aggregate-typed SELECT members the rule admits (none if absent).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    aggregates: Vec<ProfileAggregate>,
}

/// An aggregate-typed SELECT member a type rule admits.
#[derive(Serialize)]
struct ProfileAggregate {
    type_name: String,
    /// The entities each element may reference.
    admits: Vec<String>,
}

#[derive(Serialize)]
struct ProfileApd {
    status: String,
    name: String,
    year: i64,
    /// APPLICATION_CONTEXT.application description.
    description: String,
}

#[derive(Serialize)]
struct ProfileMeta {
    file_schema: Vec<String>,
    apd: ProfileApd,
}

#[derive(Serialize)]
struct ProfileTypeDef {
    aliased: String,
}

/// One ONEOF exclusivity group from a `SUPERTYPE OF` clause: a complex
/// instance may draw parts from at most **one** branch. Each branch is the
/// flat entity-name set of that ONEOF child's subtree — an `Entity` child is
/// one name; a composite child (`loop ANDOR path`, `a AND b`) contributes all
/// its subtree names as a single branch.
#[derive(Serialize)]
struct ProfileOneof {
    /// Entity whose `SUPERTYPE OF` clause declares the group.
    supertype: String,
    branches: Vec<Vec<String>>,
}

#[derive(Serialize)]
struct ProfileToml {
    meta: ProfileMeta,
    /// Lossless subtype -> supertype downgrade (e.g. `design_context` ->
    /// `product_definition_context`): entities absent from this target but
    /// reachable to a target-legal ancestor via a rename-safe parent chain.
    /// step-io's projection renames these before dropping, keeping cross-AP
    /// product structure intact. Empty if the target has no such subtypes.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    downgrade: BTreeMap<String, String>,
    /// ONEOF exclusivity groups from this target's `SUPERTYPE OF` clauses —
    /// the schema-side input for validating complex (multi-part) instances.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    oneof: Vec<ProfileOneof>,
    entity: BTreeMap<String, ProfileEntity>,
    #[serde(rename = "type", skip_serializing_if = "BTreeMap::is_empty")]
    type_aliases: BTreeMap<String, ProfileTypeDef>,
}

/// Walk `e`'s parent chain to the nearest ancestor present in `target`,
/// requiring every hop to be **rename-safe**: exactly one parent and no own /
/// redeclared attributes (a pure WHERE-constraint subtype, e.g. `design_context`
/// over `product_definition_context`). Returns `None` if any hop adds structure
/// or no target-legal ancestor exists — such an entity is left to be dropped.
fn nearest_legal_supertype(
    e: &EntitySchema,
    all: &BTreeMap<&str, &EntitySchema>,
    target: &HashMap<String, EntitySchema>,
) -> Option<String> {
    // Bound the walk by a generous inheritance-depth cap (cycle guard against a
    // malformed parent chain; real STEP hierarchies are far shallower).
    const MAX_INHERITANCE_DEPTH: usize = 64;
    let mut cur = e;
    for _ in 0..MAX_INHERITANCE_DEPTH {
        if cur.parents.len() != 1 || !cur.own_attrs.is_empty() || !cur.redeclared_attrs.is_empty() {
            return None;
        }
        let parent = cur.parents[0].as_str();
        if target.contains_key(parent) {
            return Some(parent.to_string());
        }
        cur = all.get(parent)?;
    }
    None
}

/// Every redeclaration `e` makes in the target schema, exactly as declared
/// (aggregate bounds included): the output must follow the target's own
/// types, so none is filtered out.
fn profile_redeclarations(schema: &Schema, e: &EntitySchema) -> Vec<ProfileAttr> {
    e.redeclared_attrs
        .iter()
        .map(|a| ProfileAttr {
            name: a.name.clone(),
            ty: ty_repr(&a.ty),
            owner: redeclaration_owner(schema, a),
        })
        .collect()
}

pub fn run(schemas: &[Schema]) -> Result<(), String> {
    fs::create_dir_all(OUT_DIR).map_err(|e| e.to_string())?;
    // Union of every entity across all schemas — a subtype absent from one
    // target may be defined in another AP, and the downgrade walk needs its
    // parent chain.
    let mut all_entities: BTreeMap<&str, &EntitySchema> = BTreeMap::new();
    for s in schemas {
        for (name, e) in &s.entities {
            all_entities.entry(name.as_str()).or_insert(e);
        }
    }
    for t in TARGETS {
        let schema = schemas
            .iter()
            .find(|s| s.source_label.starts_with(t.label_match))
            .ok_or_else(|| {
                format!(
                    "profile_export: no schema with source_label starting '{}' (target {})",
                    t.label_match, t.out_name
                )
            })?;
        let mut where_types = where_types::analyze(schema);
        let mut entity: BTreeMap<String, ProfileEntity> = BTreeMap::new();
        for (name, e) in &schema.entities {
            let own_attrs: Vec<ProfileAttr> = e
                .own_attrs
                .iter()
                .map(|a| ProfileAttr {
                    name: a.name.clone(),
                    ty: ty_repr(&a.ty),
                    owner: None,
                })
                .collect();
            let redeclared_attrs = profile_redeclarations(schema, e);
            let type_rules = where_types
                .rules
                .remove(name)
                .unwrap_or_default()
                .into_iter()
                .map(|r| ProfileTypeRule {
                    name: r.attr,
                    owner: r.owner,
                    rule: r.rule,
                    type_name: r.type_name,
                    admits: r.admits,
                    count: match r.count {
                        Count::AtLeastOne => "at_least_one",
                        Count::ExactlyOne => "exactly_one",
                        Count::Zero => "zero",
                    },
                    names: r.names,
                    when_self: r.when_self,
                    unless_self: r.unless_self,
                    aggregates: r
                        .aggregates
                        .into_iter()
                        .map(|a| ProfileAggregate {
                            type_name: a.type_name,
                            admits: a.admits,
                        })
                        .collect(),
                })
                .collect();
            entity.insert(
                name.clone(),
                ProfileEntity {
                    parents: e.parents.clone(),
                    is_abstract: e.is_abstract,
                    own_attrs,
                    redeclared_attrs,
                    type_rules,
                },
            );
        }

        // ONEOF exclusivity groups, in sorted-owner order (deterministic;
        // groups within one clause keep the clause's pre-order).
        let mut oneof: Vec<ProfileOneof> = Vec::new();
        let mut ents: Vec<(&String, &EntitySchema)> = schema.entities.iter().collect();
        ents.sort_by_key(|(n, _)| n.as_str());
        for (name, e) in ents {
            if let Some(expr) = &e.supertype_expr {
                oneof.extend(oneof_groups(expr).into_iter().map(|branches| ProfileOneof {
                    supertype: name.clone(),
                    branches,
                }));
            }
        }

        let mut type_aliases: BTreeMap<String, ProfileTypeDef> = BTreeMap::new();
        for (tn, td) in &schema.types {
            type_aliases.insert(
                tn.clone(),
                ProfileTypeDef {
                    aliased: ty_repr(&td.aliased),
                },
            );
        }

        // Auto-derive the lossless subtype->supertype downgrade table: every
        // entity absent from this target that walks rename-safely to a
        // target-legal ancestor (e.g. design_context -> product_definition_context
        // for AP214). step-io renames these before dropping on cross-AP output.
        let mut downgrade: BTreeMap<String, String> = BTreeMap::new();
        for (name, e) in &all_entities {
            if schema.entities.contains_key(*name) {
                continue; // legal in this target — no downgrade needed
            }
            if let Some(super_name) = nearest_legal_supertype(e, &all_entities, &schema.entities) {
                downgrade.insert((*name).to_string(), super_name);
            }
        }

        let doc = ProfileToml {
            meta: ProfileMeta {
                file_schema: t.file_schema.iter().map(|s| (*s).to_string()).collect(),
                apd: ProfileApd {
                    status: t.apd_status.to_string(),
                    name: t.apd_name.to_string(),
                    year: t.apd_year,
                    description: t.apd_desc.to_string(),
                },
            },
            downgrade,
            oneof,
            entity,
            type_aliases,
        };

        let body = toml::to_string_pretty(&doc).map_err(|e| e.to_string())?;
        let header = format!(
            "# Generated by `profile_export` — output SchemaProfile for target {out}.\n\
             # Source schema: {src} (latest IS edition of the AP).\n\
             # DO NOT hand-edit. Legal entity set + ordered attrs for schema-conditioned output.\n\
             # Presence = legal in this target; absence = illegal (step-io project drops).\n\
             # own_attrs + parents only (step-io flattens inheritance).\n\
             # ty repr: primitives lowercase; bare token = entity/TYPE-alias ref; LIST/SET/BAG/ARRAY OF, OPTIONAL, SELECT(...), ENUM(...).\n\
             # [[oneof]] = ONEOF exclusivity groups from SUPERTYPE OF clauses; branch = flat name set of one ONEOF child subtree.\n\
             # type_rules = WHERE rules restricting an attribute's types: the attribute (or each element) references only `admits`, and its TYPEOF (for a complex value, the union over its parts) holds `count` of `names`; inherited by subtypes; where `when_self` is given, only for an instance whose TYPEOF holds all of it, and where `unless_self` is, not for one whose TYPEOF holds any of it; `aggregates` = the aggregate-typed SELECT members (a value that is one aggregate, whose TYPEOF is that type and the SELECTs holding it) the rule admits, each with the entities its elements may reference — an aggregate member not listed is not admitted.\n\n",
            out = t.out_name,
            src = schema.source_label,
        );
        let path = format!("{OUT_DIR}/{}.toml", t.out_name);
        fs::write(&path, format!("{header}{body}")).map_err(|e| e.to_string())?;
        let mut report = format!(
            "# WHERE rules of {} that mention TYPEOF: `take` = exported as a type rule, \
             `skip` = not an attribute type restriction (reason given).\n",
            schema.source_label
        );
        for line in &where_types.report {
            report.push_str(line);
            report.push('\n');
        }
        fs::write(WHERE_REPORT, report).map_err(|e| e.to_string())?;
        eprintln!(
            "wrote {path}: {} entities, {} type aliases, {} downgrades, {} oneof groups (source {})",
            doc.entity.len(),
            doc.type_aliases.len(),
            doc.downgrade.len(),
            doc.oneof.len(),
            schema.source_label,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::express::load_all_schemas;

    #[test]
    fn the_profile_keeps_every_target_redeclaration_with_its_bounds() {
        let schemas = load_all_schemas(Path::new("schemas"));
        let target = schemas
            .iter()
            .find(|s| s.source_label == TARGETS[0].label_match)
            .unwrap();
        let red = |entity: &str| profile_redeclarations(target, &target.entities[entity]);
        let sdr = red("shape_definition_representation");
        assert!(
            sdr.iter().any(|a| a.name == "definition"
                && a.ty == "property_definition"
                && a.owner.as_deref() == Some("property_definition_representation")),
            "{:?}",
            sdr.iter().map(|a| (&a.name, &a.ty)).collect::<Vec<_>>()
        );
        let address = red("person_and_organization_address");
        assert!(
            address
                .iter()
                .any(|a| a.name == "people" && a.ty == "SET [1:1] OF person"),
            "{:?}",
            address.iter().map(|a| (&a.name, &a.ty)).collect::<Vec<_>>()
        );
    }
}
