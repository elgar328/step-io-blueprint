//! L1 export stage — emit `inferred/early.toml`, the faithful schema-union
//! **EarlyModel (L1)** blueprint.
//!
//! This is the entry point of the 2-layer IR direction (see step-io
//! `internal/IR_LAYERING_DIRECTION.md`). Unlike the 7 inference stages
//! (variant → naming), which flatten / unify / prune the union into the
//! ergonomic L2 (`ir.toml`), `l1_export` performs **no inference**: it
//! records every entity in the union exactly as the schema declares it
//! (ordered own attributes + parents), so the output is a single faithful
//! source for generating the early-bound L1 layer.
//!
//! Reuse: [`refgraph::build`] already unions all schemas by name and records
//! `attr_conflicts` / `abstract_entities` / `entity_parents`. The one thing
//! the union discards is **attribute declaration order** (it keys attrs in a
//! `BTreeSet`/`BTreeMap`), which L1 needs because Part21 attributes are
//! positional. So ordered `own_attrs` are taken from the per-schema
//! [`Schema`] picked newest-AP-first (`schema_rank`): names/shapes prefer the
//! newest schema, matching "이름은 최신 AP로 통일".
//!
//! Type repr is lossless and toml-safe (a string): primitives lowercase
//! (`real`/`integer`/…); a bare token is an entity or TYPE-alias ref;
//! `LIST/SET/BAG/ARRAY OF <inner>`, `OPTIONAL <inner>`, `SELECT(a, b)`,
//! `ENUM(a, b)`. TYPE aliases are kept **unresolved** (L1 is faithful;
//! resolving `length_measure → real` is L2's job) and emitted to `[type.*]`.
//!
//! Known simplifications (v1): `redeclared_attrs` (SELF\super.attr type
//! narrowing) and `supertype_expr` (SUPERTYPE OF children) are not emitted —
//! not needed for the first cluster's attribute layout; revisit when the
//! codegen generator (Phase 5) needs them.

use std::collections::BTreeMap;
use std::fs;

use serde::Serialize;

use crate::express::{AttrType, Schema};
use crate::infer::refgraph;

const OUT: &str = "inferred/early.toml";

/// Newest → oldest preference for picking an entity's canonical (ordered)
/// own-attribute declaration and TYPE aliases. Higher = preferred. The
/// newest schema carries the most entities and newest attribute shapes,
/// which is what the L1 superset wants (draft-vs-IS only matters for the
/// per-schema *output* profiles in Phase 5, not for the union).
fn schema_rank(label: &str) -> u8 {
    if label.starts_with("ap242ed3") {
        6
    } else if label.starts_with("ap242ed2") {
        5
    } else if label.starts_with("ap242") {
        4
    } else if label.starts_with("ap214") {
        3
    } else if label.starts_with("ap203e2") {
        2
    } else if label.starts_with("ap203") {
        1
    } else {
        0
    }
}

/// Lossless, toml-safe string repr of an attribute type. See module doc.
fn ty_repr(ty: &AttrType) -> String {
    match ty {
        AttrType::Primitive(p) => p.to_lowercase(),
        AttrType::Entity(name) => name.clone(),
        AttrType::List(inner) => format!("LIST OF {}", ty_repr(inner)),
        AttrType::Set(inner) => format!("SET OF {}", ty_repr(inner)),
        AttrType::Bag(inner) => format!("BAG OF {}", ty_repr(inner)),
        AttrType::Array(inner) => format!("ARRAY OF {}", ty_repr(inner)),
        AttrType::Optional(inner) => format!("OPTIONAL {}", ty_repr(inner)),
        AttrType::Select(members) => format!("SELECT({})", members.join(", ")),
        AttrType::Enumeration(members) => format!("ENUM({})", members.join(", ")),
    }
}

#[derive(Serialize)]
struct EarlyAttr {
    name: String,
    ty: String,
}

/// One faithful L1 entity declaration. Field order matters for toml: all
/// scalar/inline values precede the `own_attrs` array-of-tables (toml-rs
/// emits a key after a sub-table into the wrong table otherwise).
#[derive(Serialize)]
struct EarlyEntity {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    parents: Vec<String>,
    is_abstract: bool,
    /// `(entity, attr)` conflicts: schemas disagreed on this attr's type.
    /// Carried as a signal; the canonical (newest-AP) type is used above.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attr_conflicts: Vec<String>,
    /// Declared attributes in Part21 positional order (excludes inherited;
    /// inheritance is resolved from `parents` at codegen time).
    own_attrs: Vec<EarlyAttr>,
}

#[derive(Serialize)]
struct EarlyTypeDef {
    aliased: String,
}

#[derive(Serialize)]
struct EarlyToml {
    entity: BTreeMap<String, EarlyEntity>,
    #[serde(rename = "type", skip_serializing_if = "BTreeMap::is_empty")]
    type_aliases: BTreeMap<String, EarlyTypeDef>,
}

pub fn run(schemas: &[Schema]) -> Result<(), String> {
    let unified = refgraph::build(schemas);

    // Schemas newest-first: first to declare an entity/type wins.
    let mut ranked: Vec<&Schema> = schemas.iter().collect();
    ranked.sort_by_key(|s| std::cmp::Reverse(schema_rank(&s.source_label)));

    let mut entity: BTreeMap<String, EarlyEntity> = BTreeMap::new();
    for (name, parents) in &unified.entity_parents {
        let own_attrs: Vec<EarlyAttr> = ranked
            .iter()
            .find_map(|s| s.entities.get(name))
            .map(|e| {
                e.own_attrs
                    .iter()
                    .map(|a| EarlyAttr {
                        name: a.name.clone(),
                        ty: ty_repr(&a.ty),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let attr_conflicts: Vec<String> = unified
            .attr_conflicts
            .iter()
            .filter(|((e, _), _)| e == name)
            .map(|((_, a), variants)| format!("{a}: {}", variants.join(" | ")))
            .collect();

        entity.insert(
            name.clone(),
            EarlyEntity {
                parents: parents.clone(),
                is_abstract: unified.abstract_entities.contains(name),
                attr_conflicts,
                own_attrs,
            },
        );
    }

    let mut type_aliases: BTreeMap<String, EarlyTypeDef> = BTreeMap::new();
    for s in &ranked {
        for (tn, td) in &s.types {
            type_aliases.entry(tn.clone()).or_insert_with(|| EarlyTypeDef {
                aliased: ty_repr(&td.aliased),
            });
        }
    }

    let doc = EarlyToml {
        entity,
        type_aliases,
    };
    let body = toml::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    let header = "# Generated by `infer l1_export` — faithful schema-union EarlyModel (L1).\n\
                  # DO NOT hand-edit. No inference: every entity recorded as declared.\n\
                  # Entity names / own_attrs prefer the newest AP; parents = union.\n\
                  # ty repr: primitives lowercase; bare token = entity/TYPE-alias ref;\n\
                  #   LIST/SET/BAG/ARRAY OF, OPTIONAL, SELECT(...), ENUM(...).\n\n";
    fs::create_dir_all("inferred").map_err(|e| e.to_string())?;
    fs::write(OUT, format!("{header}{body}")).map_err(|e| e.to_string())?;
    eprintln!(
        "wrote {OUT}: {} entities, {} type aliases",
        doc.entity.len(),
        doc.type_aliases.len()
    );
    Ok(())
}
