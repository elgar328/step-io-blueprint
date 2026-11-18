//! Which attribute redeclarations (`SELF\super.attr : narrower;`) the universal
//! export may apply without rejecting input that some edition allows.
//!
//! The universal schema is read as the union of all editions, so a narrowing
//! taken from the newest declaration is safe only if every edition's own
//! effective type for that attribute — on the redeclaring entity and on every
//! subtype of it — already falls inside it. Admitted sets are compared as sets
//! of names: entities (with their subtypes) and, for non-entity types, the
//! defined type or primitive they name. Aggregates compare by element.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::export::common::{attr_owner, redeclaration_owner};
use crate::express::{AttrSpec, AttrType, EntitySchema, Schema};

/// The names a type admits.
type Admits = BTreeSet<String>;

/// The SELECT members of every type name, unioned over `ranked` (newest
/// first), and the other aliases, newest wins. This is the type table the
/// universal export writes and code generation reads.
pub(crate) fn union_types(
    ranked: &[&Schema],
) -> (BTreeMap<String, Vec<String>>, BTreeMap<String, AttrType>) {
    let mut select_members: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut other: BTreeMap<String, AttrType> = BTreeMap::new();
    for s in ranked {
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
                    other
                        .entry(tn.clone())
                        .or_insert_with(|| td.aliased.clone());
                }
            }
        }
    }
    other.retain(|tn, _| !select_members.contains_key(tn));
    (select_members, other)
}

/// One view of the entity hierarchy and type table: a single edition, or the
/// universal union.
struct View<'a> {
    parents: HashMap<&'a str, &'a [String]>,
    children: HashMap<&'a str, Vec<&'a str>>,
    selects: HashMap<&'a str, &'a [String]>,
    aliases: HashMap<&'a str, &'a AttrType>,
}

impl<'a> View<'a> {
    fn new(
        parents: HashMap<&'a str, &'a [String]>,
        selects: HashMap<&'a str, &'a [String]>,
        aliases: HashMap<&'a str, &'a AttrType>,
    ) -> Self {
        let mut children: HashMap<&'a str, Vec<&'a str>> = HashMap::new();
        for (child, ps) in &parents {
            for p in ps.iter() {
                children.entry(p.as_str()).or_default().push(child);
            }
        }
        Self {
            parents,
            children,
            selects,
            aliases,
        }
    }

    fn of_schema(s: &'a Schema) -> Self {
        let parents = s
            .entities
            .iter()
            .map(|(n, e)| (n.as_str(), e.parents.as_slice()))
            .collect();
        let mut selects = HashMap::new();
        let mut aliases = HashMap::new();
        for (tn, td) in &s.types {
            match &td.aliased {
                AttrType::Select(m) => {
                    selects.insert(tn.as_str(), m.as_slice());
                }
                other => {
                    aliases.insert(tn.as_str(), other);
                }
            }
        }
        Self::new(parents, selects, aliases)
    }

    /// `name` and every entity below it.
    fn subtypes(&self, name: &'a str) -> Admits {
        let mut out = Admits::new();
        let mut stack = vec![name];
        while let Some(n) = stack.pop() {
            if out.insert(n.to_string())
                && let Some(kids) = self.children.get(n)
            {
                stack.extend(kids.iter().copied());
            }
        }
        out
    }

    /// `name` and every entity above it.
    fn ancestors(&self, name: &'a str) -> Vec<&'a str> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![name];
        while let Some(n) = stack.pop() {
            if seen.insert(n) {
                out.push(n);
                if let Some(ps) = self.parents.get(n) {
                    stack.extend(ps.iter().map(String::as_str));
                }
            }
        }
        out
    }

    fn admits(&self, ty: &'a AttrType) -> Admits {
        let mut seen = HashSet::new();
        self.admits_inner(ty, &mut seen)
    }

