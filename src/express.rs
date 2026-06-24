//! EXPRESS schema parser — extract ENTITY and TYPE definitions from `.exp`
//! files. Sufficient for AP203 / AP203e2 / AP214e3 / AP242 of stepcode.
//!
//! Recognises:
//! - `ENTITY name SUBTYPE OF (...) ATTRS END_ENTITY;` with own ATTR types
//!   (Entity ref, LIST/SET/BAG/ARRAY, OPTIONAL, SELECT, ENUMERATION,
//!   primitives). Multi-line ATTR definitions parsed by splitting on `;`
//!   at paren-depth 0, not by line.
//! - `TYPE name = repr; [WHERE ...] END_TYPE;` for SELECT / ENUMERATION /
//!   alias chains.
//!
//! Out of scope: RULE / FUNCTION / PROCEDURE blocks, DERIVE / INVERSE /
//! WHERE / UNIQUE attribute extraction (those don't contribute to
//! STEP-encoded attribute count). SUPERTYPE OF clauses are read for
//! ABSTRACT detection but not for inverse-walking parents.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

mod supertype_parser;

/// Type of an ATTR or TYPE alias. Bound information (`[1:3]`) and inner
/// `UNIQUE`/`OPTIONAL` modifiers on aggregations are intentionally dropped
/// — infer pipeline cares only about the polymorphic / reference
/// structure, not capacity hints.
#[derive(Debug, Clone, Serialize)]
pub enum AttrType {
    /// `cartesian_point` — entity name OR TYPE alias name (resolved at
    /// analysis time using `Schema::types`).
    Entity(String),
    /// `LIST [n:m] OF X` — bounds dropped.
    List(Box<AttrType>),
    /// `SET [n:m] OF X` — bounds dropped.
    Set(Box<AttrType>),
    /// `BAG [n:m] OF X` — bounds dropped.
    Bag(Box<AttrType>),
    /// `ARRAY [n:m] OF X` — bounds dropped.
    Array(Box<AttrType>),
    /// `OPTIONAL X` — preserved because the optionality affects
    /// nullability of cross-references.
    Optional(Box<AttrType>),
    /// `SELECT (a, b, c)` — polymorphic over the listed names. Strong
    /// signal for variant-stage polymorphic context detection.
    Select(Vec<String>),
    /// `ENUMERATION OF (a, b, c)` — named string values, NOT a polymorphic
    /// signal (the values aren't entities).
    Enumeration(Vec<String>),
    /// `INTEGER` / `REAL` / `STRING` / `LOGICAL` / `BOOLEAN` / `NUMBER` /
    /// `BINARY`. The variant carries the canonical primitive name. Sized
    /// forms like `STRING(20)` collapse to bare `STRING`.
    Primitive(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct AttrSpec {
    /// Lowercase attribute name.
    pub name: String,
    /// Right-hand side of the colon, parsed.
    pub ty: AttrType,
}

/// One `DERIVE` target: an attribute the entity computes (rendered `*` on the
/// Part 21 wire). `super_qual` is the supertype of a `SELF\super.attr` form
/// (the attribute is inherited and re-declared derived); `None` for a plain
/// own-attr derive. The `:= expression` right-hand side is not retained.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DerivedTarget {
    pub super_qual: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TypeDef {
    /// Lowercase TYPE alias name.
    pub name: String,
    /// What the alias resolves to. Transitive resolution (`m2 = m1; m1 =
    /// REAL;`) happens at analysis time, not here.
    pub aliased: AttrType,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntitySchema {
    /// `cartesian_point`, `shape_aspect`, etc. (lowercase as per EXPRESS).
    pub name: String,
    /// Direct parents from `SUBTYPE OF (a, b)`, in declaration order.
    /// Multiple inheritance is supported — attribute collection walks
    /// every parent's chain (see `naming::collect_ancestor_attrs`).
    pub parents: Vec<String>,
    /// Attributes declared in this entity (excludes inherited).
    pub own_attrs: Vec<AttrSpec>,
    /// `SELF\supertype.attr : type` redeclarations — type narrowing of an
    /// inherited attribute, not a new attribute. Kept separate from
    /// `own_attrs` so variant.rs / refgraph.rs (which read `own_attrs`)
    /// are unaffected; only `build_attr_types` consumes this.
    pub redeclared_attrs: Vec<AttrSpec>,
    /// `ABSTRACT SUPERTYPE` flag.
    pub is_abstract: bool,
    /// Children declaration extracted from the `SUPERTYPE OF (...)` clause.
    /// `None` means either no SUPERTYPE clause or a parser error (the entity
    /// is then treated as a leaf with `is_abstract` set separately).
    pub supertype_expr: Option<SupertypeExpr>,
    /// `DERIVE` targets (attrs computed → `*` on the wire). Parsed LHS only;
    /// consumed by `universal_export` to mark derived/derivable slots.
    pub derived_attrs: Vec<DerivedTarget>,
}

/// Faithful tree of a SUPERTYPE OF expression. Every shape allowed by
/// EXPRESS § 9.2.4 is representable; downstream classifiers pattern-match
/// for known shapes (single ONEOF, ANDOR mixin, composite OneOf member,
/// etc.) and raise `Unresolved` for the rest.
///
/// Anonymous composition nodes (AndOr / And / OneOf appearing inside
/// another node) are preserved — they have no entity name of their own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SupertypeExpr {
    /// Bare entity reference.
    Entity { name: String },
    /// `ONEOF (a, b, ...)` — exactly one of the children. Always carries
    /// `children.len() >= 2`.
    OneOf { children: Vec<SupertypeExpr> },
    /// `a ANDOR b ANDOR c` — at least one of the children, n-ary flattened.
    /// Always carries `children.len() >= 2`.
    AndOr { children: Vec<SupertypeExpr> },
    /// `a AND b AND c` — all of the children, n-ary flattened. Always
    /// carries `children.len() >= 2`.
    And { children: Vec<SupertypeExpr> },
}

#[derive(Debug, Clone, Serialize)]
pub struct Schema {
    pub source_label: String,
    pub entities: HashMap<String, EntitySchema>,
    pub types: HashMap<String, TypeDef>,
    /// Blocks the parser saw but failed to extract — surfaced for
    /// debugging.
    pub parse_warnings: Vec<String>,
}

/// Load every `*.exp` file under `schemas_dir` as a separate `Schema`.
pub fn load_all_schemas(schemas_dir: &Path) -> Vec<Schema> {
    let mut schemas = Vec::new();
    let entries = match fs::read_dir(schemas_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error reading schemas dir {schemas_dir:?}: {e}");
            return schemas;
        }
    };
    let mut paths: Vec<_> = entries
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("exp"))
        .collect();
    paths.sort();
    for path in paths {
        match parse_express_file(&path) {
            Ok(schema) => schemas.push(schema),
            Err(e) => eprintln!("parse error {path:?}: {e}"),
        }
    }
    schemas
}

