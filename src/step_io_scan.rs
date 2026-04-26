//! Scan the step-io source tree for `check_count(attrs, N, entity_id, "NAME")`
//! call sites, producing the set of entity names step-io processes. Only
//! used by the catalog (informational — "step-io supports" column).
//!
//! Future `check` mode will replace this with trait-introspection. Until
//! then, regex grep is the source of truth for step-io's per-entity coverage.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use regex::Regex;
use walkdir::WalkDir;

pub fn scan(step_io_root: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let re = Regex::new(r#"check_count\(attrs,\s*\d+,\s*entity_id,\s*"([A-Z_0-9]+)"\)"#)
        .expect("regex");
    let reader_dir = step_io_root.join("src/reader");
    for entry in WalkDir::new(&reader_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("rs"))
    {
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for caps in re.captures_iter(&text) {
            if let Some(m) = caps.get(1) {
                names.insert(m.as_str().to_string());
            }
        }
    }
    names
}
