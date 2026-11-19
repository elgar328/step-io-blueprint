//! Attribute type restrictions an entity's `WHERE` rules make, for the output
//! profile.
//!
//! Some domain rules only restrict which entity types an attribute may
//! reference — `'X' IN TYPEOF(SELF.a)`, `SIZEOF([..] * TYPEOF(a)) = 1`, or
//! the element form `SIZEOF(QUERY(v <* a | NOT (..))) = 0`. Writing must
//! follow them, so each is resolved here to the closed set of entities it
//! admits: `TYPEOF` of an entity instance is the entity, its supertypes and
//! every SELECT any of them belongs to (nested SELECTs included), and the
//! rule counts how many listed names that set holds. A rule may also hold
//! only for some instances, by SELF's type: `.. OR 'X' IN TYPEOF(SELF)`
//! lifts it from an X, `NOT ('X' IN TYPEOF(SELF)) OR ..` holds it for an X
//! only. A SELECT member that is an aggregate type holds one aggregate value,
//! whose `TYPEOF` is that type and the SELECTs holding it: a rule may name
//! that type, admit the value as a whole, or restrict its elements through
//! `'T' IN TYPEOF(a) AND SIZEOF(QUERY(x <* a | ..)) = 0`; a query over a
//! SELECT of aggregates only restricts the elements of whichever it holds.
//! Other conditional, indexed-element, path and existence rules are not
//! attribute type restrictions and are reported, not exported. Reading is
//! never held to these rules.

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
    /// Further, `arg` may be a value of each of these aggregate types whose
    /// every element holds the given count of the given names.
    sets: Vec<(String, Vec<String>, Count)>,
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
    /// The rule holds only where SELF's `TYPEOF` holds all of these.
    pub when_self: Vec<String>,
    /// The rule does not hold where SELF's `TYPEOF` holds any of these.
    pub unless_self: Vec<String>,
    /// The aggregate types among the attribute's SELECT members the rule
    /// admits, each with the entities its elements may reference; an
    /// aggregate member not listed is not admitted.
    pub aggregates: Vec<AggregateMember>,
}

