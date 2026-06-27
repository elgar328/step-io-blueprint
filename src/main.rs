//! step-io-blueprint — EXPRESS-schema-driven faithful exporters.
//!
//! Reads `schemas/*.exp` and emits the step-io codegen inputs:
//! - `universal_export` → `inferred/universal.toml` — schema-faithful union of
//!   all AP schemas + per-entity DERIVE facts (the `codegen` generator's input).
//!   Also reads the frozen `inferred/corpus_usage.toml` for MI complex-part recovery.
//! - `profile_export`   → `profiles/<target>.toml` — per-target output profiles
//!   (legal entity set + ordered attrs for schema-conditioned writing).

use std::env;
use std::path::Path;
use std::process::ExitCode;

mod express;
mod infer;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() > 1 {
        eprintln!("unexpected argument: {}", args[1]);
        print_usage();
        return ExitCode::from(2);
    }
    match args.first().map(String::as_str) {
        Some("universal_export") => run_universal_export(),
        Some("profile_export") => run_profile_export(),
        None => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command: {other}");
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn load_schemas() -> Result<Vec<express::Schema>, ExitCode> {
    let schemas_dir = Path::new("schemas");
    if !schemas_dir.exists() {
        eprintln!("schemas/ not found in cwd — run from project root.");
        return Err(ExitCode::from(2));
    }
    let schemas = express::load_all_schemas(schemas_dir);
    if schemas.is_empty() {
        eprintln!("no schemas loaded — check schemas/*.exp.");
        return Err(ExitCode::from(2));
    }
    for s in &schemas {
        eprintln!(
            "  loaded {}: {} entities, {} types, {} parser warnings",
            s.source_label,
            s.entities.len(),
            s.types.len(),
            s.parse_warnings.len()
        );
    }
    Ok(schemas)
}

fn run_universal_export() -> ExitCode {
    let schemas = match load_schemas() {
        Ok(s) => s,
        Err(c) => return c,
    };
    match infer::universal_export::run(&schemas) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infer universal_export failed:\n{e}");
            ExitCode::from(2)
        }
    }
}

fn run_profile_export() -> ExitCode {
    let schemas = match load_schemas() {
        Ok(s) => s,
        Err(c) => return c,
    };
    match infer::profile_export::run(&schemas) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infer profile_export failed:\n{e}");
            ExitCode::from(2)
        }
    }
}

fn print_usage() {
    eprintln!(
        "\nusage:\n  \
         cargo run --release -- universal_export # → inferred/universal.toml (codegen input)\n  \
         cargo run --release -- profile_export   # → profiles/<target>.toml (output SchemaProfiles)"
    );
}
