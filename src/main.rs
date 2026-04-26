//! step-io-schema-check — entity inventory + 분류 catalog tool.
//!
//! Two CLI modes:
//! - default (no args, or `check`): trait-introspection mismatch report
//!   (currently unimplemented — pending step-io trait refactor).
//! - `catalog` sub-command: regenerate ENTITY_CATALOG.md + entity_catalog.json
//!   from `schemas/*.exp` and `groups.toml`. Schema-only — no external code scan.
//!
//! Run from the project root:
//!     cargo run --release -- catalog       # one-time entity classification
//!     cargo run --release                  # default: check (placeholder)

use std::env;
use std::path::Path;

mod catalog;
mod check;
mod express;
mod inheritance;

fn main() {
    match env::args().nth(1).as_deref() {
        Some("catalog") => run_catalog(),
        None | Some("check") => check::run(),
        Some(other) => {
            eprintln!(
                "unknown sub-command: {other}\nusage:\n  cargo run -- catalog   # entity catalog\n  cargo run               # default: check (placeholder)"
            );
            std::process::exit(2);
        }
    }
}

fn run_catalog() {
    let schemas_dir = Path::new("schemas");
    if !schemas_dir.exists() {
        eprintln!("schemas/ not found in cwd — run from project root.");
        std::process::exit(2);
    }
    println!("Loading schemas from {schemas_dir:?}...");
    let schemas = express::load_all_schemas(schemas_dir);
    if schemas.is_empty() {
        eprintln!("no schemas loaded — check schemas/*.exp files.");
        std::process::exit(2);
    }
    for s in &schemas {
        println!(
            "  {}: {} entities ({} parser warnings)",
            s.source_label,
            s.entities.len(),
            s.parse_warnings.len()
        );
    }

    let groups_toml = Path::new("groups.toml");
    let catalog = match catalog::build_catalog(&schemas, groups_toml) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("build catalog failed: {e}");
            std::process::exit(2);
        }
    };

    let md_path = Path::new("ENTITY_CATALOG.md");
    let json_path = Path::new("entity_catalog.json");
    if let Err(e) = catalog::write_markdown(&catalog, md_path) {
        eprintln!("write markdown failed: {e}");
        std::process::exit(2);
    }
    if let Err(e) = catalog::write_json(&catalog, json_path) {
        eprintln!("write json failed: {e}");
        std::process::exit(2);
    }
    println!("\nWrote ENTITY_CATALOG.md and entity_catalog.json");

    // Quick distribution summary.
    println!("\nGroup distribution:");
    let mut groups: Vec<_> = catalog.groups.iter().collect();
    groups.sort_by_key(|(_, s)| std::cmp::Reverse(s.count));
    for (name, summary) in groups {
        if summary.count == 0 {
            continue;
        }
        println!("  {:<26} {:>5} entities", name, summary.count);
    }
}
