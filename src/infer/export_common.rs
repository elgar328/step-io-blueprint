//! Shared helpers for the faithful exporters (`universal_export`,
//! `profile_export`). Kept here — not in any one exporter — so both share the
//! schema-faithful classification/signal logic.

use crate::express::{AttrType, Schema};

/// Newest → oldest preference for picking an entity's canonical (ordered)
/// own-attribute declaration and TYPE aliases. Higher = preferred. The newest
/// schema carries the most entities and newest attribute shapes — what the
/// faithful union wants (draft-vs-IS only matters for per-AP output profiles).
pub(crate) fn schema_rank(label: &str) -> u8 {
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

/// Lossless, toml-safe string repr of an attribute type. Primitives lowercase
/// (`real`/`integer`/…); a bare token is an entity or TYPE-alias ref;
/// `LIST/SET/BAG/ARRAY OF <inner>`, `OPTIONAL <inner>`, `SELECT(a, b)`,
/// `ENUM(a, b)`. TYPE aliases stay unresolved (faithful; resolving is L2's job).
pub(crate) fn ty_repr(ty: &AttrType) -> String {
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

/// Whether a `SELF\super.attr : ty` redeclaration carries a codegen signal worth
/// emitting into `redeclared_attrs`. Emitted: a **primitive** retype (scalar)
/// and a **SELECT** narrowing — the latter can flip the kind between a synth
/// select (mixed members) and an all-entity bare id, so it must override the
/// inherited type. A bare alias name (`AttrType::Entity`) is resolved against
/// the schema TYPE table to catch alias-form selects (`: foo_select;`). Pure
/// entity→entity narrowings carry no signal (both collapse to a bare id).
pub(crate) fn redeclaration_has_signal(ty: &AttrType, ranked: &[&Schema]) -> bool {
    match ty {
        AttrType::Primitive(_) | AttrType::Select(_) => true,
        AttrType::Entity(name) => ranked
            .iter()
            .find_map(|s| s.types.get(name))
            .is_some_and(|td| matches!(td.aliased, AttrType::Select(_))),
        _ => false,
    }
}