    fn admits_inner(&self, ty: &'a AttrType, seen: &mut HashSet<&'a str>) -> Admits {
        match ty {
            AttrType::List(inner, _)
            | AttrType::Set(inner, _)
            | AttrType::Bag(inner, _)
            | AttrType::Array(inner, _)
            | AttrType::Optional(inner) => self.admits_inner(inner, seen),
            AttrType::Select(members) => members
                .iter()
                .flat_map(|m| self.admits_name(m, seen))
                .collect(),
            AttrType::Entity(name) => self.admits_name(name, seen),
            AttrType::Enumeration(_) => std::iter::once("=enumeration".to_string()).collect(),
            AttrType::Primitive(p) => std::iter::once(format!("={p}")).collect(),
        }
    }

    fn admits_name(&self, name: &'a str, seen: &mut HashSet<&'a str>) -> Admits {
        if self.parents.contains_key(name) {
            return self.subtypes(name);
        }
        if !seen.insert(name) {
            return Admits::new();
        }
        if let Some(members) = self.selects.get(name) {
            return members
                .iter()
                .flat_map(|m| self.admits_name(m, seen))
                .collect();
        }
        match self.aliases.get(name) {
            Some(AttrType::Primitive(_) | AttrType::Enumeration(_)) | None => {
                std::iter::once(format!("={name}")).collect()
            }
            Some(aliased) => self.admits_inner(aliased, seen),
        }
    }
}

/// An edition's effective type for an attribute of an entity.
enum Effective {
    /// The entity is not in the edition, or the attribute is derived there:
    /// no constraint.
    Absent,
    /// The entity is in the edition but does not have the attribute.
    Missing,
    Admits(Admits),
}

fn effective<'a>(s: &'a Schema, view: &View<'a>, entity: &'a str, attr: &str) -> Effective {
    if !s.entities.contains_key(entity) {
        return Effective::Absent;
    }
    let chain = view.ancestors(entity);
    let derived = chain.iter().any(|n| {
        s.entities
            .get(*n)
            .is_some_and(|e| e.derived_attrs.iter().any(|d| d.name == attr))
    });
    if derived {
        return Effective::Absent;
    }
    let Some(owner) = attr_owner(s, entity, attr) else {
        return Effective::Missing;
    };
    let base = &s.entities[&owner]
        .own_attrs
        .iter()
        .find(|a| a.name == attr)
        .expect("owner declares the attribute")
        .ty;
    let mut set = view.admits(base);
    for e in chain.iter().filter_map(|n| s.entities.get(*n)) {
        for r in &e.redeclared_attrs {
            if r.name == attr && redeclaration_owner(s, r).as_deref() == Some(owner.as_str()) {
                let narrow = view.admits(&r.ty);
                if !narrow.is_empty() {
                    set = set.intersection(&narrow).cloned().collect();
                }
            }
        }
    }
    Effective::Admits(set)
}

/// The verdict on one redeclaration.
pub(crate) struct Verdict {
    pub(crate) union_safe: bool,
    /// Why it is not union-safe: the entity, the edition, and a few names that
    /// edition admits beyond the redeclared type.
    pub(crate) reason: Option<String>,
}

/// The universal view plus every edition's view, for judging redeclarations.
pub(crate) struct Judge<'a> {
    schemas: Vec<(&'a Schema, View<'a>)>,
    universal: View<'a>,
}

impl<'a> Judge<'a> {
    /// `decls` maps each entity name to the declaration the universal export
    /// writes for it; `selects`/`aliases` are its type table (`union_types`).
    pub(crate) fn new(
        schemas: &'a [Schema],
        decls: &BTreeMap<&'a str, &'a EntitySchema>,
        selects: &'a BTreeMap<String, Vec<String>>,
        aliases: &'a BTreeMap<String, AttrType>,
    ) -> Self {
        let universal = View::new(
            decls
                .iter()
                .map(|(n, e)| (*n, e.parents.as_slice()))
                .collect(),
            selects
                .iter()
                .map(|(n, m)| (n.as_str(), m.as_slice()))
                .collect(),
            aliases.iter().map(|(n, t)| (n.as_str(), t)).collect(),
        );
        let schemas = schemas.iter().map(|s| (s, View::of_schema(s))).collect();
        Self { schemas, universal }
    }