pub fn parse_express_file(path: &Path) -> Result<Schema, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read failed: {e}"))?;
    let label = derive_source_label(path);
    let mut entities = HashMap::new();
    let mut types = HashMap::new();
    let mut parse_warnings = Vec::new();

    // Strip `(* ... *)` block comments before scanning so they don't
    // confuse the ENTITY/TYPE block matcher.
    let stripped = strip_block_comments(&text);

    // Walk lines, accumulating ENTITY blocks until END_ENTITY; and TYPE
    // blocks until END_TYPE;.
    let mut current_block = String::new();
    let mut block_kind: Option<BlockKind> = None;
    for line in stripped.lines() {
        let trimmed = line.trim_start();
        match block_kind {
            None => {
                if let Some(rest) = match_keyword_prefix(trimmed, "ENTITY") {
                    block_kind = Some(BlockKind::Entity);
                    current_block.clear();
                    current_block.push_str("ENTITY ");
                    current_block.push_str(rest);
                    current_block.push('\n');
                    if line.contains("END_ENTITY") {
                        process_entity_block(&current_block, &mut entities, &mut parse_warnings);
                        block_kind = None;
                        current_block.clear();
                    }
                } else if let Some(rest) = match_keyword_prefix(trimmed, "TYPE") {
                    block_kind = Some(BlockKind::Type);
                    current_block.clear();
                    current_block.push_str("TYPE ");
                    current_block.push_str(rest);
                    current_block.push('\n');
                    if line.contains("END_TYPE") {
                        process_type_block(&current_block, &mut types, &mut parse_warnings);
                        block_kind = None;
                        current_block.clear();
                    }
                }
            }
            Some(BlockKind::Entity) => {
                current_block.push_str(line);
                current_block.push('\n');
                if line.contains("END_ENTITY") {
                    process_entity_block(&current_block, &mut entities, &mut parse_warnings);
                    block_kind = None;
                    current_block.clear();
                }
            }
            Some(BlockKind::Type) => {
                current_block.push_str(line);
                current_block.push('\n');
                if line.contains("END_TYPE") {
                    process_type_block(&current_block, &mut types, &mut parse_warnings);
                    block_kind = None;
                    current_block.clear();
                }
            }
        }
    }

    Ok(Schema {
        source_label: label,
        entities,
        types,
        parse_warnings,
    })
}

#[derive(Clone, Copy)]
enum BlockKind {
    Entity,
    Type,
}

fn derive_source_label(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    let lower = stem.to_lowercase();
    for suffix in ["_mim_lf", "_aim_lf", "_arm_lf"] {
        if let Some(s) = lower.strip_suffix(suffix) {
            return s.to_string();
        }
    }
    lower
}

fn strip_block_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'(' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b')') {
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i += 2;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Matches `<KEYWORD>` at the start of `line` with a word boundary after
/// (whitespace or non-word character like `[`, `(`, `;`). Case sensitive
/// (EXPRESS keywords are uppercase). Returns the remainder, leading
/// whitespace stripped.
///
/// Permitting `[` immediately after the keyword is needed for forms like
/// `LIST[4:?] OF X` (no space between `LIST` and `[`) seen in AP242.
fn match_keyword_prefix<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    match rest.chars().next() {
        None => Some(rest),
        Some(c) if c.is_alphanumeric() || c == '_' => None, // not a word boundary
        Some(_) => Some(rest.trim_start()),
    }
}

