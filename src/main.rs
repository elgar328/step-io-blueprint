//! step-io-blueprint — EXPRESS-schema-driven IR blueprint generator.
//!
//! Pipeline stages (run in order; each is a positional sub-command):
//! - `variant`  Stage 1: entity 의 IR shape (variant) 분류
//! - `arena`    Stage 2: group → arena 매핑
//! - `prune`   Stage 3: corpus_usage.toml (frozen) 로 가지치기
//! - `shape`    Stage 4: ConcreteSupertype 의 IR shape 검증 + entities.toml 응축
//! - `reshape`  Stage 5: split / merge 추상화 적용 (abstract_entities.toml)
//! - `pool`     Stage 6: pools.toml 검증 (수동 입력 vs abstract_entities 의 required arena)
//! - `naming`   Stage 7: ir.toml 청사진 산출 (abstract_entities + pools + names + schemas 통합)
//!
//! 6 schema (ap203 / ap203e2 / ap214e3 / ap242 / ap242e2 / ap242e3) 항상
//! union 으로 처리.
//! 출력은 `inferred/` 디렉토리에. 자세한 사양은 README 참조.

use std::env;
use std::path::Path;
use std::process::ExitCode;

const ALLOW_PENDING_FLAG: &str = "--allow-pending";

mod express;
mod infer;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    // Reject unknown flags so removed options (e.g. the old `prune --corpus`)
    // fail loudly as invalid syntax rather than being silently ignored.
    for a in &args {
        if a.starts_with("--") && a != ALLOW_PENDING_FLAG {
            eprintln!("unknown flag: {a}");
            print_usage();
            return ExitCode::from(2);
        }
    }
    let allow_pending = args.iter().any(|a| a == ALLOW_PENDING_FLAG);
    let positional: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .map(|s| s.as_str())
        .collect();
    if positional.len() > 1 {
        eprintln!("unexpected argument: {}", positional[1]);
        print_usage();
        return ExitCode::from(2);
    }

    match positional.first().copied() {
        Some("variant") => run_variant(),
        Some("arena") => run_arena(allow_pending),
        Some("prune") => run_prune(allow_pending),
        Some("shape") => run_shape(allow_pending),
        Some("reshape") => run_reshape(),
        Some("pool") => run_pool(allow_pending),
        Some("naming") => run_naming(),
        Some("l1_export") => run_l1_export(),
        None => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown stage: {other}");
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

fn run_l1_export() -> ExitCode {
    let schemas = match load_schemas() {
        Ok(s) => s,
        Err(c) => return c,
    };
    match infer::l1_export::run(&schemas) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infer l1_export failed:\n{e}");
            ExitCode::from(2)
        }
    }
}

fn run_prune(allow_pending: bool) -> ExitCode {
    // No corpus scan / load_schemas() — prune reads variants.toml plus the
    // frozen inferred/corpus_usage.toml (generated by step-io-reference-check's
    // corpus-usage bin and copied in).
    match infer::prune::run(allow_pending) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infer prune failed:\n{e}");
            ExitCode::from(2)
        }
    }
}

fn print_usage() {
    eprintln!(
        "\nusage (blueprint pipeline stages, run in order):\n  \
         cargo run --release -- variant\n  \
         cargo run --release -- arena\n  \
         cargo run --release -- prune\n  \
         cargo run --release -- shape\n  \
         cargo run --release -- reshape\n  \
         cargo run --release -- pool\n  \
         cargo run --release -- naming\n\n\
         standalone (2-layer IR, independent of the pipeline above):\n  \
         cargo run --release -- l1_export   # → inferred/early.toml (EarlyModel L1)"
    );
}
