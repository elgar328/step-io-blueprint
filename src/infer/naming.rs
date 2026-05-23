//! Naming stage — auto default + manual overrides → IR blueprint.
//!
//! Reads abstract_entities.toml + pools.toml + names.toml + schemas/*.exp,
//! applies the automatic default naming rules, lets manual overrides
//! in names.toml replace any default, and writes the unified IR
//! blueprint to ir.toml. Empty names.toml is valid; unknown entries
//! emit warnings rather than errors.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::express::{self, AttrType, Schema};
use crate::infer::shape::{ConcreteSupertypeShape, EntitySummary};
use crate::infer::variant::VariantSpec;

const FILE_ENTITIES: &str = "abstract_entities.toml";
const FILE_POOLS: &str = "pools.toml";
const FILE_NAMES: &str = "names.toml";
const FILE_IR: &str = "ir.toml";

#[derive(Debug, Default, Deserialize)]
struct NamesFile {
    #[serde(default, rename = "type")]
    type_: BTreeMap<String, String>,
    #[serde(default)]
    id: BTreeMap<String, String>,
    #[serde(default)]
    variant: BTreeMap<String, String>,
    #[serde(default, rename = "enum")]
    enum_: BTreeMap<String, String>,
    #[serde(default)]
    kind_enum: BTreeMap<String, String>,
    /// Key format: `"<entity>.<attr>"` (quoted single key).
    #[serde(default)]
    field: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct PoolsFile {
    #[serde(default)]
    arena: BTreeMap<String, PoolEntry>,
}

#[derive(Debug, Deserialize)]
struct PoolEntry {
    pool: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct IrField {
    name: String,
    ty: String,
    /// EXPRESS entity that *originally declared* this attribute. Lets a
    /// reader tell apart same-named attributes inherited from different
    /// supertypes (e.g. document_file gets `description` from both
    /// `document` and `characterized_object`). Unchanged by a subtype
    /// redeclaration that only narrows the type.
    from: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct IrEntity {
    kind: String,
    arena: String,
    pool: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    shape: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "type")]
    type_: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "enum")]
    enum_: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind_enum: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enum_of: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    into: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    as_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    chain: Vec<String>,

    /// STEP P21 encoding order: every supertype's attributes (parent
    /// chains left-to-right, base ancestor first) then own attributes.
    /// A `Vec` (not a name-keyed map) so order is preserved and
    /// same-named attributes from different supertypes both survive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fields: Vec<IrField>,

    #[serde(default, skip_serializing_if = "is_zero")]
    instance_count: usize,

    /// Subset of `instance_count` that came from complex MI instances
    /// (`#N=( ... NAME(...) ... );`). If equal to `instance_count` the
    /// entity is corpus complex-part-only; if 0 it's standalone-only.
    /// See `co_instantiated_with` for the leaf-set companions.
    #[serde(default, skip_serializing_if = "is_zero")]
    complex_part_count: usize,

    /// Other entities seen in the same complex MI block as this one,
    /// anywhere in the corpus (sorted). Empty when never seen in a
    /// complex block. step-io uses this to scope a
    /// `#[step_entity_complex(required=[...])]` handler.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    co_instantiated_with: Vec<String>,

    // Reshape stage metadata — present only when the entity is a
    // split / merge product. step-io reads ir.toml as the single
    // reference and uses these to understand abstraction provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    split_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    split_context: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    merge_absorbs: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    fields_union: bool,
    /// Rationale for the abstraction. Present on the primary entity
    /// (split first variant, merge target) only; virtual variants
    /// follow split_from to find it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasons: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

pub fn run() -> Result<(), String> {
    let entities: BTreeMap<String, EntitySummary> =
        crate::infer::io::read_confident(FILE_ENTITIES, "entity")
            .map_err(|e| format!("read {FILE_ENTITIES}: {e}"))?;
    if entities.is_empty() {
        return Err(format!(
            "{FILE_ENTITIES} is empty or missing — run `infer reshape` first."
        ));
    }

    let pools_path = Path::new("inferred").join(FILE_POOLS);
    let pools_body = fs::read_to_string(&pools_path)
        .map_err(|e| format!("read {pools_path:?}: {e}"))?;
    let pools_file: PoolsFile = toml::from_str(&pools_body)
        .map_err(|e| format!("parse {pools_path:?}: {e}"))?;

    let names = load_names()?;

    let schemas = express::load_all_schemas(Path::new("schemas"));
    if schemas.is_empty() {
        return Err("no schemas loaded — check schemas/*.exp.".into());
    }

    for w in validate_stale(&names, &entities) {
        eprintln!("warning: {w}");
    }

    let ir = compile_ir(&entities, &pools_file.arena, &names, &schemas)?;
    write_ir_toml(&ir)?;
    eprintln!("infer naming: wrote {FILE_IR} ({} entities)", ir.len());
    Ok(())
}

fn load_names() -> Result<NamesFile, String> {
    let path = Path::new("inferred").join(FILE_NAMES);
    if !path.exists() {
        return Ok(NamesFile::default());
    }
    let body = fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    toml::from_str(&body).map_err(|e| format!("parse {path:?}: {e}"))
}