fn process_entity_block(
    block: &str,
    entities: &mut HashMap<String, EntitySchema>,
    warnings: &mut Vec<String>,
) {
    let Some(name) = extract_entity_name(block) else {
        warnings.push(format!("ENTITY name extraction failed:\n{block}"));
        return;
    };
    let parents = extract_parents(block);
    let is_abstract = block.contains("ABSTRACT SUPERTYPE")
        || block.contains("ABSTRACT  SUPERTYPE")
        || block.contains("ABSTRACT\nSUPERTYPE");
    let supertype_expr = extract_supertype_expr(block, &name, warnings);
    let (own_attrs, redeclared_attrs) = extract_attrs(block, &name, warnings);
    let derived_attrs = extract_derived(block);

    entities.insert(
        name.clone(),
        EntitySchema {
            name,
            parents,
            own_attrs,
            redeclared_attrs,
            is_abstract,
            supertype_expr,
            derived_attrs,
        },
    );
}

/// Extract the `SUPERTYPE OF (...)` clause body and parse it into a
/// `SupertypeExpr`. Returns `None` when there is no SUPERTYPE OF clause
/// (the `ABSTRACT SUPERTYPE;` shorthand also lands here — `is_abstract`
/// is set separately) or when the parser rejects the body. In the latter
/// case a `parse_warnings` entry is pushed so callers can surface it.
fn extract_supertype_expr(
    block: &str,
    entity_name: &str,
    warnings: &mut Vec<String>,
) -> Option<SupertypeExpr> {
    let body = find_supertype_of_body(block)?;
    match supertype_parser::parse(&body) {
        Ok(expr) => Some(expr),
        Err(e) => {
            warnings.push(format!(
                "ENTITY {entity_name}: SUPERTYPE OF parse error: {e} (body={body:?})"
            ));
            None
        }
    }
}

/// Find the parenthesized body that follows `SUPERTYPE OF`. Handles
/// nested parens by tracking depth.
fn find_supertype_of_body(block: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?is)\bSUPERTYPE\s+OF\s*\(").ok()?;
    let m = re.find(block)?;
    let after_paren = m.end();
    let bytes = block.as_bytes();
    let mut depth = 1usize;
    let mut i = after_paren;
    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(block[after_paren..i].to_string());
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn process_type_block(
    block: &str,
    types: &mut HashMap<String, TypeDef>,
    warnings: &mut Vec<String>,
) {
    // `TYPE name = repr; [WHERE ...] END_TYPE;`
    let after_type = match block.strip_prefix("TYPE ") {
        Some(s) => s,
        None => {
            warnings.push(format!("TYPE prefix missing:\n{block}"));
            return;
        }
    };
    let Some(eq_pos) = after_type.find('=') else {
        warnings.push(format!("TYPE missing '=':\n{block}"));
        return;
    };
    let name_part = after_type[..eq_pos].trim();
    let name = name_part.to_lowercase();
    if !is_valid_identifier(&name) {
        warnings.push(format!("TYPE invalid name {name_part:?}"));
        return;
    }

    // Read repr from after `=` until the first `;` at paren-depth 0.
    let after_eq = &after_type[eq_pos + 1..];
    let repr_end = match find_top_level_semicolon(after_eq) {
        Some(p) => p,
        None => {
            warnings.push(format!("TYPE {name}: no terminating ';' for aliased repr"));
            return;
        }
    };
    let repr = after_eq[..repr_end].trim();
    let aliased = match parse_type_repr(repr) {
        Ok(t) => t,
        Err(e) => {
            warnings.push(format!("TYPE {name}: parse repr failed ({e}) — repr was {repr:?}"));
            return;
        }
    };
    types.insert(name.clone(), TypeDef { name, aliased });
}

fn extract_entity_name(block: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?i)\bENTITY\s+([a-z_][a-z0-9_]*)").ok()?;
    re.captures(block)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_lowercase())
}

