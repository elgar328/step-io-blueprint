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
use std::process::ExitCode;

mod express;
mod inheritance;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    match (args.next().as_deref(), args.next().as_deref()) {
        (Some("infer"), Some("variant")) => stub("infer variant"),
        (Some("infer"), Some("arena")) => stub("infer arena"),
        (Some("infer"), Some("pool")) => stub("infer pool"),
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

fn stub(name: &str) -> ExitCode {
    eprintln!("{name}: not yet implemented");
    ExitCode::from(2)
}

fn print_usage() {
    eprintln!(
        "\nusage:\n  \
         cargo run --release -- infer variant\n  \
         cargo run --release -- infer arena\n  \
         cargo run --release -- infer pool"
    );
}