/// An aggregate-typed SELECT member a rule admits.
#[derive(Debug, Clone)]
pub(crate) struct AggregateMember {
    pub type_name: String,
    /// The entities each element may reference; sorted.
    pub admits: Vec<String>,
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
    if c.elementwise || !c.sets.is_empty() {
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
    // `AND` binds tighter: a conjunction is several restrictions, not one,
    // except `'T' IN TYPEOF(a) AND SIZEOF(QUERY(x <* a | ..)) = 0`: `a` is a
    // value of the aggregate type T whose every element holds a restriction.
    let ands = split_top(t, "and");
    if ors.len() == 1 && ands.len() > 1 {
        let [is_t, each] = ands.as_slice() else {
            return None;
        };
        let (is_t, each) = (term(is_t)?, term(each)?);
        let [t_name] = is_t.names.as_slice() else {
            return None;
        };
        let fits = is_t.count == Count::AtLeastOne
            && !is_t.elementwise
            && is_t.sets.is_empty()
            && each.elementwise
            && each.sets.is_empty()
            && each.arg == is_t.arg;
        return fits.then(|| Cond {
            arg: is_t.arg.clone(),
            names: Vec::new(),
            count: Count::AtLeastOne,
            elementwise: false,
            sets: vec![(t_name.clone(), each.names, each.count)],
        });
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
            sets: parts.iter().flat_map(|p| p.sets.clone()).collect(),
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
        sets: Vec::new(),
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

/// SELF's type conditions on a disjunction, with the rest of it: a term
/// `'X' IN TYPEOF(SELF)` lifts the rest from an X (`unless`), a term
/// `NOT ('X' IN TYPEOF(SELF))` holds it for an X only (`when`).
struct SelfCondition {
    when: Vec<String>,
    unless: Vec<String>,
    rest: String,
}

fn self_condition(body: &str) -> Option<SelfCondition> {
    let is_self = |t: &str| {
        re!(r"^'([a-z0-9_]+)' in typeof\(self\)$")
            .captures(strip_parens(t))
            .map(|c| c[1].to_string())
    };
    let (mut when, mut unless, mut rest) = (Vec::new(), Vec::new(), Vec::new());
    for t in split_top(body, "or") {
        if let Some(x) = is_self(t) {
            unless.push(x);
        } else if let Some(x) = strip_not(t).and_then(is_self) {
            when.push(x);
        } else {
            rest.push(t);
        }
    }
    let rest = rest.join(" or ");
    (!(when.is_empty() && unless.is_empty()) && !rest.is_empty() && !rest.contains("typeof(self)"))
        .then_some(SelfCondition { when, unless, rest })
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
    /// TYPEOF of an instance of every entity, and of a value of every
    /// aggregate type.
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
        let aggregates = schema.types.iter().filter_map(|(n, td)| {
            matches!(
                td.aliased,
                AttrType::List(..) | AttrType::Set(..) | AttrType::Bag(..) | AttrType::Array(..)
            )
            .then_some(n.as_str())
        });
        let mut typeof_ = HashMap::new();
        for name in schema.entities.keys().map(String::as_str).chain(aggregates) {
            let mut set: BTreeSet<&str> = if schema.entities.contains_key(name) {
                view.ancestors(name).into_iter().collect()
            } else {
                BTreeSet::from([name])
            };
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
            typeof_.insert(name, set);
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
            let mut on_self = None;
            let conjuncts = if split_top(body, "xor").len() > 1 {
                report.push(format!("skip    {at}: conditional (exclusive or)"));
                continue;
            } else if split_top(body, "or").len() > 1 {
                if term(body).is_some() {
                    vec![body.to_string()]
                } else if let Some(c) = self_condition(body).filter(|c| term(&c.rest).is_some()) {
                    let rest = c.rest.clone();
                    on_self = Some(c);
                    vec![rest]
                } else {
                    report.push(format!("skip    {at}: conditional"));
                    continue;
                }
            } else {
                split_top(body, "and")
                    .into_iter()
                    .map(str::to_string)
                    .collect()
            };
            let (when_self, unless_self) =
                on_self.map_or((Vec::new(), Vec::new()), |c| (c.when, c.unless));
            if let Some(n) = when_self
                .iter()
                .chain(&unless_self)
                .find(|n| !schema.entities.contains_key(n.as_str()))
            {
                report.push(format!(
                    "skip    {at}: SELF's condition names a non-entity {n:?}"
                ));
                continue;
            }
            for c in &conjuncts {
                if !c.contains("typeof(") {
                    continue;
                }
                let Some(mut cond) = term(c) else {
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
                if cond.elementwise && !r.is_aggregate(declared) {
                    let members: Option<Vec<String>> = r
                        .view
                        .admits(declared)
                        .iter()
                        .map(|n| n.strip_prefix("[]").map(str::to_string))
                        .collect();
                    if let Some(members) = members.filter(|m| !m.is_empty()) {
                        let names = std::mem::take(&mut cond.names);
                        cond.sets = members
                            .into_iter()
                            .map(|t| (t, names.clone(), cond.count))
                            .collect();
                        cond.count = Count::AtLeastOne;
                        cond.elementwise = false;
                    }
                }
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
                let members: Vec<&str> = declared_admits
                    .iter()
                    .filter_map(|n| n.strip_prefix("[]"))
                    .collect();
                let not_entity: Vec<&String> = cond
                    .names
                    .iter()
                    .filter(|n| !members.contains(&n.as_str()))
                    .chain(cond.sets.iter().flat_map(|(_, names, _)| names))
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
                if let Some((t, ..)) = cond
                    .sets
                    .iter()
                    .find(|(t, ..)| !members.contains(&t.as_str()))
                {
                    report.push(format!(
                        "skip    {at}: {t} is not an aggregate type {attr} holds"
                    ));
                    continue;
                }
                let mut aggregates = Vec::new();
                let mut unfit = None;
                for t in members {
                    let element = match &schema.types[t].aliased {
                        AttrType::List(inner, _)
                        | AttrType::Set(inner, _)
                        | AttrType::Bag(inner, _)
                        | AttrType::Array(inner, _) => inner,
                        _ => unreachable!("an aggregate member"),
                    };
                    let elements = r.view.admits(element);
                    if elements
                        .iter()
                        .any(|n| n.starts_with('=') || n.starts_with("[]"))
                    {
                        unfit = Some(t);
                        break;
                    }
                    let mut admits: Vec<String> = elements.into_iter().collect();
                    if !r.holds(t, &cond.names, cond.count) {
                        let Some((_, names, count)) = cond.sets.iter().find(|(n, ..)| n == t)
                        else {
                            continue;
                        };
                        admits.retain(|e| r.holds(e, names, *count));
                    }
                    aggregates.push(AggregateMember {
                        type_name: t.to_string(),
                        admits,
                    });
                }
                if let Some(t) = unfit {
                    report.push(format!(
                        "skip    {at}: the elements of {t} are not all entities"
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
                let mut on = String::new();
                if !when_self.is_empty() {
                    on += &format!(" when SELF is {when_self:?}");
                }
                if !unless_self.is_empty() {
                    on += &format!(" unless SELF is {unless_self:?}");
                }
                for a in &aggregates {
                    on += &format!(", {} of {} entities", a.type_name, a.admits.len());
                }
                report.push(format!(
                    "take    {at}: {owner}.{attr} {:?}{} {:?} -> {} entities{on}",
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
                    when_self: when_self.clone(),
                    unless_self: unless_self.clone(),
                    aggregates,
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

    #[test]
    fn a_condition_on_self_is_kept_with_the_rule() {
        let w = e2_rules();
        let by_label = |e: &str, l: &str| w.rules[e].iter().find(|r| r.rule == l).unwrap();
        let styled = by_label("styled_item", "wr3");
        assert_eq!(styled.attr, "item");
        assert_eq!(
            styled.unless_self,
            ["context_dependent_over_riding_styled_item"]
        );
        assert!(styled.when_self.is_empty());
        assert!(admits(styled, "mapped_item") && admits(styled, "cartesian_point"));
        assert!(!admits(styled, "closed_shell") && !admits(styled, "shape_representation"));
        let text = by_label("draughting_annotation_occurrence", "wr7");
        assert_eq!(text.when_self, ["annotation_text_occurrence"]);
        assert_eq!(text.count, Count::ExactlyOne);
        assert!(admits(text, "text_literal") && !admits(text, "annotation_text"));
        assert!(
            w.report
                .iter()
                .any(|l| l
                    == "skip    product_definition_occurrence.wr1: conditional (exclusive or)")
        );
    }

    #[test]
    fn a_self_condition_needs_another_term() {
        let c = self_condition(&normalize(
            "(NOT ('A' IN TYPEOF(SELF))) OR ('B' IN TYPEOF(SELF.x)) OR ('C' IN TYPEOF(SELF))",
        ))
        .expect("a condition on SELF and one on SELF.x");
        assert_eq!((c.when, c.unless), (vec!["a".into()], vec!["c".into()]));
        assert_eq!(c.rest, "'b' in typeof(self.x)");
        assert!(
            self_condition(&normalize("('A' IN TYPEOF(SELF)) OR ('B' IN TYPEOF(SELF))")).is_none()
        );
        assert!(self_condition(&normalize("('A' IN TYPEOF(x)) OR ('B' IN TYPEOF(y))")).is_none());
    }

    /// A schema whose `holder.t` is SELECT(item, items), `items` being a SET
    /// OF base, under the given `WHERE` rules.
    fn aggregate_rules(rules: &str) -> WhereTypes {
        let text = format!(
            "SCHEMA s;\n\
             TYPE target = SELECT (item, items); END_TYPE;\n\
             TYPE items = SET [1:?] OF base; END_TYPE;\n\
             ENTITY base; END_ENTITY;\n\
             ENTITY item SUBTYPE OF (base); END_ENTITY;\n\
             ENTITY other SUBTYPE OF (base); END_ENTITY;\n\
             ENTITY holder;\n  t : target;\nWHERE\n{rules}\nEND_ENTITY;\n\
             END_SCHEMA;\n"
        );
        let dir = std::env::temp_dir().join(format!("where_types_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!("s{n}.exp"));
        std::fs::write(&path, text).unwrap();
        let schema = crate::express::parse_express_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        analyze(&schema)
    }

    fn members(r: &TypeRule) -> Vec<(&str, Vec<&str>)> {
        r.aggregates
            .iter()
            .map(|a| {
                (
                    a.type_name.as_str(),
                    a.admits.iter().map(String::as_str).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn an_aggregate_member_with_an_element_rule_admits_those_elements() {
        let w = aggregate_rules(
            "  WR1: ('S.ITEM' IN TYPEOF(t)) OR (('S.ITEMS' IN TYPEOF(t)) AND \
             (SIZEOF(QUERY(x <* t | NOT ('S.ITEM' IN TYPEOF(x)))) = 0));",
        );
        let r = rule(&w, "holder", "t");
        assert_eq!(r.admits, ["item"]);
        assert_eq!(members(r), [("items", vec!["item"])]);
    }

    #[test]
    fn a_rule_an_aggregate_value_holds_admits_every_element() {
        let w = aggregate_rules("  WR1: NOT ('S.OTHER' IN TYPEOF(t));");
        let r = rule(&w, "holder", "t");
        assert_eq!(members(r), [("items", vec!["base", "item", "other"])]);
        let w = aggregate_rules("  WR1: 'S.ITEM' IN TYPEOF(t);");
        assert!(rule(&w, "holder", "t").aggregates.is_empty());
    }

    #[test]
    fn a_rule_naming_an_aggregate_type_admits_only_its_values() {
        let w = aggregate_rules("  WR1: 'S.ITEMS' IN TYPEOF(t);");
        let r = rule(&w, "holder", "t");
        assert!(r.admits.is_empty());
        assert_eq!(members(r), [("items", vec!["base", "item", "other"])]);
    }

    #[test]
    fn a_query_over_a_select_of_aggregates_restricts_their_elements() {
        let schemas = load_all_schemas(Path::new("schemas"));
        let e2 = schemas
            .iter()
            .find(|s| s.source_label == "ap242e2")
            .unwrap();
        let w = analyze(e2);
        let table = rule(&w, "table_representation_item", "item_element");
        assert!(table.admits.is_empty());
        let names: Vec<&str> = table
            .aggregates
            .iter()
            .map(|a| a.type_name.as_str())
            .collect();
        assert_eq!(
            names,
            ["list_representation_item", "set_representation_item"]
        );
        assert!(
            table
                .aggregates
                .iter()
                .all(|a| a.admits == ["row_representation_item"])
        );
        let range = rule(&w, "value_range", "item_element");
        let [set] = range.aggregates.as_slice() else {
            panic!("{:?}", range.aggregates)
        };
        assert_eq!(set.type_name, "set_representation_item");
    }

    #[test]
    fn an_element_rule_on_a_type_the_attribute_does_not_hold_is_skipped() {
        let w = aggregate_rules(
            "  WR1: ('S.ITEM' IN TYPEOF(t)) OR (('S.OTHER' IN TYPEOF(t)) AND \
             (SIZEOF(QUERY(x <* t | NOT ('S.ITEM' IN TYPEOF(x)))) = 0));",
        );
        assert!(!w.rules.contains_key("holder"));
        assert!(
            w.report
                .iter()
                .any(|l| l == "skip    holder.wr1: other is not an aggregate type t holds"),
            "{:?}",
            w.report
        );
    }

    #[test]
    fn e4_styled_items_admit_a_set_of_mapped_and_geometric_items() {
        let schemas = load_all_schemas(Path::new("schemas"));
        let e4 = schemas
            .iter()
            .find(|s| s.source_label == "ap242e4")
            .unwrap();
        let w = analyze(e4);
        let wr = |label: &str| {
            w.rules["styled_item"]
                .iter()
                .find(|r| r.rule == label)
                .unwrap_or_else(|| panic!("no styled_item.{label}"))
        };
        let wr3 = wr("wr3");
        assert!(wr3.unless_self.is_empty() && wr3.when_self.is_empty());
        assert!(admits(wr3, "mapped_item") && admits(wr3, "cartesian_point"));
        assert!(!admits(wr3, "representation"));
        let [set] = wr3.aggregates.as_slice() else {
            panic!("{:?}", wr3.aggregates)
        };
        assert_eq!(set.type_name, "set_representation_item");
        assert!(set.admits.iter().any(|e| e == "mapped_item"));
        assert!(
            !set.admits
                .iter()
                .any(|e| e == "descriptive_representation_item")
        );
        let wr2 = wr("wr2");
        assert_eq!(wr2.aggregates.len(), 1);
        assert!(
            wr2.aggregates[0]
                .admits
                .iter()
                .any(|e| e == "descriptive_representation_item")
        );
    }
}