fn extract_parents(block: &str) -> Vec<String> {
    let re = match regex::Regex::new(r"(?is)\bSUBTYPE\s+OF\s*\(([^)]*)\)") {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let Some(caps) = re.captures(block) else {
        return Vec::new();
    };
    let inner = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    inner
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// `SELF\supertype.attr` redeclaration → returns the lowercase `attr`
/// name. EXPRESS standard form is a single `\` followed by
/// `supertype.attr`; we take the token after the last `.` and require it
/// to be a valid identifier. `None` for any non-redeclaration names_part.
fn parse_self_redeclaration(names_part: &str) -> Option<String> {
    let lower = names_part.trim().to_lowercase();
    if !lower.starts_with("self\\") {
        return None;
    }
    let attr = lower.rsplit('.').next()?.trim();
    if is_valid_identifier(attr) {
        Some(attr.to_string())
    } else {
        None
    }
}

/// Walk the entity body (after the header `;`, before the first
/// terminator keyword) and split into ATTR definitions by `;` at
/// paren-depth 0. Returns `(own_attrs, redeclared_attrs)` — a segment
/// whose name part is a `SELF\supertype.attr` redeclaration lands in the
/// second vector (type narrowing of an inherited attr), everything else
/// in the first. A single non-redeclaration segment can declare multiple
/// comma-separated names sharing one type.
fn extract_attrs(
    block: &str,
    entity_name: &str,
    warnings: &mut Vec<String>,
) -> (Vec<AttrSpec>, Vec<AttrSpec>) {
    let body = match find_attribute_section(block) {
        Some(b) => b,
        None => return (Vec::new(), Vec::new()),
    };

    let mut own = Vec::new();
    let mut redeclared = Vec::new();
    for segment in split_top_level_semicolons(body) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        // Find the first `:` at paren-depth 0 to split name(s) from type.
        let Some(colon) = find_top_level_colon(segment) else {
            // Probably a stray fragment (rule body remnant or such). Skip.
            continue;
        };
        let names_part = segment[..colon].trim();
        let type_part = segment[colon + 1..].trim();
        if type_part.is_empty() {
            continue;
        }
        // `SELF\supertype.attr : type` — redeclaration (attr type narrowing).
        let redeclared_name = parse_self_redeclaration(names_part);
        let names: Vec<String> = if redeclared_name.is_some() {
            Vec::new()
        } else {
            names_part
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty() && is_valid_identifier(s))
                .collect()
        };
        if redeclared_name.is_none() && names.is_empty() {
            continue;
        }
        let ty = match parse_type_repr(type_part) {
            Ok(t) => t,
            Err(e) => {
                let label = redeclared_name.clone().unwrap_or_else(|| format!("{names:?}"));
                warnings.push(format!(
                    "{entity_name}: ATTR type parse failed for {label} ({e}) — type was {type_part:?}"
                ));
                continue;
            }
        };
        if let Some(name) = redeclared_name {
            redeclared.push(AttrSpec { name, ty });
        } else {
            for n in names {
                own.push(AttrSpec {
                    name: n,
                    ty: ty.clone(),
                });
            }
        }
    }
    (own, redeclared)
}

fn find_attribute_section(block: &str) -> Option<&str> {
    // First `;` after the ENTITY header closes SUBTYPE OF / SUPERTYPE OF.
    // The header itself can span multiple lines and contain parens (ONEOF,
    // ANDOR), so use paren-depth tracking.
    let header_end = find_top_level_semicolon(block)?;
    let body = &block[header_end + 1..];
    // Section terminators appear only at line start (after optional
    // whitespace). Inline `UNIQUE` (e.g. `LIST OF UNIQUE oriented_edge`)
    // must not match. The other terminators are also section keywords
    // that always stand at line start in well-formed EXPRESS, but
    // restricting them by line position is safer regardless.
    let re = regex::Regex::new(r"(?m)^\s*(DERIVE|INVERSE|WHERE|UNIQUE|END_ENTITY)\b").unwrap();
    let end = re.find(body).map(|m| m.start()).unwrap_or(body.len());
    Some(&body[..end])
}

/// The `DERIVE` section body (between the `DERIVE` keyword and the next section
/// terminator), or `None` when the entity has no DERIVE clause. Parallel to
/// [`find_attribute_section`] but for the derived block instead of skipping it.
fn find_derive_section(block: &str) -> Option<&str> {
    let header_end = find_top_level_semicolon(block)?;
    let body = &block[header_end + 1..];
    let start = regex::Regex::new(r"(?m)^\s*DERIVE\b").unwrap().find(body)?;
    let after = &body[start.end()..];
    let end = regex::Regex::new(r"(?m)^\s*(INVERSE|WHERE|UNIQUE|END_ENTITY)\b")
        .unwrap()
        .find(after)
        .map(|m| m.start())
        .unwrap_or(after.len());
    Some(&after[..end])
}

/// Parse the LHS of one DERIVE statement (`SELF\super.attr` or `attr`, before
/// the first `:`). The `:= expression` RHS is ignored.
fn parse_derive_lhs(lhs: &str) -> Option<DerivedTarget> {
    let lower = lhs.trim().to_lowercase();
    if let Some(rest) = lower.strip_prefix("self\\") {
        // `super.attr` — attr after the last `.`, supertype before it.
        let (sup, attr) = rest.rsplit_once('.')?;
        let (sup, attr) = (sup.trim(), attr.trim());
        (is_valid_identifier(sup) && is_valid_identifier(attr)).then(|| DerivedTarget {
            super_qual: Some(sup.to_string()),
            name: attr.to_string(),
        })
    } else {
        let attr = lower.trim();
        is_valid_identifier(attr).then(|| DerivedTarget {
            super_qual: None,
            name: attr.to_string(),
        })
    }
}

/// Walk the DERIVE section and collect each statement's target (LHS only).
fn extract_derived(block: &str) -> Vec<DerivedTarget> {
    let Some(body) = find_derive_section(block) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for segment in split_top_level_semicolons(body) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let Some(colon) = find_top_level_colon(segment) else {
            continue;
        };
        if let Some(t) = parse_derive_lhs(segment[..colon].trim()) {
            out.push(t);
        }
    }
    out
}

