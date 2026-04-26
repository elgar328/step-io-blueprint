//! EXPRESS schema parser — extract ENTITY definitions (name + parents +
//! own attributes) from `.exp` files. Hand-rolled regex + state machine;
//! sufficient for AP203 / AP203e2 / AP214e3 / AP242 of stepcode.
//!
//! Out of scope: TYPE definitions, RULE / FUNCTION / PROCEDURE blocks,
//! DERIVE / INVERSE / WHERE / UNIQUE attribute extraction (those don't
//! contribute to STEP-encoded attribute count). SUPERTYPE OF clauses
//! are read for ABSTRACT detection but not for inverse-walking parents
//! (we only follow SUBTYPE OF).

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct EntitySchema {
    /// `cartesian_point`, `shape_aspect`, etc. (lowercase as per EXPRESS).
    pub name: String,
    /// Direct parents from `SUBTYPE OF (a, b)`. Multi-parent (multiple
    /// inheritance) is rare in practice; first parent's chain dominates.
    pub parents: Vec<String>,
    /// Names of attributes declared in this entity (excludes inherited).
    pub own_attrs: Vec<String>,
    /// `ABSTRACT SUPERTYPE` flag.
    pub is_abstract: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Schema {
    pub source_label: String,
    pub entities: HashMap<String, EntitySchema>,
    /// ENTITY blocks the parser saw but failed to extract — surfaced in
    /// the catalog's "Parser warnings" section.
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
    let mut parse_warnings = Vec::new();

    // Strip `(* ... *)` block comments before scanning so they don't
    // confuse the ENTITY block matcher. Comments can span lines and may
    // appear inside attribute lines; safest to remove globally.
    let stripped = strip_block_comments(&text);

    // Walk the file line by line, accumulating ENTITY blocks until
    // END_ENTITY. Inside each block we extract name / parents / abstract /
    // own attributes (stopping at DERIVE / INVERSE / WHERE / UNIQUE).
    let mut in_entity = false;
    let mut current_block = String::new();
    for line in stripped.lines() {
        let trimmed = line.trim_start();
        if !in_entity {
            // Entity block header may be split across lines; we collect
            // any line starting with "ENTITY <name>" and then keep
            // appending until END_ENTITY;
            if let Some(rest) = trimmed.strip_prefix("ENTITY ") {
                in_entity = true;
                current_block.clear();
                current_block.push_str("ENTITY ");
                current_block.push_str(rest);
                current_block.push('\n');
                if rest.contains("END_ENTITY") {
                    in_entity = false;
                    process_entity_block(&current_block, &mut entities, &mut parse_warnings);
                    current_block.clear();
                }
            }
        } else {
            current_block.push_str(line);
            current_block.push('\n');
            if line.contains("END_ENTITY") {
                in_entity = false;
                process_entity_block(&current_block, &mut entities, &mut parse_warnings);
                current_block.clear();
            }
        }
    }

    Ok(Schema {
        source_label: label,
        entities,
        parse_warnings,
    })
}

fn derive_source_label(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    // Strip common suffixes so all 4 STEP schemas labelize to short
    // identifiers: "ap203", "ap203e2", "ap214e3", "ap242".
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
            // Skip until "*)"
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b')') {
                // Preserve newlines so line numbers stay close (helps
                // future debugging).
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i += 2; // skip "*)"
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn process_entity_block(
    block: &str,
    entities: &mut HashMap<String, EntitySchema>,
    warnings: &mut Vec<String>,
) {
    // 1. ENTITY name
    let Some(name) = extract_entity_name(block) else {
        warnings.push(format!("ENTITY name extraction failed in block:\n{block}"));
        return;
    };

    // 2. SUBTYPE OF (a, b, ...)
    let parents = extract_parents(block);

    // 3. ABSTRACT SUPERTYPE flag
    let is_abstract = block.contains("ABSTRACT SUPERTYPE")
        || block.contains("ABSTRACT  SUPERTYPE")
        || block.contains("ABSTRACT\nSUPERTYPE");

    // 4. own attributes — between the entity header and the first DERIVE /
    //    INVERSE / WHERE / UNIQUE block (or END_ENTITY when none).
    let own_attrs = extract_own_attrs(block);

    entities.insert(
        name.clone(),
        EntitySchema {
            name,
            parents,
            own_attrs,
            is_abstract,
        },
    );
}

fn extract_entity_name(block: &str) -> Option<String> {
    // Match "ENTITY <name>" — optional whitespace, identifier chars only.
    let re = regex::Regex::new(r"(?i)\bENTITY\s+([a-z_][a-z0-9_]*)").ok()?;
    re.captures(block)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_lowercase())
}

fn extract_parents(block: &str) -> Vec<String> {
    // SUBTYPE OF ( parent_a, parent_b ) — capture parens content.
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

fn extract_own_attrs(block: &str) -> Vec<String> {
    // Locate the "header" portion — after ENTITY ... ; up to the first
    // DERIVE / INVERSE / WHERE / UNIQUE / END_ENTITY keyword.
    let body = match find_attribute_section(block) {
        Some(b) => b,
        None => return Vec::new(),
    };
    // Each attribute line: `name [, name2] : type;` — capture identifiers
    // before the colon.
    let mut attrs = Vec::new();
    let line_re = regex::Regex::new(r"(?im)^\s*([a-z_][a-z0-9_,\s]*?)\s*:").unwrap();
    for caps in line_re.captures_iter(body) {
        if let Some(m) = caps.get(1) {
            for raw in m.as_str().split(',') {
                let name = raw.trim().to_lowercase();
                // skip common false positives (RULE/TYPE definitions, the
                // "subtype of" phrasing, etc.)
                if name.is_empty()
                    || name.contains(' ')
                    || matches!(
                        name.as_str(),
                        "subtype" | "supertype" | "where" | "unique" | "derive" | "inverse"
                    )
                {
                    continue;
                }
                attrs.push(name);
            }
        }
    }
    attrs
}

fn find_attribute_section(block: &str) -> Option<&str> {
    // Find first ';' after the ENTITY header (closes SUBTYPE OF / SUPERTYPE
    // OF clause) and the start of any terminator keyword (DERIVE / INVERSE
    // / WHERE / UNIQUE / END_ENTITY).
    let header_end = block.find(';')?;
    let body = &block[header_end + 1..];
    let mut end = body.len();
    for kw in ["DERIVE", "INVERSE", "WHERE", "UNIQUE", "END_ENTITY"] {
        if let Some(idx) = body.find(kw) {
            end = end.min(idx);
        }
    }
    Some(&body[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cartesian_point() {
        let block = "ENTITY cartesian_point\n  SUBTYPE OF ( point );\n    coordinates : LIST [1 : 3] OF length_measure;\nEND_ENTITY;";
        let mut entities = HashMap::new();
        let mut warnings = Vec::new();
        process_entity_block(block, &mut entities, &mut warnings);
        let e = entities.get("cartesian_point").expect("parsed");
        assert_eq!(e.parents, vec!["point"]);
        assert_eq!(e.own_attrs, vec!["coordinates"]);
        assert!(!e.is_abstract);
        assert!(warnings.is_empty());
    }

    #[test]
    fn parses_shape_aspect() {
        let block = "ENTITY shape_aspect\n  SUPERTYPE OF (\n      ONEOF (\n          contacting_feature,\n          datum,\n          datum_feature,\n          datum_target));\n  name : label;\n  description : text;\n  of_shape : product_definition_shape;\n  product_definitional : LOGICAL;\nEND_ENTITY;";
        let mut entities = HashMap::new();
        let mut warnings = Vec::new();
        process_entity_block(block, &mut entities, &mut warnings);
        let e = entities.get("shape_aspect").expect("parsed");
        assert_eq!(
            e.own_attrs,
            vec!["name", "description", "of_shape", "product_definitional"]
        );
        assert_eq!(e.parents, Vec::<String>::new());
        assert!(warnings.is_empty());
    }

    #[test]
    fn skips_derive_and_where_sections() {
        let block = "ENTITY representation_item;\n  name : label;\nDERIVE\n  derived_attr : INTEGER := 42;\nWHERE\n  WR1: TRUE;\nEND_ENTITY;";
        let mut entities = HashMap::new();
        let mut warnings = Vec::new();
        process_entity_block(block, &mut entities, &mut warnings);
        let e = entities.get("representation_item").expect("parsed");
        assert_eq!(e.own_attrs, vec!["name"]);
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
    fn strips_block_comments() {
        let text = "ENTITY foo (* a comment *)\n  SUBTYPE OF ( bar );\n  x : INTEGER;\nEND_ENTITY;";
        let stripped = strip_block_comments(text);
        assert!(!stripped.contains("a comment"));
        assert!(stripped.contains("ENTITY foo"));
    }
}
