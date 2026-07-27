//! `nybl` / `nybl repl` — interactive REPL.
//!
//! Stateful across inputs: each submission runs against a
//! `nybl::ReplSession` that carries `let` / `fn` / struct / enum
//! / method / `use` state forward. Typing `let x = 5` then
//! `print(x)` on separate lines sees the same `x`, the way any
//! useful REPL works.
//!
//! Backed by rustyline for arrow-key history, emacs-style
//! editing, persisted history, multi-line input via the
//! `Validator` hook, and tab completion driven by the
//! `Completer` hook.
//!
//! **Multi-line input** works by handing the raw buffer to the
//! parser — if the parse error message looks like "end of
//! code" (unclosed brace, incomplete match, etc.), we tell
//! rustyline the input is incomplete and it prompts again
//! with `... `. Any other parse error is surfaced right away
//! so the user can see the problem without retyping the whole
//! block.
//!
//! **Tab completion** offers the intersection of:
//! - Nybl keywords (`let`, `fn`, `if`, `use`, …)
//! - `nybl::suggest::CORE_CALLABLE_BUILTINS` (language
//!   builtins)
//! - Bindings currently live in the session (via
//!   `ReplSession::binding_names`) so `let my_var = …`
//!   completes on a subsequent tab.
//! - Identifier-shaped words the user has already typed this
//!   session (covers fn parameters, struct field names, etc.
//!   that don't surface through `binding_names`).
//!
//! **Meta-commands** (lines that start with `:`):
//! - `:help` — print this list.
//! - `:vars` — list current session bindings.
//! - `:reset` / `:clear` — drop all bindings and start fresh.
//! - `:quit` / `:q` / `:exit` — exit.
//!
//! **History** lives at `$HOME/.nybl_history`. Save-on-exit is
//! best-effort; failure doesn't abort the session.
//!
//! Engine choice: walker. REPL workloads are tiny and the
//! walker's per-input startup cost is nil; the VM's hot-loop
//! speedup doesn't pay for its compile step at this scale.

use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;

use nybl::{NyblHost, NyblLimits, ReplSession, Value};
use nybl_sys::StdHost;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Context, Editor, Helper};

/// Nybl language keywords — offered as completion candidates
/// when the user hits tab on a bare prefix.
const KEYWORDS: &[&str] = &[
    "let", "const", "pub", "ref", "fn", "if", "else", "while", "repeat", "for", "in", "return",
    "break", "continue", "match", "struct", "enum", "use", "as", "true", "false", "none", "try",
];

/// Every meta-command the REPL recognises. Typing one of
/// these as the entire line (with optional `:` prefix) runs
/// the built-in rather than sending the text to the parser.
const META_HELP: &[(&str, &str)] = &[
    (":help", "show this help"),
    (":vars", "list bindings in the current session"),
    (":reset | :clear", "drop all session state and start fresh"),
    (":quit | :q | :exit", "exit the REPL"),
];

/// Combined `Helper` impl for rustyline. Holds:
///
/// - The set of identifier-shaped tokens the REPL has seen in
///   source lines (a superset of actual in-scope names, but
///   cheap to maintain).
/// - An `Rc<RefCell<Vec<String>>>` of session binding names
///   the outer loop refreshes after every successful eval.
///
/// The interior mutability lets tab completion stay honest
/// with respect to the session's current state without the
/// helper having to borrow the session itself.
struct NyblHelper {
    typed_names: RefCell<std::collections::BTreeSet<String>>,
    session_names: Rc<RefCell<Vec<String>>>,
}

impl NyblHelper {
    fn new(session_names: Rc<RefCell<Vec<String>>>) -> Self {
        Self {
            typed_names: RefCell::new(std::collections::BTreeSet::new()),
            session_names,
        }
    }

