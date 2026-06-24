//! Profile export stage — emit `profiles/<target>.toml`, the per-target
//! **output SchemaProfile** for schema-conditioned writing in step-io.
//!
//! While [`l1_export`](crate::infer::l1_export) emits ONE union `early.toml`
//! (all schemas merged, newest-AP wins) for *reading* every input, the writer
//! needs to know, *per output target*, which entities are legal and what each
//! entity's attributes are. This stage emits one profile per curated output
//! target — each the **latest IS edition** of its AP family:
//!
//! - `ap214e3`  ← AP214 ed3 (ISO 10303-214:2010 IS)
//! - `ap242e2`  ← AP242 ed2 (ISO 10303-242:2020 IS)
//! - `ap203e2`  ← AP203 ed2 (ISO 10303-403 IS)
//!
//! Unlike `l1_export`, this does NOT union or newest-AP-pick: each profile is
//! built from that target's **single** [`Schema`] (`schema.entities` verbatim),
//! so an entity's presence = legal in the target, absence = illegal (step-io's
//! projection drops it). Attribute repr (`ty_repr`) and the redeclaration
//! signal filter are reused from `l1_export` so profiles match early.toml's
//! shape; step-io flattens inheritance from `parents` (it never appears
//! pre-flattened, mirroring early.toml).
//!
//! `[meta]` (FILE_SCHEMA descriptor + APPLICATION_PROTOCOL_DEFINITION) is not
//! derivable from the `.exp` (those are Part21 header constructs), so it is
//! hard-supplied per target from corpus-verified IS values (see [`TARGETS`]).
//! `attr_conflicts` is intentionally omitted — it is a cross-schema-disagreement
//! signal with no meaning inside a single-schema profile.

use std::collections::{BTreeMap, HashMap};
use std::fs;

use serde::Serialize;

use crate::express::{EntitySchema, Schema};
use crate::infer::export_common::{redeclaration_has_signal, ty_repr};

const OUT_DIR: &str = "profiles";

/// A curated output target. `label_match` is matched against
/// [`Schema::source_label`] with `starts_with` (AP242 ed2's label is the full
/// `ap242ed2_dis2_mim_lf_v1.101`, so an exact match would miss it). FILE_SCHEMA
/// / APD values are corpus-verified IS, latest-edition descriptors (the `.exp`
/// carries no Part21 header data).
struct Target {
    /// Output file name (`profiles/<out_name>.toml`).
    out_name: &'static str,
    /// `source_label` prefix identifying the source schema (unique per target).
    label_match: &'static str,
    /// `FILE_SCHEMA` descriptor strings (AP203 ed1 needs two; the e2 long forms
    /// are single).
    file_schema: &'static [&'static str],
    /// `APPLICATION_PROTOCOL_DEFINITION(status, application, year, …)`.
    apd_status: &'static str,
    apd_name: &'static str,
    apd_year: i64,
    /// `APPLICATION_CONTEXT.application` description string (the AC the APD
    /// references; distinct from `apd_name`). Corpus-verified per target.
    apd_desc: &'static str,
}

/// The three curated output targets (latest IS edition per AP). FILE_SCHEMA /
/// APD values verified against the corpus (fusion360 = AP214e3, NIST = AP242e2 /
/// AP203e2).
const TARGETS: &[Target] = &[
    Target {
        out_name: "ap214e3",
        label_match: "ap214e3",
        file_schema: &["AUTOMOTIVE_DESIGN { 1 0 10303 214 3 1 1 }"],
        apd_status: "international standard",
        apd_name: "automotive_design",
        apd_year: 2009,
        apd_desc: "Core Data for Automotive Mechanical Design Process",
    },
    Target {
        out_name: "ap242e2",
        label_match: "ap242ed2",
        file_schema: &["AP242_MANAGED_MODEL_BASED_3D_ENGINEERING_MIM_LF { 1 0 10303 442 3 1 4 }"],
        apd_status: "international standard",
        apd_name: "ap242_managed_model_based_3d_engineering_mim_lf",
        apd_year: 2011,
        apd_desc: "managed model based 3d engineering",
    },
    Target {
        out_name: "ap203e2",
        label_match: "ap203e2",
        file_schema: &[
            "AP203_CONFIGURATION_CONTROLLED_3D_DESIGN_OF_MECHANICAL_PARTS_AND_ASSEMBLIES_MIM_LF \
             { 1 0 10303 403 2 1 2 }",
        ],
        apd_status: "international standard",
        apd_name: "config_control_design",
        apd_year: 2010,
        apd_desc: "configuration controlled 3D designs of mechanical parts and assemblies",
    },
];

#[derive(Serialize)]
struct ProfileAttr {
    name: String,
    ty: String,
}

/// One legal entity in a target's output profile. Same shape as early.toml's
/// entity (own attrs + parents; inheritance flattened by step-io) minus
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
        // Single-schema `ranked` slice for the SELECT-alias redeclaration check.
        let ranked = [schema];

        let mut entity: BTreeMap<String, ProfileEntity> = BTreeMap::new();
        for (name, e) in &schema.entities {
            let own_attrs: Vec<ProfileAttr> = e
                .own_attrs
                .iter()
                .map(|a| ProfileAttr {
                    name: a.name.clone(),
                    ty: ty_repr(&a.ty),
                })
                .collect();
            let redeclared_attrs: Vec<ProfileAttr> = e
                .redeclared_attrs
                .iter()
                .filter(|a| redeclaration_has_signal(&a.ty, &ranked))
                .map(|a| ProfileAttr {
                    name: a.name.clone(),
                    ty: ty_repr(&a.ty),
                })
                .collect();
            entity.insert(
                name.clone(),
                ProfileEntity {
                    parents: e.parents.clone(),
                    is_abstract: e.is_abstract,
                    own_attrs,
                    redeclared_attrs,
                },
            );
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
            entity,
            type_aliases,
        };

        let body = toml::to_string_pretty(&doc).map_err(|e| e.to_string())?;
        let header = format!(
            "# Generated by `infer profile_export` — output SchemaProfile for target {out}.\n\
             # Source schema: {src} (latest IS edition of the AP).\n\
             # DO NOT hand-edit. Legal entity set + ordered attrs for schema-conditioned output.\n\
             # Presence = legal in this target; absence = illegal (step-io project drops).\n\
             # own_attrs + parents only (step-io flattens inheritance); ty repr = see early.toml.\n\n",
            out = t.out_name,
            src = schema.source_label,
        );
        let path = format!("{OUT_DIR}/{}.toml", t.out_name);
        fs::write(&path, format!("{header}{body}")).map_err(|e| e.to_string())?;
        eprintln!(
            "wrote {path}: {} entities, {} type aliases, {} downgrades (source {})",
            doc.entity.len(),
            doc.type_aliases.len(),
            doc.downgrade.len(),
            schema.source_label,
        );
    }
    Ok(())
}
