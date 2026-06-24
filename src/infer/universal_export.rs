//! Universal export stage — emit `inferred/universal.toml`, the schema-faithful
//! input for the `step-io` `codegen` generator (the SchemaTarget::Universal
//! model). It is the faithful schema-union (same as `early.toml`) PLUS per-entity
//! **DERIVE facts** so codegen can mark derived/derivable (`*`) slots without a
//! hand-written mapping.
//!
//! Separation: depends only on the shared substrate (`express`, `refgraph`,
//! `export_common`) — NOT on `l1_export` or the 7-stage `ir.toml` pipeline — so
//! those can be removed later without affecting this exporter.
//!
//! DERIVE facts are emitted raw (the entity's own `DERIVE` statement targets);
//! the hard-vs-derivable interpretation (which needs complex-part knowledge)
//! happens in codegen. Each entry is `"super.attr"` for a `SELF\super.attr`
//! redeclaration or `"attr"` for a plain own-attr derive.

use std::collections::BTreeMap;
use std::fs;

use serde::Serialize;

use crate::express::Schema;
use crate::infer::export_common::{redeclaration_has_signal, schema_rank, ty_repr};
use crate::infer::refgraph;

const OUT: &str = "inferred/universal.toml";

#[derive(Serialize)]
struct UnivAttr {
    name: String,
    ty: String,
}

/// One faithful entity declaration. Field order matters for toml: all
/// scalar/inline values (incl. the inline `derives` string array) precede the
/// `own_attrs`/`redeclared_attrs` array-of-tables.
#[derive(Serialize)]
struct UnivEntity {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    parents: Vec<String>,
    is_abstract: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attr_conflicts: Vec<String>,
    /// Raw DERIVE targets: `"super.attr"` (SELF\super.attr) or `"attr"` (own).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    derives: Vec<String>,
    own_attrs: Vec<UnivAttr>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    redeclared_attrs: Vec<UnivAttr>,
}

#[derive(Serialize)]
struct UnivTypeDef {
    aliased: String,
}

#[derive(Serialize)]
struct UniversalToml {
    entity: BTreeMap<String, UnivEntity>,
    #[serde(rename = "type", skip_serializing_if = "BTreeMap::is_empty")]
    type_aliases: BTreeMap<String, UnivTypeDef>,
}

pub fn run(schemas: &[Schema]) -> Result<(), String> {
    let unified = refgraph::build(schemas);

    // Schemas newest-first: first to declare an entity/type wins.
    let mut ranked: Vec<&Schema> = schemas.iter().collect();
    ranked.sort_by_key(|s| std::cmp::Reverse(schema_rank(&s.source_label)));

    let mut entity: BTreeMap<String, UnivEntity> = BTreeMap::new();
    for name in unified.entity_parents.keys() {
        // own_attrs, parents, and derives all come from the SAME newest-AP decl.
        let decl = ranked.iter().find_map(|s| s.entities.get(name));
        let own_attrs: Vec<UnivAttr> = decl
            .map(|e| {
                e.own_attrs
                    .iter()
                    .map(|a| UnivAttr {
                        name: a.name.clone(),
                        ty: ty_repr(&a.ty),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let parents: Vec<String> = decl.map(|e| e.parents.clone()).unwrap_or_default();
        let redeclared_attrs: Vec<UnivAttr> = decl
            .map(|e| {
                e.redeclared_attrs
                    .iter()
                    .filter(|a| redeclaration_has_signal(&a.ty, &ranked))
                    .map(|a| UnivAttr {
                        name: a.name.clone(),
                        ty: ty_repr(&a.ty),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let derives: Vec<String> = decl
            .map(|e| {
                e.derived_attrs
                    .iter()
                    .map(|d| match &d.super_qual {
                        Some(s) => format!("{s}.{}", d.name),
                        None => d.name.clone(),
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
            UnivEntity {
                parents,
                is_abstract: unified.abstract_entities.contains(name),
                attr_conflicts,
                derives,
                own_attrs,
                redeclared_attrs,
            },
        );
    }

    let mut type_aliases: BTreeMap<String, UnivTypeDef> = BTreeMap::new();
    for s in &ranked {
        for (tn, td) in &s.types {
            type_aliases
                .entry(tn.clone())
                .or_insert_with(|| UnivTypeDef {
                    aliased: ty_repr(&td.aliased),
                });
        }
    }

    let doc = UniversalToml {
        entity,
        type_aliases,
    };
    let body = toml::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    let header = "# Generated by `infer universal_export` — schema-faithful codegen input.\n\
                  # DO NOT hand-edit. Faithful schema-union (= early.toml) + per-entity\n\
                  # `derives` (raw DERIVE targets: \"super.attr\" or \"attr\"); codegen marks\n\
                  # derived/derivable `*` slots from these. Names/own_attrs prefer newest AP.\n\n";
    fs::create_dir_all("inferred").map_err(|e| e.to_string())?;
    fs::write(OUT, format!("{header}{body}")).map_err(|e| e.to_string())?;
    let with_derives = doc.entity.values().filter(|e| !e.derives.is_empty()).count();
    eprintln!(
        "wrote {OUT}: {} entities ({with_derives} with derives), {} type aliases",
        doc.entity.len(),
        doc.type_aliases.len()
    );
    Ok(())
}
