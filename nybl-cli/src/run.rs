//! `nybl run FILE` — execute a script.

use std::path::Path;
use std::process::ExitCode;

use nybl::{NyblHost, NyblLimits};
use nybl_sys::StdHost;

/// Read `path`, run the match-exhaustiveness / warning pass, and
/// execute. Defaults to the bytecode VM; `no_vm` forces the
/// tree-walker.
///
/// Errors (file I/O, parse, runtime) render with
/// `NyblError::render`/`NyblWarning::render` so the terminal output
/// has source snippets + carets under the offending column when
/// the error carries one.
pub fn run_file(path: &str, no_vm: bool) -> ExitCode {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading `{path}`: {e}");
            return ExitCode::from(1);
        }
    };
    let module_root = crate::entry_path::module_root(Path::new(path));

    // Static checks (match exhaustiveness) surface before any
    // program output. Parse errors fail fast; warnings are
    // informational.
    //
    // We hand the checker a module resolver built from a
    // dedicated `StdHost` — that way an exhaustiveness
    // warning can fire on an imported enum, not just ones
    // declared in the root file. The resolver borrow is
    // scoped so we're free to build a fresh host for the
    // actual run below.
    let mut check_host = StdHost::new().with_module_root(&module_root);
    let resolver = |path: &str| check_host.resolve_module(path);
    match nybl::parse_with_warnings_and_resolver(&source, resolver) {
        Ok((_stmts, warnings)) => {
            for w in &warnings {
                eprint!("{}", w.render(&source));
            }
        }
        Err(e) => {
            eprint!("{}", e.render(&source));
            return ExitCode::from(1);
        }
    }

    let mut host = StdHost::new().with_module_root(module_root);
    let result = if no_vm {
        nybl::run(&source, &mut host, &NyblLimits::standard())
    } else {
        nybl_vm::run(&source, &mut host, &NyblLimits::standard())
    };

    if host.is_broken_pipe() {
        return ExitCode::SUCCESS;
    }

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprint!("{}", e.render(&source));
            ExitCode::from(1)
        }
    }
}
