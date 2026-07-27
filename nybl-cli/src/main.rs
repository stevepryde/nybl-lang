//! The `nybl` command-line interface.
//!
//! Subcommands:
//!
//! | command                  | what it does                                |
//! |--------------------------|---------------------------------------------|
//! | (no args)                | opens the REPL                              |
//! | `repl`                   | opens the REPL (explicit)                   |
//! | `run FILE`               | executes `FILE` with the bytecode VM        |
//! | `run --novm FILE`        | executes `FILE` with the tree-walker        |
//! | `compile FILE`           | AOT-transpiles and builds a native binary   |
//! | `compile --emit-rs FILE` | emits the transpiled Rust source only       |
//! | `--help` / `--version`   | prints command help or the installed version |
//!
//! The default `run` path goes through the bytecode VM — it's
//! 2–3× faster than the walker on realistic workloads and the
//! semantics are identical. `--novm` is kept as an escape hatch
//! for debugging or when binary size matters.
//!
//! The REPL retains bindings, functions, types, methods, and loaded modules
//! across submissions. It supports multiline input, expression echo, tab
//! completion, persistent history, `:vars`, `:reset`, `:help`, and
//! `:quit`. Non-TTY stdin uses the same submission model for scripted
//! transcripts.
//!
//! `nybl run` and the REPL surface source-aware warnings before execution.
//! Parse and runtime errors retain line/column information, source snippets,
//! hints, and the owning source for failures raised from imported modules.
//!
//! The REPL, VM runner, walker runner, and AOT compiler support the same
//! transactional `ref` parameter semantics. See the
//! [reference-parameters
//! guide](https://nybl-lang.com/docs/functions/reference-parameters/).
//!
//! Full command documentation is available at
//! <https://nybl-lang.com/docs/cli/>.

use std::process::ExitCode;

mod args;
mod compile;
mod entry_path;
mod repl;
mod run;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let cmd = match args::parse(&argv) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            eprintln!();
            args::print_usage();
            return ExitCode::from(2);
        }
    };
    match cmd {
        args::Command::Repl => repl::run(),
        args::Command::Run { file, no_vm } => run::run_file(&file, no_vm),
        args::Command::Compile {
            file,
            output,
            emit_rs,
            keep,
        } => compile::compile_file(&file, output.as_deref(), emit_rs, keep),
        args::Command::Help => {
            args::print_usage();
            ExitCode::SUCCESS
        }
        args::Command::Version => {
            println!("nybl {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
    }
}
