//! End-to-end REPL tests that drive the real `nybl` binary
//! through a subprocess.
//!
//! Unit tests in `repl.rs` cover the `step` fn and its
//! helpers directly; these go one level out — they exercise
//! the non-TTY stdin path, pipe in source, and assert on the
//! captured stdout / stderr / exit code. That catches wiring
//! issues that pure-library tests can miss (a busted
//! `main.rs`, a wrong argv parse, the non-TTY detection
//! misfiring, the stdout flush getting dropped, etc.).
//!
//! The tests depend on `cargo` having already built the
//! binary. `env!("CARGO_BIN_EXE_nybl")` gives us the path
//! cargo used for the test target, so no PATH assumptions.

use std::io::Write;
use std::process::{Command, Stdio};

/// Drive `nybl repl` in non-TTY mode with `stdin_source`.
/// Returns `(stdout, stderr, exit_code)`. Panics if the
/// binary can't even be spawned — that's a test-harness bug,
/// not a REPL bug.
fn run_repl(stdin_source: &str) -> (String, String, i32) {
    let bin = env!("CARGO_BIN_EXE_nybl");
    let mut child = Command::new(bin)
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn nybl binary");
    {
        let mut stdin = child.stdin.take().expect("child stdin");
        stdin
            .write_all(stdin_source.as_bytes())
            .expect("write stdin");
    }
    let output = child.wait_with_output().expect("wait child");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

#[test]
fn bare_expression_result_lands_on_stdout() {
    let (stdout, stderr, code) = run_repl("1 + 2\n");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout.trim_end(), "3");
    assert!(stderr.is_empty(), "unexpected stderr: {stderr}");
}

#[test]
fn print_call_goes_to_stdout_and_no_trailing_none() {
    // `print(42)` produces `42` via the host's on_print
    // (which StdHost writes to stdout). The REPL then sees
    // `Value::None` as the call's return; suppressing that
    // keeps stdout clean.
    let (stdout, _stderr, code) = run_repl("print(42)\n");
    assert_eq!(code, 0);
    assert_eq!(stdout.trim_end(), "42");
}

#[test]
fn multi_line_fn_decl_then_call_works() {
    // The non-TTY path runs the whole buffer as one step, so
    // a fn declaration spanning multiple lines followed by
    // a call on a later line works without any Validator
    // gymnastics — it's just one `session.eval`.
    let src = "fn double(x) {\n    return x + x\n}\nprint(double(21))\n";
    let (stdout, _stderr, code) = run_repl(src);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim_end(), "42");
}

#[test]
fn runtime_error_prints_to_stderr_with_carat() {
    let (stdout, stderr, code) = run_repl("print(undefined)\n");
    assert_ne!(code, 0, "expected non-zero exit on runtime error");
    assert!(
        stderr.contains("--> line 1:"),
        "expected line+col header in stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("^"),
        "expected carat in stderr, got: {stderr}"
    );
    assert!(
        stdout.is_empty(),
        "error path shouldn't have stdout output, got: {stdout}"
    );
}

#[test]
fn help_meta_command_lists_commands() {
    let (stdout, _stderr, code) = run_repl(":help\n");
    assert_eq!(code, 0);
    assert!(stdout.contains(":help"));
    assert!(stdout.contains(":vars"));
    assert!(stdout.contains(":reset"));
    assert!(stdout.contains(":quit"));
}

#[test]
fn empty_stdin_is_graceful() {
    // No input → exit 0, nothing printed. Matches what the
    // old REPL did when stdin closed immediately.
    let (stdout, stderr, code) = run_repl("");
    assert_eq!(code, 0);
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
}

#[test]
fn session_sees_bindings_across_statements_in_one_buffer() {
    // Both lines are complete statements so they get
    // submitted to the session separately (line-by-line
    // non-TTY mode). The second sees the first through the
    // session's cross-call persistence.
    let src = "let x = 5\nprint(x)\n";
    let (stdout, _stderr, code) = run_repl(src);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim_end(), "5");
}

#[test]
fn bare_expressions_echo_per_line() {
    // Each complete line is its own submission; bare
    // expressions echo their value. Prior inputs are
    // visible to later ones via the session.
    let src = "let x = 10\nx\nx + 5\n";
    let (stdout, _stderr, code) = run_repl(src);
    assert_eq!(code, 0);
    // Lines without output (the `let`) are suppressed;
    // the two bare expressions produce `10` and `15`.
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines, vec!["10", "15"]);
}

#[test]
fn error_in_one_line_does_not_abort_the_rest() {
    // `undefined` on line 2 errors; line 3 still runs
    // because the session stays alive. Exit code is 1
    // because *some* error happened, but stdout reflects
    // the partial progress.
    let src = "let ok = 1\nundefined\nprint(ok)\n";
    let (stdout, stderr, code) = run_repl(src);
    assert_eq!(code, 1, "expected non-zero exit on any error");
    assert_eq!(stdout.trim_end(), "1");
    assert!(
        stderr.contains("undefined"),
        "expected error to name undefined, got: {stderr}"
    );
}

#[test]
fn reset_meta_command_clears_session() {
    // Declare x, reset, then reference x → the reference
    // errors because the session was cleared in between.
    let src = "let x = 5\n:reset\nprint(x)\n";
    let (_stdout, stderr, code) = run_repl(src);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("Variable `x` not found") || stderr.to_lowercase().contains("not found"),
        "expected 'x not found' error after :reset, got: {stderr}"
    );
}

#[test]
fn vars_meta_lists_declared_names() {
    let src = "let alpha = 1\nfn beta() { return 2 }\n:vars\n";
    let (stdout, _stderr, code) = run_repl(src);
    assert_eq!(code, 0);
    // :vars output should name both bindings. Order is
    // alphabetical.
    assert!(stdout.contains("alpha"));
    assert!(stdout.contains("beta"));
}

#[test]
fn quit_meta_exits_early_and_ignores_remaining_input() {
    // Lines after `:quit` should be ignored — the loop
    // breaks. We prove it by having a `print` after the
    // quit that would fail loudly if it ran.
    let src = "let x = 42\n:quit\nprint(does_not_exist)\n";
    let (stdout, stderr, code) = run_repl(src);
    assert_eq!(code, 0, "quit should succeed, stderr: {stderr}");
    assert!(
        !stdout.contains("does_not_exist"),
        "expected no output after :quit, got: {stdout}"
    );
    assert!(
        stderr.is_empty(),
        "expected no errors after :quit, got: {stderr}"
    );
}

#[test]
fn struct_declaration_persists_across_lines() {
    let src = r#"struct Point { x, y }
fn Point.sum(self) { return self.x + self.y }
print(Point { x: 3, y: 4 }.sum())
"#;
    let (stdout, stderr, code) = run_repl(src);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout.trim_end(), "7");
}

#[test]
fn use_statement_imports_stay_live_across_lines() {
    // Uses the bundled std.math module — nybl-cli's StdHost
    // resolves it through nybl-std.
    let src = "use std.math\nprint(9.sqrt())\n";
    let (stdout, stderr, code) = run_repl(src);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout.trim_end(), "3");
}
