//! Attribute type restrictions an entity's `WHERE` rules make, for the output
//! profile.
//!
//! Some domain rules only restrict which entity types an attribute may
//! reference — `'X' IN TYPEOF(SELF.a)`, `SIZEOF([..] * TYPEOF(a)) = 1`, or
//! the element form `SIZEOF(QUERY(v <* a | NOT (..))) = 0`. Writing must
//! follow them, so each is resolved here to the closed set of entities it
//! admits: `TYPEOF` of an entity instance is the entity, its supertypes and
//! every SELECT any of them belongs to (nested SELECTs included), and the
//! rule counts how many listed names that set holds. Conditional,
//! indexed-element, path and existence rules are not attribute type
//! restrictions and are reported, not exported. Reading is never held to
//! these rules.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use regex::Regex;

/// A regex compiled once per call site.
macro_rules! re {
    ($p:literal) => {{
        static RE: std::sync::LazyLock<Regex> =
            std::sync::LazyLock::new(|| Regex::new($p).unwrap());
        &*RE
    }};
}

use crate::export::common::attr_owner;
use crate::export::narrowing::View;
use crate::express::{AttrType, Schema};

/// How many of the listed type names a value's `TYPEOF` must hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Count {
    AtLeastOne,
    ExactlyOne,
    Zero,
}

/// A restriction on whatever `arg` names (a `TYPEOF` argument).
#[derive(Debug, Clone)]
struct Cond {
    arg: String,
    names: Vec<String>,
    count: Count,
    /// The restriction holds for every element of the aggregate `arg`.
    elementwise: bool,
}

/// One attribute type restriction a `WHERE` rule makes.
#[derive(Debug, Clone)]
pub(crate) struct TypeRule {
    /// The restricted attribute.
    pub attr: String,
    /// The entity declaring the attribute.
    pub owner: String,
    /// The rule label (`wr1`).
    pub rule: String,
    /// The single type a positive rule names, else `<entity>_<attr>`.
    pub type_name: String,
    /// The entities the attribute (or each element of it) may reference,
    /// within its declared type; sorted.
    pub admits: Vec<String>,
    /// How many of `names` the value's `TYPEOF` must hold.
    pub count: Count,
    /// The type names the rule lists (entities, and SELECTs standing for
    /// their members). `admits` settles simple values; a complex value's
    /// `TYPEOF` is the union over its parts, so the count is over that union.
    pub names: Vec<String>,
}

/// The type restrictions of every entity of a schema, and a line per
/// `TYPEOF`-mentioning rule saying what became of it.
pub(crate) struct WhereTypes {
    pub rules: BTreeMap<String, Vec<TypeRule>>,
    pub report: Vec<String>,
}

fn normalize(expr: &str) -> String {
    let lower = expr.to_lowercase();
    let collapsed = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    // Schema-qualified type names in string literals: `'schema.name'`.
    let unqualified = re!(r"'[a-z0-9_]+\.([a-z0-9_]+)'")
        .replace_all(&collapsed, "'$1'")
        .into_owned();
    re!(r"\s*([()\[\],\\.])\s*")
        .replace_all(&unqualified, "$1")
        .into_owned()
}

/// The operand of a leading `NOT`.
fn strip_not(t: &str) -> Option<&str> {
    let rest = t.strip_prefix("not")?;
    (rest.starts_with('(') || rest.starts_with(' ')).then_some(rest)
}

/// Remove parentheses enclosing the whole of `s`.
fn strip_parens(mut s: &str) -> &str {
    loop {
        s = s.trim();
        if !(s.starts_with('(') && s.ends_with(')')) {
            return s;
        }
        let mut depth = 0i32;
        for (i, c) in s.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 && i + 1 < s.len() {
                        return s;
                    }
                }
                _ => {}
            }
        }
        s = &s[1..s.len() - 1];
    }
}

/// Split `s` at the keyword operator `op` outside parentheses, brackets and
/// string literals.
fn split_top<'s>(s: &'s str, op: &str) -> Vec<&'s str> {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let (mut depth, mut quoted, mut last) = (0i32, false, 0usize);
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            '\'' => quoted = !quoted,
            '(' | '[' if !quoted => depth += 1,
            ')' | ']' if !quoted => depth -= 1,
            _ if !quoted
                && depth == 0
                && s[i..].starts_with(op)
                && !word(s[..i].chars().next_back())
                && !word(s[i + op.len()..].chars().next()) =>
            {
                out.push(strip_parens(&s[last..i]));
                i += op.len();
                last = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(strip_parens(&s[last..]));
    out
}

