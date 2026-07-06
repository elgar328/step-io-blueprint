//! Shared helpers for the faithful exporters (`universal_export`,
//! `profile_export`). Kept here — not in any one exporter — so both share the
//! schema-faithful classification/signal logic.

use crate::express::{AggBounds, AttrType, Schema};

/// Preference for picking an entity's canonical (ordered) own-attribute
/// declaration and TYPE aliases in the faithful union. Higher = preferred.
///
/// AP242 ed2 (the latest IS, and step-io's single output target) is the **base**
/// (highest): shared entities take their ed2 shape, so AP242 ed2 output stays
/// exact. Lower ranks only gap-fill entities the base lacks — ed3 contributes its
/// ed3-only entities (e.g. PMI leader-lines) for reading, then the legacy APs.
/// ed1 is fully covered by ed2 (it never wins anything) but kept in its natural
/// place rather than excluded. When ed3 becomes an IS, promote it to base.
pub(crate) fn schema_rank(label: &str) -> u8 {
    match label {
        "ap242e2" => 6,
        "ap242e3" => 5,
        "ap242e1" => 4,
        "ap214e3" => 3,
        "ap203e2" => 2,
        "ap203e1" => 1,
        _ => 0,
    }
}

/// Lossless, toml-safe string repr of an attribute type. Primitives lowercase
/// (`real`/`integer`/…); a bare token is an entity or TYPE-alias ref;
/// `LIST/SET/BAG/ARRAY OF <inner>`, `OPTIONAL <inner>`, `SELECT(a, b)`,
/// `ENUM(a, b)`. TYPE aliases stay unresolved (faithful; resolving is L2's job).
/// Bound suffix for an aggregation repr: empty for the unbounded default
/// (`[0:?]`, including the EXPRESS bare form) so unbounded attrs keep their
/// historical string; otherwise ` [n:m]` / ` [n:?]`.
fn bounds_repr(b: AggBounds) -> String {
    if b == AggBounds::UNBOUNDED {
        return String::new();
    }
    match b.upper {
        Some(u) => format!(" [{}:{}]", b.lower, u),
        None => format!(" [{}:?]", b.lower),
    }
}

pub(crate) fn ty_repr(ty: &AttrType) -> String {
    match ty {
        AttrType::Primitive(p) => p.to_lowercase(),
        AttrType::Entity(name) => name.clone(),
        AttrType::List(inner, b) => format!("LIST{} OF {}", bounds_repr(*b), ty_repr(inner)),
        AttrType::Set(inner, b) => format!("SET{} OF {}", bounds_repr(*b), ty_repr(inner)),
        AttrType::Bag(inner, b) => format!("BAG{} OF {}", bounds_repr(*b), ty_repr(inner)),
        AttrType::Array(inner, b) => format!("ARRAY{} OF {}", bounds_repr(*b), ty_repr(inner)),
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