    /// Whether `r`, redeclared on `entity` in the universal declaration, admits
    /// everything any edition admits for that attribute on `entity` and on every
    /// subtype of it.
    pub(crate) fn judge(&self, entity: &'a str, r: &'a AttrSpec) -> Verdict {
        let allowed = self.universal.admits(&r.ty);
        for f in self.universal.subtypes(entity) {
            for (s, view) in &self.schemas {
                let Some((f, _)) = s.entities.get_key_value(f.as_str()) else {
                    continue;
                };
                match effective(s, view, f, &r.name) {
                    Effective::Absent => {}
                    Effective::Missing => {
                        return Verdict {
                            union_safe: false,
                            reason: Some(format!(
                                "{f} has no attribute {} in {}",
                                r.name, s.source_label
                            )),
                        };
                    }
                    Effective::Admits(set) => {
                        let extra: Vec<&String> = set.difference(&allowed).take(4).collect();
                        if !extra.is_empty() {
                            return Verdict {
                                union_safe: false,
                                reason: Some(format!(
                                    "{f} in {} also admits {extra:?}",
                                    s.source_label
                                )),
                            };
                        }
                    }
                }
            }
        }
        Verdict {
            union_safe: true,
            reason: None,
        }
    }

    /// Whether `ty` admits only entities under the universal view: a
    /// reference, as opposed to a defined type, enumeration or primitive.
    pub(crate) fn is_reference(&self, ty: &'a AttrType) -> bool {
        let a = self.universal.admits(ty);
        !a.is_empty() && a.iter().all(|n| !n.starts_with('='))
    }

