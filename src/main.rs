//! step-io-schema-check — schema-driven IR classification tool.
//!
//! Sub-commands:
//! - `infer variant`  Stage 1: entity 의 IR shape (variant) 분류
//! - `infer arena`    Stage 2: group → arena 매핑
//! - `infer prune --corpus <path>`  Stage 3: 53k STEP corpus 가지치기
//! - `infer shape`    Stage 4: ConcreteSupertype 의 IR shape 검증 + entities.toml 응축
//! - `infer reshape` Stage 5: split / merge 추상화 적용 (abstract_entities.toml)
//! - `infer pool`     Stage 6: pools.toml 검증 (수동 입력 vs abstract_entities 의 required arena)
//! - `infer naming`   Stage 7: ir.toml 청사진 산출 (abstract_entities + pools + names + schemas 통합)
//!
//! 4 schema (ap203 / ap203e2 / ap214e3 / ap242) 항상 union 으로 처리.
//! 출력은 `inferred/` 디렉토리에. 자세한 사양은 README + INFER_TUNING.md
//! 참조.

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const ALLOW_PENDING_FLAG: &str = "--allow-pending";

mod express;
mod infer;

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
        (Some("infer"), Some("prune")) => run_prune(&args, allow_pending),
        (Some("infer"), Some("shape")) => run_shape(allow_pending),
        (Some("infer"), Some("reshape")) => run_reshape(),
        (Some("infer"), Some("pool")) => run_pool(allow_pending),
        (Some("infer"), Some("naming")) => run_naming(),
        (Some("infer"), Some(stage)) => {
            eprintln!("unknown infer stage: {stage}");
            print_usage();
            ExitCode::from(2)
        }
        (Some("infer"), None) => {
            eprintln!(
                "infer requires a stage argument: variant | arena | prune | shape | reshape | pool | naming"
            );
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
    match infer::pool::run(allow_pending) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infer pool failed:\n{e}");
            ExitCode::from(2)
        }
    }
}

fn run_shape(allow_pending: bool) -> ExitCode {
    match infer::shape::run(allow_pending) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infer shape failed:\n{e}");
            ExitCode::from(2)
        }
    }
}

fn run_naming() -> ExitCode {
    match infer::naming::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infer naming failed:\n{e}");
            ExitCode::from(2)
        }
    }
}

fn run_reshape() -> ExitCode {
    match infer::reshape::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infer reshape failed:\n{e}");
            ExitCode::from(2)
        }
    }
}

fn run_prune(args: &[String], allow_pending: bool) -> ExitCode {
    let corpus_path = match parse_corpus_arg(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    // No load_schemas() — prune reads variants.toml only.
    match infer::prune::run(&corpus_path, allow_pending) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infer prune failed:\n{e}");
            ExitCode::from(2)
        }
    }
}

fn parse_corpus_arg(args: &[String]) -> Result<PathBuf, String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--corpus" {
            return iter
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| "--corpus requires a path argument".into());
        }
    }
    Err("--corpus <path> required for `infer prune`".into())
}

fn print_usage() {
    eprintln!(
        "\nusage:\n  \
         cargo run --release -- infer variant\n  \
         cargo run --release -- infer arena\n  \
         cargo run --release -- infer prune --corpus <path>\n  \
         cargo run --release -- infer shape\n  \
         cargo run --release -- infer reshape\n  \
         cargo run --release -- infer pool\n  \
         cargo run --release -- infer naming"
    );
}