fn names_of(list: &str) -> Vec<String> {
    re!(r"'([a-z0-9_]+)'")
        .captures_iter(list)
        .map(|c| c[1].to_string())
        .collect()
}

/// The negation of `c`, where it is again a restriction: not of an
/// element-wise one (that says some element fails, not how each must be).
fn negate(c: Cond) -> Option<Cond> {
    if c.elementwise {
        return None;
    }
    let count = match c.count {
        Count::AtLeastOne => Count::Zero,
        Count::Zero => Count::AtLeastOne,
        Count::ExactlyOne => return None,
    };
    Some(Cond { count, ..c })
}

fn sizeof_count(op: &str, n: &str) -> Option<Count> {
    match (op, n) {
        ("=", "1") => Some(Count::ExactlyOne),
        (">=", "1") | (">", "0") => Some(Count::AtLeastOne),
        ("=", "0") => Some(Count::Zero),
        _ => None,
    }
}

/// `a <> n` read as `NOT (a = n)`: the equality, if `t` compares a `SIZEOF`
/// with `<>`.
fn not_equal(t: &str) -> Option<String> {
    let c = re!(r"^(sizeof\(.*\)) ?<> ?(\d+)$").captures(t)?;
    Some(format!("{}={}", &c[1], &c[2]))
}

/// The restriction a single normalized term makes, if it is one.
fn term(t: &str) -> Option<Cond> {
    let t = strip_parens(t);
    let ors = split_top(t, "or");
    // `AND` binds tighter: a conjunction is several restrictions, not one.
    if ors.len() == 1 && split_top(t, "and").len() > 1 {
        return None;
    }
    if ors.len() > 1 {
        let parts: Vec<Cond> = ors.iter().map(|o| term(o)).collect::<Option<_>>()?;
        let first = &parts[0];
        let same = parts.iter().all(|p| {
            p.count == Count::AtLeastOne && p.arg == first.arg && p.elementwise == first.elementwise
        });
        return same.then(|| Cond {
            arg: first.arg.clone(),
            names: parts.iter().flat_map(|p| p.names.clone()).collect(),
            count: Count::AtLeastOne,
            elementwise: first.elementwise,
        });
    }
    if let Some(rest) = strip_not(t) {
        return negate(term(rest)?);
    }
    if let Some(eq) = not_equal(t) {
        return negate(term(&eq)?);
    }
    let cond = |arg: &str, names: Vec<String>, count| Cond {
        arg: arg.to_string(),
        names,
        count,
        elementwise: false,
    };
    if let Some(c) = re!(r"^'([a-z0-9_]+)' in typeof\(([^()]*)\)$").captures(t) {
        return Some(cond(&c[2], vec![c[1].to_string()], Count::AtLeastOne));
    }
    if let Some(c) =
        re!(r"^sizeof\(\[([^\]]*)\] ?\* ?typeof\(([^()]*)\)\) ?(=|>=|>) ?(\d+)$").captures(t)
    {
        return Some(cond(&c[2], names_of(&c[1]), sizeof_count(&c[3], &c[4])?));
    }
    if let Some(c) =
        re!(r"^sizeof\(typeof\(([^()]*)\) ?\* ?\[([^\]]*)\]\) ?(=|>=|>) ?(\d+)$").captures(t)
    {
        return Some(cond(&c[1], names_of(&c[2]), sizeof_count(&c[3], &c[4])?));
    }
    if let Some(c) =
        re!(r"^sizeof\(query\(([a-z0-9_]+) ?<\* ?([^|()]+?) ?\| ?(.*)\)\) ?= ?0$").captures(t)
    {
        // No element may satisfy the predicate: each satisfies its negation.
        let pred = strip_parens(&c[3]);
        // `NOT` binds tighter than `OR`: `NOT a OR b` is `(NOT a) OR b`, so
        // only a single term may have its `NOT` taken off.
        let each = if split_top(pred, "or").len() > 1 {
            negate(term(pred)?)?
        } else if let Some(inner) = strip_not(pred) {
            term(inner)?
        } else if let Some(eq) = not_equal(pred) {
            term(&eq)?
        } else {
            negate(term(pred)?)?
        };
        if each.arg != c[1] || each.elementwise {
            return None;
        }
        return Some(Cond {
            arg: c[2].to_string(),
            elementwise: true,
            ..each
        });
    }
    None
}