    /// The names any edition admits for `attr` on `entity` that the universal
    /// types `tys` (the declared type and the redeclarations applied to it,
    /// all of which hold) do not: where the universal schema is narrower than
    /// the union of editions.
    pub(crate) fn narrower_than_union(
        &self,
        entity: &'a str,
        attr: &str,
        tys: &[&'a AttrType],
    ) -> Vec<String> {
        let mut allowed: Option<Admits> = None;
        for ty in tys {
            let a = self.universal.admits(ty);
            allowed = Some(match allowed {
                Some(prev) => prev.intersection(&a).cloned().collect(),
                None => a,
            });
        }
        let allowed = allowed.unwrap_or_default();
        let mut extra = Admits::new();
        for (s, view) in &self.schemas {
            if let Some((e, _)) = s.entities.get_key_value(entity)
                && let Effective::Admits(set) = effective(s, view, e, attr)
            {
                extra.extend(set.difference(&allowed).cloned());
            }
        }
        extra.into_iter().collect()
    }
}

/// `base` with its innermost element type replaced by `narrow`'s: the
/// redeclared element under the declared aggregate and optionality. `None` if
/// the two nest aggregates differently.
pub(crate) fn with_element(base: &AttrType, narrow: &AttrType) -> Option<AttrType> {
    fn strip(t: &AttrType) -> &AttrType {
        match t {
            AttrType::Optional(inner) => strip(inner),
            _ => t,
        }
    }
    let narrow = strip(narrow);
    match (base, narrow) {
        (AttrType::Optional(b), _) => {
            with_element(b, narrow).map(|t| AttrType::Optional(Box::new(t)))
        }
        (AttrType::List(b, bb), AttrType::List(n, _)) => {
            with_element(b, n).map(|t| AttrType::List(Box::new(t), *bb))
        }
        (AttrType::Set(b, bb), AttrType::Set(n, _)) => {
            with_element(b, n).map(|t| AttrType::Set(Box::new(t), *bb))
        }
        (AttrType::Bag(b, bb), AttrType::Bag(n, _)) => {
            with_element(b, n).map(|t| AttrType::Bag(Box::new(t), *bb))
        }
        (AttrType::Array(b, bb), AttrType::Array(n, _)) => {
            with_element(b, n).map(|t| AttrType::Array(Box::new(t), *bb))
        }
        (AttrType::List(..) | AttrType::Set(..) | AttrType::Bag(..) | AttrType::Array(..), _)
        | (_, AttrType::List(..) | AttrType::Set(..) | AttrType::Bag(..) | AttrType::Array(..)) => {
            None
        }
        _ => Some(narrow.clone()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::express::{AggBounds, TypeDef};

    fn attr(name: &str, ty: AttrType, qual: Option<&str>) -> AttrSpec {
        AttrSpec {
            name: name.into(),
            ty,
            super_qual: qual.map(Into::into),
        }
    }

    fn ent(parents: &[&str], own: Vec<AttrSpec>, red: Vec<AttrSpec>) -> EntitySchema {
        EntitySchema {
            parents: parents.iter().map(|s| s.to_string()).collect(),
            own_attrs: own,
            redeclared_attrs: red,
            is_abstract: false,
            supertype_expr: None,
            derived_attrs: Vec::new(),
        }
    }

    fn e(n: &str) -> AttrType {
        AttrType::Entity(n.into())
    }

    fn schema(label: &str, entities: Vec<(&str, EntitySchema)>) -> Schema {
        Schema {
            source_label: label.into(),
            entities: entities
                .into_iter()
                .map(|(n, e)| (n.to_string(), e))
                .collect(),
            types: HashMap::<String, TypeDef>::new(),
            parse_warnings: Vec::new(),
        }
    }

    /// `holder.target : item` with item ← point ← cartesian; `narrowing`
    /// redeclares it as `point` in each edition listed.
    fn editions(narrowed_in: &[bool]) -> Vec<Schema> {
        narrowed_in
            .iter()
            .enumerate()
            .map(|(i, &narrow)| {
                let red = if narrow {
                    vec![attr("target", e("point"), Some("holder"))]
                } else {
                    Vec::new()
                };
                schema(
                    &format!("s{i}"),
                    vec![
                        ("item", ent(&[], vec![], vec![])),
                        ("point", ent(&["item"], vec![], vec![])),
                        ("other", ent(&["item"], vec![], vec![])),
                        ("cartesian", ent(&["point"], vec![], vec![])),
                        (
                            "holder",
                            ent(&[], vec![attr("target", e("item"), None)], vec![]),
                        ),
                        ("narrowing", ent(&["holder"], vec![], red)),
                    ],
                )
            })
            .collect()
    }

    fn verdict(schemas: &[Schema]) -> Verdict {
        let decls: BTreeMap<&str, &EntitySchema> = schemas[0]
            .entities
            .iter()
            .map(|(n, e)| (n.as_str(), e))
            .collect();
        let (selects, aliases) = (BTreeMap::new(), BTreeMap::new());
        let judge = Judge::new(schemas, &decls, &selects, &aliases);
        let r = &schemas[0].entities["narrowing"].redeclared_attrs[0];
        judge.judge("narrowing", r)
    }

    #[test]
    fn a_narrowing_every_edition_shares_is_union_safe() {
        assert!(verdict(&editions(&[true, true])).union_safe);
    }

    #[test]
    fn a_narrowing_one_edition_lacks_is_not() {
        let v = verdict(&editions(&[true, false]));
        assert!(!v.union_safe);
        assert!(v.reason.unwrap().contains("s1"), "names the edition");
    }

    #[test]
    fn a_wider_subtype_in_another_edition_breaks_it() {
        let mut schemas = editions(&[true, true]);
        // In s1 a subtype of `narrowing` takes its attribute from a different
        // parent, unnarrowed.
        schemas[1]
            .entities
            .insert("sub".into(), ent(&["holder"], vec![], vec![]));
        schemas[0]
            .entities
            .insert("sub".into(), ent(&["narrowing"], vec![], vec![]));
        assert!(!verdict(&schemas).union_safe);
    }

    /// The real editions, judged as the universal export judges them.
    mod real {
        use std::collections::BTreeMap;
        use std::path::Path;

        use super::super::*;
        use crate::export::common::schema_rank;
        use crate::express::load_all_schemas;

        fn with_judge(check: impl FnOnce(&Judge, &[Schema], &BTreeMap<&str, &EntitySchema>)) {
            let schemas = load_all_schemas(Path::new("schemas"));
            let mut ranked: Vec<&Schema> = schemas.iter().collect();
            ranked.sort_by_key(|s| std::cmp::Reverse(schema_rank(&s.source_label)));
            let mut decls: BTreeMap<&str, &EntitySchema> = BTreeMap::new();
            for s in &ranked {
                for (n, e) in &s.entities {
                    decls.entry(n.as_str()).or_insert(e);
                }
            }
            let (selects, aliases) = union_types(&ranked);
            let judge = Judge::new(&schemas, &decls, &selects, &aliases);
            check(&judge, &schemas, &decls);
        }

        fn safe(
            judge: &Judge,
            decls: &BTreeMap<&str, &EntitySchema>,
            entity: &str,
            attr: &str,
        ) -> bool {
            let (name, e) = decls.get_key_value(entity).expect("entity");
            let r = e
                .redeclared_attrs
                .iter()
                .find(|a| a.name == attr)
                .unwrap_or_else(|| panic!("{entity} redeclares {attr}"));
            judge.judge(name, r).union_safe
        }

        #[test]
        fn narrowings_every_edition_shares_are_union_safe() {
            with_judge(|judge, _, decls| {
                for (e, a) in [
                    ("presentation_representation", "context_of_items"),
                    ("model_geometric_view", "item"),
                    ("camera_image", "mapping_source"),
                    ("camera_image", "mapping_target"),
                    ("camera_usage", "mapping_origin"),
                    (
                        "feature_for_datum_target_relationship",
                        "related_shape_aspect",
                    ),
                    ("tessellated_annotation_occurrence", "item"),
                    ("annotation_placeholder_occurrence", "item"),
                    (
                        "draughting_model_item_association_with_placeholder",
                        "identified_item",
                    ),
                ] {
                    assert!(safe(judge, decls, e, a), "{e}.{a} should be union-safe");
                }
            });
        }

        #[test]
        fn narrowings_an_older_edition_widens_are_not() {
            with_judge(|judge, _, decls| {
                for (e, a) in [
                    ("shape_definition_representation", "definition"),
                    ("shape_definition_representation", "used_representation"),
                    ("draughting_model", "items"),
                    ("shape_dimension_representation", "items"),
                    ("shape_representation_with_parameters", "items"),
                    (
                        "shape_aspect_deriving_relationship",
                        "relating_shape_aspect",
                    ),
                    ("constructive_geometry_representation_relationship", "rep_2"),
                    (
                        "mechanical_design_geometric_presentation_representation",
                        "items",
                    ),
                    (
                        "mechanical_design_shaded_presentation_representation",
                        "items",
                    ),
                ] {
                    assert!(
                        !safe(judge, decls, e, a),
                        "{e}.{a} should not be union-safe"
                    );
                }
            });
        }

        /// AP242 e1 narrowed a tessellated shape representation's items to
        /// tessellated items; e2 dropped that, so it must not apply on read.
        #[test]
        fn the_e1_tessellated_items_narrowing_is_not_union_safe() {
            with_judge(|judge, schemas, _| {
                let e1 = schemas
                    .iter()
                    .find(|s| s.source_label == "ap242e1")
                    .unwrap();
                let (name, e) = e1
                    .entities
                    .get_key_value("tessellated_shape_representation")
                    .unwrap();
                let r = e
                    .redeclared_attrs
                    .iter()
                    .find(|a| a.name == "items")
                    .unwrap();
                assert!(!judge.judge(name, r).union_safe);
            });
        }
    }

    #[test]
    fn an_aggregate_keeps_its_declared_wrapper() {
        let set = |inner: AttrType, lower| {
            AttrType::Set(Box::new(inner), AggBounds { lower, upper: None })
        };
        let narrowed = with_element(&set(e("item"), 1), &set(e("point"), 2)).unwrap();
        match narrowed {
            AttrType::Set(inner, b) => {
                assert!(matches!(*inner, AttrType::Entity(ref n) if n == "point"));
                assert_eq!(b.lower, 1);
            }
            other => panic!("{other:?}"),
        }
        assert!(with_element(&e("item"), &set(e("point"), 1)).is_none());
    }
}