/// Word-level acronyms. A snake_case word matching one of these keys
/// is emitted as the mapped form instead of the default capitalize-
/// first-letter rule.
const KNOWN_ACRONYMS: &[(&str, &str)] = &[
    ("pcurve", "PCurve"),
    ("rgb", "RGB"),
];

fn snake_to_pascal(s: &str) -> String {
    s.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            if let Some((_, mapped)) = KNOWN_ACRONYMS.iter().find(|(k, _)| *k == w) {
                return (*mapped).to_string();
            }
            let mut c = w.chars();
            match c.next() {
                Some(h) => h.to_uppercase().chain(c).collect::<String>(),
                None => String::new(),
            }
        })
        .collect()
}

/// Categories an entity supports — derived from its VariantSpec / shape.
struct Categories {
    has_type: bool,
    has_id: bool,
    has_variant: bool,
    has_enum: bool,
    has_kind_enum: bool,
    has_fields: bool,
}

fn categories_for(summary: &EntitySummary) -> Categories {
    use VariantSpec::*;
    match &summary.variant {
        SingleStruct => Categories {
            has_type: true,
            has_id: true,
            has_variant: false,
            has_enum: false,
            has_kind_enum: false,
            has_fields: true,
        },
        InEnum { .. } => Categories {
            has_type: true,
            has_id: true,
            has_variant: true,
            has_enum: false,
            has_kind_enum: false,
            has_fields: true,
        },
        EnumBase { .. } => Categories {
            has_type: false,
            has_id: true,
            has_variant: false,
            has_enum: true,
            has_kind_enum: false,
            has_fields: false,
        },
        ConcreteSupertype => match summary.shape {
            Some(ConcreteSupertypeShape::Carrier) => Categories {
                has_type: true,
                has_id: true,
                has_variant: true,
                has_enum: true,
                has_kind_enum: false,
                has_fields: true,
            },
            Some(ConcreteSupertypeShape::BaseParallel) => Categories {
                has_type: true,
                has_id: true,
                has_variant: true,
                has_enum: false,
                has_kind_enum: true,
                has_fields: true,
            },
            None => Categories {
                has_type: true,
                has_id: true,
                has_variant: false,
                has_enum: false,
                has_kind_enum: false,
                has_fields: true,
            },
        },
        ComplexSupertype { .. } | CompositeOneOf { .. } => Categories {
            has_type: true,
            has_id: true,
            has_variant: false,
            has_enum: true,
            has_kind_enum: false,
            has_fields: true,
        },
        NestedField { .. } => Categories {
            has_type: false,
            has_id: false,
            has_variant: false,
            has_enum: false,
            has_kind_enum: false,
            has_fields: true,
        },
        MergedInto { .. } => Categories {
            has_type: false,
            has_id: false,
            has_variant: false,
            has_enum: false,
            has_kind_enum: false,
            has_fields: false,
        },
    }
}

fn kind_str(spec: &VariantSpec) -> &'static str {
    use VariantSpec::*;
    match spec {
        SingleStruct => "single_struct",
        InEnum { .. } => "in_enum",
        EnumBase { .. } => "enum_base",
        ConcreteSupertype => "concrete_supertype",
        ComplexSupertype { .. } => "complex_supertype",
        CompositeOneOf { .. } => "composite_one_of",
        NestedField { .. } => "nested_field",
        MergedInto { .. } => "merged_into",
    }
}

fn enum_of_for(spec: &VariantSpec) -> Option<String> {
    if let VariantSpec::InEnum { enum_name } = spec {
        Some(enum_name.clone())
    } else {
        None
    }
}

fn id_arena_for<'a>(
    summary: &'a EntitySummary,
    arena_id_lookup: &HashMap<String, String>,
) -> Option<String> {
    arena_id_lookup.get(&summary.arena).cloned()
}