/// Returns byte index of the first top-level (paren-depth 0) `;`.
fn find_top_level_semicolon(s: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ';' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Returns byte index of the first top-level (paren-depth 0) `:`.
fn find_top_level_colon(s: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ':' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Split `s` at each top-level `;`. Result excludes the separators.
fn split_top_level_semicolons(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ';' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Split `s` at each top-level `,` (paren-depth 0). Trims and lowercases
/// each result.
fn split_top_level_commas_lower(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                let item = s[start..i].trim();
                if !item.is_empty() {
                    out.push(item.to_lowercase());
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = s[start..].trim();
    if !last.is_empty() {
        out.push(last.to_lowercase());
    }
    out
}

fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

const PRIMITIVES: &[&str] = &[
    "INTEGER", "REAL", "STRING", "LOGICAL", "BOOLEAN", "NUMBER", "BINARY",
];

/// Parse a type repr like `OPTIONAL LIST [1:?] OF cartesian_point` into
/// an `AttrType`. Whitespace and newlines tolerated. Case sensitive on
/// keywords (EXPRESS uses uppercase by convention).
pub fn parse_type_repr(input: &str) -> Result<AttrType, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("empty type repr".into());
    }

    // OPTIONAL prefix
    if let Some(rest) = match_keyword_prefix(trimmed, "OPTIONAL") {
        return Ok(AttrType::Optional(Box::new(parse_type_repr(rest)?)));
    }

    // Aggregations: LIST/SET/BAG/ARRAY [bound] OF [UNIQUE | OPTIONAL] inner
    for (kw, ctor) in [
        ("LIST", AttrType::List as fn(Box<AttrType>) -> AttrType),
        ("SET", AttrType::Set),
        ("BAG", AttrType::Bag),
        ("ARRAY", AttrType::Array),
    ] {
        if let Some(rest) = match_keyword_prefix(trimmed, kw) {
            let after_bound = strip_bracket_bound(rest).trim_start();
            let after_of = match_keyword_prefix(after_bound, "OF").ok_or_else(|| {
                format!("{kw} not followed by OF: {after_bound:?}")
            })?;
            // Skip optional UNIQUE / OPTIONAL modifier inside aggregation.
            let after_mod = match_keyword_prefix(after_of.trim_start(), "UNIQUE")
                .or_else(|| match_keyword_prefix(after_of.trim_start(), "OPTIONAL"))
                .unwrap_or(after_of);
            let inner = parse_type_repr(after_mod)?;
            return Ok(ctor(Box::new(inner)));
        }
    }

    // SELECT (a, b, c) — paren content is comma-separated names.
    if let Some(rest) = match_keyword_prefix(trimmed, "SELECT") {
        let inside = extract_paren_content(rest)?;
        return Ok(AttrType::Select(split_top_level_commas_lower(inside)));
    }

    // ENUMERATION [BASED_ON parent WITH] OF (a, b, c) — only the value list
    // matters for our purpose; treat BASED_ON parent reference as an
    // ordinary enumeration (parent's values are inherited but we don't
    // care for analysis).
    if let Some(rest) = match_keyword_prefix(trimmed, "ENUMERATION") {
        // Skip optional `BASED_ON parent WITH` chain.
        let mut cur = rest.trim_start();
        if let Some(after) = match_keyword_prefix(cur, "BASED_ON") {
            let with_pos = after.to_uppercase().find("WITH").ok_or_else(|| {
                format!("ENUMERATION BASED_ON without WITH: {after:?}")
            })?;
            cur = after[with_pos + "WITH".len()..].trim_start();
        }
        let after_of = match_keyword_prefix(cur, "OF").ok_or_else(|| {
            format!("ENUMERATION not followed by OF: {cur:?}")
        })?;
        let inside = extract_paren_content(after_of)?;
        return Ok(AttrType::Enumeration(split_top_level_commas_lower(inside)));
    }

    // Primitive (possibly sized like STRING(20) / BINARY(8)).
    let upper = trimmed.to_uppercase();
    for prim in PRIMITIVES {
        if upper == *prim {
            return Ok(AttrType::Primitive((*prim).to_string()));
        }
        let with_paren = format!("{prim}(");
        if upper.starts_with(&with_paren) {
            return Ok(AttrType::Primitive((*prim).to_string()));
        }
        // STRING FIXED / BINARY FIXED variants are rare; accept by prefix.
        let with_space = format!("{prim} ");
        if upper.starts_with(&with_space) {
            return Ok(AttrType::Primitive((*prim).to_string()));
        }
    }

    // Identifier — entity ref or TYPE alias. Strip trailing whitespace.
    let id = trimmed.trim_end_matches(|c: char| c.is_whitespace() || c == ';');
    if is_valid_identifier(id) {
        return Ok(AttrType::Entity(id.to_lowercase()));
    }

    Err(format!("unrecognised type repr: {trimmed:?}"))
}

/// Strip a leading `[...]` bound clause (paren/bracket-aware). Returns the
/// remainder (possibly with leading whitespace).
fn strip_bracket_bound(s: &str) -> &str {
    let s = s.trim_start();
    if !s.starts_with('[') {
        return s;
    }
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return &s[i + 1..];
                }
            }
            _ => {}
        }
    }
    s
}