    /// Harvest identifier-shaped words from a source line and
    /// add them to the completion set. Covers fn parameters
    /// and struct field names, which never show up as session
    /// bindings on their own.
    fn absorb_names(&self, source: &str) {
        let mut out = self.typed_names.borrow_mut();
        let mut chars = source.char_indices().peekable();
        while let Some((start, ch)) = chars.next() {
            if !ch.is_alphabetic() && ch != '_' {
                continue;
            }
            let mut end = start + ch.len_utf8();
            while let Some(&(i, c)) = chars.peek() {
                if c.is_alphanumeric() || c == '_' {
                    end = i + c.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            let word = &source[start..end];
            if word.len() >= 2 {
                out.insert(word.to_string());
            }
        }
    }
}

impl Helper for NyblHelper {}
impl Highlighter for NyblHelper {}
impl Hinter for NyblHelper {
    type Hint = String;
}

impl Completer for NyblHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        // Walk backwards from the cursor to find the start of
        // the current identifier. Non-ident chars (spaces,
        // punctuation) terminate the scan.
        let prefix_start = line[..pos]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
            .last()
            .map(|(i, _)| i)
            .unwrap_or(pos);
        let prefix = &line[prefix_start..pos];

        if prefix.is_empty() {
            return Ok((pos, Vec::new()));
        }

        let typed = self.typed_names.borrow();
        let session = self.session_names.borrow();
        let mut candidates: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for &kw in KEYWORDS {
            if kw.starts_with(prefix) {
                candidates.insert(kw.to_string());
            }
        }
        for &b in nybl::suggest::CORE_CALLABLE_BUILTINS {
            if b.starts_with(prefix) {
                candidates.insert(b.to_string());
            }
        }
        for name in session.iter() {
            if name.starts_with(prefix) && name.as_str() != prefix {
                candidates.insert(name.clone());
            }
        }
        for name in typed.iter() {
            if name.starts_with(prefix) && name.as_str() != prefix {
                candidates.insert(name.clone());
            }
        }
        let pairs = candidates
            .into_iter()
            .map(|s| Pair {
                display: s.clone(),
                replacement: s,
            })
            .collect();
        Ok((prefix_start, pairs))
    }
}

/// Classify `input` as either "ready to execute" or "needs
/// more input". Extracted from the `Validator` impl so tests
/// can exercise the heuristic without going through
/// rustyline's private `ValidationContext` constructor.
fn is_incomplete_input(input: &str) -> bool {
    if input.trim().is_empty() {
        return false;
    }
    // `:` meta-commands are always complete — they never span
    // multiple lines.
    if input.trim_start().starts_with(':') {
        return false;
    }
    match nybl::parse(input) {
        Ok(_) => false,
        Err(e) => {
            // Heuristic: parse errors caused by running off
            // the end of the buffer mean the user is still
            // typing. Nybl's parser spells this "end of code".
            let msg = e.message.to_lowercase();
            msg.contains("end of code")
                || msg.contains("end of input")
                || msg.contains("unexpected eof")
        }
    }
}

impl Validator for NyblHelper {
    fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        if is_incomplete_input(ctx.input()) {
            Ok(ValidationResult::Incomplete)
        } else {
            Ok(ValidationResult::Valid(None))
        }
    }

    fn validate_while_typing(&self) -> bool {
        false
    }
}

/// Outcome of running one submission through the session. The
/// REPL loop converts this into stdout / stderr output; tests
/// inspect it directly to avoid scraping terminal text.
#[derive(Debug)]
enum StepOutcome {
    /// Nothing to print — a `let`, `fn`, etc.
    Ok,
    /// A bare expression ran; display its value.
    Value(Value),
    /// A runtime or parse error fired; render it with the
    /// source attached.
    Err(nybl::NyblError),
    /// User asked to exit.
    Quit,
    /// Meta-command text to echo. Distinct from `Value` so
    /// the REPL can style it differently (plain stdout line
    /// rather than an inspected value).
    Note(String),
}