fn validate_stale(
    names: &NamesFile,
    entities: &BTreeMap<String, EntitySummary>,
) -> Vec<String> {
    let mut out = Vec::new();
    let check_entity_category =
        |entity: &str, category: &str, has: fn(&Categories) -> bool, warnings: &mut Vec<String>| {
            match entities.get(entity) {
                None => warnings.push(format!(
                    "{FILE_NAMES} [{category}.{entity}] — entity not in {FILE_ENTITIES}"
                )),
                Some(s) => {
                    let cats = categories_for(s);
                    if !has(&cats) {
                        warnings.push(format!(
                            "{FILE_NAMES} [{category}.{entity}] — entity is {} (no {category})",
                            kind_str(&s.variant)
                        ));
                    }
                }
            }
        };

    for k in names.type_.keys() {
        check_entity_category(k, "type", |c| c.has_type, &mut out);
    }
    for k in names.id.keys() {
        check_entity_category(k, "id", |c| c.has_id, &mut out);
    }
    for k in names.variant.keys() {
        check_entity_category(k, "variant", |c| c.has_variant, &mut out);
    }
    for k in names.enum_.keys() {
        check_entity_category(k, "enum", |c| c.has_enum, &mut out);
    }
    for k in names.kind_enum.keys() {
        check_entity_category(k, "kind_enum", |c| c.has_kind_enum, &mut out);
    }
    for k in names.field.keys() {
        let (entity, attr) = match k.split_once('.') {
            Some(p) => p,
            None => {
                out.push(format!(
                    "{FILE_NAMES} [field.\"{k}\"] — key must be \"<entity>.<attr>\""
                ));
                continue;
            }
        };
        match entities.get(entity) {
            None => out.push(format!(
                "{FILE_NAMES} [field.\"{k}\"] — entity {entity} not in {FILE_ENTITIES}"
            )),
            Some(s) if !categories_for(s).has_fields => out.push(format!(
                "{FILE_NAMES} [field.\"{k}\"] — entity {entity} is {} (no fields)",
                kind_str(&s.variant)
            )),
            _ => {
                // attr existence is checked by attr_type lookup later;
                // here we only catch obvious entity-level mistakes.
                let _ = attr;
            }
        }
    }
    out
}

/// Resolve the type name for a TYPE alias that points to another type.
/// Bounded by a depth limit to defend against accidental cycles.
fn resolve_alias<'a>(name: &'a str, types: &'a HashMap<String, AttrType>) -> Option<&'a AttrType> {
    let mut current = name;
    for _ in 0..32 {
        let aliased = types.get(current)?;
        match aliased {
            AttrType::Entity(next) if types.contains_key(next) => {
                current = next;
            }
            other => return Some(other),
        }
    }
    None
}

fn ty_string(ty: &AttrType, types: &HashMap<String, AttrType>) -> String {
    match ty {
        AttrType::Primitive(p) => match p.to_lowercase().as_str() {
            "boolean" => "bool".to_string(),
            other => other.to_string(),
        },
        AttrType::Entity(name) => {
            // If the name resolves to a TYPE alias, unfold it.
            if let Some(resolved) = resolve_alias(name, types) {
                if !matches!(resolved, AttrType::Entity(n) if n == name) {
                    return ty_string(resolved, types);
                }
            }
            format!("ref_{name}")
        }
        AttrType::List(inner) => format!("list_{}", ty_string(inner, types)),
        AttrType::Set(inner) => format!("set_{}", ty_string(inner, types)),
        AttrType::Bag(inner) => format!("bag_{}", ty_string(inner, types)),
        AttrType::Array(inner) => format!("array_{}", ty_string(inner, types)),
        AttrType::Optional(inner) => format!("opt_{}", ty_string(inner, types)),
        AttrType::Enumeration(_) => "enum".to_string(),
        AttrType::Select(_) => "select".to_string(),
    }
}

/// One attribute slot in STEP P21 encoding order.
struct FieldRec {
    name: String,
    ty: String,
    /// EXPRESS entity that originally declared the attribute.
    from: String,
}

/// Apply a redeclaration (subtype attr type narrowing) to the ordered
/// field list: find the existing slot by name and overwrite its `ty`.
/// `from` is left unchanged — the slot still belongs to its original
/// declarer. Falls back to appending if the name is absent (abnormal
/// schema; preserves the old map-insert behaviour rather than dropping).
fn apply_redeclaration(out: &mut Vec<FieldRec>, name: &str, ty: String, redeclarer: &str) {
    if let Some(f) = out.iter_mut().find(|f| f.name == name) {
        f.ty = ty;
    } else {
        out.push(FieldRec {
            name: name.to_string(),
            ty,
            from: redeclarer.to_string(),
        });
    }
}

/// Build a per-entity ordered attribute list, pulling in inherited attrs
/// from *every* parent of the entity's inheritance DAG (multiple
/// inheritance included). Order follows STEP P21 encoding: each parent
/// chain (base ancestor first), parents left-to-right, then own attrs.
/// Redeclarations narrow an inherited slot in place.
fn build_attr_types(schemas: &[Schema]) -> HashMap<String, Vec<FieldRec>> {
    let mut entity_to_schema: HashMap<&str, &express::EntitySchema> = HashMap::new();
    let mut all_types: HashMap<String, AttrType> = HashMap::new();
    for s in schemas {
        for (name, e) in &s.entities {
            entity_to_schema.entry(name.as_str()).or_insert(e);
        }
        for (name, t) in &s.types {
            all_types.entry(name.clone()).or_insert(t.aliased.clone());
        }
    }

    let mut out: HashMap<String, Vec<FieldRec>> = HashMap::new();
    for (entity_name, entity) in &entity_to_schema {
        let mut fields: Vec<FieldRec> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        collect_ancestor_attrs(
            entity,
            &entity_to_schema,
            &all_types,
            &mut fields,
            &mut visited,
        );
        for a in &entity.own_attrs {
            fields.push(FieldRec {
                name: a.name.clone(),
                ty: ty_string(&a.ty, &all_types),
                from: (*entity_name).to_string(),
            });
        }
        // Redeclarations narrow an inherited slot — apply last.
        for a in &entity.redeclared_attrs {
            apply_redeclaration(
                &mut fields,
                &a.name,
                ty_string(&a.ty, &all_types),
                entity_name,
            );
        }
        out.insert((*entity_name).to_string(), fields);
    }
    out
}

