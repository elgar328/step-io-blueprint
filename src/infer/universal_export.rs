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

use std::collections::BTreeSet;

use crate::express::{AttrType, Schema, SupertypeExpr};
use crate::infer::export_common::{redeclaration_has_signal, schema_rank, ty_repr};
use crate::infer::refgraph;

const OUT: &str = "inferred/universal.toml";

/// serde `skip_serializing_if` for a `false` bool (keep the file small).
fn is_false(b: &bool) -> bool {
    !*b
}

/// Collect entity names that appear inside an `AndOr`/`And` node (recursively).
/// These are the multiple-inheritance-combinable leaves — a top-level `OneOf`
/// (mutually exclusive subtypes, no ANDOR) is NOT combinable, so entities are
/// only collected once `in_andor` is set by entering an AndOr/And.
fn collect_combinable(expr: &SupertypeExpr, in_andor: bool, out: &mut BTreeSet<String>) {
    match expr {
        SupertypeExpr::Entity { name } => {
            if in_andor {
                out.insert(name.clone());
            }
        }
        SupertypeExpr::OneOf { children } => {
            for c in children {
                collect_combinable(c, in_andor, out);
            }
        }
        SupertypeExpr::AndOr { children } | SupertypeExpr::And { children } => {
            for c in children {
                collect_combinable(c, true, out);
            }
        }
    }
}

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
    /// True if this entity can appear as a part of a Part21 complex (multiple-
    /// inheritance) record: it is an ANDOR/AND-combinable leaf or a transitive
    /// supertype of one. codegen gives it a part-bag variant (dual-appearance).
    #[serde(skip_serializing_if = "is_false")]
    is_complex_part: bool,
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

    // complex-part set = entities inside any AndOr/And node (combinable leaves)
    // plus their transitive supertypes (which also appear in the complex record).
    let mut combinable: BTreeSet<String> = BTreeSet::new();
    for name in unified.entity_parents.keys() {
        if let Some(expr) = ranked
            .iter()
            .find_map(|s| s.entities.get(name))
            .and_then(|e| e.supertype_expr.as_ref())
        {
            collect_combinable(expr, false, &mut combinable);
        }
    }
    // Implicit combinability: a bare supertype (declares NO `SUPERTYPE OF`
    // clause) does not ONEOF-restrict its subtypes, so they may co-occur in a
    // Part21 complex record (e.g. representation_context's geometric/parametric/
    // global subtypes form the context complex). Mark such subtypes combinable.
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in unified.entity_parents.keys() {
        if let Some(decl) = ranked.iter().find_map(|s| s.entities.get(name)) {
            for p in &decl.parents {
                children.entry(p.clone()).or_default().push(name.clone());
            }
        }
    }
    for name in unified.entity_parents.keys() {
        let bare = ranked
            .iter()
            .find_map(|s| s.entities.get(name))
            .is_some_and(|e| e.supertype_expr.is_none());
        if bare {
            if let Some(kids) = children.get(name) {
                combinable.extend(kids.iter().cloned());
            }
        }
    }
    // Corpus-augmented: entities observed as complex-record parts in the corpus
    // (complex_part_count > 0) are genuine combinable leaves, even when the
    // EXPRESS declaration only ONEOF-restricts them (no enclosing AndOr). This
    // recovers real MI combinations the declaration-only rule misses — e.g. the
    // `repositioned_tessellated_geometric_set` complex combines two ONEOF
    // siblings of tessellated_item. The entity_parents guard drops corpus-only
    // measure types (length_measure etc.) that are not schema entities.
    let summary = crate::infer::prune::load_corpus_summary()?;
    for (name, rec) in &summary {
        if rec.complex_part_count > 0 && unified.entity_parents.contains_key(name) {
            combinable.insert(name.clone());
        }
    }
    let mut is_complex_part: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = combinable.into_iter().collect();
    while let Some(n) = stack.pop() {
        if !is_complex_part.insert(n.clone()) {
            continue;
        }
        if let Some(decl) = ranked.iter().find_map(|s| s.entities.get(&n)) {
            for p in &decl.parents {
                stack.push(p.clone());
            }
        }
    }

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
                is_complex_part: is_complex_part.contains(name),
                attr_conflicts,
                derives,
                own_attrs,
                redeclared_attrs,
            },
        );
    }

    // universal = read superset over ALL schemas. SELECT membership is UNIONed
    // across editions (a member legal in any edition must be readable); non-SELECT
    // aliases keep newest-wins. (Older editions can carry members a newer edition
    // dropped — e.g. approved_item has product_definition in ap203 but not ap242.)
    // Iterate newest-first so the union order is newest members then older-only
    // members appended; per-name accumulation makes the result order-independent
    // of the per-schema HashMap iteration.
    let mut select_members: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut other_aliased: BTreeMap<String, String> = BTreeMap::new();
    for s in &ranked {
        for (tn, td) in &s.types {
            match &td.aliased {
                AttrType::Select(members) => {
                    let acc = select_members.entry(tn.clone()).or_default();
                    for m in members {
                        if !acc.contains(m) {
                            acc.push(m.clone());
                        }
                    }
                }
                _ => {
                    other_aliased
                        .entry(tn.clone())
                        .or_insert_with(|| ty_repr(&td.aliased));
                }
            }
        }
    }
    let mut type_aliases: BTreeMap<String, UnivTypeDef> = BTreeMap::new();
    for (tn, members) in select_members {
        type_aliases.insert(
            tn,
            UnivTypeDef {
                aliased: format!("SELECT({})", members.join(", ")),
            },
        );
    }
    // A name that is SELECT in some schema and a plain alias in another keeps the
    // SELECT union (inserted above); only names that are never SELECT land here.
    for (tn, aliased) in other_aliased {
        type_aliases.entry(tn).or_insert(UnivTypeDef { aliased });
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
    let with_derives = doc
        .entity
        .values()
        .filter(|e| !e.derives.is_empty())
        .count();
    eprintln!(
        "wrote {OUT}: {} entities ({with_derives} with derives), {} type aliases",
        doc.entity.len(),
        doc.type_aliases.len()
    );
    Ok(())
}