/// Process one REPL submission. Pure with respect to the
/// REPL's own state (session + host), so tests can drive it
/// directly.
///
/// Meta-commands (`:help`, `:vars`, `:reset`, `:quit`) are
/// handled here before the text ever reaches the parser.
/// Everything else goes through `session.eval`.
fn step<H: NyblHost>(session: &mut ReplSession, host: &mut H, input: &str) -> StepOutcome {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return StepOutcome::Ok;
    }
    if trimmed.starts_with(':') {
        return handle_meta(session, trimmed);
    }
    match session.eval(input, host, &NyblLimits::standard()) {
        // `print(...)` and other side-effecting calls return
        // `Value::None`; echoing "none" in the REPL after
        // they've already produced their output is noise, so
        // suppress it. Matches Python's / node's behaviour
        // where function-statement results are only shown
        // when they're interesting.
        Ok(Some(Value::None)) => StepOutcome::Ok,
        Ok(Some(v)) => StepOutcome::Value(v),
        Ok(None) => StepOutcome::Ok,
        Err(e) => StepOutcome::Err(e),
    }
}

fn handle_meta(session: &mut ReplSession, cmd: &str) -> StepOutcome {
    match cmd {
        ":help" | ":h" | ":?" => {
            let mut out = String::from("REPL commands:\n");
            for (name, desc) in META_HELP {
                out.push_str(&format!("  {name:<22} {desc}\n"));
            }
            StepOutcome::Note(out)
        }
        ":vars" => {
            let names = session.binding_names();
            if names.is_empty() {
                StepOutcome::Note(String::from("(no bindings yet)\n"))
            } else {
                StepOutcome::Note(format!("{}\n", names.join("\n")))
            }
        }
        ":reset" | ":clear" => {
            *session = ReplSession::new();
            StepOutcome::Note(String::from("session cleared.\n"))
        }
        ":quit" | ":q" | ":exit" => StepOutcome::Quit,
        other => StepOutcome::Note(format!("unknown command `{other}` — try `:help`\n")),
    }
}

/// Render one `StepOutcome` to the given stdout/stderr
/// writers. Split out so tests can assert on captured output
/// instead of the real terminal.
fn render_outcome<W: std::io::Write, E: std::io::Write>(
    outcome: &StepOutcome,
    source: &str,
    out: &mut W,
    err: &mut E,
) {
    match outcome {
        StepOutcome::Ok | StepOutcome::Quit => {}
        StepOutcome::Value(v) => {
            // `writeln!` failures are terminal-detached — if
            // stdout is closed the REPL is probably ending
            // soon anyway. Ignore rather than panic.
            let _ = writeln!(out, "{v}");
        }
        StepOutcome::Err(e) => {
            let _ = write!(err, "{}", e.render(source));
        }
        StepOutcome::Note(s) => {
            let _ = write!(out, "{s}");
        }
    }
}

/// Best-effort history-file path. Returns `None` if we can't
/// figure out a home dir (rare on current platforms); the REPL
/// then runs with no persisted history.
fn history_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| {
            let mut p = std::path::PathBuf::from(h);
            p.push(".nybl_history");
            p
        })
}

pub fn run() -> ExitCode {
    // When stdin isn't a terminal (piped input, heredoc,
    // `nybl repl <<EOF ... EOF`), rustyline's multi-line
    // Validator doesn't fire between stdin lines. Fall back
    // to running the whole buffer as a single session step
    // so multi-line pipes work and bare expressions still
    // echo. Matches `python3` / `node` behaviour.
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return run_non_tty();
    }

    let session_names = Rc::new(RefCell::new(Vec::<String>::new()));
    let helper = NyblHelper::new(Rc::clone(&session_names));
    let mut rl = match Editor::<NyblHelper, rustyline::history::FileHistory>::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: couldn't initialise line editor: {e}");
            return ExitCode::from(1);
        }
    };
    rl.set_helper(Some(helper));

    let hist = history_path();
    if let Some(ref p) = hist {
        let _ = rl.load_history(p);
    }

    let mut host = StdHost::new();
    let mut session = ReplSession::new();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();

    loop {
        match rl.readline("> ") {
            Ok(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line.as_str());
                if let Some(h) = rl.helper() {
                    h.absorb_names(&line);
                }
                let outcome = step(&mut session, &mut host, &line);
                if host.is_broken_pipe() {
                    break;
                }
                let mut out = stdout.lock();
                let mut err = stderr.lock();
                render_outcome(&outcome, &line, &mut out, &mut err);
                // Refresh completer's view of the session's
                // bindings so the next tab sees anything just
                // introduced.
                *session_names.borrow_mut() = session.binding_names();
                if matches!(outcome, StepOutcome::Quit) {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) => continue, // Ctrl-C
            Err(ReadlineError::Eof) => break,            // Ctrl-D
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        }
    }

    if let Some(ref p) = hist {
        let _ = rl.save_history(p);
    }

    ExitCode::SUCCESS
}