/// Recursively gather attrs from *all* parents of `entity` (multiple
/// inheritance included), appending in STEP P21 order. `visited` guards
/// against re-walking a shared ancestor reached via several paths — in a
/// diamond the ancestor's attrs are already in `out`, so skipping is
/// correctness-neutral and avoids exponential re-traversal.
fn collect_ancestor_attrs(
    entity: &express::EntitySchema,
    entity_to_schema: &HashMap<&str, &express::EntitySchema>,
    types: &HashMap<String, AttrType>,
    out: &mut Vec<FieldRec>,
    visited: &mut HashSet<String>,
) {
    for parent in &entity.parents {
        if !visited.insert(parent.clone()) {
            continue;
        }
        let parent_entity = match entity_to_schema.get(parent.as_str()) {
            Some(e) => e,
            None => continue,
        };
        collect_ancestor_attrs(parent_entity, entity_to_schema, types, out, visited);
        for a in &parent_entity.own_attrs {
            out.push(FieldRec {
                name: a.name.clone(),
                ty: ty_string(&a.ty, types),
                from: parent.clone(),
            });
        }
        for a in &parent_entity.redeclared_attrs {
            apply_redeclaration(out, &a.name, ty_string(&a.ty, types), parent);
        }
    }
}

/// Build arena → arena-id-name lookup. Default = `<arena>` snake →
/// PascalCase + "Id". Manual `[id]` overrides on the entity that owns
/// the arena (group key) replace this default.
fn build_arena_id_lookup(
    entities: &BTreeMap<String, EntitySummary>,
    names: &NamesFile,
) -> HashMap<String, String> {
    let mut arenas: BTreeSet<String> = entities.values().map(|s| s.arena.clone()).collect();
    // Find any `[id] X = "..."` whose X happens to also be the arena
    // name and let it override. (Most arenas equal their group/entity
    // name, so this lines up naturally.)
    let mut out = HashMap::new();
    for arena in arenas.iter() {
        let default = format!("{}Id", snake_to_pascal(arena));
        let chosen = names.id.get(arena).cloned().unwrap_or(default);
        out.insert(arena.clone(), chosen);
    }
    arenas.clear();
    out
}

fn compile_ir(
    entities: &BTreeMap<String, EntitySummary>,
    pools: &BTreeMap<String, PoolEntry>,
    names: &NamesFile,
    schemas: &[Schema],
) -> Result<BTreeMap<String, IrEntity>, String> {
    let attr_types = build_attr_types(schemas);
    let arena_id_lookup = build_arena_id_lookup(entities, names);

    let mut out = BTreeMap::new();
    for (entity, summary) in entities {
        let cats = categories_for(summary);

        let pool = pools
            .get(&summary.arena)
            .ok_or_else(|| format!("arena {} missing in {FILE_POOLS}", summary.arena))?
            .pool
            .clone();

        let auto_pascal = snake_to_pascal(entity);
        let auto_type = match (&summary.variant, summary.shape) {
            (VariantSpec::ConcreteSupertype, Some(ConcreteSupertypeShape::Carrier)) => {
                format!("{auto_pascal}Data")
            }
            _ => auto_pascal.clone(),
        };

        let type_ = if cats.has_type {
            Some(
                names
                    .type_
                    .get(entity)
                    .cloned()
                    .unwrap_or_else(|| auto_type.clone()),
            )
        } else {
            None
        };

        let id = if cats.has_id {
            Some(arena_id_lookup.get(&summary.arena).cloned().unwrap_or_else(
                || format!("{}Id", snake_to_pascal(&summary.arena)),
            ))
        } else {
            None
        };

        let variant = if cats.has_variant {
            // For Carrier the variant default is "Itself"; for BaseParallel
            // it's "Plain"; for InEnum it's the entity's PascalCase name.
            let default = match (&summary.variant, summary.shape) {
                (VariantSpec::ConcreteSupertype, Some(ConcreteSupertypeShape::Carrier)) => {
                    "Itself".to_string()
                }
                (VariantSpec::ConcreteSupertype, Some(ConcreteSupertypeShape::BaseParallel)) => {
                    "Plain".to_string()
                }
                _ => auto_pascal.clone(),
            };
            Some(names.variant.get(entity).cloned().unwrap_or(default))
        } else {
            None
        };

        let enum_ = if cats.has_enum {
            Some(
                names
                    .enum_
                    .get(entity)
                    .cloned()
                    .unwrap_or_else(|| auto_pascal.clone()),
            )
        } else {
            None
        };

        let kind_enum = if cats.has_kind_enum {
            Some(
                names
                    .kind_enum
                    .get(entity)
                    .cloned()
                    .unwrap_or_else(|| format!("{auto_pascal}Kind")),
            )
        } else {
            None
        };

        let enum_of = enum_of_for(&summary.variant);

        let (into, as_field, target, chain) = match &summary.variant {
            VariantSpec::NestedField {
                into,
                as_field,
                ..
            } => (Some(into.clone()), Some(as_field.clone()), None, Vec::new()),
            VariantSpec::MergedInto { target, chain } => {
                (None, None, Some(target.clone()), chain.clone())
            }
            _ => (None, None, None, Vec::new()),
        };

        let mut fields: Vec<IrField> = Vec::new();
        if cats.has_fields {
            if let Some(attrs) = attr_types.get(entity) {
                for rec in attrs {
                    // Field rename keyed by `entity.attr`. A same-name
                    // collision shares the key, so a rename applies to
                    // both colliding slots — acceptable; tighten to
                    // `entity.from.attr` only if precise rename is ever
                    // needed.
                    let key = format!("{entity}.{}", rec.name);
                    let renamed =
                        names.field.get(&key).cloned().unwrap_or_else(|| rec.name.clone());
                    fields.push(IrField {
                        name: renamed,
                        ty: rec.ty.clone(),
                        from: rec.from.clone(),
                    });
                }
            }
        }

        let shape = summary.shape.map(|s| match s {
            ConcreteSupertypeShape::Carrier => "carrier".to_string(),
            ConcreteSupertypeShape::BaseParallel => "base_parallel".to_string(),
        });

        out.insert(
            entity.clone(),
            IrEntity {
                kind: kind_str(&summary.variant).to_string(),
                arena: summary.arena.clone(),
                pool,
                shape,
                type_,
                id,
                variant,
                enum_,
                kind_enum,
                enum_of,
                into,
                as_field,
                target,
                chain,
                fields,
                instance_count: summary.instance_count,
                complex_part_count: summary.complex_part_count,
                co_instantiated_with: summary.co_instantiated_with.clone(),
                split_from: summary.split_from.clone(),
                split_context: summary.split_context.clone(),
                merge_absorbs: summary.merge_absorbs.clone(),
                fields_union: summary.fields_union,
                reasons: summary.reasons.clone(),
            },
        );
    }
    Ok(out)
}