/// Why a rule mentioning `TYPEOF` restricts no attribute type.
fn why_not(t: &str) -> &'static str {
    let args: Vec<String> = re!(r"typeof\(([^()]*)\)")
        .captures_iter(t)
        .map(|c| c[1].to_string())
        .collect();
    if !args.is_empty() && args.iter().all(|a| a == "self") {
        "restricts SELF"
    } else if args.iter().any(|a| a.contains('[')) {
        "indexed element"
    } else if t.contains("usedin(") || t.contains("using_representations(") {
        "references to the instance"
    } else if args
        .iter()
        .any(|a| a.trim_start_matches("self\\").contains(['.', '\\']) && !simple_self(a))
    {
        "attribute path"
    } else if t.contains("query(") {
        "query form"
    } else {
        "other form"
    }
}

fn simple_self(arg: &str) -> bool {
    re!(r"^self(\\[a-z0-9_]+)?\.[a-z0-9_]+$").is_match(arg)
}

struct Resolver<'a> {
    schema: &'a Schema,
    view: View<'a>,
    /// TYPEOF of every entity.
    typeof_: HashMap<&'a str, BTreeSet<&'a str>>,
}

impl<'a> Resolver<'a> {
    fn new(schema: &'a Schema) -> Self {
        let view = View::of_schema(schema);
        let selects: Vec<(&str, &Vec<String>)> = schema
            .types
            .iter()
            .filter_map(|(n, td)| match &td.aliased {
                AttrType::Select(m) => Some((n.as_str(), m)),
                _ => None,
            })
            .collect();
        let mut typeof_ = HashMap::new();
        for name in schema.entities.keys() {
            let mut set: BTreeSet<&str> = view.ancestors(name).into_iter().collect();
            loop {
                let before = set.len();
                for (s, members) in &selects {
                    if !set.contains(s) && members.iter().any(|m| set.contains(m.as_str())) {
                        set.insert(s);
                    }
                }
                if set.len() == before {
                    break;
                }
            }
            typeof_.insert(name.as_str(), set);
        }
        Self {
            schema,
            view,
            typeof_,
        }
    }

    /// The attribute a `TYPEOF` argument names on `entity`, with its owner.
    fn attribute(&self, entity: &str, arg: &str) -> Result<(String, String), &'static str> {
        let (from, attr) = if let Some(c) = re!(r"^self\\([a-z0-9_]+)\.([a-z0-9_]+)$").captures(arg)
        {
            (c[1].to_string(), c[2].to_string())
        } else if let Some(a) = arg.strip_prefix("self.") {
            (entity.to_string(), a.to_string())
        } else {
            (entity.to_string(), arg.to_string())
        };
        if arg == "self" {
            return Err("restricts SELF");
        }
        if attr.contains('[') {
            return Err("indexed element");
        }
        if !re!(r"^[a-z_][a-z0-9_]*$").is_match(&attr) {
            return Err("attribute path");
        }
        let owner = attr_owner(self.schema, &from, &attr).ok_or("not an explicit attribute")?;
        Ok((attr, owner))
    }

    fn declared(&self, owner: &str, attr: &str) -> &'a AttrType {
        let o = &self.schema.entities[owner];
        &o.own_attrs.iter().find(|a| a.name == attr).unwrap().ty
    }

    fn is_aggregate(&self, ty: &AttrType) -> bool {
        match ty {
            AttrType::Optional(inner) => self.is_aggregate(inner),
            AttrType::List(..) | AttrType::Set(..) | AttrType::Bag(..) | AttrType::Array(..) => {
                true
            }
            AttrType::Entity(name) => self
                .schema
                .types
                .get(name)
                .is_some_and(|td| self.is_aggregate(&td.aliased)),
            _ => false,
        }
    }

    fn holds(&self, entity: &str, names: &[String], count: Count) -> bool {
        let t = &self.typeof_[entity];
        let n = names.iter().filter(|n| t.contains(n.as_str())).count();
        match count {
            Count::AtLeastOne => n >= 1,
            Count::ExactlyOne => n == 1,
            Count::Zero => n == 0,
        }
    }
}

