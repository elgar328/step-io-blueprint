//! step-io-schema-check — schema-driven IR classification tool.
//!
//! Sub-commands:
//! - `infer variant`  Stage 1: arena enum variant 분류
//! - `infer arena`    Stage 2: arena 분류
//! - `infer pool`     Stage 3: pool 분류
//!
//! 4 schema (ap203 / ap203e2 / ap214e3 / ap242) 항상 union 으로 처리.
//! 출력은 `inferred/` 디렉토리에. 자세한 사양은 `internal/INFRA_PLAN.md` 와
//! 본 repo 의 plan 파일 참조.

use std::env;
use std::path::Path;
use std::process::ExitCode;

const ALLOW_PENDING_FLAG: &str = "--allow-pending";

mod express;
mod infer;
mod inheritance;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let allow_pending = args.iter().any(|a| a == ALLOW_PENDING_FLAG);
    let positional: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .map(|s| s.as_str())
        .collect();

    match (positional.first().copied(), positional.get(1).copied()) {
        (Some("infer"), Some("variant")) => run_variant(),
        (Some("infer"), Some("arena")) => run_arena(allow_pending),
        (Some("infer"), Some("pool")) => run_pool(allow_pending),
        (Some("infer"), Some(stage)) => {
            eprintln!("unknown infer stage: {stage}");
            print_usage();
            ExitCode::from(2)
        }
        (Some("infer"), None) => {
            eprintln!("infer requires a stage argument: variant | arena | pool");
            print_usage();
            ExitCode::from(2)
        }
        (None, _) => {
            print_usage();
            ExitCode::SUCCESS
        }
        (Some(other), _) => {
            eprintln!("unknown sub-command: {other}");
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

fn run_variant() -> ExitCode {
    let schemas = match load_schemas() {
        Ok(s) => s,
        Err(c) => return c,
    };
    match infer::variant::run(&schemas) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infer variant failed:\n{e}");
            ExitCode::from(2)
        }
    }
}

fn run_arena(allow_pending: bool) -> ExitCode {
    let schemas = match load_schemas() {
        Ok(s) => s,
        Err(c) => return c,
    };
    match infer::arena::run(&schemas, allow_pending) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infer arena failed:\n{e}");
            ExitCode::from(2)
        }
    }
}

fn run_pool(allow_pending: bool) -> ExitCode {
    let schemas = match load_schemas() {
        Ok(s) => s,
        Err(c) => return c,
    };
    match infer::pool::run(&schemas, allow_pending) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infer pool failed:\n{e}");
            ExitCode::from(2)
        }
    }
}

fn print_usage() {
    eprintln!(
        "\nusage:\n  \
         cargo run --release -- infer variant\n  \
         cargo run --release -- infer arena\n  \
         cargo run --release -- infer pool"
    );
}
