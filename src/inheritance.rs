//! SUBTYPE chain resolution — walk parent ancestry within a single
//! schema. Helpers used by the catalog classifier and (future) by the
//! check tool's attribute count comparison.

use std::collections::HashSet;

use crate::express::Schema;

/// Total attributes including inherited from all ancestors. Returns
/// `None` when the entity is not present in this schema.
pub fn effective_attr_count(name: &str, schema: &Schema) -> Option<usize> {
    let mut visited = HashSet::new();
    Some(walk_count(name, schema, &mut visited))
}

fn walk_count(name: &str, schema: &Schema, visited: &mut HashSet<String>) -> usize {
    if !visited.insert(name.to_string()) {
        return 0;
    }
    let Some(entity) = schema.entities.get(name) else {
        return 0;
    };
    let mut total = entity.own_attrs.len();
    for parent in &entity.parents {
        total += walk_count(parent, schema, visited);
    }
    total
}

/// Walk SUBTYPE chain to its furthest ancestor (no `parents` left).
/// For multi-parent entities, follows the first parent (inheritance
/// dominance). Returns the entity's own name when it has no parents.
pub fn root_supertype(name: &str, schema: &Schema) -> Option<String> {
    let mut current = name.to_string();
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current.clone()) {
            return Some(current);
        }
        let Some(entity) = schema.entities.get(&current) else {
            return Some(current);
        };
        let Some(parent) = entity.parents.first() else {
            return Some(current);
        };
        current = parent.clone();
    }
}

/// All ancestor names of the given entity (parents, grandparents, ...).
/// Useful for "is this in the X family" checks at classification time.
pub fn ancestors(name: &str, schema: &Schema) -> Vec<String> {
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    walk_ancestors(name, schema, &mut visited, &mut out);
    out
}

fn walk_ancestors(
    name: &str,
    schema: &Schema,
    visited: &mut HashSet<String>,
    out: &mut Vec<String>,
) {
    let Some(entity) = schema.entities.get(name) else {
        return;
    };
    for parent in &entity.parents {
        if visited.insert(parent.clone()) {
            out.push(parent.clone());
            walk_ancestors(parent, schema, visited, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::express::EntitySchema;
    use std::collections::HashMap;

    fn schema_with(entries: &[(&str, &[&str], &[&str])]) -> Schema {
        let mut entities = HashMap::new();
        for (name, parents, attrs) in entries {
            entities.insert(
                (*name).to_string(),
                EntitySchema {
                    name: (*name).to_string(),
                    parents: parents.iter().map(|s| (*s).to_string()).collect(),
                    own_attrs: attrs.iter().map(|s| (*s).to_string()).collect(),
                    is_abstract: false,
                },
            );
        }
        Schema {
            source_label: "test".into(),
            entities,
            parse_warnings: Vec::new(),
        }
    }

    #[test]
    fn cartesian_point_chain() {
        // representation_item.name + (no own) + (no own) + cartesian_point.coordinates = 2
        let s = schema_with(&[
            ("representation_item", &[], &["name"]),
            ("geometric_representation_item", &["representation_item"], &[]),
            ("point", &["geometric_representation_item"], &[]),
            ("cartesian_point", &["point"], &["coordinates"]),
        ]);
        assert_eq!(effective_attr_count("cartesian_point", &s), Some(2));
        assert_eq!(
            root_supertype("cartesian_point", &s).as_deref(),
            Some("representation_item")
        );
        let anc = ancestors("cartesian_point", &s);
        assert_eq!(
            anc,
            vec![
                "point".to_string(),
                "geometric_representation_item".to_string(),
                "representation_item".to_string(),
            ]
        );
    }

    #[test]
    fn unknown_entity_returns_zero() {
        let s = schema_with(&[("foo", &[], &["x"])]);
        assert_eq!(effective_attr_count("nonexistent", &s), Some(0));
    }
}