pub(crate) fn analyze(schema: &Schema) -> WhereTypes {
    let r = Resolver::new(schema);
    let mut rules: BTreeMap<String, Vec<TypeRule>> = BTreeMap::new();
    let mut report = Vec::new();
    let mut entities: Vec<&String> = schema.entities.keys().collect();
    entities.sort();
    for entity in entities {
        for w in &schema.entities[entity].where_rules {
            let expr = normalize(&w.expr);
            if !expr.contains("typeof(") {
                continue;
            }
            let at = format!("{entity}.{}", w.label);
            let body = strip_parens(&expr);
            let conjuncts = if split_top(body, "xor").len() > 1 {
                report.push(format!("skip    {at}: conditional"));
                continue;
            } else if split_top(body, "or").len() > 1 {
                if term(body).is_none() {
                    report.push(format!("skip    {at}: conditional"));
                    continue;
                }
                vec![body]
            } else {
                split_top(body, "and")
            };
            for c in conjuncts {
                if !c.contains("typeof(") {
                    continue;
                }
                let Some(cond) = term(c) else {
                    report.push(format!("skip    {at}: {}", why_not(c)));
                    continue;
                };
                let (attr, owner) = match r.attribute(entity, &cond.arg) {
                    Ok(a) => a,
                    Err(why) => {
                        report.push(format!("skip    {at}: {why}"));
                        continue;
                    }
                };
                let declared = r.declared(&owner, &attr);
                if r.is_aggregate(declared) != cond.elementwise {
                    report.push(format!(
                        "skip    {at}: {attr} is not shaped as the rule reads it"
                    ));
                    continue;
                }
                let declared_admits = r.view.admits(declared);
                if declared_admits.iter().any(|n| n.starts_with('=')) {
                    report.push(format!("skip    {at}: {attr} holds non-entity values"));
                    continue;
                }
                let not_entity: Vec<&String> = cond
                    .names
                    .iter()
                    .filter(|n| {
                        !schema.entities.contains_key(n.as_str())
                            && !schema
                                .types
                                .get(n.as_str())
                                .is_some_and(|td| matches!(td.aliased, AttrType::Select(_)))
                    })
                    .collect();
                if !not_entity.is_empty() {
                    report.push(format!(
                        "skip    {at}: names types that are neither entities nor SELECTs {not_entity:?}"
                    ));
                    continue;
                }
                let admits: Vec<String> = declared_admits
                    .into_iter()
                    .filter(|e| {
                        schema.entities.contains_key(e) && r.holds(e, &cond.names, cond.count)
                    })
                    .collect();
                let type_name = match (cond.count, cond.names.as_slice()) {
                    (Count::AtLeastOne | Count::ExactlyOne, [one]) => one.clone(),
                    _ => format!("{entity}_{attr}"),
                };
                report.push(format!(
                    "take    {at}: {owner}.{attr} {:?}{} {:?} -> {} entities",
                    cond.count,
                    if cond.elementwise { " each" } else { "" },
                    cond.names,
                    admits.len()
                ));
                rules.entry(entity.clone()).or_default().push(TypeRule {
                    attr,
                    owner,
                    rule: w.label.clone(),
                    type_name,
                    admits,
                    count: cond.count,
                    names: cond.names,
                });
            }
        }
    }
    WhereTypes { rules, report }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::express::load_all_schemas;

    fn e2_rules() -> WhereTypes {
        let schemas = load_all_schemas(Path::new("schemas"));
        let e2 = schemas
            .iter()
            .find(|s| s.source_label == "ap242e2")
            .unwrap();
        analyze(e2)
    }

    fn rule<'w>(w: &'w WhereTypes, entity: &str, attr: &str) -> &'w TypeRule {
        w.rules[entity]
            .iter()
            .find(|r| r.attr == attr)
            .unwrap_or_else(|| panic!("no rule on {entity}.{attr}"))
    }

    fn admits(r: &TypeRule, e: &str) -> bool {
        r.admits.iter().any(|a| a == e)
    }

    #[test]
    fn a_bare_attribute_name_is_read_and_exactly_one_is_counted() {
        let w = e2_rules();
        let face = rule(&w, "advanced_face", "face_geometry");
        assert_eq!(face.owner, "face_surface");
        assert_eq!(face.type_name, "advanced_face_face_geometry");
        assert!(admits(face, "plane") && admits(face, "b_spline_surface_with_knots"));
        assert!(!admits(face, "offset_surface"));
        let brep = rule(&w, "advanced_brep_shape_representation", "items");
        assert!(admits(brep, "manifold_solid_brep") && admits(brep, "brep_with_voids"));
        assert!(
            !admits(brep, "faceted_brep"),
            "faceted_brep names two listed types"
        );
    }

    #[test]
    fn typeof_holds_the_selects_an_entity_belongs_to() {
        let w = e2_rules();
        let pds = rule(&w, "product_definition_shape", "definition");
        assert!(admits(pds, "product_definition"));
        assert!(admits(pds, "next_assembly_usage_occurrence"));
        assert!(!admits(pds, "shape_aspect"));
    }

    #[test]
    fn the_reversed_and_negated_forms_are_read() {
        let w = e2_rules();
        let assoc = rule(
            &w,
            "annotation_occurrence_associativity",
            "related_annotation_occurrence",
        );
        assert!(admits(assoc, "leader_curve"));
        assert!(!admits(assoc, "annotation_curve_occurrence"));
        let edge = rule(&w, "oriented_edge", "edge_element");
        assert_eq!(edge.type_name, "oriented_edge_edge_element");
        assert_eq!(
            (edge.count, edge.names.as_slice()),
            (Count::Zero, &["oriented_edge".to_string()][..])
        );
        assert!(admits(edge, "edge_curve") && !admits(edge, "oriented_edge"));
        let unit = rule(&w, "length_measure_with_unit", "unit_component");
        assert_eq!(unit.type_name, "length_unit");
        assert_eq!(unit.count, Count::AtLeastOne);
        assert!(admits(unit, "length_unit") && !admits(unit, "mass_unit"));
    }

    #[test]
    fn not_binds_tighter_than_or_in_a_query_predicate() {
        let mixed = normalize(
            "SIZEOF(QUERY(x <* SELF.items | NOT ('A' IN TYPEOF(x)) OR ('B' IN TYPEOF(x)))) = 0",
        );
        assert!(term(&mixed).is_none(), "(NOT a) OR b restricts no type");
        let whole = normalize(
            "SIZEOF(QUERY(x <* SELF.items | NOT (('A' IN TYPEOF(x)) OR ('B' IN TYPEOF(x))))) = 0",
        );
        let c = term(&whole).expect("NOT (a OR b): each element is an a or a b");
        assert_eq!((c.count, c.elementwise), (Count::AtLeastOne, true));
        assert_eq!(c.names, ["a", "b"]);
        let both = normalize("('A' IN TYPEOF(SELF.x)) AND ('B' IN TYPEOF(SELF.y))");
        assert!(
            term(&both).is_none(),
            "a conjunction is not one restriction"
        );
    }

    #[test]
    fn exactly_one_keeps_the_names_it_counts() {
        let w = e2_rules();
        let cgr = rule(&w, "constructive_geometry_representation", "items");
        assert_eq!(cgr.count, Count::ExactlyOne);
        assert!(cgr.names.iter().any(|n| n == "point") && cgr.names.iter().any(|n| n == "edge"));
    }

    #[test]
    fn not_equal_reads_as_a_negated_equality() {
        let w = e2_rules();
        let cgr = rule(&w, "constructive_geometry_representation", "items");
        assert_eq!(cgr.type_name, "constructive_geometry_representation_items");
        assert!(admits(cgr, "axis2_placement_3d") && admits(cgr, "edge_curve"));
        assert!(admits(cgr, "vertex_point") && admits(cgr, "advanced_face"));
        assert!(!admits(cgr, "mapped_item") && !admits(cgr, "styled_item"));
    }

    #[test]
    fn a_conjunct_is_taken_and_a_condition_is_not() {
        let w = e2_rules();
        let cgr = rule(
            &w,
            "constructive_geometry_representation",
            "context_of_items",
        );
        assert!(admits(cgr, "geometric_representation_context"));
        assert!(
            !w.rules
                .get("geometric_tolerance_with_defined_area_unit")
                .is_some_and(|rs| rs.iter().any(|r| r.attr == "second_unit_size"))
        );
        assert!(
            w.report
                .iter()
                .any(|l| l == "skip    geometric_tolerance_with_defined_area_unit.wr2: conditional")
        );
        assert!(
            !w.rules["tessellated_shape_representation"]
                .iter()
                .any(|r| r.attr == "items"),
            "wr2 asks for one tessellated item, not that every item is one"
        );
    }
}
