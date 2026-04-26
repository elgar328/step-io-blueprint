//! Default CLI mode placeholder — schema vs step-io trait-introspection
//! mismatch report. Activates after step-io's trait + per-module refactor
//! lands a stable trait registry.

pub fn run() {
    eprintln!(
        "check mode: not yet implemented — pending step-io trait + per-module refactor.\n\
         use the `catalog` sub-command to regenerate ENTITY_CATALOG.md / entity_catalog.json."
    );
    std::process::exit(2);
}
