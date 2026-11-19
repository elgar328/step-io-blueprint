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

use crate::export::common::{attr_owner, redeclaration_owner, schema_rank, ty_repr};
use crate::express::{AggBounds, AttrSpec, AttrType, EntitySchema, Schema};

/// The names a type admits.
pub(crate) type Admits = BTreeSet<String>;

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
pub(crate) struct View<'a> {
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

    pub(crate) fn of_schema(s: &'a Schema) -> Self {
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
    pub(crate) fn ancestors(&self, name: &'a str) -> Vec<&'a str> {
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

    pub(crate) fn admits(&self, ty: &'a AttrType) -> Admits {
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
                .flat_map(|m| self.admits_member(m, seen))
                .collect(),
            AttrType::Entity(name) => self.admits_name(name, seen),
            AttrType::Enumeration(_) => std::iter::once("=enumeration".to_string()).collect(),
            AttrType::Primitive(p) => std::iter::once(format!("={p}")).collect(),
        }
    }

    /// What a SELECT member admits: a member whose type is an aggregate holds
    /// that aggregate as one value, not its elements, so it admits `[]name`.
    fn admits_member(&self, name: &'a str, seen: &mut HashSet<&'a str>) -> Admits {
        match self.aliases.get(name) {
            Some(
                AttrType::List(..) | AttrType::Set(..) | AttrType::Bag(..) | AttrType::Array(..),
            ) => std::iter::once(format!("[]{name}")).collect(),
            _ => self.admits_name(name, seen),
        }
    }

    /// What the type or entity `name` admits.
    fn admits_name_fresh(&self, name: &'a str) -> Admits {
        self.admits_name(name, &mut HashSet::new())
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
                .flat_map(|m| self.admits_member(m, seen))
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

    /// Whether `redeclared` and the `declared` type it narrows both admit only
    /// entities, and `redeclared` admits nothing `declared` does not: leaving
    /// the redeclaration out then reads `declared`, which only widens.
    pub(crate) fn only_narrows_entities(
        &self,
        declared: &'a AttrType,
        redeclared: &'a AttrType,
    ) -> bool {
        let entities = |ty: &'a AttrType| {
            let a = self.universal.admits(ty);
            (!a.is_empty()
                && a.iter()
                    .all(|n| !n.starts_with('=') && !n.starts_with("[]")))
            .then_some(a)
        };
        match (entities(declared), entities(redeclared)) {
            (Some(d), Some(r)) => r.is_subset(&d),
            _ => false,
        }
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

    /// The declared attribute types the universal schema must widen so that
    /// each admits what every edition declares for it, and the SELECTs to add
    /// for the widenings no declared type covers. `decls` is the declaration
    /// written for each entity (the newest edition's).
    pub(crate) fn widen(&self, decls: &BTreeMap<&'a str, &'a EntitySchema>) -> Widening {
        let mut editions: Vec<&(&'a Schema, View<'a>)> = self.schemas.iter().collect();
        editions.sort_by_key(|(s, _)| std::cmp::Reverse(schema_rank(&s.source_label)));
        let mut out = Widening::default();
        for (&name, decl) in decls {
            for a in &decl.own_attrs {
                let slot = format!("{name}.{}", a.name);
                let mut declared: Vec<(&'a Schema, &View<'a>, &'a AttrType)> = Vec::new();
                let mut owner_differs = None;
                let mut union = Admits::new();
                for (s, view) in &editions {
                    let Some(e) = s.entities.get(name) else {
                        continue;
                    };
                    match e.own_attrs.iter().find(|x| x.name == a.name) {
                        Some(x) => declared.push((s, view, &x.ty)),
                        None => {
                            // Another entity declares it in this edition:
                            // count what it admits, but do not widen.
                            if let Some(o) = attr_owner(s, name, &a.name)
                                && let Some(x) =
                                    s.entities[&o].own_attrs.iter().find(|x| x.name == a.name)
                            {
                                union.extend(view.admits(&x.ty));
                                owner_differs =
                                    Some(format!("declared by {o} in {}", s.source_label));
                            }
                        }
                    }
                }
                if !matches!(a.ty, AttrType::Optional(_))
                    && let Some((s, _, _)) = declared
                        .iter()
                        .find(|(_, _, t)| matches!(t, AttrType::Optional(_)))
                {
                    out.optional_elsewhere
                        .push(format!("{slot} : OPTIONAL in {}", s.source_label));
                }
                let written = aggregate_shape(&a.ty);
                if let Some((s, _, t)) = declared
                    .iter()
                    .find(|(_, _, t)| !aggregates_cover(&written, &aggregate_shape(t)))
                {
                    out.aggregate_elsewhere.push(format!(
                        "{slot} : {} in {}, {} here",
                        ty_repr(t),
                        s.source_label,
                        ty_repr(&a.ty)
                    ));
                }
                for (_, view, t) in &declared {
                    union.extend(view.admits(t));
                }
                let allowed = self.universal.admits(&a.ty);
                if union.is_subset(&allowed) {
                    continue;
                }
                let not = |why: String| format!("{slot} : {why}");
                if let Some(why) = owner_differs {
                    out.not_widened.push(not(format!("owner differs ({why})")));
                    continue;
                }
                if union.iter().any(|n| n.starts_with('=')) {
                    out.not_widened
                        .push(not("non-reference declaration differs".into()));
                    continue;
                }
                let mut names: Vec<&'a str> = Vec::new();
                for (_, _, t) in &declared {
                    match element(t) {
                        AttrType::Entity(n) => {
                            if !names.contains(&n.as_str()) {
                                names.push(n);
                            }
                        }
                        other => {
                            out.not_widened
                                .push(not(format!("inline element type {other:?}")));
                            names.clear();
                            break;
                        }
                    }
                }
                if names.is_empty() {
                    continue;
                }
                if names.len() == 1 {
                    out.not_widened.push(not(format!(
                        "hierarchy differs (every edition declares {})",
                        names[0]
                    )));
                    continue;
                }
                let admits_of = |n: &'a str| self.universal.admits_name_fresh(n);
                let covering = names.iter().find(|n| union.is_subset(&admits_of(n)));
                let target = match covering {
                    Some(n) => n.to_string(),
                    None => {
                        // Keep the names no other name already covers.
                        let sets: Vec<Admits> = names.iter().map(|n| admits_of(n)).collect();
                        let kept: Vec<usize> = (0..names.len())
                            .filter(|&i| {
                                !sets.iter().enumerate().any(|(j, other)| {
                                    j != i
                                        && sets[i].is_subset(other)
                                        && (sets[i] != *other || j < i)
                                })
                            })
                            .collect();
                        let covered: Admits =
                            kept.iter().flat_map(|&i| sets[i].iter().cloned()).collect();
                        let members: Vec<String> =
                            kept.iter().map(|&i| names[i].to_string()).collect();
                        if !union.is_subset(&covered) {
                            out.not_widened.push(not("no union covers it".into()));
                            continue;
                        }
                        let synth = members.join("_or_");
                        let taken = self.universal.selects.contains_key(synth.as_str())
                            || self.universal.aliases.contains_key(synth.as_str())
                            || self.universal.parents.contains_key(synth.as_str());
                        let same = out.synthetic.get(&synth).is_some_and(|m| *m == members);
                        if taken || (out.synthetic.contains_key(&synth) && !same) {
                            out.not_widened
                                .push(not(format!("name {synth} already taken")));
                            continue;
                        }
                        out.synthetic.insert(synth.clone(), members);
                        synth
                    }
                };
                let t = replace_element(&a.ty, &AttrType::Entity(target));
                out.widened_report
                    .push(format!("{slot} : {} -> {}", ty_repr(&a.ty), ty_repr(&t)));
                out.widened.insert((name.to_string(), a.name.clone()), t);
            }
        }
        out
    }
}

/// Declarations the universal schema widens to the union of editions
/// (`Judge::widen`), and what it reports about them.
#[derive(Default)]
pub(crate) struct Widening {
    /// (entity, attribute) → the widened declared type.
    pub(crate) widened: BTreeMap<(String, String), AttrType>,
    /// SELECTs made up for widenings no declared type covers: name → members.
    pub(crate) synthetic: BTreeMap<String, Vec<String>>,
    /// `entity.attr : old -> new` per widening.
    pub(crate) widened_report: Vec<String>,
    /// Declarations narrower than the union that are not widened, with why.
    pub(crate) not_widened: Vec<String>,
    /// Attributes mandatory here that some edition declares OPTIONAL.
    pub(crate) optional_elsewhere: Vec<String>,
    /// Attributes some edition declares with other aggregates or bounds.
    pub(crate) aggregate_elsewhere: Vec<String>,
}

/// `t` with its innermost element replaced by `element`, under the same
/// `OPTIONAL` and aggregates.
fn replace_element(t: &AttrType, element: &AttrType) -> AttrType {
    let inner = |i: &AttrType| Box::new(replace_element(i, element));
    match t {
        AttrType::Optional(i) => AttrType::Optional(inner(i)),
        AttrType::List(i, b) => AttrType::List(inner(i), *b),
        AttrType::Set(i, b) => AttrType::Set(inner(i), *b),
        AttrType::Bag(i, b) => AttrType::Bag(inner(i), *b),
        AttrType::Array(i, b) => AttrType::Array(inner(i), *b),
        _ => element.clone(),
    }
}

/// The aggregates around an attribute type's element, outermost first, past
/// `OPTIONAL`: each kind (`LIST`, `SET`, …) with its bounds.
fn aggregate_shape(t: &AttrType) -> Vec<(&'static str, AggBounds)> {
    let mut out = Vec::new();
    let mut t = t;
    loop {
        t = match t {
            AttrType::Optional(i) => i,
            AttrType::List(i, b) => {
                out.push(("LIST", *b));
                i
            }
            AttrType::Set(i, b) => {
                out.push(("SET", *b));
                i
            }
            AttrType::Bag(i, b) => {
                out.push(("BAG", *b));
                i
            }
            AttrType::Array(i, b) => {
                out.push(("ARRAY", *b));
                i
            }
            _ => return out,
        };
    }
}

/// Whether aggregates `written` take every value `other` does: the same
/// kinds, each with bounds at least as wide.
fn aggregates_cover(
    written: &[(&'static str, AggBounds)],
    other: &[(&'static str, AggBounds)],
) -> bool {
    written.len() == other.len()
        && written.iter().zip(other).all(|((wk, wb), (ok, ob))| {
            wk == ok
                && wb.lower <= ob.lower
                && match (wb.upper, ob.upper) {
                    (None, _) => true,
                    (Some(_), None) => false,
                    (Some(w), Some(o)) => w >= o,
                }
        })
}

/// The innermost element of an attribute type, past `OPTIONAL` and
/// aggregates.
fn element(t: &AttrType) -> &AttrType {
    match t {
        AttrType::Optional(inner)
        | AttrType::List(inner, _)
        | AttrType::Set(inner, _)
        | AttrType::Bag(inner, _)
        | AttrType::Array(inner, _) => element(inner),
        other => other,
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
            where_rules: Vec::new(),
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

        /// Declarations AP203/AP214 make wider than AP242's are widened to
        /// cover every edition.
        #[test]
        fn declarations_older_editions_widen_are_widened() {
            with_judge(|judge, _, decls| {
                let w = judge.widen(decls);
                let ty = |e: &str, a: &str| {
                    w.widened
                        .get(&(e.to_string(), a.to_string()))
                        .map(ty_repr)
                        .unwrap_or_else(|| panic!("{e}.{a} should be widened"))
                };
                assert_eq!(
                    ty("geometric_tolerance", "magnitude"),
                    "OPTIONAL measure_with_unit"
                );
                assert_eq!(
                    ty("geometric_tolerance_with_defined_unit", "unit_size"),
                    "measure_with_unit"
                );
                assert_eq!(
                    ty("projected_zone_definition", "projected_length"),
                    "measure_with_unit"
                );
                assert_eq!(
                    ty("styled_item", "item"),
                    "styled_item_target_or_representation_item"
                );
                assert_eq!(
                    w.synthetic["styled_item_target_or_representation_item"],
                    ["styled_item_target", "representation_item"]
                );
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

    /// `holder.target` declared as `newer` in ap242e2 and `older` in ap214e3,
    /// over item ← point and an unrelated `rep`, plus a SELECT `sel(point,
    /// rep)`; widened as the universal export widens it.
    fn widen_target(newer: AttrType, older: AttrType) -> Widening {
        let entities = |ty: AttrType| {
            vec![
                ("item", ent(&[], vec![], vec![])),
                ("point", ent(&["item"], vec![], vec![])),
                ("rep", ent(&[], vec![], vec![])),
                ("holder", ent(&[], vec![attr("target", ty, None)], vec![])),
            ]
        };
        let mut schemas = vec![
            schema("ap242e2", entities(newer)),
            schema("ap214e3", entities(older)),
        ];
        schemas[0].types.insert(
            "sel".into(),
            TypeDef {
                aliased: AttrType::Select(vec!["point".into(), "rep".into()]),
            },
        );
        let decls: BTreeMap<&str, &EntitySchema> = schemas[0]
            .entities
            .iter()
            .map(|(n, e)| (n.as_str(), e))
            .collect();
        let ranked: Vec<&Schema> = schemas.iter().collect();
        let (selects, aliases) = union_types(&ranked);
        Judge::new(&schemas, &decls, &selects, &aliases).widen(&decls)
    }

    fn widened_target(w: &Widening) -> Option<String> {
        w.widened
            .get(&("holder".to_string(), "target".to_string()))
            .map(ty_repr)
    }

    #[test]
    fn an_older_declaration_that_covers_the_union_is_used() {
        let w = widen_target(AttrType::Optional(Box::new(e("point"))), e("item"));
        assert_eq!(widened_target(&w).as_deref(), Some("OPTIONAL item"));
        assert!(w.synthetic.is_empty());
    }

    #[test]
    fn declarations_neither_covers_get_a_select_of_both() {
        let w = widen_target(e("sel"), e("item"));
        assert_eq!(widened_target(&w).as_deref(), Some("sel_or_item"));
        assert_eq!(w.synthetic["sel_or_item"], ["sel", "item"]);
    }

    #[test]
    fn an_aggregate_is_widened_by_its_element() {
        let set = |inner: AttrType| {
            AttrType::Set(
                Box::new(inner),
                AggBounds {
                    lower: 1,
                    upper: None,
                },
            )
        };
        let w = widen_target(set(e("point")), set(e("item")));
        assert_eq!(widened_target(&w).as_deref(), Some("SET [1:?] OF item"));
        assert!(w.aggregate_elsewhere.is_empty());
    }

    #[test]
    fn an_aggregate_in_another_edition_is_reported() {
        let w = widen_target(
            e("point"),
            AttrType::List(Box::new(e("point")), AggBounds::UNBOUNDED),
        );
        assert!(w.aggregate_elsewhere[0].contains("ap214e3"));
    }

    #[test]
    fn only_an_entity_narrowing_inside_the_declared_type_only_narrows_entities() {
        let mut s = schema(
            "ap242e2",
            vec![
                ("item", ent(&[], vec![], vec![])),
                ("point", ent(&["item"], vec![], vec![])),
                ("rep", ent(&[], vec![], vec![])),
            ],
        );
        s.types.insert(
            "points".into(),
            TypeDef {
                aliased: AttrType::Select(vec!["point".into()]),
            },
        );
        s.types.insert(
            "point_or_rep".into(),
            TypeDef {
                aliased: AttrType::Select(vec!["point".into(), "rep".into()]),
            },
        );
        s.types.insert(
            "point_or_set".into(),
            TypeDef {
                aliased: AttrType::Select(vec!["point".into(), "items".into()]),
            },
        );
        s.types.insert(
            "items".into(),
            TypeDef {
                aliased: AttrType::Set(Box::new(e("item")), AggBounds::UNBOUNDED),
            },
        );
        let schemas = vec![s];
        let decls: BTreeMap<&str, &EntitySchema> = schemas[0]
            .entities
            .iter()
            .map(|(n, e)| (n.as_str(), e))
            .collect();
        let ranked: Vec<&Schema> = schemas.iter().collect();
        let (selects, aliases) = union_types(&ranked);
        let judge = Judge::new(&schemas, &decls, &selects, &aliases);
        let (item, points, point_or_rep, point_or_set) =
            (e("item"), e("points"), e("point_or_rep"), e("point_or_set"));
        assert!(judge.only_narrows_entities(&item, &points));
        assert!(!judge.only_narrows_entities(&item, &point_or_rep));
        assert!(!judge.only_narrows_entities(&point_or_set, &points));
        let text = AttrType::Primitive("STRING".into());
        assert!(!judge.only_narrows_entities(&text, &points));
    }

    #[test]
    fn a_select_member_aggregate_does_not_admit_its_elements() {
        // `with_set` = SELECT(rep, items) where `items` = SET OF item: a lone
        // `point` value is not an `items` value, so the older `point`
        // declaration still widens the newer one.
        let mut schemas = vec![
            schema(
                "ap242e2",
                vec![
                    ("item", ent(&[], vec![], vec![])),
                    ("point", ent(&["item"], vec![], vec![])),
                    ("rep", ent(&[], vec![], vec![])),
                    (
                        "holder",
                        ent(&[], vec![attr("target", e("with_set"), None)], vec![]),
                    ),
                ],
            ),
            schema(
                "ap214e3",
                vec![
                    ("item", ent(&[], vec![], vec![])),
                    ("point", ent(&["item"], vec![], vec![])),
                    (
                        "holder",
                        ent(&[], vec![attr("target", e("point"), None)], vec![]),
                    ),
                ],
            ),
        ];
        schemas[0].types.insert(
            "with_set".into(),
            TypeDef {
                aliased: AttrType::Select(vec!["rep".into(), "items".into()]),
            },
        );
        schemas[0].types.insert(
            "items".into(),
            TypeDef {
                aliased: AttrType::Set(Box::new(e("item")), AggBounds::UNBOUNDED),
            },
        );
        let decls: BTreeMap<&str, &EntitySchema> = schemas[0]
            .entities
            .iter()
            .map(|(n, e)| (n.as_str(), e))
            .collect();
        let ranked: Vec<&Schema> = schemas.iter().collect();
        let (selects, aliases) = union_types(&ranked);
        let judge = Judge::new(&schemas, &decls, &selects, &aliases);
        let admits = judge
            .universal
            .admits(&schemas[0].entities["holder"].own_attrs[0].ty);
        assert!(
            admits.contains("[]items") && !admits.contains("point"),
            "{admits:?}"
        );
        let w = judge.widen(&decls);
        assert_eq!(widened_target(&w).as_deref(), Some("with_set_or_point"));
    }

    #[test]
    fn agreeing_declarations_are_left_alone() {
        let w = widen_target(e("item"), e("point"));
        assert!(w.widened.is_empty() && w.not_widened.is_empty());
    }

    #[test]
    fn a_differing_scalar_is_reported_not_widened() {
        let w = widen_target(
            AttrType::Primitive("STRING".into()),
            AttrType::Primitive("INTEGER".into()),
        );
        assert!(w.widened.is_empty());
        assert!(w.not_widened[0].contains("non-reference"));
    }

    #[test]
    fn an_attribute_another_edition_declares_elsewhere_is_reported() {
        // ap214e3 declares `target` on a supertype of `holder` instead.
        let older = vec![
            ("item", ent(&[], vec![], vec![])),
            ("point", ent(&["item"], vec![], vec![])),
            (
                "base",
                ent(&[], vec![attr("target", e("item"), None)], vec![]),
            ),
            ("holder", ent(&["base"], vec![], vec![])),
        ];
        let newer = vec![
            ("item", ent(&[], vec![], vec![])),
            ("point", ent(&["item"], vec![], vec![])),
            (
                "holder",
                ent(&[], vec![attr("target", e("point"), None)], vec![]),
            ),
        ];
        let schemas = vec![schema("ap242e2", newer), schema("ap214e3", older)];
        let decls: BTreeMap<&str, &EntitySchema> = schemas[0]
            .entities
            .iter()
            .map(|(n, e)| (n.as_str(), e))
            .collect();
        let ranked: Vec<&Schema> = schemas.iter().collect();
        let (selects, aliases) = union_types(&ranked);
        let w = Judge::new(&schemas, &decls, &selects, &aliases).widen(&decls);
        assert!(w.widened.is_empty());
        assert!(w.not_widened[0].contains("owner differs"));
    }
}