fn write_ir_toml(ir: &BTreeMap<String, IrEntity>) -> Result<(), String> {
    let mut outer: BTreeMap<&str, &BTreeMap<String, IrEntity>> = BTreeMap::new();
    outer.insert("entity", ir);
    let body = toml::to_string_pretty(&outer)
        .map_err(|e| format!("serialize {FILE_IR}: {e}"))?;
    let header = "# Generated by `infer naming`. Do not edit manually.\n\
                  # Single source of truth for step-io codegen.\n\
                  # Inputs: abstract_entities.toml + pools.toml + names.toml + schemas/*.exp\n\n";
    fs::write(
        Path::new("inferred").join(FILE_IR),
        format!("{header}{body}"),
    )
    .map_err(|e| format!("write {FILE_IR}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_to_pascal_basic() {
        assert_eq!(snake_to_pascal("b_spline_curve"), "BSplineCurve");
        assert_eq!(snake_to_pascal("cartesian_point"), "CartesianPoint");
        assert_eq!(snake_to_pascal("line"), "Line");
    }

    #[test]
    fn snake_to_pascal_handles_double_underscores() {
        assert_eq!(snake_to_pascal("foo__bar"), "FooBar");
        assert_eq!(snake_to_pascal("__leading_trailing__"), "LeadingTrailing");
    }

    #[test]
    fn snake_to_pascal_known_acronym_pcurve() {
        assert_eq!(snake_to_pascal("pcurve"), "PCurve");
        assert_eq!(snake_to_pascal("bounded_pcurve"), "BoundedPCurve");
    }

    #[test]
    fn snake_to_pascal_known_acronym_rgb() {
        assert_eq!(snake_to_pascal("colour_rgb"), "ColourRGB");
    }

    #[test]
    fn ty_string_primitives_and_refs() {
        let types = HashMap::new();
        assert_eq!(ty_string(&AttrType::Primitive("REAL".into()), &types), "real");
        assert_eq!(ty_string(&AttrType::Primitive("BOOLEAN".into()), &types), "bool");
        assert_eq!(
            ty_string(&AttrType::Entity("cartesian_point".into()), &types),
            "ref_cartesian_point"
        );
    }

    #[test]
    fn ty_string_aggregates_and_optional() {
        let types = HashMap::new();
        let list_real = AttrType::List(Box::new(AttrType::Primitive("REAL".into())));
        assert_eq!(ty_string(&list_real, &types), "list_real");
        let opt_ref = AttrType::Optional(Box::new(AttrType::Entity("line".into())));
        assert_eq!(ty_string(&opt_ref, &types), "opt_ref_line");
    }

    #[test]
    fn ty_string_unfolds_type_alias_to_primitive() {
        let mut types = HashMap::new();
        types.insert(
            "length_measure".to_string(),
            AttrType::Primitive("REAL".into()),
        );
        let ty = AttrType::Entity("length_measure".into());
        assert_eq!(ty_string(&ty, &types), "real");
    }

    fn specs(attrs: &[(&str, AttrType)]) -> Vec<express::AttrSpec> {
        attrs
            .iter()
            .map(|(n, t)| express::AttrSpec {
                name: n.to_string(),
                ty: t.clone(),
            })
            .collect()
    }

    /// ty of the first field named `name` in an ordered FieldRec list.
    fn fty<'a>(fields: &'a [FieldRec], name: &str) -> Option<&'a str> {
        fields.iter().find(|f| f.name == name).map(|f| f.ty.as_str())
    }

    /// Ordered list of field names — for asserting STEP P21 order.
    fn fnames(fields: &[FieldRec]) -> Vec<&str> {
        fields.iter().map(|f| f.name.as_str()).collect()
    }

    fn ent(name: &str, parents: &[&str], attrs: &[(&str, AttrType)]) -> express::EntitySchema {
        ent_redecl(name, parents, attrs, &[])
    }

    fn ent_redecl(
        name: &str,
        parents: &[&str],
        attrs: &[(&str, AttrType)],
        redeclared: &[(&str, AttrType)],
    ) -> express::EntitySchema {
        express::EntitySchema {
            name: name.to_string(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            own_attrs: specs(attrs),
            redeclared_attrs: specs(redeclared),
            is_abstract: false,
            supertype_expr: None,
        }
    }

    fn schema_of(entities: Vec<express::EntitySchema>) -> Schema {
        Schema {
            source_label: "test".to_string(),
            entities: entities.into_iter().map(|e| (e.name.clone(), e)).collect(),
            types: HashMap::new(),
            parse_warnings: Vec::new(),
        }
    }

    #[test]
    fn build_attr_types_single_inheritance() {
        let schemas = vec![schema_of(vec![
            ent("parent", &[], &[("name", AttrType::Primitive("STRING".into()))]),
            ent(
                "child",
                &["parent"],
                &[("coords", AttrType::Primitive("REAL".into()))],
            ),
        ])];
        let attrs = build_attr_types(&schemas);
        let child = attrs.get("child").expect("child present");
        assert_eq!(fty(child, "name"), Some("string"));
        assert_eq!(fty(child, "coords"), Some("real"));
        // STEP P21 order: inherited parent attr before own attr.
        assert_eq!(fnames(child), vec!["name", "coords"]);
    }

    #[test]
    fn build_attr_types_multiple_inheritance_collects_all_parents() {
        // child SUBTYPE OF (a, b): both parents' own_attrs must surface.
        let schemas = vec![schema_of(vec![
            ent("a", &[], &[("name", AttrType::Primitive("STRING".into()))]),
            ent("b", &[], &[("value", AttrType::Primitive("NUMBER".into()))]),
            ent("child", &["a", "b"], &[]),
        ])];
        let attrs = build_attr_types(&schemas);
        let child = attrs.get("child").expect("child present");
        assert_eq!(fty(child, "name"), Some("string"));
        assert_eq!(
            fty(child, "value"),
            Some("number"),
            "2nd-parent attr must not be dropped"
        );
        // parent `a` chain before parent `b` chain.
        assert_eq!(fnames(child), vec!["name", "value"]);
    }

    #[test]
    fn build_attr_types_multiple_inheritance_walks_2nd_parent_chain() {
        // child SUBTYPE OF (a, b); b SUBTYPE OF (grandparent).
        // The grandparent's attr (reached only via the 2nd parent) counts.
        let schemas = vec![schema_of(vec![
            ent("a", &[], &[("name", AttrType::Primitive("STRING".into()))]),
            ent(
                "grandparent",
                &[],
                &[("the_value", AttrType::Primitive("NUMBER".into()))],
            ),
            ent("b", &["grandparent"], &[]),
            ent("child", &["a", "b"], &[]),
        ])];
        let attrs = build_attr_types(&schemas);
        let child = attrs.get("child").expect("child present");
        assert_eq!(fty(child, "name"), Some("string"));
        assert_eq!(
            fty(child, "the_value"),
            Some("number"),
            "attr inherited via 2nd parent's grandparent must surface"
        );
    }

    #[test]
    fn build_attr_types_redeclaration_overrides_direct_parent_attr() {
        // child redeclares its direct parent's attr with a narrower type
        // via redeclared_attrs (the EXPRESS `SELF\parent.attr` form).
        let schemas = vec![schema_of(vec![
            ent(
                "parent",
                &[],
                &[("the_value", AttrType::Primitive("NUMBER".into()))],
            ),
            ent_redecl(
                "child",
                &["parent"],
                &[],
                &[("the_value", AttrType::Primitive("INTEGER".into()))],
            ),
        ])];
        let attrs = build_attr_types(&schemas);
        let child = attrs.get("child").expect("child present");
        assert_eq!(
            fty(child, "the_value"),
            Some("integer"),
            "redeclaration must narrow the inherited slot in place"
        );
        assert_eq!(
            fnames(child),
            vec!["the_value"],
            "redeclaration narrows in place — no extra slot"
        );
    }

    #[test]
    fn build_attr_types_redeclared_attr_narrows_inherited_via_2nd_parent() {
        // child SUBTYPE OF (a, b); b SUBTYPE OF (grandparent).
        // grandparent declares the_value : NUMBER; b redeclares it INTEGER.
        // The narrowed type must reach `child`.
        let schemas = vec![schema_of(vec![
            ent("a", &[], &[("name", AttrType::Primitive("STRING".into()))]),
            ent(
                "grandparent",
                &[],
                &[("the_value", AttrType::Primitive("NUMBER".into()))],
            ),
            ent_redecl(
                "b",
                &["grandparent"],
                &[],
                &[("the_value", AttrType::Primitive("INTEGER".into()))],
            ),
            ent("child", &["a", "b"], &[]),
        ])];
        let attrs = build_attr_types(&schemas);
        let b = attrs.get("b").expect("b present");
        assert_eq!(
            fty(b, "the_value"),
            Some("integer"),
            "redeclaration must override the inherited NUMBER on b itself"
        );
        // redeclaration narrows in place — no extra slot.
        assert_eq!(fnames(b), vec!["the_value"]);
        let child = attrs.get("child").expect("child present");
        assert_eq!(
            fty(child, "the_value"),
            Some("integer"),
            "redeclaration on 2nd parent must propagate to the child"
        );
        assert_eq!(fty(child, "name"), Some("string"));
    }

    #[test]
    fn build_attr_types_same_name_collision_keeps_both_slots() {
        // child SUBTYPE OF (a, b): a and b are unrelated entities that
        // each declare an attr named `label`. STEP P21 has two slots.
        let schemas = vec![schema_of(vec![
            ent("a", &[], &[("label", AttrType::Primitive("STRING".into()))]),
            ent("b", &[], &[("label", AttrType::Primitive("STRING".into()))]),
            ent("child", &["a", "b"], &[]),
        ])];
        let attrs = build_attr_types(&schemas);
        let child = attrs.get("child").expect("child present");
        assert_eq!(
            fnames(child),
            vec!["label", "label"],
            "same-name attrs from distinct ancestors must both survive"
        );
        assert_eq!(child[0].from, "a");
        assert_eq!(child[1].from, "b");
    }

    fn make_summary(variant: VariantSpec, arena: &str, shape: Option<ConcreteSupertypeShape>) -> EntitySummary {
        EntitySummary {
            variant,
            group: arena.to_string(),
            arena: arena.to_string(),
            shape,
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

    fn pools_with(pairs: &[(&str, &str)]) -> BTreeMap<String, PoolEntry> {
        pairs
            .iter()
            .map(|(arena, pool)| {
                (
                    arena.to_string(),
                    PoolEntry {
                        pool: pool.to_string(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn compile_ir_single_struct() {
        let mut entities = BTreeMap::new();
        entities.insert(
            "cartesian_point".to_string(),
            make_summary(VariantSpec::SingleStruct, "cartesian_point", None),
        );
        let pools = pools_with(&[("cartesian_point", "geometry")]);
        let names = NamesFile::default();
        let schemas: Vec<Schema> = Vec::new();

        let ir = compile_ir(&entities, &pools, &names, &schemas).unwrap();
        let row = &ir["cartesian_point"];
        assert_eq!(row.kind, "single_struct");
        assert_eq!(row.pool, "geometry");
        assert_eq!(row.type_.as_deref(), Some("CartesianPoint"));
        assert_eq!(row.id.as_deref(), Some("CartesianPointId"));
        assert!(row.variant.is_none());
        assert!(row.enum_.is_none());
        assert!(row.kind_enum.is_none());
    }

    #[test]
    fn compile_ir_in_enum_uses_arena_id_and_enum_of() {
        let mut entities = BTreeMap::new();
        entities.insert(
            "line".to_string(),
            make_summary(
                VariantSpec::InEnum {
                    enum_name: "curve".into(),
                },
                "curve",
                None,
            ),
        );
        entities.insert(
            "curve".to_string(),
            make_summary(
                VariantSpec::EnumBase {
                    enum_name: "curve".into(),
                },
                "curve",
                None,
            ),
        );
        let pools = pools_with(&[("curve", "geometry")]);
        let names = NamesFile::default();
        let schemas: Vec<Schema> = Vec::new();

        let ir = compile_ir(&entities, &pools, &names, &schemas).unwrap();

        let line = &ir["line"];
        assert_eq!(line.type_.as_deref(), Some("Line"));
        assert_eq!(line.id.as_deref(), Some("CurveId"));
        assert_eq!(line.enum_of.as_deref(), Some("curve"));
        assert_eq!(line.variant.as_deref(), Some("Line"));

        let curve = &ir["curve"];
        assert_eq!(curve.kind, "enum_base");
        assert!(curve.type_.is_none());
        assert_eq!(curve.enum_.as_deref(), Some("Curve"));
        assert_eq!(curve.id.as_deref(), Some("CurveId"));
    }

    #[test]
    fn compile_ir_carrier_has_data_suffix_and_itself_variant() {
        let mut entities = BTreeMap::new();
        entities.insert(
            "face_bound".to_string(),
            make_summary(
                VariantSpec::ConcreteSupertype,
                "face_bound",
                Some(ConcreteSupertypeShape::Carrier),
            ),
        );
        let pools = pools_with(&[("face_bound", "topology")]);
        let names = NamesFile::default();
        let schemas: Vec<Schema> = Vec::new();

        let ir = compile_ir(&entities, &pools, &names, &schemas).unwrap();
        let row = &ir["face_bound"];
        assert_eq!(row.shape.as_deref(), Some("carrier"));
        assert_eq!(row.type_.as_deref(), Some("FaceBoundData"));
        assert_eq!(row.enum_.as_deref(), Some("FaceBound"));
        assert_eq!(row.variant.as_deref(), Some("Itself"));
    }

    #[test]
    fn compile_ir_base_parallel_has_kind_enum_and_plain_variant() {
        let mut entities = BTreeMap::new();
        entities.insert(
            "styled_item".to_string(),
            make_summary(
                VariantSpec::ConcreteSupertype,
                "styled_item",
                Some(ConcreteSupertypeShape::BaseParallel),
            ),
        );
        let pools = pools_with(&[("styled_item", "visualization")]);
        let names = NamesFile::default();
        let schemas: Vec<Schema> = Vec::new();

        let ir = compile_ir(&entities, &pools, &names, &schemas).unwrap();
        let row = &ir["styled_item"];
        assert_eq!(row.shape.as_deref(), Some("base_parallel"));
        assert_eq!(row.type_.as_deref(), Some("StyledItem"));
        assert_eq!(row.kind_enum.as_deref(), Some("StyledItemKind"));
        assert!(row.enum_.is_none());
        assert_eq!(row.variant.as_deref(), Some("Plain"));
    }

    #[test]
    fn compile_ir_override_replaces_default() {
        let mut entities = BTreeMap::new();
        entities.insert(
            "b_spline_curve".to_string(),
            make_summary(VariantSpec::SingleStruct, "b_spline_curve", None),
        );
        let pools = pools_with(&[("b_spline_curve", "geometry")]);
        let mut names = NamesFile::default();
        names
            .type_
            .insert("b_spline_curve".to_string(), "BSpline".to_string());
        let schemas: Vec<Schema> = Vec::new();

        let ir = compile_ir(&entities, &pools, &names, &schemas).unwrap();
        assert_eq!(ir["b_spline_curve"].type_.as_deref(), Some("BSpline"));
    }

    #[test]
    fn validate_stale_unknown_entity() {
        let entities: BTreeMap<String, EntitySummary> = BTreeMap::new();
        let mut names = NamesFile::default();
        names
            .type_
            .insert("ghost_entity".to_string(), "Ghost".to_string());
        let warns = validate_stale(&names, &entities);
        assert!(warns.iter().any(|w| w.contains("ghost_entity")));
    }

    #[test]
    fn validate_stale_kind_mismatch() {
        let mut entities = BTreeMap::new();
        entities.insert(
            "curve".to_string(),
            make_summary(
                VariantSpec::EnumBase {
                    enum_name: "curve".into(),
                },
                "curve",
                None,
            ),
        );
        let mut names = NamesFile::default();
        names.type_.insert("curve".to_string(), "Curve".to_string());
        let warns = validate_stale(&names, &entities);
        assert!(warns.iter().any(|w| w.contains("enum_base")));
    }

    #[test]
    fn ir_entity_toml_round_trip() {
        let fields = vec![IrField {
            name: "coordinates".to_string(),
            ty: "list_real".to_string(),
            from: "cartesian_point".to_string(),
        }];
        let row = IrEntity {
            kind: "single_struct".to_string(),
            arena: "cartesian_point".to_string(),
            pool: "geometry".to_string(),
            shape: None,
            type_: Some("CartesianPoint".to_string()),
            id: Some("CartesianPointId".to_string()),
            variant: None,
            enum_: None,
            kind_enum: None,
            enum_of: None,
            into: None,
            as_field: None,
            target: None,
            chain: Vec::new(),
            fields,
            instance_count: 5,
            complex_part_count: 0,
            co_instantiated_with: Vec::new(),
            split_from: None,
            split_context: None,
            merge_absorbs: Vec::new(),
            fields_union: false,
            reasons: None,
        };

        let mut ir = BTreeMap::new();
        ir.insert("cartesian_point".to_string(), row);

        let mut outer: BTreeMap<&str, &BTreeMap<String, IrEntity>> = BTreeMap::new();
        outer.insert("entity", &ir);
        let body = toml::to_string_pretty(&outer).unwrap();

        #[derive(Deserialize)]
        struct Outer {
            entity: BTreeMap<String, IrEntity>,
        }
        let parsed: Outer = toml::from_str(&body).unwrap();
        assert_eq!(parsed.entity, ir);
    }
}