/// From a string starting with `(...)` (whitespace tolerated before the
/// `(`), return the content between matched parens.
fn extract_paren_content(s: &str) -> Result<&str, String> {
    let s = s.trim_start();
    if !s.starts_with('(') {
        return Err(format!("expected '(' at start of {s:?}"));
    }
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&s[1..i]);
                }
            }
            _ => {}
        }
    }
    Err(format!("unterminated paren in {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_attrs_for(block: &str) -> Vec<AttrSpec> {
        let name = extract_entity_name(block).unwrap();
        let mut warnings = Vec::new();
        extract_attrs(block, &name, &mut warnings).0
    }

    fn parse_redeclared_for(block: &str) -> Vec<AttrSpec> {
        let name = extract_entity_name(block).unwrap();
        let mut warnings = Vec::new();
        extract_attrs(block, &name, &mut warnings).1
    }

    #[test]
    fn redeclaration_primitive_narrowing() {
        // int_literal narrows literal_number.the_value NUMBER -> INTEGER.
        let block = "ENTITY int_literal\n  SUBTYPE OF ( literal_number );\n    SELF\\literal_number.the_value : INTEGER;\nEND_ENTITY;";
        let own = parse_attrs_for(block);
        let redeclared = parse_redeclared_for(block);
        assert!(own.is_empty(), "redeclaration must not land in own_attrs");
        assert_eq!(redeclared.len(), 1);
        assert_eq!(redeclared[0].name, "the_value");
        match &redeclared[0].ty {
            AttrType::Primitive(p) => assert_eq!(p, "INTEGER"),
            other => panic!("expected INTEGER, got {other:?}"),
        }
    }

    #[test]
    fn redeclaration_entity_ref_narrowing() {
        let block = "ENTITY annotation_curve_occurrence\n  SUBTYPE OF ( annotation_occurrence );\n    SELF\\styled_item.item : curve_or_curve_set;\nEND_ENTITY;";
        let own = parse_attrs_for(block);
        let redeclared = parse_redeclared_for(block);
        assert!(own.is_empty());
        assert_eq!(redeclared.len(), 1);
        assert_eq!(redeclared[0].name, "item");
        match &redeclared[0].ty {
            AttrType::Entity(n) => assert_eq!(n, "curve_or_curve_set"),
            other => panic!("expected Entity, got {other:?}"),
        }
    }

    #[test]
    fn redeclaration_mixed_with_plain_attr() {
        // A block with both a plain own attr and a redeclaration.
        let block = "ENTITY mixed\n  SUBTYPE OF ( parent );\n    extra : label;\n    SELF\\parent.item : narrowed_type;\nEND_ENTITY;";
        let own = parse_attrs_for(block);
        let redeclared = parse_redeclared_for(block);
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].name, "extra");
        assert_eq!(redeclared.len(), 1);
        assert_eq!(redeclared[0].name, "item");
    }

    #[test]
    fn parses_cartesian_point() {
        let block = "ENTITY cartesian_point\n  SUBTYPE OF ( point );\n    coordinates : LIST [1 : 3] OF length_measure;\nEND_ENTITY;";
        let attrs = parse_attrs_for(block);
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].name, "coordinates");
        match &attrs[0].ty {
            AttrType::List(inner) => match inner.as_ref() {
                AttrType::Entity(n) => assert_eq!(n, "length_measure"),
                other => panic!("expected Entity inside LIST, got {other:?}"),
            },
            other => panic!("expected LIST, got {other:?}"),
        }
    }

    #[test]
    fn parses_shape_aspect() {
        let block = "ENTITY shape_aspect\n  SUPERTYPE OF (\n      ONEOF (\n          contacting_feature,\n          datum,\n          datum_feature,\n          datum_target));\n  name : label;\n  description : text;\n  of_shape : product_definition_shape;\n  product_definitional : LOGICAL;\nEND_ENTITY;";
        let attrs = parse_attrs_for(block);
        let names: Vec<_> = attrs.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["name", "description", "of_shape", "product_definitional"]);
        match &attrs[3].ty {
            AttrType::Primitive(p) => assert_eq!(p, "LOGICAL"),
            other => panic!("expected LOGICAL, got {other:?}"),
        }
    }

    #[test]
    fn skips_derive_and_where_sections() {
        let block = "ENTITY representation_item;\n  name : label;\nDERIVE\n  derived_attr : INTEGER := 42;\nWHERE\n  WR1: TRUE;\nEND_ENTITY;";
        let attrs = parse_attrs_for(block);
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].name, "name");
    }

    #[test]
    fn detects_abstract_supertype() {
        let block = "ENTITY geometric_representation_item\n  ABSTRACT SUPERTYPE\n  SUBTYPE OF ( representation_item );\n  WHERE\n    WR1: TRUE;\nEND_ENTITY;";
        let mut entities = HashMap::new();
        let mut warnings = Vec::new();
        process_entity_block(block, &mut entities, &mut warnings);
        let e = entities.get("geometric_representation_item").expect("parsed");
        assert!(e.is_abstract);
        assert_eq!(e.parents, vec!["representation_item"]);
    }

    #[test]
    fn strips_block_comments_helper() {
        let text = "ENTITY foo (* a comment *)\n  SUBTYPE OF ( bar );\n  x : INTEGER;\nEND_ENTITY;";
        let stripped = strip_block_comments(text);
        assert!(!stripped.contains("a comment"));
        assert!(stripped.contains("ENTITY foo"));
    }

    #[test]
    fn parses_multi_line_attr() {
        let block = "ENTITY foo;\n  bar : LIST [1 : ?] OF\n         length_measure;\n  baz : SET OF surface;\nEND_ENTITY;";
        let attrs = parse_attrs_for(block);
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[0].name, "bar");
        assert_eq!(attrs[1].name, "baz");
    }

    #[test]
    fn parses_select_select_and_optional() {
        // OPTIONAL wrapping a SELECT alias, plus an inline SELECT-typed attr.
        let block = "ENTITY foo;\n  a : OPTIONAL my_select;\n  b : SELECT (alpha, beta);\nEND_ENTITY;";
        let attrs = parse_attrs_for(block);
        assert_eq!(attrs.len(), 2);
        match &attrs[0].ty {
            AttrType::Optional(inner) => match inner.as_ref() {
                AttrType::Entity(n) => assert_eq!(n, "my_select"),
                other => panic!("expected Entity inside OPTIONAL, got {other:?}"),
            },
            other => panic!("expected OPTIONAL, got {other:?}"),
        }
        match &attrs[1].ty {
            AttrType::Select(names) => assert_eq!(names, &vec!["alpha".to_string(), "beta".to_string()]),
            other => panic!("expected SELECT, got {other:?}"),
        }
    }

    #[test]
    fn parses_set_bag_array() {
        for (kw, ctor_check) in [
            ("SET", "SET"),
            ("BAG", "BAG"),
            ("ARRAY", "ARRAY"),
        ] {
            let block = format!("ENTITY foo;\n  x : {kw} [1 : 5] OF point;\nEND_ENTITY;");
            let attrs = parse_attrs_for(&block);
            assert_eq!(attrs.len(), 1);
            let ok = match &attrs[0].ty {
                AttrType::Set(_) if ctor_check == "SET" => true,
                AttrType::Bag(_) if ctor_check == "BAG" => true,
                AttrType::Array(_) if ctor_check == "ARRAY" => true,
                _ => false,
            };
            assert!(ok, "expected {ctor_check} for {kw}, got {:?}", attrs[0].ty);
        }
    }

    #[test]
    fn parses_primitives_with_size() {
        let block = "ENTITY foo;\n  s : STRING(20);\n  s2 : STRING;\n  i : INTEGER;\n  r : REAL;\n  l : LOGICAL;\nEND_ENTITY;";
        let attrs = parse_attrs_for(block);
        let prims: Vec<&str> = attrs.iter().filter_map(|a| match &a.ty {
            AttrType::Primitive(p) => Some(p.as_str()),
            _ => None,
        }).collect();
        assert_eq!(prims, vec!["STRING", "STRING", "INTEGER", "REAL", "LOGICAL"]);
    }

    #[test]
    fn parses_type_alias() {
        let block = "TYPE length_measure = REAL;\n  END_TYPE;\n";
        let mut types = HashMap::new();
        let mut warnings = Vec::new();
        process_type_block(block, &mut types, &mut warnings);
        let td = types.get("length_measure").expect("parsed");
        match &td.aliased {
            AttrType::Primitive(p) => assert_eq!(p, "REAL"),
            other => panic!("expected REAL, got {other:?}"),
        }
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
    }

    #[test]
    fn parses_type_select() {
        let block = "TYPE shape_definition = SELECT\n    ( shape_aspect,\n     shape_aspect_relationship,\n     property_definition );\n  END_TYPE;\n";
        let mut types = HashMap::new();
        let mut warnings = Vec::new();
        process_type_block(block, &mut types, &mut warnings);
        let td = types.get("shape_definition").expect("parsed");
        match &td.aliased {
            AttrType::Select(names) => assert_eq!(
                names,
                &vec![
                    "shape_aspect".to_string(),
                    "shape_aspect_relationship".to_string(),
                    "property_definition".to_string(),
                ]
            ),
            other => panic!("expected SELECT, got {other:?}"),
        }
    }

    #[test]
    fn parses_type_enumeration() {
        let block = "TYPE actuated_direction = ENUMERATION OF\n    ( bidirectional,\n     positive_only,\n     negative_only,\n     not_actuated );\n  END_TYPE;\n";
        let mut types = HashMap::new();
        let mut warnings = Vec::new();
        process_type_block(block, &mut types, &mut warnings);
        let td = types.get("actuated_direction").expect("parsed");
        match &td.aliased {
            AttrType::Enumeration(names) => assert_eq!(names.len(), 4),
            other => panic!("expected ENUMERATION, got {other:?}"),
        }
    }

    #[test]
    fn parses_type_with_where_clause() {
        let block = "TYPE day_in_year_number = INTEGER;\n  WHERE\n    wr1: ( ( 1 <= SELF ) AND ( SELF <= 366 ) );\n  END_TYPE;\n";
        let mut types = HashMap::new();
        let mut warnings = Vec::new();
        process_type_block(block, &mut types, &mut warnings);
        let td = types.get("day_in_year_number").expect("parsed");
        match &td.aliased {
            AttrType::Primitive(p) => assert_eq!(p, "INTEGER"),
            other => panic!("expected INTEGER, got {other:?}"),
        }
    }

    #[test]
    fn parses_type_list_alias() {
        let block = "TYPE common_datum_list = LIST [2 : ?] OF datum_reference_element;\n  END_TYPE;\n";
        let mut types = HashMap::new();
        let mut warnings = Vec::new();
        process_type_block(block, &mut types, &mut warnings);
        let td = types.get("common_datum_list").expect("parsed");
        match &td.aliased {
            AttrType::List(inner) => match inner.as_ref() {
                AttrType::Entity(n) => assert_eq!(n, "datum_reference_element"),
                other => panic!("expected Entity inside LIST, got {other:?}"),
            },
            other => panic!("expected LIST, got {other:?}"),
        }
    }

    /// Smoke test against the real 4 schemas in `schemas/`. Verifies that
    /// parsing completes for every file and that entity / type counts
    /// land in plausible ranges, with no parser warnings.
    #[test]
    fn parses_all_real_schemas() {
        let schemas = load_all_schemas(Path::new("schemas"));
        assert_eq!(schemas.len(), 6, "expected 6 schemas, got {}", schemas.len());

        let by_label: HashMap<&str, &Schema> = schemas.iter().map(|s| (s.source_label.as_str(), s)).collect();
        for label in ["ap203", "ap203e2", "ap214e3", "ap242"] {
            let s = by_label.get(label).unwrap_or_else(|| panic!("missing schema {label}"));
            assert!(
                s.entities.len() >= 100,
                "{label}: too few entities ({}). likely a parser regression",
                s.entities.len()
            );
            assert!(
                s.parse_warnings.is_empty(),
                "{label}: {} parser warnings — first: {:?}",
                s.parse_warnings.len(),
                s.parse_warnings.first()
            );
        }

        // AP242 is the most comprehensive — sanity bound.
        let ap242 = by_label.get("ap242").unwrap();
        assert!(
            ap242.entities.len() >= 700,
            "ap242: entity count {} below expected lower bound 700",
            ap242.entities.len()
        );
        assert!(
            ap242.types.len() >= 200,
            "ap242: type count {} below expected lower bound 200",
            ap242.types.len()
        );

        // AP203 is small; mainly here to catch the schema-loader skipping it.
        let ap203 = by_label.get("ap203").unwrap();
        assert!(
            ap203.entities.len() >= 100,
            "ap203: entity count {} suspiciously low",
            ap203.entities.len()
        );

        // Non-trivial SUPERTYPE patterns surveyed during planning. Each
        // must have a Some supertype_expr — the parser cannot have
        // silently dropped any of them.
        let b7_entities: &[(&str, &str)] = &[
            ("ap203", "surface_curve"),
            ("ap203e2", "b_spline_curve"),
            ("ap203e2", "b_spline_surface"),
            ("ap203e2", "draughting_callout"),
            ("ap203e2", "edge_blended_solid"),
            ("ap203e2", "named_unit"),
            ("ap203e2", "solid_with_depression"),
            ("ap203e2", "solid_with_slot"),
            ("ap203e2", "solid_with_stepped_round_hole"),
            ("ap203e2", "topological_representation_item"),
            ("ap203e2", "zone_structural_makeup"),
            ("ap214e3", "surface_curve"),
            ("ap242", "solid_with_slot"),
        ];
        for (schema_label, entity_name) in b7_entities {
            let s = by_label.get(schema_label).unwrap_or_else(|| {
                panic!("missing schema {schema_label}")
            });
            let ent = s.entities.get(*entity_name).unwrap_or_else(|| {
                panic!("missing entity {schema_label}/{entity_name}")
            });
            assert!(
                ent.supertype_expr.is_some(),
                "{schema_label}/{entity_name}: supertype_expr unexpectedly None — silent fallback?"
            );
        }

        // B5 regression: solid_with_slot uses ONEOF AND ONEOF, the only
        // entity in the corpus that triggers the And keyword path.
        let slot = by_label
            .get("ap203e2")
            .unwrap()
            .entities
            .get("solid_with_slot")
            .unwrap();
        assert!(
            matches!(&slot.supertype_expr, Some(SupertypeExpr::And { .. })),
            "ap203e2/solid_with_slot: expected And {{ .. }} at root, got {:?}",
            slot.supertype_expr
        );

        // B7 tree-preservation regression: topological_representation_item
        // must keep its anonymous AndOr member intact (not flatten loop +
        // path into separate alternatives, which is what the old silent
        // parser did).
        let trep = by_label
            .get("ap203e2")
            .unwrap()
            .entities
            .get("topological_representation_item")
            .unwrap();
        let Some(SupertypeExpr::OneOf { children }) = &trep.supertype_expr else {
            panic!(
                "topological_representation_item: expected OneOf root, got {:?}",
                trep.supertype_expr
            );
        };
        let composite_count = children
            .iter()
            .filter(|c| matches!(c, SupertypeExpr::AndOr { .. }))
            .count();
        assert_eq!(
            composite_count, 1,
            "topological_representation_item: expected exactly 1 anonymous AndOr child, got {composite_count}"
        );
    }
}