/// Non-interactive path: accumulate stdin line by line,
/// submitting each *complete* chunk to the session the same
/// way the TTY loop submits each prompt. This makes
/// transcripts piped in behave like the user typed them: a
/// multi-line `fn` decl stays as one submission (the
/// incomplete-input detector holds it open), bare
/// expressions echo their result, meta-commands work, and
/// runtime errors keep the session alive so later inputs
/// still run.
///
/// The first runtime / parse error sets the exit code to 1
/// but we keep consuming stdin until EOF — dropping later
/// input on the floor is surprising for scripts that are
/// piping structured transcripts. Callers that want
/// fail-fast semantics can run each input through a
/// separate process.
fn run_non_tty() -> ExitCode {
    use std::io::BufRead;

    let mut host = StdHost::new();
    let mut session = ReplSession::new();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut any_error = false;

    let stdin = std::io::stdin();
    let mut buffer = String::new();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("error reading stdin: {e}");
                return ExitCode::from(1);
            }
        };
        // Accumulate into the current submission. Submit when
        // the accumulated buffer parses (or its parse error
        // isn't an "unfinished input" one).
        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(&line);
        if is_incomplete_input(&buffer) {
            continue;
        }
        // Ready to run. Capture the submission for error
        // rendering, then clear for the next one.
        let submission = std::mem::take(&mut buffer);
        let outcome = step(&mut session, &mut host, &submission);
        if host.is_broken_pipe() {
            return ExitCode::SUCCESS;
        }
        let mut out = stdout.lock();
        let mut err = stderr.lock();
        render_outcome(&outcome, &submission, &mut out, &mut err);
        match outcome {
            StepOutcome::Err(_) => any_error = true,
            StepOutcome::Quit => break,
            _ => {}
        }
    }
    // Drain any trailing buffer that never became complete —
    // run it anyway so the user sees the parse error rather
    // than silent truncation.
    if !buffer.trim().is_empty() {
        let outcome = step(&mut session, &mut host, &buffer);
        if host.is_broken_pipe() {
            return ExitCode::SUCCESS;
        }
        let mut out = stdout.lock();
        let mut err = stderr.lock();
        render_outcome(&outcome, &buffer, &mut out, &mut err);
        if matches!(outcome, StepOutcome::Err(_)) {
            any_error = true;
        }
    }

    if any_error {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    // Test host: captures prints, no module resolution.
    struct TestHost {
        prints: RefCell<Vec<String>>,
    }
    impl TestHost {
        fn new() -> Self {
            Self {
                prints: RefCell::new(Vec::new()),
            }
        }
    }
    impl NyblHost for TestHost {
        fn call(&mut self, _: &str, _: &[Value], _: u32) -> Option<Result<Value, nybl::NyblError>> {
            None
        }
        fn on_print(&mut self, message: &str) {
            self.prints.borrow_mut().push(message.to_string());
        }
    }

    /// Drive a sequence of REPL submissions and return:
    /// - everything printed via `print(...)` (through
    ///   `on_print`), joined with `\n`;
    /// - everything the REPL would have written to stdout
    ///   (echoed values, meta-command notes), as captured
    ///   source strings.
    fn drive(inputs: &[&str]) -> (Vec<String>, Vec<String>, Vec<StepOutcome>) {
        let mut host = TestHost::new();
        let mut session = ReplSession::new();
        let mut outcomes = Vec::new();
        let mut stdout_lines = Vec::new();
        for input in inputs {
            let outcome = step(&mut session, &mut host, input);
            let mut out: Vec<u8> = Vec::new();
            let mut err: Vec<u8> = Vec::new();
            render_outcome(&outcome, input, &mut out, &mut err);
            let text = String::from_utf8(out).unwrap();
            if !text.is_empty() {
                stdout_lines.push(text);
            }
            outcomes.push(outcome);
        }
        let prints = host.prints.borrow().clone();
        (prints, stdout_lines, outcomes)
    }

    // ─── Validator / completer (unchanged from the previous
    //     iteration; here because they still need to pass).

    #[test]
    fn complete_statement_is_valid() {
        assert!(!is_incomplete_input("let x = 1"));
    }

    #[test]
    fn unclosed_fn_body_is_incomplete() {
        assert!(is_incomplete_input("fn double(x) {\n    return x + x"));
    }

    #[test]
    fn unclosed_if_body_is_incomplete() {
        assert!(is_incomplete_input("if true {"));
    }

    #[test]
    fn trailing_plus_is_incomplete() {
        assert!(is_incomplete_input("let x = 1 +"));
    }

    #[test]
    fn real_parse_error_is_not_incomplete() {
        assert!(!is_incomplete_input("let 1x = 2"));
    }

    #[test]
    fn empty_input_is_not_incomplete() {
        assert!(!is_incomplete_input(""));
        assert!(!is_incomplete_input("   \n\t "));
    }

    #[test]
    fn meta_command_is_always_complete() {
        // `:help` should never prompt for continuation even
        // though it isn't a valid Nybl statement.
        assert!(!is_incomplete_input(":help"));
        assert!(!is_incomplete_input(":vars"));
    }

    #[test]
    fn completer_prefix_matches_keyword() {
        let session_names = Rc::new(RefCell::new(Vec::new()));
        let h = NyblHelper::new(session_names);
        let (_start, cands) = h
            .complete(
                "le",
                2,
                &rustyline::Context::new(&rustyline::history::FileHistory::new()),
            )
            .unwrap();
        let disp: Vec<String> = cands.iter().map(|p| p.display.clone()).collect();
        assert!(disp.contains(&"let".to_string()));

        let (_start, cands) = h
            .complete(
                "re",
                2,
                &rustyline::Context::new(&rustyline::history::FileHistory::new()),
            )
            .unwrap();
        let disp: Vec<String> = cands.iter().map(|p| p.display.clone()).collect();
        assert!(disp.contains(&"ref".to_string()));
    }

    #[test]
    fn completer_uses_session_binding_names() {
        let session_names = Rc::new(RefCell::new(vec!["my_binding".to_string()]));
        let h = NyblHelper::new(Rc::clone(&session_names));
        let (_start, cands) = h
            .complete(
                "my",
                2,
                &rustyline::Context::new(&rustyline::history::FileHistory::new()),
            )
            .unwrap();
        let disp: Vec<String> = cands.iter().map(|p| p.display.clone()).collect();
        assert!(
            disp.contains(&"my_binding".to_string()),
            "expected session binding to surface in completions, got: {disp:?}",
        );
    }

    // ─── State persistence through the REPL `step` fn ──────────────

    #[test]
    fn let_survives_between_steps() {
        let (prints, _, _) = drive(&["let x = 5", "print(x)"]);
        assert_eq!(prints, vec!["5"]);
    }

    #[test]
    fn fn_survives_between_steps() {
        let (prints, _, _) = drive(&["fn double(x) { return x + x }", "print(double(21))"]);
        assert_eq!(prints, vec!["42"]);
    }

    #[test]
    fn struct_and_method_survive_between_steps() {
        let (prints, _, _) = drive(&[
            "struct Point { x, y }\nfn Point.sum(self) { return self.x + self.y }",
            "print(Point { x: 3, y: 4 }.sum())",
        ]);
        assert_eq!(prints, vec!["7"]);
    }

    #[test]
    fn bare_expression_echoes_its_value() {
        let (_prints, stdout, _) = drive(&["let x = 5", "x + 1"]);
        // First input is `let`, no stdout. Second echoes `6`.
        assert_eq!(stdout.len(), 1);
        assert_eq!(stdout[0].trim_end(), "6");
    }

    #[test]
    fn bare_expression_does_not_echo_none_for_statements() {
        let (_prints, stdout, _) = drive(&["let x = 5"]);
        // `let` returns `Ok(None)` and nothing should have
        // gone to stdout — the REPL should not print "none".
        assert!(stdout.is_empty(), "got unexpected stdout: {stdout:?}");
    }

    #[test]
    fn print_call_does_not_echo_trailing_none() {
        // `print(...)` runs as a bare expression: its return
        // value is `Value::None`. Before we suppressed the
        // None echo, the REPL would print the captured value
        // (`42`) via the host *and* then print "none" to
        // stdout. The suppression keeps the output clean.
        let (prints, stdout, _) = drive(&["print(42)"]);
        assert_eq!(prints, vec!["42"]);
        assert!(
            stdout.is_empty(),
            "expected no echo after print(), got: {stdout:?}"
        );
    }

    #[test]
    fn explicit_none_literal_is_suppressed_too() {
        // Symmetric: typing `none` at the prompt used to echo
        // "none". With the suppression, nothing goes to
        // stdout. Trade-off: users who really want to see
        // `none` can ask for `none.type()` or inspect it.
        let (_prints, stdout, _) = drive(&["none"]);
        assert!(stdout.is_empty());
    }

    #[test]
    fn error_in_step_is_captured_without_aborting() {
        let (prints, _, outcomes) = drive(&[
            "let good = 1",
            "let bad = undefined", // runtime error
            "print(good)",         // still runs; `good` survives
        ]);
        assert!(matches!(outcomes[1], StepOutcome::Err(_)));
        assert_eq!(prints, vec!["1"]);
    }

    // ─── Meta-commands ─────────────────────────────────────────────

    #[test]
    fn help_meta_command_prints_known_commands() {
        let (_prints, stdout, _) = drive(&[":help"]);
        assert_eq!(stdout.len(), 1);
        let text = &stdout[0];
        assert!(text.contains(":help"));
        assert!(text.contains(":vars"));
        assert!(text.contains(":reset"));
        assert!(text.contains(":quit"));
    }

    #[test]
    fn vars_lists_current_bindings() {
        let (_prints, stdout, _) = drive(&["let alpha = 1", "fn beta() { return 2 }", ":vars"]);
        // :vars output is the last line printed.
        let last = stdout.last().unwrap();
        assert!(last.contains("alpha"));
        assert!(last.contains("beta"));
    }

    #[test]
    fn reset_drops_previous_bindings() {
        let (prints, _, outcomes) = drive(&[
            "let x = 5",
            ":reset",
            "print(x)", // x is gone → runtime error
        ]);
        // :reset outcome is a Note; the subsequent print
        // errors because `x` no longer exists.
        assert!(matches!(outcomes[2], StepOutcome::Err(_)));
        assert!(prints.is_empty());
    }

    #[test]
    fn quit_signals_shutdown() {
        let (_, _, outcomes) = drive(&[":quit"]);
        assert!(matches!(outcomes[0], StepOutcome::Quit));
    }

    #[test]
    fn unknown_meta_command_surfaces_friendly_note() {
        let (_, stdout, _) = drive(&[":what"]);
        let text = stdout.last().unwrap();
        assert!(text.contains("unknown command"));
        assert!(text.contains(":help"));
    }

    // ─── Render / IO separation ────────────────────────────────────

    #[test]
    fn render_writes_value_to_stdout_not_stderr() {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        render_outcome(
            &StepOutcome::Value(Value::Int(42)),
            "source",
            &mut out,
            &mut err,
        );
        assert_eq!(String::from_utf8(out).unwrap().trim_end(), "42");
        assert!(err.is_empty());
    }

    #[test]
    fn render_writes_errors_to_stderr_with_source_snippet() {
        let err_val = nybl::NyblError::runtime_at("boom", 1, std::num::NonZeroU32::new(5));
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        render_outcome(&StepOutcome::Err(err_val), "let x = 1", &mut out, &mut err);
        let err_text = String::from_utf8(err).unwrap();
        assert!(err_text.contains("boom"));
        assert!(err_text.contains("let x = 1"));
        assert!(out.is_empty());
    }
}
