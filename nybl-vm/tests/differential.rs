//! Differential tests — every program runs through both the
//! tree-walking evaluator in `nybl-lang` and the bytecode VM in
//! `nybl-vm`, and the two engines must agree on prints and on whether
//! (and why) the program errored.
//!
//! This is step 2c of the execution-modes plan: the engines are
//! compared against each other on every test, so the second one can
//! never silently drift from the first.
//!
//! # Comparison rules
//!
//! - Prints: strict `Vec<String>` equality.
//! - Success/failure: must agree. If one engine errors and the other
//!   succeeds, the harness panics with both outcomes side by side.
//! - Error messages: compared on `NyblError::message` text only.
//!   Line numbers legitimately diverge (the walker emits `line: 0`
//!   for `Signal::Break` / `Signal::Continue` that bubble up to
//!   `run()`; the VM compiler catches the same cases with the real
//!   source line). Columns and friendly hints are not part of the
//!   contract.
//! - Resource-limit errors (`too many steps`, `Memory limit`) count
//!   the same way — the existing safety tests assert via substring,
//!   so the slightly different step-counts between engines don't
//!   matter as long as *some* limit-based error fires on both.
//! - Step-limit error *lines*, however, are aligned: both engines
//!   report the header line of the outermost active source loop at
//!   the moment the budget trips (see `tick` in each engine), which
//!   is stable no matter where each engine's counter happens to
//!   cross the limit. The only residual divergence is a budget trip
//!   with no loop on the call stack (e.g. branching recursion, or a
//!   finite loop whose step cost differs enough between the
//!   engines' accounting that only one of them trips inside it) —
//!   there the engines fall back to their own current line.
//!
//! # Fuzzer
//!
//! At the bottom of this file, [`fuzz_smoke_diff`] generates random
//! programs from a constrained grammar (no functions, no `while`, no
//! methods — just literals / arithmetic / logic / `let` / assign /
//! `print` / `if` / `repeat` / arrays) and runs them through both
//! engines. Seeded RNG, deterministic, 100 programs per run. The
//! extended budget is behind `#[ignore]` for local / nightly use.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use nybl::{HostValue, NyblError, NyblHost, NyblLimits, Value};
use nybl_vm::NyblInstance;

// ─── Harness ───────────────────────────────────────────────────────

/// Result of running a single program through one engine.
#[derive(Debug, Clone)]
struct Outcome {
    prints: Vec<String>,
    /// `None` = success; `Some(message)` = runtime or compile error.
    error: Option<String>,
}

impl Outcome {
    fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

struct RecordHost {
    prints: RefCell<Vec<String>>,
}

impl RecordHost {
    fn new() -> Self {
        Self {
            prints: RefCell::new(Vec::new()),
        }
    }
}

impl NyblHost for RecordHost {
    fn call(
        &mut self,
        _name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        None
    }

    fn on_print(&mut self, message: &str) {
        self.prints.borrow_mut().push(message.to_string());
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        // Test-overrides take priority so an individual test
        // can shadow a stdlib module by name; otherwise fall
        // through to the bundled stdlib.
        if let Some(src) = MODULES.with(|m| m.borrow().get(name).cloned()) {
            return Some(Ok(src));
        }
        nybl::stdlib::resolve(name).map(|s| Ok(s.to_string()))
    }
}

// Per-test module table — tests that use imports populate it via
// `set_modules` and the `resolve_module` impl above reads from it.
// Kept in a thread-local so the simple `RecordHost` struct stays
// shareable between walker and VM runs.
thread_local! {
    static MODULES: std::cell::RefCell<std::collections::HashMap<String, String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn set_modules(modules: &[(&str, &str)]) {
    MODULES.with(|m| {
        let mut map = m.borrow_mut();
        map.clear();
        for (name, src) in modules {
            map.insert((*name).to_string(), (*src).to_string());
        }
    });
}

/// Run `code` through both engines with the given limits. Both
/// engines must agree on prints and on success/failure; otherwise
/// the harness panics with a diff. Returns the shared outcome so
/// callers can inspect prints or the error message.
fn run_both(code: &str, limits: &NyblLimits) -> Outcome {
    let tw = {
        let mut host = RecordHost::new();
        let result = nybl::run(code, &mut host, limits);
        Outcome {
            prints: host.prints.borrow().clone(),
            error: result.err().map(|e| e.message),
        }
    };
    let vm = {
        let mut host = RecordHost::new();
        let result = nybl_vm::run(code, &mut host, limits);
        Outcome {
            prints: host.prints.borrow().clone(),
            error: result.err().map(|e| e.message),
        }
    };

    if tw.prints != vm.prints {
        panic!(
            "prints diverged on:\n{}\n\ntree-walker: {:?}\nbytecode vm: {:?}",
            code, tw.prints, vm.prints
        );
    }

    match (&tw.error, &vm.error) {
        (None, None) => {}
        (Some(a), Some(b)) if a == b => {}
        (Some(a), Some(b)) => {
            panic!("error messages diverged on:\n{code}\n\ntree-walker: {a}\nbytecode vm: {b}")
        }
        (Some(a), None) => panic!(
            "tree-walker errored but VM succeeded on:\n{}\n\ntree-walker error: {}\nvm prints: {:?}",
            code, a, vm.prints
        ),
        (None, Some(b)) => panic!(
            "VM errored but tree-walker succeeded on:\n{}\n\nvm error: {}\ntw prints: {:?}",
            code, b, tw.prints
        ),
    }

    tw
}

fn standard() -> NyblLimits {
    NyblLimits::standard()
}

#[test]
#[ignore = "bounded release-mode scaling probe; run explicitly before releases"]
fn type_publication_scaling_stays_below_quadratic_growth() {
    use std::time::{Duration, Instant};

    fn source(count: usize) -> String {
        (0..count)
            .map(|index| format!("struct Type{index} {{}}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn best_of(
        source: &str,
        run: impl Fn(&str, &mut RecordHost, &NyblLimits) -> Result<(), NyblError>,
    ) -> Duration {
        let limits = NyblLimits {
            max_steps: 100_000,
            max_memory: 64 * 1024 * 1024,
        };
        (0..3)
            .map(|_| {
                let mut host = RecordHost::new();
                let started = Instant::now();
                run(source, &mut host, &limits).expect("type-heavy program should run");
                started.elapsed()
            })
            .min()
            .expect("at least one sample")
    }

    let mut walker = Vec::new();
    let mut vm = Vec::new();
    for count in [1_000, 2_000, 4_000] {
        let source = source(count);
        walker.push(best_of(&source, |source, host, limits| {
            nybl::run(source, host, limits)
        }));
        vm.push(best_of(&source, |source, host, limits| {
            nybl_vm::run(source, host, limits)
        }));
    }
    eprintln!("type publication walker 1k/2k/4k: {walker:?}");
    eprintln!("type publication VM     1k/2k/4k: {vm:?}");

    for (engine, samples) in [("walker", walker), ("VM", vm)] {
        for pair in samples.windows(2) {
            assert!(
                pair[1].as_nanos() < pair[0].as_nanos().saturating_mul(7) / 2,
                "{engine} type publication grew too quickly: {samples:?}"
            );
        }
    }
}

fn tight() -> NyblLimits {
    NyblLimits {
        max_steps: 500,
        max_memory: 64 * 1024,
    }
}

/// Run both engines, assert success, return last print.
fn say(code: &str) -> String {
    let out = run_both(code, &standard());
    assert!(
        out.is_ok(),
        "program errored ({}): {}",
        out.error.as_deref().unwrap_or(""),
        code
    );
    out.prints
        .last()
        .cloned()
        .unwrap_or_else(|| panic!("no print output for: {code}"))
}

/// Run both engines, assert failure, return error message.
fn run_err(code: &str) -> String {
    let out = run_both(code, &standard());
    out.error
        .unwrap_or_else(|| panic!("expected an error, but program succeeded: {code}"))
}

fn run_err_with_limits(code: &str, limits: NyblLimits) -> String {
    let out = run_both(code, &limits);
    out.error
        .unwrap_or_else(|| panic!("expected an error, but program succeeded: {code}"))
}

/// Run both engines with the given limits, assert both errored, and
/// return `(tree_walker_message, vm_message)`.
///
/// Use this for resource-limit tests where the engines legitimately
/// halt on different error classes — e.g. a memory bomb that also
/// churns through enough bytecode ops to trip the step budget. The
/// existing safety tests already tolerate either error class via
/// `contains("Memory limit") || contains("too many steps")`, so we
/// just apply that check to both messages independently rather than
/// requiring exact equality.
fn run_err_loose(code: &str, limits: NyblLimits) -> (String, String) {
    let tw = {
        let mut host = RecordHost::new();
        nybl::run(code, &mut host, &limits).err().map(|e| e.message)
    };
    let vm = {
        let mut host = RecordHost::new();
        nybl_vm::run(code, &mut host, &limits)
            .err()
            .map(|e| e.message)
    };
    match (tw, vm) {
        (Some(a), Some(b)) => (a, b),
        (None, Some(_)) => panic!("tree-walker succeeded on:\n{code}"),
        (Some(_), None) => panic!("bytecode vm succeeded on:\n{code}"),
        (None, None) => panic!("both engines succeeded on:\n{code}"),
    }
}

/// Helper for safety tests: assert the `"Memory limit"` OR
/// `"too many steps"` bailout fires on both engines.
#[track_caller]
fn assert_both_resource_limit(tw: &str, vm: &str) {
    for (engine, msg) in [("tree-walker", tw), ("bytecode vm", vm)] {
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "{engine} did not hit a resource limit; got: {msg}"
        );
    }
}

/// Run both engines, assert both hit the step limit, and assert the
/// error carries `expected_line` — the header line of the outermost
/// active loop. The two engines charge steps at different points
/// (per-statement vs per-bytecode-instruction), so this only holds
/// because both resolve the error line through their loop context
/// rather than reporting wherever the counter happened to trip.
#[track_caller]
fn assert_both_step_limit_at_line(code: &str, limits: &NyblLimits, expected_line: u32) {
    for engine in ["tree-walker", "bytecode vm"] {
        let mut host = RecordHost::new();
        let result = if engine == "tree-walker" {
            nybl::run(code, &mut host, limits)
        } else {
            nybl_vm::run(code, &mut host, limits)
        };
        let err = match result {
            Err(err) => err,
            Ok(()) => panic!("{engine} did not error on:\n{code}"),
        };
        assert!(err.is_fatal, "{engine} returned non-fatal error: {err}");
        assert!(
            err.message.contains("too many steps"),
            "{} errored with `{}` instead of the step limit on:\n{}",
            engine,
            err.message,
            code
        );
        assert_eq!(
            err.line,
            Some(expected_line),
            "{engine} reported the step limit at the wrong line on:\n{code}"
        );
    }
}

#[track_caller]
fn assert_both_value_depth_errors(code: &str, expected_line: u32) {
    for engine in ["tree-walker", "bytecode vm"] {
        let mut host = RecordHost::new();
        let result = if engine == "tree-walker" {
            nybl::run(code, &mut host, &NyblLimits::standard())
        } else {
            nybl_vm::run(code, &mut host, &NyblLimits::standard())
        };
        assert!(host.prints.borrow().is_empty(), "{engine} printed");
        let err = result.unwrap_err();
        assert!(err.is_fatal, "{engine} returned non-fatal error: {err}");
        assert_eq!(err.message, nybl::value::VALUE_DEPTH_ERROR_MESSAGE);
        assert_eq!(err.line, Some(expected_line));
    }
}

#[test]
fn targeted_parse_diagnostics_match_walker_and_vm_entry_points() {
    let cases = [
        (
            "let label = match 1 {\n  1 => { print(\"one\") },\n  _ => \"other\",\n}",
            "`{ ... }` after `=>` is a dictionary expression, not a match-arm block",
            2,
            8,
            "`match` arm bodies must be a single expression; put it directly after `=>`, or quote dictionary keys if you meant to return a dictionary.",
        ),
        (
            "for i in 0..3 {}",
            "`..` range syntax is not supported in expressions",
            1,
            11,
            "use `range(start, end)` instead, for example `range(0, 3)`.",
        ),
        (
            "const Y = 2\nmatch 3 { Y => 0 }",
            "`match` pattern binding `Y` looks like a constant, but a value name is required here",
            2,
            11,
            "uppercase names can't be pattern bindings. To match against the value of `Y`, bind a lowercase name and compare it in a guard: `n if n == Y => ...`",
        ),
    ];

    for (source, message, line, column, hint) in cases {
        for engine in ["tree-walker", "bytecode vm"] {
            let mut host = RecordHost::new();
            let error = if engine == "tree-walker" {
                nybl::run(source, &mut host, &standard())
            } else {
                nybl_vm::run(source, &mut host, &standard())
            }
            .expect_err("invalid syntax must fail before execution");

            assert!(host.prints.borrow().is_empty(), "{engine} printed");
            assert_eq!(error.message, message, "engine: {engine}");
            assert_eq!(error.line, Some(line), "engine: {engine}");
            assert_eq!(error.column, Some(column), "engine: {engine}");
            assert_eq!(
                error.friendly_hint.as_deref(),
                Some(hint),
                "engine: {engine}"
            );
        }
    }
}

// ─── Automatic semicolon insertion ────────────────────────────────

#[test]
fn multiline_delimiters_and_leading_dot_match_both_engines() {
    let out = run_both(
        r#"fn add(a, b) { return a + b }
let values = [
    1,
    add(
        2,
        3
    ),
    [
        6,
    ][
        0
    ],
]
let config = {
    "target": values[
        1
    ],
    "label": "nybl"
}
if (
    config[
        "target"
    ] == 5 && values.len() == 3
) {
    print(
        values[0] +
        values[1] +
        values[2]
    )
}
let length = values
    // Leading-dot continuation may cross comments and blank lines.

    .len()
    .to_str()
print(length)"#,
        &standard(),
    );
    assert!(out.is_ok(), "program errored: {:?}", out.error);
    assert_eq!(out.prints, ["12", "3"]);
}

#[test]
fn nested_lambda_braces_keep_statement_boundaries_in_both_engines() {
    assert_eq!(
        say(r#"let functions = [
    fn() {
        let x = 1
        let y = 2
        return x + y
    },
]
let wrapped = (fn() {
    let x = 4
    let y = 5
    return x + y
})
print(functions[0]() + wrapped())"#),
        "12"
    );
}

#[test]
fn return_newline_semantics_match_both_engines() {
    let out = run_both(
        r#"fn bare() {
    return
    42
}
fn grouped() {
    return (
        42
    )
}
print(bare())
print(grouped())"#,
        &standard(),
    );
    assert!(out.is_ok(), "program errored: {:?}", out.error);
    assert_eq!(out.prints, ["none", "42"]);
}

// ─── Arithmetic ───────────────────────────────────────────────────

#[test]
fn add_numbers() {
    assert_eq!(say("print(1 + 2)"), "3");
}

#[test]
fn subtract() {
    assert_eq!(say("print(10 - 3)"), "7");
}

#[test]
fn multiply() {
    assert_eq!(say("print(4 * 5)"), "20");
}

#[test]
fn divide_float() {
    assert_eq!(say("print(7 / 2)"), "3.5");
}

#[test]
fn divide_whole() {
    assert_eq!(say("print(6 / 2)"), "3");
}

#[test]
fn modulo() {
    assert_eq!(say("print(10 % 3)"), "1");
}

#[test]
fn precedence() {
    assert_eq!(say("print(2 + 3 * 4)"), "14");
}

#[test]
fn parentheses() {
    assert_eq!(say("print((2 + 3) * 4)"), "20");
}

#[test]
fn unary_neg() {
    assert_eq!(say("print(-5)"), "-5");
}

#[test]
fn unary_not() {
    assert_eq!(say("print(!true)"), "false");
}

// ─── Strings ──────────────────────────────────────────────────────

#[test]
fn string_concat() {
    assert_eq!(say(r#"print("hello" + " " + "world")"#), "hello world");
}

#[test]
fn string_repeat() {
    assert_eq!(say(r#"print("ab" * 3)"#), "ababab");
}

#[test]
fn string_interpolation() {
    assert_eq!(
        say(r#"let name = "nybl"
print("hi {name}!")"#),
        "hi nybl!"
    );
}

#[test]
fn string_interpolation_sees_function_parameters_locals_and_shadowing() {
    assert_eq!(
        say(r#"fn greet(name) {
    let punctuation = "!"
    if true {
        let name = "inner"
        return "hi {name}{punctuation}"
    }
    return "unreachable"
}
print(greet("outer"))"#),
        "hi inner!"
    );
}

#[test]
fn indexed_locals_preserve_redeclaration_scope_exit_and_ref_updates_diff() {
    assert_eq!(
        say(r#"fn update(ref value) {
    let local = value
    let local = local + 1
    if true {
        let value = 40
        let value = value + 2
        print(value)
    }
    value = local
}
let value = 3
update(ref value)
print(value)"#),
        "4"
    );
}

#[test]
fn string_interpolation_missing_binding_error_matches_walker() {
    let out = run_both(r#"print("missing: {unknown}")"#, &standard());
    assert_eq!(out.error.as_deref(), Some("Variable `unknown` not found"));
}

#[test]
fn lambda_interpolation_missing_binding_error_matches_walker() {
    let out = run_both(
        r#"let read = fn() { return "{unknown}" }
print(read())"#,
        &standard(),
    );
    assert_eq!(out.error.as_deref(), Some("Variable `unknown` not found"));
}

#[test]
fn string_auto_coerce_in_add() {
    assert_eq!(say(r#"print("val=" + 42)"#), "val=42");
}

// ─── Comparisons & Logic ──────────────────────────────────────────

#[test]
fn equality() {
    assert_eq!(say("print(1 == 1)"), "true");
    assert_eq!(say("print(1 == 2)"), "false");
    assert_eq!(say("print(1 != 2)"), "true");
}

#[test]
fn ordering() {
    assert_eq!(say("print(3 < 5)"), "true");
    assert_eq!(say("print(5 <= 5)"), "true");
    assert_eq!(say("print(6 > 5)"), "true");
    assert_eq!(say("print(5 >= 6)"), "false");
}

#[test]
fn logical_and_or() {
    assert_eq!(say("print(true && false)"), "false");
    assert_eq!(say("print(true || false)"), "true");
}

#[test]
fn short_circuit_and() {
    assert_eq!(say("print(false && x)"), "false");
}

#[test]
fn short_circuit_or() {
    assert_eq!(say("print(true || x)"), "true");
}

// ─── Variables ────────────────────────────────────────────────────

#[test]
fn let_and_use() {
    assert_eq!(say("let x = 10\nprint(x)"), "10");
}

#[test]
fn assign() {
    assert_eq!(say("let x = 1\nx = 5\nprint(x)"), "5");
}

#[test]
fn compound_assign() {
    assert_eq!(say("let x = 10\nx += 5\nprint(x)"), "15");
    assert_eq!(say("let x = 10\nx -= 3\nprint(x)"), "7");
    assert_eq!(say("let x = 4\nx *= 3\nprint(x)"), "12");
    assert_eq!(say("let x = 10\nx /= 4\nprint(x)"), "2.5");
    assert_eq!(say("let x = 10\nx %= 3\nprint(x)"), "1");
}

#[test]
fn named_container_assignment_preserves_cow_value_semantics_diff() {
    let outcome = run_both(
        r#"let array = [1, 2]
let old_array = array
array[0] = 9
array[1] += 3
print(array)
print(old_array)

let dict = {"n": 4}
let old_dict = dict
dict["n"] += 6
dict["extra"] = 8
print(dict)
print(old_dict)

struct Counter { n, label }
let counter = Counter { n: 3, label: "old" }
let old_counter = counter
counter.n *= 4
counter.label = "new"
print(counter.n)
print(counter.label)
print(old_counter.n)
print(old_counter.label)"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        [
            "[9, 5]",
            "[1, 2]",
            r#"{"n": 10, "extra": 8}"#,
            r#"{"n": 4}"#,
            "12",
            "new",
            "3",
            "old",
        ]
    );
}

#[test]
fn const_container_assignment_is_rejected_by_both_engines_diff() {
    let cases = [
        "const VALUES = [1, 2]\nVALUES[0] = 9",
        "const VALUES = [1, 2]\nVALUES[0] += 9",
        "const LOOKUP = {\"n\": 1}\nLOOKUP[\"n\"] = 9",
        "const LOOKUP = {\"n\": 1}\nLOOKUP[\"n\"] += 9",
        "struct Counter { n }\nconst COUNTER = Counter { n: 1 }\nCOUNTER.n = 9",
        "struct Counter { n }\nconst COUNTER = Counter { n: 1 }\nCOUNTER.n += 9",
        "const VALUES = [1, 2]\n((VALUES))[0] = 9",
        "const GRID = [[1]]\nGRID[0][0] += 9",
    ];

    for source in cases {
        let message = run_err(source);
        assert!(
            message.contains("can't reassign") && message.contains("constant"),
            "source: {source}\nerror: {message}"
        );
    }
}

#[test]
fn const_array_mutating_methods_are_rejected_by_both_engines_diff() {
    for call in [
        "VALUES.push(4)",
        "VALUES.pop()",
        "VALUES.insert(0, 4)",
        "VALUES.remove(0)",
        "VALUES.reverse()",
        "(VALUES).sort()",
    ] {
        let source = format!("const VALUES = [3, 1, 2]\n{call}");
        assert_eq!(
            run_err(&source),
            "can't reassign `VALUES` — it's a constant",
            "source: {source}"
        );
    }

    assert_eq!(
        run_err(
            r#"fn mutate() {
    const VALUES = [3, 1, 2]
    VALUES.sort()
}
mutate()"#
        ),
        "can't reassign `VALUES` — it's a constant"
    );
}

#[test]
fn const_non_array_receivers_keep_ordinary_method_dispatch_diff() {
    assert_eq!(
        say(r#"struct Accumulator { total }
fn Accumulator.push(self, value) { return self.total + value }
fn Accumulator.pop(self) { return self.total }
const ACCUMULATOR = Accumulator { total: 7 }
const LOOKUP = {"n": 1}
print([ACCUMULATOR.push(5), ACCUMULATOR.pop(), LOOKUP.keys()])"#),
        r#"[12, 7, ["n"]]"#
    );
    assert_eq!(
        run_err(
            r#"const LOOKUP = {"n": 1}
LOOKUP.remove("n")"#
        ),
        "Dict doesn't have a .remove() method"
    );
}

#[test]
fn lowercase_array_mutating_methods_remain_valid_diff() {
    assert_eq!(
        say(r#"let values = [3, 1, 2]
values.sort()
values.reverse()
values.insert(1, 4)
values.remove(0)
values.push(5)
values.pop()
print(values)"#),
        "[4, 2, 1]"
    );
}

#[test]
fn const_declaration_shadowing_remains_engine_consistent_diff() {
    assert_eq!(
        say("const VALUE = [1]\nconst VALUE = [2]\nprint(VALUE[0])"),
        "2"
    );
}

#[test]
fn const_index_reads_in_mutable_targets_remain_engine_consistent_diff() {
    assert_eq!(
        say("const INDEX = 0\nlet values = [1]\nvalues[INDEX] += 2\nprint(values)"),
        "[3]"
    );
}

#[test]
fn named_index_assignment_observes_rhs_then_index_then_live_current_diff() {
    assert_eq!(
        say(r#"let values = [1, 2]
values[0] += values.remove(0)
print(values)"#),
        "[3]"
    );

    let message = run_err(
        r#"let values = [0, 1]
values[values.pop()] = 9"#,
    );
    assert!(message.contains("out of bounds"), "got: {message}");
}

#[test]
fn undefined_variable_error() {
    assert!(run_err("print(nope)").contains("not found"));
}

#[test]
fn assign_undeclared_error() {
    assert!(run_err("x = 5").contains("doesn't exist"));
}

// ─── If / Else ────────────────────────────────────────────────────

#[test]
fn if_true_branch() {
    assert_eq!(
        say("if true { print(\"yes\") } else { print(\"no\") }"),
        "yes"
    );
}

#[test]
fn if_false_branch() {
    assert_eq!(
        say("if false { print(\"yes\") } else { print(\"no\") }"),
        "no"
    );
}

#[test]
fn struct_literals_in_condition_delimiters_diff() {
    let outcome = run_both(
        r#"struct Point { x, y }
enum Maybe { Some(value) }
fn get_x(point) { return point.x }
fn Point.same_x(self, other) { return self.x == other.x }
let choices = [false, true]

if get_x(Point { x: 1, y: 2 }) == 1 { print("call") }
if (Point { x: 2, y: 0 }).x == 2 { print("paren") }
if (Point { x: 3, y: 0 }).same_x(Point { x: 3, y: 1 }) { print("method-arg") }
if choices[Point { x: 1, y: 0 }.x] { print("index") }
if [Point { x: 4, y: 0 }][0].x == 4 { print("array") }
if {"point": Point { x: 5, y: 0 }}["point"].x == 5 { print("dict") }
if Ok(Point { x: 6, y: 0 }).is_ok() { print("result") }
if match Maybe::Some(Point { x: 7, y: 0 }) {
    Maybe::Some(point) => point.x,
} == 7 { print("enum-tuple") }
if match 1 {
    value if Point { x: value, y: 0 }.x == 1 => Point { x: 8, y: 0 }.x,
    _ => 0,
} == 8 { print("match") }
if (if true { Point { x: 9, y: 0 }.x } else { 0 }) == 9 { print("if-expr") }
if fn() { return Point { x: 10, y: 0 }.x }() == 10 { print("lambda") }

let count = 0
while get_x(Point { x: count, y: 0 }) < 1 { count += 1 }
print("while")
for point in [Point { x: 11, y: 0 }] { print(point.x) }
repeat [Point { x: 12, y: 0 }].len() { print("repeat") }"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        [
            "call",
            "paren",
            "method-arg",
            "index",
            "array",
            "dict",
            "result",
            "enum-tuple",
            "match",
            "if-expr",
            "lambda",
            "while",
            "11",
            "repeat",
        ]
    );
}

#[test]
fn control_flow_head_braces_stay_disambiguated_diff() {
    assert_eq!(
        say(r#"const CONDITION = false
const ITEMS = [1, 2]
const COUNT = 0
const VALUE = 1
if CONDITION { print("bad-if") }
while CONDITION { print("bad-while") }
for item in ITEMS { print(item) }
repeat COUNT { print("bad-repeat") }
print(match VALUE { _ => "ok" })"#,),
        "ok"
    );
}

#[test]
fn if_else_if() {
    assert_eq!(
        say(r#"let x = 2
if x == 1 { print("one") } else if x == 2 { print("two") } else { print("other") }"#),
        "two"
    );
}

#[test]
fn if_expression() {
    assert_eq!(say("let x = if true { 1 } else { 2 }\nprint(x)"), "1");
}

#[test]
fn multiline_if_expression_layout_diff() {
    let outcome = run_both(
        r#"let first = if true {
    // A continued expression remains one branch value.

    1 +
        2;
}
else {
    99
}
let second = if false {
    0
} else {
    if true {
        4
    }
    else {
        5
    }
}
print(first)
print(second)
let third = if true {
    (
        5
        + 6
    )
} else {
    0
}
print(third)"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["3", "4", "11"]);
}

#[test]
fn if_expression_branches_remain_single_expression_diff() {
    let newline_error = run_err(
        r#"let value = if true {
    1
    2
} else {
    3
}"#,
    );
    assert_eq!(newline_error, "Expected `}` but found `an integer`");

    let second_statement = run_err(
        r#"let value = if true {
    1
    let inner = 2
} else {
    3
}"#,
    );
    assert_eq!(second_statement, "Expected `}` but found `let`");

    let statement_error = run_err("let value = if true { let inner = 1 } else { 2 }");
    assert_eq!(statement_error, "I didn't expect `let` here");

    let leading_semicolon = run_err("let value = if true { ; 1 } else { 2 }");
    assert_eq!(leading_semicolon, "I didn't expect `;` here");

    let boundary_semicolon = run_err("let value = if true { 1 };\nelse { 2 }");
    assert_eq!(boundary_semicolon, "Expected `else` but found `;`");
}

#[test]
fn if_expression_jump_targets_survive_all_peephole_fusions_diff() {
    let outcome = run_both(
        r#"
fn add_const(c, a, b) { return (if c { a } else { b }) + 1 }
fn add_local(c, a, b, d) { return (if c { a } else { b }) + d }
fn sub_const(c, a, b) { return (if c { a } else { b }) - 1 }
fn lt_const(c, a, b) { return (if c { a } else { b }) < 15 }
fn lt_local(c, a, b, d) { return (if c { a } else { b }) < d }
fn nested(c1, c2, a, b, d) {
    return (if c1 { if c2 { a } else { b } } else { d }) + 1
}

print(add_const(true, 10, 20))
print(add_const(false, 10, 20))
print(add_local(true, 10, 20, 100))
print(add_local(false, 10, 20, 100))
print(sub_const(true, 10, 20))
print(sub_const(false, 10, 20))
print(lt_const(true, 10, 20))
print(lt_const(false, 10, 20))
print(lt_local(true, 10, 20, 15))
print(lt_local(false, 10, 20, 15))
print(nested(true, true, 10, 20, 30))
print(nested(true, false, 10, 20, 30))
print(nested(false, true, 10, 20, 30))
"#,
        &standard(),
    );

    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        vec![
            "11", "21", "110", "120", "9", "19", "true", "false", "true", "false", "11", "21",
            "31",
        ]
    );
}

#[test]
fn inc_local_int_matches_direct_compound_and_control_flow_semantics_diff() {
    let outcome = run_both(
        r#"
fn direct(value) {
    value = value + 3
    value = value - 1
    return value
}
fn compound(value) {
    value += 7
    value -= 2
    return value
}
fn branch(condition, value) {
    value = if condition { value + 1 } else { value + 2 }
    value += 4
    return value
}
fn large(value) {
    value = value + 2147483648
    return value
}
fn generic(value) {
    value += 1
    return value
}

print(direct(10))
print(compound(10))
print(branch(true, 10))
print(branch(false, 10))
print(large(1))
print(generic(1.5))
print(generic("v"))
"#,
        &standard(),
    );

    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        ["12", "15", "15", "16", "2147483649", "2.5", "v1"]
    );
}

#[test]
fn local_subtraction_preserves_generic_operator_errors_diff() {
    for source in [
        r#"fn expression(value) { return value - 1 }
print(expression("v"))"#,
        r#"fn direct(value) {
    value = value - 1
    return value
}
print(direct("v"))"#,
        r#"fn different_slot(source, target) {
    target = source - 1
    return target
}
print(different_slot("v", 0))"#,
        r#"fn compound(value) {
    value -= 1
    return value
}
print(compound("v"))"#,
    ] {
        let outcome = run_both(source, &standard());
        assert_eq!(
            outcome.error.as_deref(),
            Some("Can't use `-` with string and int")
        );
    }
}

#[test]
fn local_add_superinstructions_preserve_canonical_overflow_errors_diff() {
    for source in [
        r#"fn expression(value) { return value + 1 }
print(expression(9223372036854775807))"#,
        r#"fn direct(value) {
    value = value + 1
    return value
}
print(direct(9223372036854775807))"#,
        r#"fn compound(value) {
    value += 1
    return value
}
print(compound(9223372036854775807))"#,
        r#"fn local_local(left, right) { return left + right }
print(local_local(9223372036854775807, 1))"#,
    ] {
        let outcome = run_both(source, &standard());
        assert_eq!(outcome.error.as_deref(), Some("Integer overflow in `+`"));
    }
}

// ─── While ────────────────────────────────────────────────────────

#[test]
fn while_loop() {
    assert_eq!(say("let i = 0\nwhile i < 5 { i += 1 }\nprint(i)"), "5");
}

#[test]
fn while_break() {
    assert_eq!(
        say("let i = 0\nwhile true { i += 1\nif i == 3 { break } }\nprint(i)"),
        "3"
    );
}

#[test]
fn while_continue() {
    assert_eq!(
        say(r#"let sum = 0
let i = 0
while i < 10 {
    i += 1
    if i % 2 == 0 { continue }
    sum += i
}
print(sum)"#),
        "25"
    );
}

// ─── For ──────────────────────────────────────────────────────────

#[test]
fn for_over_array() {
    assert_eq!(
        say(r#"let sum = 0
for x in [10, 20, 30] { sum += x }
print(sum)"#),
        "60"
    );
}

#[test]
fn for_over_range() {
    assert_eq!(
        say("let sum = 0\nfor i in range(5) { sum += i }\nprint(sum)"),
        "10"
    );
}

#[test]
fn for_over_string() {
    assert_eq!(
        say(r#"let out = ""
for ch in "abc" { out += ch + "-" }
print(out)"#),
        "a-b-c-"
    );
}

#[test]
fn for_with_break() {
    assert_eq!(
        say("let last = 0\nfor i in range(100) { if i == 3 { break }\nlast = i }\nprint(last)"),
        "2"
    );
}

#[test]
fn nested_for_break_preserves_outer_iterator_diff() {
    let outcome = run_both(
        r#"for i in [1, 2, 3] {
    for j in [10, 20, 30] { break }
    print(i)
}"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["1", "2", "3"]);
}

#[test]
fn inner_repeat_break_preserves_outer_iterator_diff() {
    assert_eq!(
        say(r#"let seen = []
for i in [1, 2, 3] {
    repeat 5 {
        if true { break }
    }
    seen.push(i)
}
print(seen)"#),
        "[1, 2, 3]"
    );
}

#[test]
fn inner_while_break_does_not_pop_outer_iterator_diff() {
    assert_eq!(
        say(r#"let seen = []
for i in [1, 2, 3] {
    while true { break }
    seen.push(i)
}
print(seen)"#),
        "[1, 2, 3]"
    );
}

#[test]
fn mixed_nested_loop_control_preserves_function_frames_diff() {
    let outcome = run_both(
        r#"fn exercise() {
    let seen = []
    for outer in [1, 2] {
        let n = 0
        while n < 3 {
            n += 1
            repeat 3 {
                if n == 1 { break }
                for inner in [20, 10, 30] {
                    if inner == 20 { continue }
                    seen.push(outer * 100 + n * 10 + inner)
                    break
                }
                break
            }
            if n < 3 { continue }
            break
        }
    }
    return seen
}
print(exercise())
print(exercise())"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        ["[130, 140, 230, 240]", "[130, 140, 230, 240]"]
    );
}

#[test]
fn break_and_continue_do_not_leak_top_level_bindings_diff() {
    let break_error = run_err(
        r#"for item in [1, 2, 3] {
    if true { break }
}
print(item)"#,
    );
    assert!(break_error.contains("not found"), "got: {break_error}");

    let continue_error = run_err(
        r#"let n = 0
while n < 3 {
    n += 1
    if true {
        let inner = n
        continue
    }
}
print(inner)"#,
    );
    assert!(
        continue_error.contains("not found"),
        "got: {continue_error}"
    );
}

#[test]
fn nested_top_level_loop_control_unwinds_only_innermost_loop_diff() {
    assert_eq!(
        say(r#"let visits = 0
for outer in [1, 2] {
    repeat 3 {
        while true {
            let local = outer
            break
        }
        if outer == 1 { continue }
        visits += 1
        break
    }
}
print(visits)"#),
        "1"
    );
}

#[test]
fn match_scopes_compose_with_loop_control_diff() {
    assert_eq!(
        say(r#"let seen = []
for value in [1, 2, 3] {
    let label = match value {
        n if n == 2 => "two",
        n => n.to_str(),
    }
    if value == 2 { continue }
    seen.push(label)
}
print(seen)"#),
        "[\"1\", \"3\"]"
    );
}

#[test]
fn methods_and_closures_keep_slot_scopes_on_loop_control_diff() {
    let outcome = run_both(
        r#"struct Runner { limit }
fn Runner.run(self) {
    let i = 0
    let sum = 0
    while i < self.limit {
        i += 1
        if i % 2 == 0 { continue }
        sum += i
    }
    return sum
}
let build = fn(limit) {
    return fn() {
        let n = 0
        repeat limit {
            if n == 2 { break }
            n += 1
        }
        return n
    }
}
print(Runner { limit: 5 }.run())
print(build(5)())"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["9", "2"]);
}

// ─── Repeat ───────────────────────────────────────────────────────

#[test]
fn repeat_loop() {
    assert_eq!(say("let n = 0\nrepeat 4 { n += 1 }\nprint(n)"), "4");
}

#[test]
fn repeat_zero() {
    assert_eq!(say("let n = 99\nrepeat 0 { n = 0 }\nprint(n)"), "99");
}

// ─── Functions ────────────────────────────────────────────────────

#[test]
fn fn_basic() {
    assert_eq!(say("fn double(x) { return x * 2 }\nprint(double(5))"), "10");
}

#[test]
fn fn_implicit_return_none() {
    assert_eq!(
        say(r#"fn noop() { let x = 1 }
print(noop().type())"#),
        "none"
    );
}

#[test]
fn fn_multiple_params() {
    assert_eq!(say("fn add(a, b) { return a + b }\nprint(add(3, 7))"), "10");
}

#[test]
fn fn_scope_isolation() {
    assert_eq!(
        say(r#"let secret = 42
fn peek() { return secret }
print(peek())"#),
        "42"
    );
}

#[test]
fn fn_wrong_arg_count() {
    assert!(run_err("fn f(a, b) { return a }\nf(1)").contains("expects 2"));
}

#[test]
fn fn_recursion() {
    assert_eq!(
        say(r#"fn fib(n) {
    if n <= 1 { return n }
    return fib(n - 1) + fib(n - 2)
}
print(fib(10))"#),
        "55"
    );
}

// ─── Structs ───────────────────────────────────────────────────────

#[test]
fn struct_basic_diff() {
    assert_eq!(
        say(r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
print(p.x + p.y)"#),
        "7"
    );
}

#[test]
fn struct_display_and_equality_diff() {
    assert_eq!(
        say(r#"struct Point { x, y }
let a = Point { x: 1, y: 2 }
let b = Point { x: 1, y: 2 }
print(a)
print(a == b)"#),
        "true"
    );
}

#[test]
fn struct_field_assign_diff() {
    assert_eq!(
        say(r#"struct Counter { n }
let c = Counter { n: 10 }
c.n += 5
c.n *= 2
print(c.n)"#),
        "30"
    );
}

#[test]
fn struct_nested_diff() {
    assert_eq!(
        say(r#"struct Inner { v }
struct Outer { name, inner }
let o = Outer { name: "a", inner: Inner { v: 42 } }
print(o.inner.v)"#),
        "42"
    );
}

#[test]
fn struct_missing_field_error_diff() {
    let msg = run_err(
        r#"struct P { x, y }
let p = P { x: 1 }"#,
    );
    assert!(msg.contains("Missing field"), "got: {msg}");
}

#[test]
fn runtime_type_declaration_validation_matches_walker_diff() {
    let errors = [
        (
            "struct P { value, value }",
            "Struct `P` has duplicate field `value`",
        ),
        ("enum E { A, A }", "Enum `E` has duplicate variant `A`"),
        (
            "enum E { Pair(value, value) }",
            "Enum variant `E::Pair` has duplicate field `value`",
        ),
        (
            "enum E { Named { value, value } }",
            "Enum variant `E::Named` has duplicate field `value`",
        ),
        (
            "struct P { value }\nstruct P { value, value }",
            "Struct `P` has duplicate field `value`",
        ),
        (
            "struct P { left, right }\nstruct P { right, left }",
            "Struct `P` is already declared",
        ),
        (
            "enum E { First, Second }\nenum E { Second, First }",
            "Enum `E` is already declared",
        ),
    ];
    for (source, expected) in errors {
        assert_eq!(run_err(source), expected, "source: {source}");
    }

    let dead = run_both(
        r#"if false {
    struct P { value, value }
    enum E { A, A, Pair(value, value), Named { item, item } }
}
print("ok")"#,
        &standard(),
    );
    assert!(dead.is_ok(), "unexpected error: {:?}", dead.error);
    assert_eq!(dead.prints, ["ok"]);
}

// ─── Enums ─────────────────────────────────────────────────────────

#[test]
fn enum_unit_variant_diff() {
    assert_eq!(
        say(r#"enum E { A, B }
print(E::A == E::A)
print(E::A == E::B)"#),
        "false"
    );
}

#[test]
fn enum_tuple_variant_diff() {
    assert_eq!(
        say(r#"enum Pair { Pt(x, y) }
let p = Pair::Pt(3, 4)
print(p)"#),
        "Pair::Pt(3, 4)"
    );
}

#[test]
fn enum_struct_variant_with_field_access_diff() {
    assert_eq!(
        say(r#"enum Shape { Rect { w, h } }
let r = Shape::Rect { w: 4, h: 3 }
print(r.w * r.h)"#),
        "12"
    );
}

#[test]
fn enum_structural_equality_diff() {
    assert_eq!(
        say(r#"enum E { A, B(n) }
print(E::B(1) == E::B(1))
print(E::B(1) == E::B(2))
print(E::A == E::B(1))"#),
        "false"
    );
}

#[test]
fn enum_variant_arity_mismatch_diff() {
    let msg = run_err(
        r#"enum E { P(a, b) }
let x = E::P(1)"#,
    );
    assert!(msg.contains("expects 2"), "got: {msg}");
}

// ─── User-defined methods ─────────────────────────────────────────

#[test]
fn method_on_struct_diff() {
    assert_eq!(
        say(r#"struct Point { x, y }
fn Point.sum(self) { return self.x + self.y }
let p = Point { x: 3, y: 4 }
print(p.sum())"#),
        "7"
    );
}

#[test]
fn method_chain_diff() {
    assert_eq!(
        say(r#"struct Adder { n }
fn Adder.then(self, m) { return Adder { n: self.n + m } }
let r = Adder { n: 1 }.then(2).then(3).then(4)
print(r.n)"#),
        "10"
    );
}

#[test]
fn method_on_enum_diff() {
    assert_eq!(
        say(r#"enum Shape { Circle(r), Rect { w, h } }
fn Shape.label(self) { return "shape" }
print(Shape::Circle(5).label())
print(Shape::Rect { w: 4, h: 3 }.label())"#),
        "shape"
    );
}

#[test]
fn method_overrides_builtin_diff() {
    assert_eq!(
        say(r#"struct Wrapper { data }
fn Wrapper.len(self) { return 99 }
let w = Wrapper { data: [1, 2, 3] }
print(w.len())"#),
        "99"
    );
}

#[test]
fn value_receiver_mutation_is_a_parse_error_diff() {
    let error = run_err(
        r#"struct Counter { n }
fn Counter.bump(self) { self.n = self.n + 1 }"#,
    );
    assert!(error.contains("can't mutate value receiver `self`"));

    let delegated = run_err(
        r#"struct Counter { n }
fn Counter.bump(ref self) { self.n += 1 }
fn Counter.bump_twice(self) {
  self.bump()
  self.bump()
}"#,
    );
    assert!(delegated.contains("can't mutate value receiver `self`"));
}

#[test]
fn ref_method_receiver_commits_after_arguments_and_rolls_back_errors_diff() {
    let out = run_both(
        r#"struct Counter { n }
fn Counter.bump(ref self, amount) {
  self.n = self.n + amount
  return self.n
}
fn Counter.fail(ref self) {
  self.n = 99
  panic("stop")
}
let counter = Counter { n: 1 }
fn side() {
  counter.n = 10
  return 2
}
print(counter.bump(side()), counter.n)
fn attempt() { counter.fail() }
print(try_call(attempt), counter.n)"#,
        &standard(),
    );
    assert!(out.is_ok(), "{:?}", out.error);
    assert_eq!(
        out.prints,
        vec![
            "12 12",
            "Result::Err(RuntimeError { message: \"stop\", line: 8 }) 12"
        ]
    );
}

#[test]
fn ref_method_receiver_must_be_a_unique_mutable_binding_diff() {
    let temporary = run_err(
        r#"struct Counter { n }
fn Counter.bump(ref self) { self.n = self.n + 1 }
Counter { n: 1 }.bump()"#,
    );
    assert!(temporary.contains("a `ref` method receiver must be a mutable place"));

    let duplicate = run_err(
        r#"struct Counter { n }
fn Counter.copy(ref self, ref other) { self.n = other.n }
let counter = Counter { n: 1 }
counter.copy(ref counter)"#,
    );
    assert!(duplicate.contains("same variable"));
}

// ─── Pattern matching ─────────────────────────────────────────────

#[test]
fn match_literal_number_diff() {
    assert_eq!(
        say(r#"let x = 2
let r = match x {
  1 => "one",
  2 => "two",
  _ => "other",
}
print(r)"#),
        "two"
    );
}

#[test]
fn match_wildcard_catches_all_diff() {
    assert_eq!(
        say(r#"let x = 42
let r = match x {
  0 => "zero",
  _ => "big",
}
print(r)"#),
        "big"
    );
}

#[test]
fn match_binding_captures_scrutinee_diff() {
    assert_eq!(
        say(r#"let x = 7
let r = match x {
  n => n * 2,
}
print(r)"#),
        "14"
    );
}

#[test]
fn match_guard_selects_arm_diff() {
    assert_eq!(
        say(r#"let x = 10
let r = match x {
  n if n < 5 => "small",
  n if n < 20 => "medium",
  _ => "big",
}
print(r)"#),
        "medium"
    );
}

#[test]
fn match_bindings_shadow_same_named_parameters_and_locals_diff() {
    let out = run_both(
        r#"fn from_parameter(value) {
    return match 7 {
        value if value == 99 => "outer parameter",
        value if value == 7 => value,
    }
}

fn from_local() {
    let value = 99
    return match 8 {
        value if value == 99 => "outer local",
        value if value == 8 => value,
    }
}

print(from_parameter(99))
print(from_local())"#,
        &standard(),
    );
    assert!(out.is_ok(), "unexpected error: {:?}", out.error);
    assert_eq!(out.prints, ["7", "8"]);
}

#[test]
fn match_bindings_shadow_same_named_interpolation_and_callee_slots_diff() {
    let out = run_both(
        r#"fn render(label) {
    return match 7 {
        label => "matched {label}",
    }
}

fn invoke(run) {
    return match fn() { return "matched callable" } {
        run => run(),
    }
}

print(render("outer label"))
print(invoke(fn() { return "outer callable" }))"#,
        &standard(),
    );
    assert!(out.is_ok(), "unexpected error: {:?}", out.error);
    assert_eq!(out.prints, ["matched 7", "matched callable"]);
}

#[test]
fn match_bindings_shadow_same_named_parent_slots_in_lambda_captures_diff() {
    let out = run_both(
        r#"fn update(value) {
    return match 7 {
        value => fn() {
            let previous = value
            value += 1
            return [previous, value]
        }(),
    }
}

print(update(99))"#,
        &standard(),
    );
    assert!(out.is_ok(), "unexpected error: {:?}", out.error);
    assert_eq!(out.prints, ["[7, 8]"]);
}

#[test]
fn match_binding_mutating_method_does_not_update_same_named_parameter_diff() {
    let out = run_both(
        r#"fn mutate(items) {
    let ignored = match [1] {
        items => items.push(2),
    }
    print(items)
}

mutate([9])"#,
        &standard(),
    );
    assert!(out.is_ok(), "unexpected error: {:?}", out.error);
    assert_eq!(out.prints, ["[9]"]);
}

#[test]
fn match_or_pattern_diff() {
    assert_eq!(
        say(r#"let x = 3
let r = match x {
  1 | 2 | 3 => "low",
  _ => "other",
}
print(r)"#),
        "low"
    );
}

#[test]
fn inconsistent_or_pattern_bindings_are_rejected_before_execution_diff() {
    for source in [
        "let x = 1\nlet r = match x { 1 | y => y, _ => 0 }\nprint(r)",
        "let x = 5\nlet r = match x { 1 | y => y, _ => 0 }\nprint(r)",
    ] {
        assert_eq!(
            run_err(source),
            "`or`-pattern alternative 2 binds `y`, but alternative 1 binds no names"
        );
    }
}

#[test]
fn complex_or_pattern_bindings_are_consistent_across_engines_diff() {
    let outcome = run_both(
        r#"struct Node { left, right }
enum Boxed { Pair(left, right), Record { first, second } }
let values = [
    Node { left: 1, right: 2 },
    Boxed::Pair(3, 4),
    Boxed::Record { first: 5, second: 6 },
]
for value in values {
    let encoded = match value {
        Node { left, right } | Boxed::Pair(right, left) | Boxed::Record { first: left, second: right } => left * 10 + right,
    }
    print(encoded)
}

enum Packet { Values(items, marker) }
let packets = [Packet::Values([1, 2, 3], 9), Packet::Values([7], 8)]
for packet in packets {
    let encoded = match packet {
        Packet::Values([_, head, ..tail] | [head, ..tail], marker) => head * 100 + marker * 10 + tail.len(),
    }
    print(encoded)
}

for items in [[1, 2], [3]] {
    print(match items { [x, x] | [x] => x })
}"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["12", "43", "56", "291", "780", "2", "3"]);
}

#[test]
fn match_enum_unit_variant_diff() {
    assert_eq!(
        say(r#"enum Light { Red, Green }
let l = Light::Green
let r = match l {
  Light::Red => "stop",
  Light::Green => "go",
}
print(r)"#),
        "go"
    );
}

#[test]
fn match_enum_tuple_binds_diff() {
    assert_eq!(
        say(r#"enum Result { Ok(v), Err(m) }
let r = Result::Ok(42)
let out = match r {
  Result::Ok(v) => v,
  Result::Err(_) => -1,
}
print(out)"#),
        "42"
    );
}

#[test]
fn match_enum_struct_variant_binds_diff() {
    assert_eq!(
        say(r#"enum Shape { Rect { w, h } }
let s = Shape::Rect { w: 4, h: 3 }
let a = match s {
  Shape::Rect { w, h } => w * h,
}
print(a)"#),
        "12"
    );
}

#[test]
fn match_struct_destructure_diff() {
    assert_eq!(
        say(r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
let r = match p {
  Point { x, y } => x + y,
}
print(r)"#),
        "7"
    );
}

#[test]
fn match_struct_partial_rest_diff() {
    assert_eq!(
        say(r#"struct Point { x, y, z }
let p = Point { x: 1, y: 2, z: 3 }
let r = match p {
  Point { y, .. } => y * 10,
}
print(r)"#),
        "20"
    );
}

#[test]
fn match_nested_enum_struct_diff() {
    assert_eq!(
        say(r#"enum FileError { NotFound(path) }
enum Result { Ok(v), Err(e) }
let r = Result::Err(FileError::NotFound("missing.txt"))
let msg = match r {
  Result::Ok(_) => "ok",
  Result::Err(FileError::NotFound(p)) => p,
}
print(msg)"#),
        "missing.txt"
    );
}

#[test]
fn match_array_exact_diff() {
    assert_eq!(
        say(r#"let xs = [1, 2, 3]
let r = match xs {
  [a, b, c] => a + b + c,
  _ => -1,
}
print(r)"#),
        "6"
    );
}

#[test]
fn match_array_with_rest_diff() {
    assert_eq!(
        say(r#"let xs = [10, 20, 30, 40]
let r = match xs {
  [first, ..rest] => first,
  _ => -1,
}
print(r)"#),
        "10"
    );
}

#[test]
fn match_no_arm_errors_diff() {
    let msg = run_err(
        r#"let x = 99
match x {
  1 => print("one"),
  2 => print("two"),
}"#,
    );
    assert!(
        msg.contains("No match arm matched the scrutinee"),
        "got: {msg}"
    );
}

#[test]
fn match_guard_rejects_then_next_arm_diff() {
    // Guard rejects the first arm even though the pattern matched,
    // so the scrutinee falls through to the wildcard.
    assert_eq!(
        say(r#"let x = 3
let r = match x {
  n if n > 100 => "huge",
  _ => "small",
}
print(r)"#),
        "small"
    );
}

#[test]
fn match_binding_scope_isolated_diff() {
    // `n` bound inside the arm doesn't leak to outer scope —
    // both engines must agree this errors cleanly.
    let msg = run_err(
        r#"let x = 5
match x {
  n => print(n),
}
print(n)"#,
    );
    assert!(
        msg.contains("not found") || msg.contains("Variable"),
        "got: {msg}"
    );
}

#[test]
fn named_fn_match_binding_scope_isolated_diff() {
    // Named-function frames have no initial value scope. Their first match
    // scope must still be popped when the arm completes.
    let msg = run_err(
        r#"fn read() {
  let value = match 1 { leaked => leaked }
  return leaked
}
print(read())"#,
    );
    assert!(
        msg.contains("not found") || msg.contains("Variable"),
        "got: {msg}"
    );
}

#[test]
fn match_scopes_in_loops_do_not_shadow_calls_or_leak_between_calls_diff() {
    // Exercise match scopes inside a slot-resolved loop/block, then resolve a
    // same-named function. A leaked pattern binding would make `target()` try
    // to call the matched integer instead. Repeating the outer call also
    // checks that frame cleanup restores the caller's scope depths exactly.
    let out = run_both(
        r#"fn target() { return 7 }
fn check(value) {
  repeat 2 {
    let ignored = match value { target => target }
  }
  return target()
}
print(check(1))
print(check(2))"#,
        &standard(),
    );
    assert!(out.is_ok(), "got: {:?}", out.error);
    assert_eq!(out.prints, vec!["7", "7"]);
}

#[test]
fn user_method_match_binding_scope_isolated_diff() {
    // User methods use the same zero-base frame layout as named functions.
    let msg = run_err(
        r#"struct Box { value }
fn Box.read(self) {
  let value = match self.value { leaked => leaked }
  return leaked
}
print(Box { value: 3 }.read())"#,
    );
    assert!(
        msg.contains("not found") || msg.contains("Variable"),
        "got: {msg}"
    );
}

#[test]
fn closure_match_scope_preserves_capture_floor_diff() {
    // Closure frames retain their capture map as scope zero. Popping a match
    // scope must reveal that map again rather than remove it.
    let out = run_both(
        r#"let captured = 10
let add = fn(value) {
  let matched = match value { captured => captured }
  return captured + matched
}
print(add(2))
print(add(5))"#,
        &standard(),
    );
    assert!(out.is_ok(), "got: {:?}", out.error);
    assert_eq!(out.prints, vec!["12", "15"]);
}

// ─── `try` operator ───────────────────────────────────────────────

#[test]
fn try_unwraps_ok_diff() {
    assert_eq!(
        say(r#"enum Result { Ok(v), Err(e) }
fn doit() {
    let v = try Result::Ok(42)
    return v
}
print(doit())"#),
        "42"
    );
}

#[test]
fn try_propagates_err_diff() {
    assert_eq!(
        say(r#"enum Result { Ok(v), Err(e) }
fn doit() {
    let v = try Result::Err("boom")
    return Result::Ok(v)
}
let r = doit()
print(match r {
    Result::Ok(v) => v,
    Result::Err(e) => e,
})"#),
        "boom"
    );
}

#[test]
fn try_chains_through_nested_calls_diff() {
    assert_eq!(
        say(r#"enum Result { Ok(v), Err(e) }
fn leaf() { return Result::Err("leaf-err") }
fn middle() {
    let v = try leaf()
    return Result::Ok(v + 1)
}
fn top() {
    let v = try middle()
    return Result::Ok(v * 2)
}
print(match top() {
    Result::Ok(v) => v,
    Result::Err(e) => e,
})"#),
        "leaf-err"
    );
}

#[test]
fn user_result_with_unit_ok_coexists_with_builtin_diff() {
    // Module-scoped types (phase 2b): the program's
    // `enum Result { Ok, Err(e) }` lives under `<root>.Result`,
    // distinct from the builtin `<builtin>.Result`. `try`
    // unwraps the user's unit `Ok` variant into `none` — same
    // behaviour the walker now exhibits.
    assert_eq!(
        say(r#"enum Result { Ok, Err(e) }
fn doit() {
    let v = try Result::Ok
    return v.type()
}
print(doit())"#),
        "none"
    );
}

#[test]
fn try_inside_lambda_returns_from_lambda_diff() {
    assert_eq!(
        say(r#"enum Result { Ok(v), Err(e) }
let f = fn() {
    let v = try Result::Err("inner")
    return Result::Ok(v)
}
let r = f()
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e,
})"#),
        "inner"
    );
}

#[test]
fn try_on_non_result_errors_diff() {
    let msg = run_err(
        r#"fn doit() {
    let v = try 42
    return v
}
doit()"#,
    );
    assert!(msg.contains("Result-shaped"), "got: {msg}");
}

#[test]
fn try_at_top_level_on_err_errors_diff() {
    let source = r#"enum Result { Ok(v), Err(e) }
let r = try Result::Err("boom")"#;
    let msg = run_err(source);
    assert!(msg.contains("top-level"), "got: {msg}");

    let mut host = RecordHost::new();
    let vm_error = nybl_vm::run(source, &mut host, &standard())
        .expect_err("top-level try on Err must fail in the VM");
    assert_eq!(
        vm_error.message,
        nybl::error_messages::TOP_LEVEL_TRY_ERROR_MESSAGE
    );
    assert_eq!(
        vm_error.friendly_hint.as_deref(),
        Some(nybl::error_messages::TOP_LEVEL_TRY_HINT)
    );
    assert_eq!(vm_error.line, Some(2));
    assert_eq!(vm_error.column, None);
}

#[test]
fn try_in_for_loop_short_circuits_diff() {
    assert_eq!(
        say(r#"enum Result { Ok(v), Err(e) }
fn lookup(i) {
    if i == 2 { return Result::Err("stop") }
    return Result::Ok(i * 10)
}
fn sum_until_err() {
    let total = 0
    for i in range(5) {
        let v = try lookup(i)
        total = total + v
    }
    return Result::Ok(total)
}
print(match sum_until_err() {
    Result::Ok(v) => v,
    Result::Err(e) => e,
})"#),
        "stop"
    );
}

// ─── Integer type (phase 6) ───────────────────────────────────────

#[test]
fn int_literal_type_diff() {
    assert_eq!(say("print(42.type())"), "int");
}

#[test]
fn float_literal_type_diff() {
    assert_eq!(say("print(42.0.type())"), "number");
}

#[test]
fn int_arithmetic_stays_int_diff() {
    assert_eq!(say("print(1 + 2)"), "3");
    assert_eq!(say("print((1 + 2).type())"), "int");
    assert_eq!(say("print(10 - 4)"), "6");
    assert_eq!(say("print(3 * 4)"), "12");
}

#[test]
fn division_always_returns_number_diff() {
    assert_eq!(say("print((10 / 3).type())"), "number");
    assert_eq!(say("print(10 / 4)"), "2.5");
    assert_eq!(say("print((10 / 5).type())"), "number");
}

#[test]
fn int_division_via_int_of_quotient_diff() {
    // There's no dedicated `//`; `(a / b).to_int()` does the job
    // and agrees across both engines.
    assert_eq!(say("print((10 / 3).to_int().type())"), "int");
    assert_eq!(say("print((10 / 3).to_int())"), "3");
    assert_eq!(say("print((-7 / 2).to_int())"), "-3");
}

#[test]
fn int_mixed_widens_to_number_diff() {
    assert_eq!(say("print((1 + 2.0).type())"), "number");
    assert_eq!(say("print(1 + 2.0)"), "3");
}

#[test]
fn int_number_equality_is_numeric_diff() {
    assert_eq!(say("print(1 == 1.0)"), "true");
    assert_eq!(say("print(2 > 1.5)"), "true");
}

#[test]
fn division_by_zero_errors_diff() {
    let msg = run_err("print(10 / 0)");
    assert!(msg.contains("Division by zero"), "got: {msg}");
}

#[test]
fn int_overflow_errors_diff() {
    let msg = run_err("print(9223372036854775807 + 1)");
    assert!(msg.contains("Integer overflow"), "got: {msg}");
}

#[test]
fn i64_min_literal_expression_and_pattern_diff() {
    assert_eq!(say("print(-9223372036854775808)"), "-9223372036854775808");
    assert_eq!(say("print((-9223372036854775808).type())"), "int");
    assert_eq!(
        say("print(-9223372036854775808 + 1)"),
        "-9223372036854775807"
    );
    assert_eq!(
        say("print(-9223372036854775808 < -9223372036854775807)"),
        "true"
    );
    assert_eq!(
        say(r#"print(match -9223372036854775808 {
    -9223372036854775808 => "minimum",
    _ => "other",
})"#,),
        "minimum"
    );
}

#[test]
fn i64_min_literal_overflow_and_range_errors_diff() {
    for source in [
        "print(--9223372036854775808)",
        "print(-9223372036854775808 - 1)",
    ] {
        assert_eq!(run_err(source), "Integer overflow in `-`", "{source}");
    }

    for source in [
        "print(9223372036854775808)",
        "print(9223372036854775809)",
        "print(-9223372036854775809)",
        "print(0 - 9223372036854775808)",
    ] {
        let message = run_err(source);
        assert!(
            message.contains("out of range for i64"),
            "{source}: {message}"
        );
    }
}

#[test]
fn len_returns_int_diff() {
    assert_eq!(say(r#"print("hi".len().type())"#), "int");
}

#[test]
fn range_int_elements_diff() {
    assert_eq!(say("print((range(3)[0]).type())"), "int");
}

#[test]
fn int_builtin_diff() {
    assert_eq!(say("print(3.7.to_int())"), "3");
    assert_eq!(say("print(3.7.to_int().type())"), "int");
}

#[test]
fn float_builtin_diff() {
    assert_eq!(say("print(42.to_float())"), "42");
    assert_eq!(say("print(42.to_float().type())"), "number");
}

#[test]
fn int_match_literal_diff() {
    assert_eq!(
        say(r#"let x = 2
print(match x {
    1 => "one",
    2 => "two",
    _ => "other",
})"#),
        "two"
    );
}

// ─── nybl-std stdlib (phase 7) ─────────────────────────────────────
//
// Every stdlib use is resolved via `nybl::stdlib::resolve` (see the
// `RecordHost::resolve_module` fallback), so walker and VM use
// identical source. These tests confirm the two engines agree on
// the stdlib's observable behaviour — the type-transfer import
// path is a new code path this phase introduces.

#[test]
fn result_method_helpers_diff() {
    // `is_ok` / `is_err` / `unwrap_or` are engine-level methods
    // on the built-in `Result` — no `use` required. Walker and
    // VM must produce the same printed output.
    set_modules(&[]);
    assert_eq!(
        say(r#"print(Result::Ok(1).is_ok())
print(Result::Err("boom").is_err())
print(Result::Err("x").unwrap_or(42))"#),
        "42"
    );
}

#[test]
fn result_map_and_and_then_methods_diff() {
    // Callable-taking Result methods need the engine's closure-
    // call plumbing to produce the same `Result::Ok(v)` /
    // `Result::Err(e)` shapes in both walker and VM.
    set_modules(&[]);
    assert_eq!(
        say(r#"fn halve(x) {
    if x % 2 == 0 { return Result::Ok((x / 2).to_int()) }
    return Result::Err("odd")
}
let r = Result::Ok(8).and_then(halve).and_then(halve)
print(match r { Result::Ok(v) => v, Result::Err(_) => -1 })"#),
        "2"
    );
}

#[test]
fn result_map_err_on_err_transforms_payload_diff() {
    // Covers the VM's `FrameWrap::ResultErr` path: the closure
    // body runs with the Err payload, and the frame wraps the
    // return value back up in `Result::Err(...)`.
    set_modules(&[]);
    assert_eq!(
        say(
            r#"let e = Result::Err("bad").map_err(fn(s) { return s + "!" })
print(match e { Result::Err(v) => v, Result::Ok(_) => "ok?" })"#
        ),
        "bad!"
    );
}

#[test]
fn std_math_constants_diff() {
    // Constants renamed to all-caps (`PI` / `E` / `TAU`) now
    // that they're `const` declarations. Walker and VM share
    // the same parsed source, so the output has to match.
    set_modules(&[]);
    let out = say(r#"use std.math
print(PI)
print(E)"#);
    // Both engines should yield the same string for `E`.
    assert!(out.starts_with("2.71828"), "got: {out}");
}

#[test]
fn std_math_clamp_sign_factorial_diff() {
    set_modules(&[]);
    assert_eq!(
        say(r#"use std.math
print(clamp(5, 0, 10))
print(clamp(-3, 0, 10))
print(sign(-7))
print(factorial(5))"#),
        "120"
    );
}

#[test]
fn std_math_gcd_lcm_diff() {
    set_modules(&[]);
    assert_eq!(
        say(r#"use std.math
print(gcd(12, 18))
print(lcm(4, 6))"#),
        "12"
    );
}

#[test]
fn std_iter_map_filter_reduce_diff() {
    set_modules(&[]);
    assert_eq!(
        say(r#"use std.iter
let nums = [1, 2, 3, 4, 5]
let doubled = map(nums, fn(x) { return x * 2 })
print(doubled)
let evens = filter(nums, fn(x) { return x % 2 == 0 })
print(evens)
print(reduce(nums, 0, fn(a, b) { return a + b }))"#),
        "15"
    );
}

#[test]
fn std_iter_any_all_find_diff() {
    set_modules(&[]);
    assert_eq!(
        say(r#"use std.iter
let is_pos = fn(x) { return x > 0 }
print(all([1, 2, 3], is_pos))
print(any([-1, -2, 3], is_pos))
print(find([1, 2, 3], fn(x) { return x > 1 }))"#),
        "2"
    );
}

#[test]
fn std_iter_take_drop_zip_diff() {
    set_modules(&[]);
    assert_eq!(
        say(r#"use std.iter
print(take([1, 2, 3, 4], 2))
print(drop([1, 2, 3, 4], 2))
print(zip([1, 2], ["a", "b"]))"#),
        "[[1, \"a\"], [2, \"b\"]]"
    );
}

#[test]
fn std_string_helpers_diff() {
    set_modules(&[]);
    assert_eq!(
        say(r#"use std.string
print(pad_left("42", 5, " "))
print(reverse("hello"))
print(is_palindrome("racecar"))"#),
        "true"
    );
}

#[test]
fn std_test_assertions_pass_diff() {
    set_modules(&[]);
    assert_eq!(
        say(r#"use std.test
assert(true, "ok")
assert_eq(1 + 1, 2)
print("done")"#),
        "done"
    );
}

#[test]
fn core_math_builtins_no_import_diff() {
    set_modules(&[]);
    assert_eq!(say("print(16.sqrt())"), "4");
    set_modules(&[]);
    assert_eq!(say("print(3.7.floor())"), "3");
    set_modules(&[]);
    assert_eq!(say("print(3.2.ceil())"), "4");
    set_modules(&[]);
    assert_eq!(say("print(2.pow(10))"), "1024");
}

#[test]
fn rounding_methods_respect_i64_boundaries_diff() {
    set_modules(&[]);
    let outcome = run_both(
        r#"print(9223372036854775808.0.floor().type(), 9223372036854775808.0.ceil().type(), 9223372036854775808.0.round().type())
print((-9223372036854775808.0).floor(), (-9223372036854775808.0).ceil(), (-9223372036854775808.0).round())
print(9223372036854774784.0.floor(), 9223372036854774784.0.ceil(), 9223372036854774784.0.round())"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "program errored: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        [
            "number number number",
            "-9223372036854775808 -9223372036854775808 -9223372036854775808",
            "9223372036854774784 9223372036854774784 9223372036854774784",
        ]
    );
}

#[test]
fn imported_fn_can_call_sibling_fn_diff() {
    // Regression for the phase-7 use path: an imported fn
    // whose body references another imported sibling needs to
    // resolve both — walker and VM should agree.
    set_modules(&[(
        "helpers",
        r#"fn double(x) { return x * 2 }
fn quadruple(x) { return double(double(x)) }"#,
    )]);
    assert_eq!(
        say(r#"use helpers
print(quadruple(3))"#),
        "12"
    );
}

#[test]
fn imported_struct_type_usable_in_caller_diff() {
    // Type transfer: a struct declared in a module can be
    // constructed and pattern-matched in the importer.
    set_modules(&[(
        "shapes",
        r#"struct Point { x, y }
fn make_point(x, y) { return Point { x: x, y: y } }"#,
    )]);
    assert_eq!(
        say(r#"use shapes
let p = make_point(3, 4)
print(p.x + p.y)
let q = Point { x: 1, y: 2 }
print(q.x)"#),
        "1"
    );
}

#[test]
fn imported_enum_type_usable_in_caller_diff() {
    set_modules(&[("shapes", r#"enum Shape { Circle(r), Rect { w, h } }"#)]);
    assert_eq!(
        say(r#"use shapes
let s = Shape::Rect { w: 4, h: 3 }
print(match s {
    Shape::Circle(r) => r,
    Shape::Rect { w, h } => w * h,
})"#),
        "12"
    );
}

// ─── "did you mean?" suggestions (phase 9 polish) ─────────────────
//
// The walker has dedicated integration tests for these; the
// point here is that the VM produces the *same hint field* so
// embedders that upgrade from walker to VM don't see worse
// diagnostics. We only check the VM directly — comparing hints
// across engines via `run_both` would conflate with the existing
// prints / error.message agreement contract.

fn vm_hint(code: &str) -> Option<String> {
    let mut host = RecordHost::new();
    nybl_vm::run(code, &mut host, &standard())
        .err()
        .and_then(|e| e.friendly_hint)
}

#[test]
fn vm_variable_typo_suggests_closest_local_diff() {
    let hint = vm_hint(
        r#"let length = 5
print(lenght)"#,
    );
    assert_eq!(hint.as_deref(), Some("Did you mean `length`?"));
}

#[test]
fn vm_function_typo_suggests_user_fn_diff() {
    let hint = vm_hint(
        r#"fn greet(name) { print("hi " + name) }
gret("world")"#,
    );
    assert_eq!(hint.as_deref(), Some("Did you mean `greet`?"));
}

#[test]
fn vm_function_typo_suggests_core_builtin_diff() {
    let hint = vm_hint("rang(5)");
    assert_eq!(hint.as_deref(), Some("Did you mean `range`?"));
}

#[test]
fn vm_struct_field_at_construction_suggests_declared_diff() {
    let hint = vm_hint(
        r#"struct Point { x, y }
let p = Point { x: 1, ya: 2 }"#,
    );
    assert_eq!(hint.as_deref(), Some("Did you mean `y`?"));
}

#[test]
fn vm_struct_field_at_access_suggests_declared_diff() {
    let hint = vm_hint(
        r#"struct Point { x, y }
let p = Point { x: 1, y: 2 }
print(p.z)"#,
    );
    // `x` and `y` both fit; declaration order wins the tie.
    assert_eq!(hint.as_deref(), Some("Did you mean `x`?"));
}

#[test]
fn vm_enum_variant_typo_suggests_declared_diff() {
    let hint = vm_hint(
        r#"enum Shape { Circle(r), Rectangle { w, h } }
let s = Shape::Circel(5)"#,
    );
    assert_eq!(hint.as_deref(), Some("Did you mean `Circle`?"));
}

// ─── `try_call` builtin ───────────────────────────────────────────

#[test]
fn try_call_wraps_ok_diff() {
    assert_eq!(
        say(r#"let r = try_call(fn() { return 42 })
print(match r {
    Result::Ok(v) => v,
    Result::Err(_) => -1,
})"#),
        "42"
    );
}

#[test]
fn try_call_wraps_non_fatal_err_diff() {
    assert_eq!(
        say(r#"let r = try_call(fn() { return 1 / 0 })
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e.message,
})"#),
        "Division by zero"
    );
}

#[test]
fn try_call_err_carries_line_diff() {
    assert_eq!(
        say(r#"let r = try_call(fn() {
    let x = 1
    return x / 0
})
print(match r {
    Result::Ok(_) => -1,
    Result::Err(e) => e.line,
})"#),
        "3"
    );
}

#[test]
fn try_call_composes_with_try_operator_diff() {
    assert_eq!(
        say(r#"fn risky(x) {
    let arr = [1, 2]
    return arr[x]
}
let r = try_call(fn() { return risky(5) })
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e.message,
})"#),
        "Index 5 is out of bounds (array has 2 items)"
    );
}

#[test]
fn try_call_wrong_arg_count_errors_diff() {
    let msg = run_err("try_call()");
    assert!(msg.contains("try_call` expects 1"), "got: {msg}");
}

#[test]
fn try_call_non_function_errors_diff() {
    let msg = run_err("try_call(42)");
    assert!(msg.contains("try_call` expects a function"), "got: {msg}");
}

#[test]
fn try_call_nested_outer_catches_inner_err_as_ok_diff() {
    assert_eq!(
        say(r#"let r = try_call(fn() {
    let inner = try_call(fn() { return 1 / 0 })
    return inner
})
print(match r {
    Result::Ok(Result::Err(e)) => e.message,
    Result::Ok(Result::Ok(_)) => "inner ok?",
    Result::Err(_) => "outer caught",
})"#),
        "Division by zero"
    );
}

#[test]
fn try_call_step_limit_is_fatal_diff() {
    // Run through both engines with a tight step budget;
    // both must report the fatal step-limit error rather
    // than swallowing it via `try_call`.
    let tight = NyblLimits {
        max_steps: 200,
        max_memory: 1 << 20,
    };
    let code = r#"let r = try_call(fn() {
    while true { }
})
print("should never run")"#;

    let tw = {
        let mut host = RecordHost::new();
        let result = nybl::run(code, &mut host, &tight);
        (
            host.prints.borrow().clone(),
            result.err().map(|e| (e.message, e.is_fatal)),
        )
    };
    let vm = {
        let mut host = RecordHost::new();
        let result = nybl_vm::run(code, &mut host, &tight);
        (
            host.prints.borrow().clone(),
            result.err().map(|e| (e.message, e.is_fatal)),
        )
    };
    assert!(tw.0.is_empty(), "walker printed: {:?}", tw.0);
    assert!(vm.0.is_empty(), "vm printed: {:?}", vm.0);
    let (tw_msg, tw_fatal) = tw.1.expect("walker should error");
    let (vm_msg, vm_fatal) = vm.1.expect("vm should error");
    assert!(tw_fatal, "walker non-fatal: {tw_msg}");
    assert!(vm_fatal, "vm non-fatal: {vm_msg}");
    assert!(tw_msg.contains("too many steps"), "walker msg: {tw_msg}");
    assert!(vm_msg.contains("too many steps"), "vm msg: {vm_msg}");
}

// ─── Step-limit error lines ────────────────────────────────────────
//
// The engines charge steps at different points (per-statement vs
// per-bytecode-instruction, with the VM's budget scaled by
// STEP_SCALE), so *where* the counter crosses the limit is an
// accident of phase. Both engines therefore attribute the error to
// the outermost active loop's header line. These tests pin that
// alignment, including padding variants that shift each engine's
// trip point relative to the loop body.

#[test]
fn step_limit_line_is_the_loop_header_on_both_engines() {
    // Padding shifts the step-count phase; the reported line must
    // not move with it.
    for pad in 0u32..4 {
        let mut code = String::new();
        for i in 0..pad {
            code.push_str(&format!("let pad{i} = {i}\n"));
        }
        code.push_str("let n = 0\nwhile true {\n    n = n + 1\n}");
        assert_both_step_limit_at_line(&code, &tight(), pad + 2);
    }
}

#[test]
fn step_limit_line_in_imported_function_matches_across_engines() {
    // The original divergence repro: import shifts the two engines'
    // step counts by different amounts, so before loop-header
    // attribution the walker and VM tripped on different lines of
    // the module.
    set_modules(&[(
        "spin",
        r#"fn spin_forever() {
    let n = 0
    while true {
        n = n + 1
    }
    return n
}"#,
    )]);
    assert_both_step_limit_at_line("use spin.{spin_forever}\nspin_forever()", &tight(), 3);
    set_modules(&[]);
}

#[test]
fn step_limit_line_in_nested_loops_is_the_outermost_header() {
    // Infinite outer loop with a finite inner loop: each engine may
    // trip inside the inner loop or between passes, so only the
    // outermost header is stable.
    assert_both_step_limit_at_line(
        "while true {\n    for x in [1, 2, 3] {\n        let y = x\n    }\n}",
        &tight(),
        1,
    );
    // Infinite inner loop: the outer `for` never finishes its first
    // pass, so it is the outermost active loop on both engines.
    assert_both_step_limit_at_line(
        "let items = [1, 2, 3]\nfor x in items {\n    while true {\n        let y = x\n    }\n}",
        &tight(),
        2,
    );
}

#[test]
fn step_limit_line_for_repeat_is_the_loop_header() {
    assert_both_step_limit_at_line("repeat 999999999 {\n    let y = 1\n}", &tight(), 1);
}

#[test]
fn step_limit_line_inside_loop_free_callee_is_the_calling_loop() {
    // The budget can trip while executing a callee that contains no
    // loop; both engines walk back to the calling loop's header.
    assert_both_step_limit_at_line(
        "fn work(a) {\n    let b = a + 1\n    return b\n}\nlet n = 0\nwhile true {\n    n = work(n)\n}",
        &tight(),
        6,
    );
}

#[test]
fn try_call_value_depth_limit_is_fatal_diff() {
    assert_both_value_depth_errors(
        r#"let r = try_call(fn() {
    let a = [1]
    repeat 128 { a = [a] }
})
print("should never run")"#,
        3,
    );
}

#[test]
fn try_call_value_depth_wrapper_uses_call_site_line_diff() {
    // Returning a maximum-depth value succeeds; wrapping it in Result::Ok
    // adds the level that fails. The diagnostic belongs to the initiating
    // try_call expression, not the callee's return instruction.
    assert_both_value_depth_errors(
        r#"fn deep() {
    let value = none
    repeat 64 { value = [value] }
    return value
}
let r = try_call(deep)"#,
        6,
    );
}

// ─── Modules / use ─────────────────────────────────────────────

#[test]
fn import_basic_let_binding() {
    set_modules(&[("greet", r#"let hello = "hi""#)]);
    assert_eq!(
        say(r#"use greet
print(hello)"#),
        "hi"
    );
}

#[test]
fn import_named_fn_callable() {
    set_modules(&[("math", "fn square(n) { return n * n }")]);
    assert_eq!(
        say(r#"use math
print(square(7))"#),
        "49"
    );
}

#[test]
fn import_named_fn_as_value() {
    // Proves the module's named fn survives use as a
    // first-class `Value::Fn`. Matters because the VM needs to
    // carry VM-compiled chunks in `Value::Fn`, and an imported
    // fn is loaded via a sub-VM.
    set_modules(&[("ops", "fn double(n) { return n * 2 }")]);
    assert_eq!(
        say(r#"use ops
let f = double
print(f(21))"#),
        "42"
    );
}

#[test]
fn import_dotted_path() {
    set_modules(&[("std.math", "let pi = 3")]);
    assert_eq!(
        say(r#"use std.math
print(pi)"#),
        "3"
    );
}

#[test]
fn import_missing_module_errors() {
    set_modules(&[]);
    let msg = run_err("use nope");
    assert!(msg.contains("Module `nope` not found"), "got: {msg}");
}

#[test]
fn import_transitive_modules() {
    set_modules(&[
        ("a", "use b\nlet doubled_pi = pi + pi"),
        ("b", "let pi = 3"),
    ]);
    assert_eq!(
        say(r#"use a
print(doubled_pi)"#),
        "6"
    );
}

#[test]
fn import_circular_detected() {
    set_modules(&[("a", "use b\nlet x = 1"), ("b", "use a\nlet y = 2")]);
    let msg = run_err("use a");
    assert!(msg.contains("Circular import"), "got: {msg}");
}

#[test]
fn import_is_idempotent_at_injection_site() {
    set_modules(&[("m", "let x = 1")]);
    assert_eq!(
        say(r#"use m
use m
print(x)"#),
        "1"
    );
}

#[test]
fn function_local_use_frontiers_preserve_shadowing_and_duplicate_parameters_diff() {
    set_modules(&[
        (
            "parameters",
            "let parameter = 10\nlet first = 20\nlet from_parameters = 30",
        ),
        ("future", "let future = 40"),
        ("nested", "let inner = 45\nlet nested_value = 50"),
    ]);
    let outcome = run_both(
        r#"fn inspect(parameter, parameter) {
    let first = 1
    let first = 2
    use parameters
    print(parameter, first, from_parameters)
    use future
    print(future)
    let future = 3
    print(future)
    if true {
        let inner = 4
        let inner = 5
        use nested
        print(inner, nested_value)
    }
}
inspect(6, 7)"#,
        &standard(),
    );

    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["7 2 30", "40", "3", "5 50"]);
}

#[test]
fn block_local_module_alias_does_not_leak_diff() {
    set_modules(&[("types", "struct Point { value }")]);
    let error = run_err(
        r#"if true {
    use types as t
}
print(t.Point { value: 42 })"#,
    );
    assert!(
        error.contains("isn't a module alias in scope"),
        "got: {error}"
    );
}

#[test]
fn function_and_lambda_local_module_aliases_do_not_leak_diff() {
    set_modules(&[("types", "struct Point { value }")]);
    for source in [
        r#"fn seed() {
    use types as t
}
seed()
print(t.Point { value: 42 })"#,
        r#"let seed = fn() {
    use types as t
}
seed()
print(t.Point { value: 42 })"#,
    ] {
        let error = run_err(source);
        assert!(
            error.contains("isn't a module alias in scope"),
            "got: {error}"
        );
    }
}

#[test]
fn assigned_block_local_module_alias_does_not_leak_diff() {
    set_modules(&[
        ("first", "struct Point { first }"),
        ("second", "struct Point { second }"),
    ]);
    for body in ["", "        dep = other\n"] {
        let source = format!(
            r#"use second as other
fn make() {{
    if true {{
        use first as dep
{body}    }}
    return dep.Point {{ second: 7 }}
}}
make()"#
        );
        assert_eq!(run_err(&source), "`dep` isn't a module alias in scope");
    }
}

#[test]
fn block_local_type_and_callable_import_metadata_do_not_leak_diff() {
    set_modules(&[("funcmod", "fn answer() { return 42 }")]);
    let type_error = run_err(
        r#"fn make() {
    if true { struct Point { value } }
    return Point { value: 7 }
}
make()"#,
    );
    assert_eq!(type_error, "Struct `Point` is not declared");

    let function_error = run_err(
        r#"fn make() {
    if true { use funcmod.{answer} }
    return answer()
}
make()"#,
    );
    assert_eq!(function_error, "Function `answer` not found");
}

#[test]
fn block_local_import_scope_unwinds_on_return_and_error_diff() {
    set_modules(&[("first", "struct Point { value }")]);
    for source in [
        r#"fn seed() {
    if true { use first as dep; return 1 }
}
seed()
dep.Point { value: 7 }"#,
        r#"fn seed() {
    if true { use first as dep; panic("stop") }
}
try_call(seed)
dep.Point { value: 7 }"#,
    ] {
        assert_eq!(run_err(source), "`dep` isn't a module alias in scope");
    }
}

#[test]
fn named_function_does_not_dynamically_inherit_caller_alias_diff() {
    set_modules(&[("types", "struct Point { value }")]);
    let error = run_err(
        r#"fn build() { return t.Point { value: 42 } }
if true {
    use types as t
    print(build())
}"#,
    );
    assert!(
        error.contains("isn't a module alias in scope"),
        "got: {error}"
    );
}

#[test]
fn namespaced_construction_validates_alias_before_payload_diff() {
    set_modules(&[(
        "types",
        "struct Stack { items }\nenum Maybe { Some(value), Named { value } }",
    )]);
    for source in [
        r#"fn invalid() { return dep.Stack { items: panic("payload") } }
invalid()
use types as dep"#,
        r#"fn invalid() { return dep.Maybe::Some(panic("payload")) }
invalid()
use types as dep"#,
        r#"fn invalid() { return dep.Maybe::Named { value: panic("payload") } }
invalid()
use types as dep"#,
    ] {
        assert_eq!(
            run_err(source),
            "`dep` isn't a module alias in scope",
            "{source}"
        );
    }
}

#[test]
fn constructor_shape_validation_precedes_every_payload_diff() {
    set_modules(&[(
        "types",
        "struct Stack { items }\nenum Maybe { Some(value), Named { value } }",
    )]);
    let cases = [
        (
            r#"use types as dep
dep.Stack { wrong: panic("payload") }"#,
            "Struct `Stack` has no field `wrong`",
        ),
        (
            r#"use types as dep
dep.Stack { items: panic("payload"), wrong: 2 }"#,
            "Struct `Stack` has no field `wrong`",
        ),
        (
            r#"use types as dep
dep.Maybe::Some(panic("payload"), 2)"#,
            "`Maybe::Some` expects 1 argument, but got 2",
        ),
        (
            r#"use types as dep
dep.Maybe::Missing(panic("payload"))"#,
            "Enum `Maybe` has no variant `Missing`",
        ),
        (
            r#"use types as dep
dep.Maybe::Named { value: panic("payload"), wrong: 2 }"#,
            "Variant `Maybe::Named` has no field `wrong`",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(run_err(source), expected, "{source}");
    }
}

#[test]
fn callee_preflight_precedes_call_argument_evaluation_diff() {
    set_modules(&[("types", "fn exported(value) { return value }")]);
    for (source, expected) in [
        (
            r#"fn invalid() { return dep.nope(panic("payload")) }
invalid()
use types as dep"#,
            "Variable `dep` not found",
        ),
        (
            r#"fn invalid() { return "receiver".nope(panic("payload")) }
invalid()"#,
            "payload",
        ),
        (
            r#"fn invalid() { return dep["exported"](panic("payload")) }
invalid()
use types as dep"#,
            "Variable `dep` not found",
        ),
    ] {
        assert_eq!(run_err(source), expected, "{source}");
    }
}

#[test]
fn inner_module_alias_shadow_restores_outer_alias_diff() {
    set_modules(&[
        ("first", "struct Point { first }"),
        ("second", "struct Point { second }"),
    ]);
    let outcome = run_both(
        r#"use first as t
if true {
    use second as t
    print(t.Point { second: 2 }.second)
}
print(t.Point { first: 1 }.first)"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["2", "1"]);
}

#[test]
fn module_alias_conflicting_with_named_function_is_rejected_diff() {
    set_modules(&[("dep", "let value = 42")]);
    let error = run_err(
        r#"fn dep() { return 1 }
use dep as dep"#,
    );
    assert!(error.contains("already bound"), "got: {error}");
}

#[test]
fn selective_and_glob_function_imports_preserve_existing_named_fn_diff() {
    set_modules(&[("dep", "fn pick() { return 42 }")]);
    for import in ["use dep.{pick}", "use dep"] {
        let source = format!("fn pick() {{ return 1 }}\n{import}\nprint(pick())");
        assert_eq!(say(&source), "1");
    }
}

#[test]
fn plain_glob_idempotency_is_lexical_across_siblings_and_root_diff() {
    set_modules(&[("dep", "let value = 42")]);
    let outcome = run_both(
        r#"if true {
    use dep
    print(value)
}
if true {
    use dep
    print(value)
}
use dep
print(value)"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["42", "42", "42"]);
}

#[test]
fn local_imported_callable_shadows_then_restores_and_does_not_leak_diff() {
    set_modules(&[
        ("outer", "fn helper() { return 1 }"),
        ("inner", "fn helper() { return 2 }"),
    ]);
    let outcome = run_both(
        r#"use outer
fn local() {
    use inner.{helper}
    return helper()
}
print(local())
print(helper())"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["2", "1"]);

    set_modules(&[("inner", "fn helper() { return 2 }")]);
    let error = run_err(
        r#"fn seed() {
    use inner.{helper}
    return helper()
}
seed()
print(helper())"#,
    );
    assert!(
        error.contains("Function `helper` not found"),
        "got: {error}"
    );

    set_modules(&[("inner", "fn helper() { return 2 }")]);
    let outcome = run_both(
        r#"fn helper() { return 1 }
fn local() {
    use inner.{helper}
    return helper()
}
print(local())
print(helper())"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["2", "1"]);
}

#[test]
fn aliased_module_functions_keep_private_sibling_scope_without_bare_leaks() {
    set_modules(&[(
        "internal",
        r#"fn helper(n) { return n + 1 }
fn twice(n) { return helper(n) + helper(n) }
fn recurse(n) {
    if n == 0 { return 0 }
    return 1 + recurse(n - 1)
}
fn factory() {
    fn nested(n) { return helper(n) }
    return nested
}
let closure = fn(n) { return helper(n) }
struct Thing { value }
fn Thing.bump(self) { return helper(self.value) }
let public = 10
let _private = 99"#,
    )]);

    let positive = run_both(
        r#"use internal as module
print(module.twice(3))
print(module.recurse(4))
let returned = module.twice
print(returned(5))
let nested = module.factory()
print(nested(7))
print(module.closure(6))
print(module.Thing { value: 7 }.value)
print(module.Thing { value: 8 }.bump())
print(module._private)"#,
        &standard(),
    );
    assert!(positive.is_ok(), "unexpected error: {:?}", positive.error);
    assert_eq!(positive.prints, ["8", "4", "12", "8", "7", "7", "9", "99"]);

    let function_error = run_err(
        r#"use internal as module
print(helper(1))"#,
    );
    assert!(function_error.contains("Function `helper` not found"));

    let value_error = run_err(
        r#"use internal as module
print(public)"#,
    );
    assert!(value_error.contains("Variable `public` not found"));

    let type_error = run_err(
        r#"use internal as module
print(Thing { value: 1 })"#,
    );
    assert!(type_error.contains("Struct `Thing` is not declared"));
}

#[test]
fn root_named_functions_keep_module_alias_context_diff() {
    set_modules(&[(
        "types",
        r#"struct Point { value }
fn make(value) { return Point { value: value } }"#,
    )]);
    let outcome = run_both(
        r#"use types as t
fn build(value) {
    let direct = t.Point { value: value }
    let called = t.make(direct.value + 1)
    return match called {
        t.Point { value: found } => found,
        _ => 0,
    }
}
print(build(41))"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["42"]);
}

#[test]
fn declaration_alias_is_shadowed_by_function_parameter_diff() {
    set_modules(&[("types", "struct Point { value }")]);
    let error = run_err(
        r#"use types as t
fn build(t) { return t.Point { value: 42 } }
print(build(1))"#,
    );
    assert!(
        error.contains("`t` is a int, not a module alias"),
        "got: {error}"
    );
}

#[test]
fn imported_functions_and_methods_keep_defining_module_context_diff() {
    set_modules(&[
        (
            "first_types",
            r#"struct Point { value }
fn make(value) { return Point { value: value } }"#,
        ),
        (
            "second_types",
            r#"struct Point { value }
fn make(value) { return Point { value: value + 100 } }"#,
        ),
        (
            "first_holder",
            r#"use first_types as dep
struct Holder { value }
fn build(value) {
    let point = dep.make(value)
    return match point { dep.Point { value: found } => found, _ => 0 }
}
fn Holder.build(self) { return dep.make(self.value).value }"#,
        ),
        (
            "second_holder",
            r#"use second_types as dep
struct Holder { value }
fn build(value) {
    let point = dep.Point { value: value }
    return match point { dep.Point { value: found } => found, _ => 0 }
}
fn Holder.build(self) { return dep.make(self.value).value }"#,
        ),
    ]);
    let outcome = run_both(
        r#"use first_holder as first
use second_holder as second
use second_types as dep
print(first.build(1), first.Holder { value: 2 }.build())
print(second.build(3), second.Holder { value: 4 }.build())"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["1 2", "3 104"]);
}

#[test]
fn declaration_alias_interpolation_and_call_paths_keep_context_diff() {
    set_modules(&[
        (
            "types",
            r#"struct Point { value }
fn push(value) { return value + 1 }"#,
        ),
        (
            "holder",
            r#"use types as dep
struct Holder { value }
fn show() { return "{dep}" == dep.to_str() }
fn Holder.show(self) { return "{dep}" == dep.to_str() }
fn shadow(dep) { return "{dep}" }
fn call_push() { return dep.push(1) }"#,
        ),
    ]);
    let outcome = run_both(
        r#"use holder as holder
print(holder.show(), holder.Holder { value: 0 }.show())
print(holder.shadow("local"), holder.call_push())"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["true true", "local 2"]);
}

#[test]
fn declaration_alias_bare_call_is_a_non_callable_value_diff() {
    set_modules(&[("types", "struct Point { value }")]);
    let error = run_err(
        r#"use types as dep
fn invoke() { return dep() }
invoke()"#,
    );
    assert!(
        error.contains("`dep` is a module, not a function"),
        "got: {error}"
    );
}

#[test]
fn future_declaration_alias_does_not_shadow_earlier_call_diff() {
    set_modules(&[("types", "struct Point { value }")]);
    let outcome = run_both(
        r#"fn before() { print("before") }
before()
use types as print"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["before"]);
}

#[test]
fn declaration_alias_reads_are_lazy_across_branches_and_lambdas_diff() {
    set_modules(&[("types", "struct Stack { items }")]);
    let outcome = run_both(
        r#"fn dead_branch() {
    if false { print(dep) }
    return 1
}
fn maker() {
    return fn() { return dep.Stack { items: [] } }
}
let before = try_call(maker)
print(dead_branch(), before.is_err())
use types as dep
print(before.unwrap()().items.len())"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["1 false", "0"]);
}

#[test]
fn declaration_alias_overlay_propagates_through_nested_lambdas_diff() {
    set_modules(&[
        ("first", "struct Point { value }"),
        ("second", "struct Point { value }"),
    ]);
    let outcome = run_both(
        r#"fn dynamic_maker() {
    return fn() { return fn() { return dep.Point { value: 9 }.value } }
}
let dynamic = dynamic_maker()()
use first as dep
use second as other
fn assigned_maker() {
    dep = other
    return fn() {
        return fn(value) {
            return match value { dep.Point { value: found } => found, _ => 0 }
        }
    }
}
let assigned = assigned_maker()()
print(dynamic(), assigned(other.Point { value: 7 }))"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["9 7"]);
}

#[test]
fn reexported_type_origins_survive_two_facades_and_dynamic_namespaces_diff() {
    set_modules(&[
        (
            "leaf",
            r#"struct Point { value }
enum Signal { Idle, Count(value), Named { value } }
fn make_point(value) { return Point { value: value } }
fn make_named(value) { return Signal::Named { value: value } }"#,
        ),
        ("middle", "use leaf"),
        ("top", "use middle"),
        ("other", "struct Point { other }"),
    ]);
    let outcome = run_both(
        r#"use top
use top.{Point, Signal, make_point, make_named} as api
use other as other
let bare = Point { value: 1 }
let unit = api.Signal::Idle
let tuple = api.Signal::Count(2)
let named = api.make_named(3)
fn read(namespace, value) {
    return match value { namespace.Point { value: found } => found, _ => 0 }
}
print(bare.value, read(api, api.make_point(4)), read(other, bare))
print(match unit { api.Signal::Idle => "idle", _ => "bad" })
print(match tuple { api.Signal::Count(value) => value, _ => 0 })
print(match named { api.Signal::Named { value } => value, _ => 0 })"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["1 4 0", "idle", "2", "3"]);
}

#[test]
fn reexported_type_origins_preserve_diamonds_and_first_win_diff() {
    set_modules(&[
        ("leaf", "struct Shared { value }"),
        ("left", "use leaf"),
        ("right", "use leaf"),
        ("diamond", "use left\nuse right"),
        ("a", "struct Same { a }"),
        ("b", "struct Same { b }"),
        ("fa", "use a"),
        ("fb", "use b"),
        ("order_ab", "use fa\nuse fb"),
        ("order_ba", "use fb\nuse fa"),
    ]);
    let outcome = run_both(
        r#"use diamond as diamond
use order_ab as ab
use order_ba as ba
let shared = diamond.Shared { value: 1 }
let first = ab.Same { a: 2 }
let second = ba.Same { b: 3 }
print(shared.value, first.a, second.b)"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["1 2 3"]);
}

#[test]
fn reexported_type_privacy_and_local_overwrite_rules_diff() {
    set_modules(&[
        ("leaf", "struct Public { value }\nstruct _Hidden { value }"),
        ("glob", "use leaf"),
        ("selective", "use leaf.{_Hidden}"),
        ("local_before", "struct Public { local }\nuse leaf"),
        ("local_after", "use leaf\nstruct Public { local }"),
        ("nested", "if true { struct Nested { value } }"),
    ]);
    let outcome = run_both(
        r#"use leaf as direct
use selective as selected
use local_before as before
use local_after as after
print(direct._Hidden { value: 1 }.value)
print(selected._Hidden { value: 2 }.value)
print(before.Public { local: 3 }.local, after.Public { local: 4 }.local)"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["1", "2", "3 4"]);

    let error = run_err("use glob as facade\nfacade._Hidden { value: 1 }");
    assert!(error.contains("_Hidden"), "got: {error}");
    let nested_error = run_err("use nested.{Nested}");
    assert!(nested_error.contains("Nested"), "got: {nested_error}");
}

#[test]
fn reexported_types_keep_callable_context_and_method_identity_diff() {
    set_modules(&[
        ("helper", "fn increment(value) { return value + 1 }"),
        (
            "leaf",
            r#"use helper as dep
struct Box { value }
fn Box.bump(self) { return dep.increment(self.value) }"#,
        ),
        (
            "facade",
            r#"use leaf
fn make(value) { return Box { value: value } }
fn matcher() {
    return fn(value) {
        return match value { Box { value: found } => found, _ => 0 }
    }
}"#,
        ),
        (
            "callers",
            r#"use facade as dep
fn make_alias(value) { return dep.Box { value: value } }
fn alias_matcher() {
    return fn(value) {
        return match value { dep.Box { value: found } => found, _ => 0 }
    }
}"#,
        ),
    ]);
    let outcome = run_both(
        r#"use facade as api
use callers as calls
let value = api.make(4)
let aliased = calls.make_alias(6)
print(value.bump(), api.matcher()(value))
print(aliased.bump(), calls.alias_matcher()(aliased))"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["5 4", "7 6"]);
}

#[test]
fn reexported_module_aliases_keep_surface_and_callable_context_diff() {
    set_modules(&[
        (
            "dep",
            r#"struct Point { value }
enum Signal { Idle, Count(value), Named { value } }
fn make(value) { return Point { value: value } }
fn hidden() { return 99 }"#,
        ),
        ("wrapper", "use dep.{Point, Signal, make} as api"),
        ("middle", "use wrapper"),
        (
            "top",
            r#"use middle.{api}
struct Runner { offset }
fn Runner.run(self, value) { return api.make(value + self.offset) }
fn build(value) {
    let point = api.Point { value: value }
    return match point { api.Point { value: found } => api.make(found + 1), _ => none }
}
fn matcher() {
    return fn(value) {
        return match value { api.Point { value: found } => found, _ => 0 }
    }
}"#,
        ),
    ]);
    let outcome = run_both(
        r#"use top
fn root_build(value) { return api.Point { value: value } }
let point = build(4)
let runner = Runner { offset: 2 }
let unit = api.Signal::Idle
let tuple = api.Signal::Count(3)
let named = api.Signal::Named { value: 5 }
print(point.value, matcher()(point), runner.run(6).value, root_build(7).value)
print(match unit { api.Signal::Idle => "idle", _ => "bad" })
print(match tuple { api.Signal::Count(value) => value, _ => 0 })
print(match named { api.Signal::Named { value } => value, _ => 0 })
print(try_call(fn() { return api.hidden() }).is_err())"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["5 5 8 7", "idle", "3", "5", "true"]);
}

#[test]
fn reexported_module_aliases_obey_order_privacy_and_isolation_diff() {
    set_modules(&[
        (
            "a",
            "struct Point { a }\nfn make(value) { return Point { a: value } }",
        ),
        (
            "b",
            "struct Point { b }\nfn make(value) { return Point { b: value } }",
        ),
        (
            "wa",
            "use a as api\nfn make_a(value) { return api.make(value) }",
        ),
        (
            "wb",
            "use b as api\nfn make_b(value) { return api.make(value) }",
        ),
        ("private", "use a as _api"),
        ("private_glob", "use private"),
        ("private_selected", "use private.{_api}"),
        ("values", "let api = 11"),
        ("module_first", "use wa\nuse values"),
        ("value_first", "use values\nuse wa"),
        ("local_after", "use wa\nlet api = 12"),
        ("local_before", "let api = 13\nuse wa"),
        ("nested", "fn load() { use a as api; return api.make(1) }"),
    ]);

    let isolated = run_both(
        "use wa.{make_a}\nuse wb.{make_b}\nprint(make_a(2).a, make_b(3).b)",
        &standard(),
    );
    assert!(isolated.is_ok(), "unexpected error: {:?}", isolated.error);
    assert_eq!(isolated.prints, ["2 3"]);

    let selected = run_both(
        "use private_selected.{_api}\nprint(_api.Point { a: 4 }.a)",
        &standard(),
    );
    assert!(selected.is_ok(), "unexpected error: {:?}", selected.error);
    assert_eq!(selected.prints, ["4"]);

    for (module, expected) in [
        ("module_first", "5"),
        ("value_first", "11"),
        ("local_after", "12"),
        ("local_before", "13"),
    ] {
        let source = if module == "module_first" {
            format!("use {module}\nprint(api.Point {{ a: 5 }}.a)")
        } else {
            format!("use {module}\nprint(api)")
        };
        let outcome = run_both(&source, &standard());
        assert!(outcome.is_ok(), "{module}: {:?}", outcome.error);
        assert_eq!(outcome.prints, [expected], "{module}");
    }

    for source in ["use private_glob.{_api}", "use nested.{api}"] {
        let error = run_err(source);
        assert!(error.contains("isn't exported"), "got: {error}");
    }
}

#[test]
fn reexported_module_aliases_follow_call_and_function_winners_diff() {
    set_modules(&[
        ("dep", "struct Point { value }"),
        ("wrapper", "use dep as api"),
        (
            "timing",
            r#"fn build() { return api.Point { value: 9 } }
let before = try_call(build)
use wrapper
let after = try_call(build)"#,
        ),
        (
            "alias_before_fn",
            r#"use dep as api
fn api() { return 99 }
fn build() { return api.Point { value: 7 } }"#,
        ),
        (
            "fn_before_alias",
            r#"fn api() { return 99 }
use dep as api"#,
        ),
    ]);

    let timing = run_both(
        "use timing\nprint(before.is_err(), after.is_ok(), after.unwrap().value)",
        &standard(),
    );
    assert!(timing.is_ok(), "unexpected error: {:?}", timing.error);
    assert_eq!(timing.prints, ["true true 9"]);

    let winner = run_both(
        "use alias_before_fn\nprint(build().value, api.Point { value: 8 }.value)",
        &standard(),
    );
    assert!(winner.is_ok(), "unexpected error: {:?}", winner.error);
    assert_eq!(winner.prints, ["7 8"]);

    let error = run_err("use fn_before_alias");
    assert!(error.contains("already bound"), "got: {error}");
}

#[test]
fn reexported_module_aliases_track_copy_assignment_and_flat_fn_order_diff() {
    set_modules(&[
        ("a", "struct Point { value }"),
        ("b", "struct Other { value }"),
        ("wrapper", "use a as api"),
        (
            "copies",
            r#"use a as api
use b as other
let copy = api
api = other
fn from_copy(value) { return copy.Point { value: value } }
fn from_reassigned(value) { return api.Other { value: value } }"#,
        ),
        (
            "flat_fn_first",
            r#"fn api() { return 21 }
use wrapper
fn result() { return api() }"#,
        ),
        (
            "flat_module_first",
            r#"use wrapper
fn api() { return 22 }
fn result() { return api.Point { value: 23 } }"#,
        ),
        (
            "module_fn_then_value",
            "use a as api\nfn api() { return 24 }\nlet api = 25",
        ),
    ]);

    let outcome = run_both(
        r#"use copies.{from_copy, from_reassigned}
use flat_fn_first.{result} as first
use flat_module_first.{result} as second
print(from_copy(1).value, from_reassigned(2).value)
print(first.result(), second.result().value)
use module_fn_then_value.{api} as final_value
print(final_value.api)"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["1 2", "21 23", "25"]);
}

#[test]
fn type_bindings_follow_top_level_source_order_diff() {
    set_modules(&[(
        "types",
        r#"struct Imported { value }
enum ImportedSignal { Idle, Pair(left, right), Named { value } }"#,
    )]);
    let outcome = run_both(
        r#"fn direct() { return Point { value: 1 } }
fn direct_enum() { return Signal::Idle }
fn imported() { return Imported { value: 2 } }
fn imported_enum() { return ImportedSignal::Pair(3, 4) }
fn direct_pattern() {
    return match (Point { value: 5 }) { Point { value } => value, _ => 0 }
}
struct Runner { marker }
fn Runner.build(self) { return Point { value: self.marker } }
let runner = Runner { marker: 7 }
let delayed = fn() { return Point { value: 6 } }
print(try_call(direct).is_err(), try_call(direct_enum).is_err())
print(try_call(delayed).is_err(), try_call(fn() { return runner.build() }).is_err())
print(try_call(imported).is_err(), try_call(imported_enum).is_err())
struct Point { value }
enum Signal { Idle, Pair(left, right), Named { value } }
print(direct().value, direct_pattern(), delayed().value, runner.build().value)
print(match direct_enum() { Signal::Idle => "idle", _ => "bad" })
print(match Signal::Pair(7, 8) { Signal::Pair(left, right) => left + right, _ => 0 })
print(match (Signal::Named { value: 9 }) { Signal::Named { value } => value, _ => 0 })
use types.{Imported}
use types
print(imported().value)
print(match imported_enum() { ImportedSignal::Pair(left, right) => left + right, _ => 0 })"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        [
            "true true",
            "true true",
            "true true",
            "1 5 6 7",
            "idle",
            "15",
            "9",
            "2",
            "7",
        ]
    );
}

#[test]
fn imported_module_type_timing_and_retry_cleanup_diff() {
    set_modules(&[
        ("dep", "print(\"dep\")\nstruct P { value }"),
        (
            "timing",
            r#"fn build() { return P { value: 10 } }
fn matches() { return match (P { value: 11 }) { P { value } => value, _ => 0 } }
print(try_call(build).is_err(), try_call(matches).is_err())
use dep.{P}
print(build().value, matches())"#,
        ),
        (
            "bad",
            r#"fn bare() { return P { value: 12 } }
fn qualified() { return api.P { value: 13 } }
print(try_call(bare).is_err(), try_call(qualified).is_err())
use dep.{P}
use dep as api
print(try_call(bare).is_ok(), try_call(qualified).is_ok())
let boom = 1 / 0"#,
        ),
    ]);
    let outcome = run_both(
        r#"use timing
fn load_bad() { use bad }
print(try_call(load_bad).is_err())
print(try_call(load_bad).is_err())"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        [
            "true true",
            "dep",
            "10 11",
            "true true",
            "true true",
            "true",
            "true true",
            "true true",
            "true",
        ]
    );
}

#[test]
fn declaration_alias_mutable_places_require_an_overlay_diff() {
    set_modules(&[("types", "struct Point { value }")]);
    let outcome = run_both(
        r#"use types as dep
struct Box { value }
fn invalid_index() { dep["value"] = 1 }
fn invalid_field() { dep.value = 1 }
fn valid_index() {
    dep = {"value": 0}
    dep["value"] = 2
    return dep["value"]
}
fn valid_field() {
    dep = Box { value: 0 }
    dep.value = 3
    return dep.value
}
print(try_call(invalid_index).is_err(), try_call(invalid_field).is_err())
print(valid_index(), valid_field())"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["true true", "2 3"]);
}

#[test]
fn declaration_alias_mutable_place_errors_are_canonical_diff() {
    set_modules(&[("types", "struct Point { value }")]);
    for (source, expected) in [
        (
            r#"use types as dep
fn invalid() { dep["value"] = 1 }
invalid()"#,
            "Can't set index with these types",
        ),
        (
            r#"use types as dep
fn invalid() { dep.value = 1 }
invalid()"#,
            "Can't assign to field `value` on module",
        ),
    ] {
        assert_eq!(run_err(source), expected);
    }
}

#[test]
fn future_declaration_alias_assignment_errors_are_operation_specific_diff() {
    set_modules(&[("types", "struct Point { value }")]);
    let simple = run_err(
        r#"fn invalid() { dep = 1 }
invalid()
use types as dep"#,
    );
    assert_eq!(simple, "Variable `dep` doesn't exist yet");

    let compound = run_err(
        r#"fn invalid() { dep += 1 }
invalid()
use types as dep"#,
    );
    assert_eq!(compound, "Variable `dep` not found");
}

#[test]
fn compound_assignment_rhs_precedes_target_and_place_resolution_diff() {
    set_modules(&[("types", "struct Point { value }")]);
    for source in [
        r#"fn invalid() { dep += panic("payload") }
invalid()
use types as dep"#,
        r#"fn invalid() { dep[panic("index")] += panic("payload") }
invalid()
use types as dep"#,
        r#"fn invalid() { dep.value += panic("payload") }
invalid()
use types as dep"#,
    ] {
        assert_eq!(run_err(source), "payload", "{source}");
    }
}

#[test]
fn nested_named_function_uses_declaration_alias_not_outer_local_diff() {
    set_modules(&[("types", "struct Point { value }")]);
    let outcome = run_both(
        r#"use types as dep
fn outer() {
    let dep = 1
    fn inner() { return dep.Point { value: 4 }.value }
    return inner()
}
print(outer())"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["4"]);
}

#[test]
fn imported_function_keeps_bare_type_binding_from_defining_module_diff() {
    set_modules(&[
        ("types", "struct Point { value }"),
        (
            "holder",
            r#"use types.{Point}
fn build(value) {
    let point = Point { value: value }
    return match point { Point { value: found } => found, _ => 0 }
}"#,
        ),
    ]);
    assert_eq!(
        say(r#"use holder as module
print(module.build(42))"#),
        "42"
    );
}

#[test]
fn imported_functions_do_not_see_root_declaration_context_diff() {
    set_modules(&[
        ("types", "struct Point { value }"),
        (
            "alias_holder",
            "fn build() { return t.Point { value: 42 } }",
        ),
        ("type_holder", "fn build() { return Point { value: 42 } }"),
        ("fn_holder", "fn build() { return helper() }"),
    ]);

    let alias_error = run_err(
        r#"use types as t
use alias_holder as holder
print(holder.build())"#,
    );
    assert!(
        alias_error.contains("isn't a module alias"),
        "got: {alias_error}"
    );

    let type_error = run_err(
        r#"use types.{Point}
use type_holder as holder
print(holder.build())"#,
    );
    assert!(type_error.contains("Struct `Point` is not declared"));

    let function_error = run_err(
        r#"fn helper() { return 42 }
use fn_holder as holder
print(holder.build())"#,
    );
    assert!(function_error.contains("Function `helper` not found"));
}

#[test]
fn module_functions_resolve_siblings_while_the_module_is_loading_diff() {
    set_modules(&[(
        "loading",
        r#"fn helper(n) { return n + 1 }
fn recurse(n) { if n == 0 { return 0 } return recurse(n - 1) + 1 }
fn build() { return helper(40) + recurse(1) }
let value = build()"#,
    )]);
    let outcome = run_both("use loading\nprint(value)", &standard());
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["42"]);
}

#[test]
fn namespace_only_lambda_captures_and_shadows_match_diff() {
    set_modules(&[("types", "struct Point { value }")]);
    let positive = run_both(
        r#"use types as dep
fn outer(dep) {
    return fn() {
        let point = dep.Point { value: 42 }
        return match point { dep.Point { value: found } => found, _ => 0 }
    }
}
print(outer(dep)())"#,
        &standard(),
    );
    assert!(positive.is_ok(), "unexpected error: {:?}", positive.error);
    assert_eq!(positive.prints, ["42"]);

    let construct_error = run_err(
        r#"use types as dep
fn outer(dep) { return fn() { return dep.Point { value: 42 } } }
print(outer(1)())"#,
    );
    assert!(
        construct_error.contains("`dep` is a int, not a module alias"),
        "got: {construct_error}"
    );

    let pattern = run_both(
        r#"use types as dep
let point = dep.Point { value: 42 }
fn outer(dep) { return fn(value) { return match value { dep.Point { value: found } => found, _ => 0 } } }
print(outer(1)(point))"#,
        &standard(),
    );
    assert!(pattern.is_ok(), "unexpected error: {:?}", pattern.error);
    assert_eq!(pattern.prints, ["0"]);
}

#[test]
fn pattern_namespace_resolves_before_same_named_arm_binding_diff() {
    set_modules(&[
        ("types_a", "struct Point { dep }"),
        ("types_b", "struct Point { dep }"),
    ]);
    let outcome = run_both(
        r#"use types_a as dep
use types_b as other
fn make() {
    let dep = other
    return fn(value) {
        return match value { dep.Point { dep } => dep, _ => -1 }
    }
}
let f = make()
print(f(other.Point { dep: 42 }))"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["42"]);
}

#[test]
fn aliased_nested_import_does_not_reexport_dependency_bare_names() {
    set_modules(&[
        ("shared", "fn helper(n) { return n + 1 }\nlet public = 10"),
        (
            "wrapper",
            "use shared as dep\nlet via_alias = dep.helper(3)\nlet via_bare = helper(4)",
        ),
    ]);
    let error = run_err(
        r#"use wrapper
print(via_alias)"#,
    );
    assert!(error.contains("Function `helper` not found"));
}

#[test]
fn selective_imports_remain_exact_at_root_and_in_nested_modules() {
    set_modules(&[
        ("shared", "fn helper(n) { return n + 1 }\nlet public = 10"),
        (
            "wrapper",
            "use shared.{public}\nlet via_public = public\nlet via_bare = helper(4)",
        ),
    ]);

    let root_error = run_err(
        r#"use shared.{public}
print(helper(1))"#,
    );
    assert!(root_error.contains("Function `helper` not found"));

    let nested_error = run_err(
        r#"use wrapper
print(via_public)"#,
    );
    assert!(nested_error.contains("Function `helper` not found"));
}

// ─── Module-qualified type identity (phase 2b) ──────────────────

#[test]
fn two_modules_same_name_different_shapes_diff() {
    // Core promise of phase 2b: two independently-declared
    // `Color` types coexist with distinct runtime identity,
    // even under the same bare name. Walker and VM must
    // agree.
    set_modules(&[
        ("paint", "enum Color { Red, Blue }"),
        ("other", "enum Color { Red, Green, Yellow }"),
    ]);
    assert_eq!(
        say(r#"use paint as p
use other as o
let a = p.Color::Red
let b = o.Color::Red
print(a == b)
print(a == a)"#),
        "true"
    );
}

#[test]
fn namespaced_pattern_discriminates_between_modules_diff() {
    // Patterns resolve through the alias: `p.Color::Red`
    // matches only values tagged with the paint module.
    set_modules(&[
        ("paint", "enum Color { Red, Blue }"),
        ("other", "enum Color { Red, Green }"),
    ]);
    assert_eq!(
        say(r#"use paint as p
use other as o
fn label(c) {
    return match c {
        p.Color::Red => "paint-red",
        o.Color::Red => "other-red",
        _ => "none",
    }
}
print(label(p.Color::Red))
print(label(o.Color::Red))
print(label(p.Color::Blue))"#),
        "none"
    );
}

// ─── Closures / first-class functions ─────────────────────────────
//
// These run through both the tree-walker and the bytecode VM to
// prove the VM's `MakeLambda` / `CallValue` machinery matches the
// walker's snapshot-and-invoke model.

#[test]
fn shared_named_function_definitions_preserve_semantics_diff() {
    set_modules(&[(
        "funcs",
        "fn twice(n) { return n * 2 }\nfn call(n) { return twice(n) + 1 }",
    )]);
    let outcome = run_both(
        r#"fn recurse(n) {
    if n == 0 { return 0 }
    return recurse(n - 1) + 1
}
let original = recurse
fn recurse(n) { return n + 10 }
fn bump(ref value) { value += 2 }
let bump_alias = bump
let value = 1
bump(ref value)
bump_alias(ref value)
struct Box { value }
fn Box.add(self, amount) { return self.value + amount }
use funcs as funcs
let callback = funcs.call
print(original(4), recurse(4), value)
print(Box { value: 2 }.add(3))
print(funcs.call(5), callback(6))"#,
        &standard(),
    );

    assert!(outcome.is_ok(), "outcome: {outcome:?}");
    assert_eq!(outcome.prints, ["4 14 5", "5", "11 13"]);
}

#[test]
fn closure_lambda_basic() {
    assert_eq!(
        say(r#"let double = fn(x) { return x * 2 }
print(double(5))"#),
        "10"
    );
}

#[test]
fn lambda_parameter_binding_semantics_match_named_functions() {
    let outcome = run_both(
        r#"fn named(value, value) { return value }
struct Holder { n }
fn Holder.pick(self, value, value) { return self.n + value }
fn make(value) { return fn(value) { return value } }
let outer = 40
let closure = fn(_ignored, value, value) { return outer + value }
print(closure(1, 2, 3))
print(named(4, 5))
print(Holder { n: 6 }.pick(7, 8))
print(make(9)(10))"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "outcome: {outcome:?}");
    assert_eq!(outcome.prints, ["43", "5", "14", "10"]);
}

#[test]
fn duplicate_parameter_ref_semantics_match_across_callable_kinds() {
    let outcome = run_both(
        r#"fn value_value(value, value) { return value }
fn ref_value(ref value, value) { value += 2 }
fn value_ref(value, ref value) { value += 3 }
fn ref_ref(ref value, ref value) { value += 4 }
struct Holder { n }
fn Holder.update(self, ref value, value) { value += self.n }
fn Holder.replace(ref self, self) { self = Holder { n: self } }
let first = 1
let second = 1
let third = 1
let fourth = 1
let fifth = 2
let sixth = 2
let seventh = 2
let eighth = 3
let method_target = 1
let receiver = Holder { n: 1 }
ref_value(ref first, 7)
let alias = value_ref
alias(7, ref second)
ref_ref(ref third, ref fourth)
let lambda_ref_value = fn(ref value, value) { value += 5 }
let lambda_value_ref = fn(value, ref value) { value += 6 }
let lambda_ref_ref = fn(ref value, ref value) { value += 7 }
lambda_ref_value(ref fifth, 8)
lambda_value_ref(8, ref sixth)
lambda_ref_ref(ref seventh, ref eighth)
Holder { n: 6 }.update(ref method_target, 9)
receiver.replace(12)
print(value_value(10, 11))
print([first, second, third, fourth, fifth, sixth, seventh, eighth, method_target, receiver.n])"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "outcome: {outcome:?}");
    assert_eq!(
        outcome.prints,
        ["11", "[9, 4, 5, 5, 13, 8, 10, 10, 15, 12]"]
    );
}

#[test]
fn duplicate_ref_targets_remain_atomic_and_unique() {
    let rollback = run_both(
        r#"let fail = fn(ref value, value) {
    value = 99
    panic("rollback")
}
let target = 1
fn invoke() { fail(ref target, 7) }
print(try_call(invoke).is_err())
print(target)"#,
        &standard(),
    );
    assert!(rollback.is_ok(), "outcome: {rollback:?}");
    assert_eq!(rollback.prints, ["true", "1"]);

    let duplicate = run_both(
        r#"let both = fn(ref value, ref value) { value = 9 }
let target = 1
both(ref target, ref target)"#,
        &standard(),
    );
    assert!(
        duplicate
            .error
            .as_deref()
            .is_some_and(|message| message.contains("same variable")),
        "outcome: {duplicate:?}"
    );
    assert!(duplicate.prints.is_empty());
}

#[test]
fn explicit_ref_arguments_commit_through_nested_places_diff() {
    let outcome = run_both(
        r#"struct Holder { values }
fn add(ref value, amount) { value += amount }
let rows = [[1, 2], [3, 4]]
let holder = Holder { values: {"answer": 40} }
add(ref rows[1][0], 5)
add(ref holder.values["answer"], 2)
print(rows)
print(holder.values["answer"])"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "outcome: {outcome:?}");
    assert_eq!(outcome.prints, ["[[1, 2], [8, 4]]", "42"]);
}

#[test]
fn nested_ref_aliases_are_rejected_by_root_before_value_arguments_diff() {
    let outcome = run_both(
        r#"fn combine(ref left, value, ref right) {
    left = value
    right = value
}
fn side() { print("side"); return 9 }
let rows = [[1], [2]]
combine(ref rows[0], side(), ref rows[1])"#,
        &standard(),
    );
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|message| message.contains("same variable")),
        "outcome: {outcome:?}"
    );
    assert!(outcome.prints.is_empty());
}

#[test]
fn nested_ref_self_rereads_after_arguments_and_rolls_back_diff() {
    let outcome = run_both(
        r#"struct Counter { n }
fn Counter.bump(ref self, amount) {
    self.n += amount
    return self.n
}
fn Counter.fail(ref self) {
    self.n = 99
    panic("stop")
}
let rows = [Counter { n: 1 }]
fn side() {
    rows[0].n = 10
    return 2
}
print(rows[0].bump(side()), rows[0].n)
fn attempt() { rows[0].fail() }
print(try_call(attempt).is_err(), rows[0].n)"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "outcome: {outcome:?}");
    assert_eq!(outcome.prints, ["12 12", "true 12"]);
}

#[test]
fn any_duplicate_ref_position_prevents_closure_capture() {
    for source in [
        "fn bad(ref value, value) { return fn() { return value } }\nlet target = 1\nbad(ref target, 2)",
        "fn bad(value, ref value) { return fn() { return value } }\nlet target = 1\nbad(2, ref target)",
        "let bad = fn(ref value, value) { return fn() { return value } }\nlet target = 1\nbad(ref target, 2)",
    ] {
        let outcome = run_both(source, &standard());
        assert_eq!(
            outcome.error.as_deref(),
            Some("a `ref` parameter can't be captured by a closure"),
            "source: {source}"
        );
    }
}

#[test]
fn closure_captures_value() {
    assert_eq!(
        say(r#"let n = 5
let add_n = fn(x) { return x + n }
print(add_n(3))"#),
        "8"
    );
}

#[test]
fn closure_captures_are_snapshot() {
    assert_eq!(
        say(r#"let n = 5
let add_n = fn(x) { return x + n }
n = 100
print(add_n(3))"#),
        "8"
    );
}

#[test]
fn repeated_compiled_pool_dispatch_keeps_semantics_diff() {
    let outcome = run_both(
        r#"struct Counter { n }
fn Counter.bump(self, amount) { return self.n + amount }
fn exercise(limit) {
    let total = 0
    for i in range(limit) {
        let add_i = fn(x) { return x + i }
        let label = match i % 3 {
            0 => "zero",
            1 => "one",
            _ => "two",
        }
        total += add_i(label.len())
        total += Counter { n: i }.bump(1)
        let rendered = "{i}:{label}:{total}"
        if rendered.len() > 0 { total += 1 }
    }
    return total
}
print(exercise(180))"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["33180"]);
}

#[test]
fn unresolved_direct_lambda_capture_reports_variable_not_found_diff() {
    assert_eq!(
        run_err(
            r#"let read = fn() { return missing }
print(read())"#
        ),
        "Variable `missing` not found"
    );
}

#[test]
fn named_fn_lambda_does_not_capture_module_let_diff() {
    assert_eq!(
        say(r#"let g = 5
fn read_global() {
    let read = fn() { return g }
    return read()
}
print(read_global())"#),
        "5"
    );
}

#[test]
fn unresolved_capture_in_arithmetic_keeps_binding_error_diff() {
    assert_eq!(
        run_err(
            r#"fn calculate() {
    let add = fn() { return missing + 1 }
    return add()
}
print(calculate())"#
        ),
        "Variable `missing` not found"
    );
}

#[test]
fn unresolved_capture_propagates_through_nested_lambdas_diff() {
    assert_eq!(
        run_err(
            r#"fn build() {
    return fn() {
        return fn() { return missing }
    }
}
let outer = build()
let inner = outer()
print(inner())"#
        ),
        "Variable `missing` not found"
    );
}

#[test]
fn uncaptured_named_functions_remain_reachable_from_lambdas_diff() {
    assert_eq!(
        say(r#"fn helper() { return 7 }
fn call_factory() { return fn() { return helper() } }
fn value_factory() { return fn() { return helper } }
let call_helper = call_factory()
let load_helper = value_factory()
let helper_value = load_helper()
print(call_helper() + helper_value())"#),
        "14"
    );
}

#[test]
fn legitimate_none_captures_remain_present_through_nesting_diff() {
    assert_eq!(
        say(r#"let module_none = none
let read_module = fn() { return module_none }
fn build_local() {
    let local_none = none
    return fn() { return local_none }
}
fn build_nested() {
    let nested_none = none
    return fn() { return fn() { return nested_none } }
}
let read_local = build_local()
let read_nested = build_nested()()
print(read_module().is_none() && read_local().is_none() && read_nested().is_none())"#),
        "true"
    );
}

#[test]
fn closure_captures_array_used_only_by_in_place_method_diff() {
    assert_eq!(
        run_err(
            r#"fn make_mutator() {
    let values = []
    return fn() { values.push(1) }
}
let mutate = make_mutator()
print(mutate())"#
        ),
        "`ref` argument 1 can't target a closure-captured binding"
    );
}

#[test]
fn closure_captures_values_used_only_by_in_place_assignment_targets_diff() {
    assert_eq!(
        say(r#"struct Holder { n }
fn make_mutator() {
    let array = [0]
    let dict = {"n": 0}
    let holder = Holder { n: 0 }
    return fn() {
        array[0] = 1
        dict["n"] += 2
        holder.n = 3
    }
}
let mutate = make_mutator()
print(mutate())"#),
        "none"
    );
}

#[test]
fn closure_ignores_unreferenced_max_depth_binding_diff() {
    assert_eq!(
        say(r#"let unrelated = none
repeat 64 { unrelated = [unrelated] }
let answer = fn() { return 42 }
print(answer())"#),
        "42"
    );
}

#[test]
fn closure_free_vars_respect_lexical_scopes_diff() {
    assert_eq!(
        say(r#"let outer = 10
let build = fn(param) {
    let sequential = param + outer
    if true {
        let block = sequential + 1
        return match [block] {
            [bound] => fn(extra) {
                let local = extra
                return bound + local
            },
        }
    }
    return fn(extra) { return extra }
}
let add = build(1)
outer = 100
print(add(2))"#),
        "14"
    );
}

#[test]
fn closure_match_binding_shadows_unrelated_deep_name_diff() {
    assert_eq!(
        say(r#"let bound = none
repeat 64 { bound = [bound] }
let build = fn() {
    return match [7] {
        [bound] => fn() { return bound },
    }
}
let read = build()
print(read())"#),
        "7"
    );
}

#[test]
fn closure_captures_string_interpolation_variables_diff() {
    assert_eq!(
        say(r#"let name = "world"
let greet = fn() { return "hello {name}" }
name = "later"
print(greet())"#),
        "hello world"
    );
}

#[test]
fn nested_closures_capture_values_used_only_by_interpolation_diff() {
    assert_eq!(
        say(r#"fn build(prefix) {
    let local = "local"
    return fn(suffix) {
        let middle = ":"
        return fn() { return "{prefix}{middle}{local}{suffix}" }
    }
}
let finish = build("start:")("end")
print(finish())"#),
        "start::localend"
    );
}

#[test]
fn closure_assignment_targets_are_captured_diff() {
    assert_eq!(
        say(r#"let counter = 1
let values = [0]
let mutate = fn() {
    counter += 2
    values[0] = counter
    return values[0]
}
counter = 100
values = [99]
print(mutate())"#),
        "3"
    );
}

#[test]
fn closure_factory_pattern() {
    assert_eq!(
        say(r#"fn make_adder(n) { return fn(x) { return x + n } }
let add5 = make_adder(5)
let add10 = make_adder(10)
print(add5(3))
print(add10(3))"#),
        "13"
    );
}

#[test]
fn named_fn_as_first_class_value() {
    assert_eq!(
        say(r#"fn double(x) { return x * 2 }
let f = double
print(f(7))"#),
        "14"
    );
}

#[test]
fn fn_in_array_indexed_call() {
    assert_eq!(
        say(r#"fn add(x, y) { return x + y }
fn mul(x, y) { return x * y }
let ops = [add, mul]
print(ops[0](2, 3))
print(ops[1](2, 3))"#),
        "6"
    );
}

#[test]
fn higher_order_apply() {
    assert_eq!(
        say(r#"fn apply(f, x) { return f(x) }
fn square(n) { return n * n }
print(apply(square, 4))
print(apply(fn(n) { return n + 1 }, 4))"#),
        "5"
    );
}

#[test]
fn type_of_fn_is_fn() {
    assert_eq!(say("fn f() { }\nprint(f.type())"), "fn");
    assert_eq!(say("let g = fn() { }\nprint(g.type())"), "fn");
}

#[test]
fn calling_non_callable_errors() {
    assert!(run_err("let x = 5\nx(1)").contains("not a function"));
}

#[test]
fn iife_value_call() {
    assert_eq!(say("print((fn(x) { return x * 3 })(4))"), "12");
}

// ─── Arrays ───────────────────────────────────────────────────────

#[test]
fn array_literal_and_index() {
    assert_eq!(say("let a = [10, 20, 30]\nprint(a[1])"), "20");
}

#[test]
fn array_negative_index() {
    assert_eq!(say("let a = [10, 20, 30]\nprint(a[-1])"), "30");
}

#[test]
fn array_assign_index() {
    assert_eq!(say("let a = [1, 2, 3]\na[1] = 99\nprint(a[1])"), "99");
}

#[test]
fn array_push_pop() {
    assert_eq!(
        say(r#"let a = [1, 2]
a.push(3)
print(a.len())"#),
        "3"
    );
    assert_eq!(
        say(r#"let a = [1, 2, 3]
let last = a.pop()
print(last)"#),
        "3"
    );
}

#[test]
fn array_mutation_fast_path_preserves_value_semantics_diff() {
    let outcome = run_both(
        r#"let original = [1, 2]
let alias = original
original.push(3)
print(original)
print(alias)

let nested = [1, 2]
nested.push(nested.pop())
print(nested)

let transient_source = [7]
(if true { transient_source } else { [] }).push(8)
[9].push(10)
print(transient_source)"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["[1, 2, 3]", "[1, 2]", "[1, 2]", "[7]"]);
}

#[test]
fn array_push_name_dispatches_by_runtime_receiver_type_diff() {
    assert_eq!(
        say(r#"struct Accumulator { total }
fn Accumulator.push(self, value) { return self.total + value }
let accumulator = Accumulator { total: 7 }
print(accumulator.push(5))"#),
        "12"
    );
}

#[test]
fn array_large_append_loop_and_mutators_diff() {
    let outcome = run_both(
        r#"let values = []
let next = 0
repeat 2048 {
    values.push(next)
    next += 1
}
print(values.len())
print(values[0])
print(values[-1])

let changed = [4, 1, 3]
print(changed.push(2))
print(changed.insert(1, 5))
print(changed.remove(2))
print(changed.pop())
changed.sort()
changed.reverse()
print(changed)"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        ["2048", "0", "2047", "none", "none", "1", "2", "[5, 4, 3]"]
    );
}

#[test]
fn array_mutation_errors_are_atomic_diff() {
    let outcome = run_both(
        r#"let values = [1, 2, 3]
print(try_call(fn() { return values.push() }).is_err())
print(try_call(fn() { return values.insert(99, 4) }).is_err())
print(try_call(fn() { return values.remove(99) }).is_err())
print(values)"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["true", "true", "true", "[1, 2, 3]"]);
}

#[test]
fn array_push_rejects_excessive_value_depth_diff() {
    assert_both_value_depth_errors(
        r#"let deep = none
repeat 64 { deep = [deep] }
let values = []
values.push(deep)"#,
        4,
    );
}

#[test]
fn array_has() {
    assert_eq!(say("print([1, 2, 3].has(2))"), "true");
    assert_eq!(say("print([1, 2, 3].has(9))"), "false");
}

#[test]
fn array_index_of() {
    assert_eq!(say("print([10, 20, 30].index_of(20))"), "1");
    assert_eq!(say("print([10, 20, 30].index_of(99))"), "-1");
}

#[test]
fn array_slice() {
    assert_eq!(say("print([1, 2, 3, 4, 5].slice(1, 4))"), "[2, 3, 4]");
}

#[test]
fn array_join() {
    assert_eq!(say(r#"print([1, 2, 3].join("-"))"#), "1-2-3");
}

#[test]
fn array_sort() {
    assert_eq!(say("let a = [3, 1, 2]\na.sort()\nprint(a)"), "[1, 2, 3]");
}

#[test]
fn array_reverse() {
    assert_eq!(say("let a = [1, 2, 3]\na.reverse()\nprint(a)"), "[3, 2, 1]");
}

#[test]
fn array_insert_remove() {
    assert_eq!(
        say(r#"let a = [1, 3]
a.insert(1, 2)
print(a)"#),
        "[1, 2, 3]"
    );
    assert_eq!(
        say(r#"let a = [1, 2, 3]
let removed = a.remove(1)
print(removed)"#),
        "2"
    );
}

#[test]
fn array_negative_remove_insert_and_slice_diff() {
    let outcome = run_both(
        r#"let values = [10, 20, 30, 40]
let removed = values.remove(-1)
let inserted = values.insert(-1, 25)
print(removed)
print(inserted)
print(values)
print(values.slice(-3, -1))
print(values.slice(-99, 99))
print(values.slice(99, -99))"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        [
            "40",
            "none",
            "[10, 20, 25, 30]",
            "[20, 25]",
            "[10, 20, 25, 30]",
            "[]"
        ]
    );
}

#[test]
fn mutating_method_bad_arity_preflight_suppresses_argument_side_effects_diff() {
    let outcome = run_both(
        r#"
let calls = 0
let values = [1]
fn side() {
    calls += 1
    return 2
}
fn bad_arity() { values.push(side(), 3) }
print(try_call(bad_arity).is_err())
print(calls)
"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["true", "0"]);
    assert_eq!(
        run_err("let values = [1]\nvalues.push(1, 2)"),
        "`.push()` needs 1 argument"
    );
}

#[test]
fn zero_parameter_user_method_reports_the_missing_receiver_arity_diff() {
    assert_eq!(
        run_err(
            r#"
struct Empty {}
fn Empty.zero() { return none }
Empty {}.zero()
"#,
        ),
        "`Empty.zero` expects 0 arguments (including `self`), but got 1"
    );
}

#[test]
fn array_signed_index_boundaries_and_empty_values_diff() {
    let outcome = run_both(
        r#"let values = [20, 30]
print(values.insert(-2, 10))
print(values.insert(3, 40))
print(values.remove(-4))
print(values[-3])
print(values)
let empty = []
print(empty.insert(0, 1))
print(empty)
print([].slice(-5, 5))
print(try_call(fn() { return [].remove(-1) }).is_err())
print(try_call(fn() { return [].insert(-1, 1) }).is_err())
print(try_call(fn() { return values.remove(3) }).is_err())
print(try_call(fn() { return values[-4] }).is_err())"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        [
            "none",
            "none",
            "10",
            "20",
            "[20, 30, 40]",
            "none",
            "[1]",
            "[]",
            "true",
            "true",
            "true",
            "true"
        ]
    );
}

#[test]
fn signed_index_failures_are_nonfatal_and_catchable_diff() {
    let outcome = run_both(
        r#"let values = [10, 20, 30]
let remove_result = try_call(fn() { return values.remove(-4) })
let insert_result = try_call(fn() { return values.insert(-4, 0) })
let set_result = try_call(fn() { values[-4] = 0 })
print(remove_result.is_err())
print(insert_result.is_err())
print(set_result.is_err())
print(values)"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    // Lambdas snapshot captures by value, so the final print documents that
    // existing value-semantics contract; mutation atomicity itself follows
    // from the shared helpers returning Err before any replacement value.
    assert_eq!(outcome.prints, ["true", "true", "true", "[10, 20, 30]"]);
}

#[test]
fn nested_array_mutation_commits_through_places_diff() {
    let outcome = run_both(
        r#"struct Holder { items }
let indexed = {"items": [1]}
let fielded = Holder { items: [1, 2] }
indexed["items"].push(2)
fielded.items.pop()
print(indexed["items"])
print(fielded.items)"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["[1, 2]", "[1]"]);
}

#[test]
fn every_array_mutator_commits_through_nested_places_diff() {
    for (call, expected) in [
        ("push(3)", "[2, 1, 3]"),
        ("pop()", "[2]"),
        ("insert(0, 3)", "[3, 2, 1]"),
        ("remove(0)", "[1]"),
        ("reverse()", "[1, 2]"),
        ("sort()", "[1, 2]"),
    ] {
        let source =
            format!("let d = {{\"items\": [2, 1]}}\nd[\"items\"].{call}\nprint(d[\"items\"])");
        assert_eq!(say(&source), expected, "call: {call}");
    }
}

#[test]
fn nested_array_mutation_supports_grouped_receivers_diff() {
    let cases = [
        (
            r#"let d = {"items": [1]}
d["items"].push(2)
print(d["items"])"#,
            "[1, 2]",
        ),
        (
            r#"struct Holder { items }
let holder = Holder { items: [1] }
holder.items.pop()
print(holder.items)"#,
            "[]",
        ),
        (
            r#"let d = {"items": [1]}
(d["items"]).push(2)
print(d["items"])"#,
            "[1, 2]",
        ),
        (
            r#"struct Holder { items }
let holder = Holder { items: [1] }
(holder.items).pop()
print(holder.items)"#,
            "[]",
        ),
    ];

    for (source, expected) in cases {
        assert_eq!(say(source), expected);
    }
}

#[test]
fn true_temporaries_and_same_named_user_methods_remain_legal_diff() {
    let outcome = run_both(
        r#"fn make_array() { return [7] }
print([1].push(2))
print(make_array().pop())
print((if true { [3, 1] } else { [2] }).sort())
struct Gadget { n }
fn Gadget.push(self, amount) { return self.n + amount }
struct Holder { item }
let holder = Holder { item: Gadget { n: 10 } }
let indexed = {"item": Gadget { n: 20 }}
print(holder.item.push(2))
print(indexed["item"].push(3))"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["none", "7", "none", "12", "23"]);
}

#[test]
fn numeric_indices_truncate_toward_zero_diff() {
    let outcome = run_both(
        r#"let values = [10, 20, 30]
print(values[1.9])
values[-1.9] = 99
print(values.remove(-1.9))
print(values.insert(1.9, 15))
print(values)
print(values.slice(0.9, 2.9))
print("a🙂é"[1.9])
print("a🙂é".slice(0.9, 2.9))"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        ["20", "99", "none", "[10, 15, 20]", "[10, 15]", "🙂", "a🙂"]
    );
}

#[test]
fn signed_index_operations_reject_non_numeric_values_diff() {
    for (code, expected) in [
        (r#"print([1]["0"])"#, "Can't index array with string"),
        (r#"[1].remove("0")"#, "expects a number"),
        (r#"[1].insert("0", 2)"#, "expects a number"),
        ("[1].slice(false, 1)", "expects a number"),
        (r#"print("a"["0"])"#, "Can't index string with string"),
        (r#"print("abc".slice("0", 1))"#, "expects a number"),
    ] {
        let message = run_err(code);
        assert!(
            message.contains(expected),
            "expected {expected:?} for {code:?}, got {message:?}"
        );
    }
}

#[test]
fn signed_index_errors_report_the_original_index_diff() {
    for (code, expected) in [
        ("[1, 2].remove(-3)", "Remove index -3 is out of bounds"),
        ("[1, 2].insert(-3, 0)", "Insert index -3 is out of bounds"),
        (
            "print([1, 2][-3])",
            "Index -3 is out of bounds (array has 2 items)",
        ),
    ] {
        assert_eq!(run_err(code), expected, "source: {code}");
    }
}

#[test]
fn signed_indices_handle_i64_extremes_without_overflow_diff() {
    let outcome = run_both(
        r#"let min = -9223372036854775807 - 1
let max = 9223372036854775807
let values = [1, 2]
print(values.slice(min, max))
print(values.slice(max, min))
print(try_call(fn() { return values[min] }).is_err())
print(try_call(fn() { return values.remove(min) }).is_err())
print(try_call(fn() { return values.insert(max, 3) }).is_err())
print(values)"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        ["[1, 2]", "[]", "true", "true", "true", "[1, 2]"]
    );
}

#[test]
fn signed_index_failures_keep_the_call_site_line_diff() {
    let code = "let values = [1, 2]\nvalues.remove(-3)";
    for engine in ["tree-walker", "bytecode vm"] {
        let mut host = RecordHost::new();
        let result = if engine == "tree-walker" {
            nybl::run(code, &mut host, &standard())
        } else {
            nybl_vm::run(code, &mut host, &standard())
        };
        let err = result.unwrap_err();
        assert_eq!(err.line, Some(2), "{engine} line");
        assert!(!err.is_fatal, "{engine} returned a fatal error");
        assert_eq!(err.message, "Remove index -3 is out of bounds");
    }
}

#[test]
fn array_concat() {
    assert_eq!(say("print([1, 2] + [3, 4])"), "[1, 2, 3, 4]");
}

#[test]
fn array_out_of_bounds() {
    assert!(run_err("let a = [1]\nprint(a[5])").contains("out of bounds"));
}

// ─── Strings (methods) ────────────────────────────────────────────

#[test]
fn string_len() {
    assert_eq!(say(r#"print("hello".len())"#), "5");
}

#[test]
fn string_contains() {
    assert_eq!(say(r#"print("abcdef".contains("cd"))"#), "true");
    assert_eq!(say(r#"print("abcdef".contains("zz"))"#), "false");
}

#[test]
fn string_starts_ends_with() {
    assert_eq!(say(r#"print("hello".starts_with("he"))"#), "true");
    assert_eq!(say(r#"print("hello".ends_with("lo"))"#), "true");
}

#[test]
fn string_split() {
    assert_eq!(say(r#"print("a,b,c".split(","))"#), r#"["a", "b", "c"]"#);
}

#[test]
fn string_replace() {
    assert_eq!(
        say(r#"print("hello world".replace("world", "nybl"))"#),
        "hello nybl"
    );
}

#[test]
fn string_upper_lower_trim() {
    assert_eq!(say(r#"print("Hello".upper())"#), "HELLO");
    assert_eq!(say(r#"print("Hello".lower())"#), "hello");
    assert_eq!(say(r#"print("  hi  ".trim())"#), "hi");
}

#[test]
fn string_slice() {
    assert_eq!(say(r#"print("hello".slice(1, 4))"#), "ell");
}

#[test]
fn string_index_of() {
    assert_eq!(say(r#"print("hello".index_of("ll"))"#), "2");
    assert_eq!(say(r#"print("hello".index_of("zz"))"#), "-1");
}

#[test]
fn string_index_char() {
    assert_eq!(say(r#"print("abc"[1])"#), "b");
}

#[test]
fn string_negative_indices_and_slices_use_unicode_chars_diff() {
    let outcome = run_both(
        r#"let text = "a🙂é界"
print(text[-1])
print(text[-4])
print(text.slice(-3, -1))
print(text.slice(-4, 4))
print(text.slice(-99, 99))
print(text.slice(99, -99))
print("".slice(-1, 1))"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        ["界", "a", "🙂é", "a🙂é界", "a🙂é界", "", ""]
    );

    for code in [r#"print("a🙂é界"[-5])"#, r#"print(""[-1])"#] {
        let message = run_err(code);
        assert!(message.contains("out of bounds"), "got: {message}");
    }
}

// ─── Dicts ────────────────────────────────────────────────────────

#[test]
fn dict_literal_and_access() {
    assert_eq!(
        say(r#"let d = {"name": "nybl", "hp": 100}
print(d["name"])"#),
        "nybl"
    );
}

#[test]
fn dict_assign_key() {
    assert_eq!(
        say(r#"let d = {"a": 1}
d["b"] = 2
print(d["b"])"#),
        "2"
    );
}

#[test]
fn dict_methods() {
    assert_eq!(
        say(r#"let d = {"x": 1, "y": 2}
print(d.len())"#),
        "2"
    );
    assert_eq!(say(r#"print({"a": 1, "b": 2}.has("a"))"#), "true");
    assert_eq!(say(r#"print({"a": 1, "b": 2}.has("z"))"#), "false");
}

#[test]
fn dict_keys_values() {
    assert_eq!(say(r#"print({"a": 1, "b": 2}.keys())"#), r#"["a", "b"]"#);
    assert_eq!(say(r#"print({"a": 1, "b": 2}.values())"#), "[1, 2]");
}

// ─── Built-in functions ───────────────────────────────────────────

#[test]
fn builtin_range_1arg() {
    assert_eq!(say("print(range(5))"), "[0, 1, 2, 3, 4]");
}

#[test]
fn builtin_range_2args() {
    assert_eq!(say("print(range(2, 5))"), "[2, 3, 4]");
}

#[test]
fn builtin_range_3args() {
    assert_eq!(say("print(range(0, 10, 3))"), "[0, 3, 6, 9]");
}

#[test]
fn builtin_range_reverse() {
    assert_eq!(say("print(range(5, 0))"), "[5, 4, 3, 2, 1]");
}

#[test]
fn builtin_range_boundary_is_not_truncated_diff() {
    let outcome = run_both(
        r#"let at_old_cap = range(10000)
print(at_old_cap.len())
print(at_old_cap[9999])"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["10000", "9999"]);
}

#[test]
fn builtin_range_large_signed_steps_are_exact_diff() {
    let outcome = run_both(
        r#"let ascending = range(-7, 29993, 3)
let descending = range(29993, -7, -3)
print(ascending.len())
print(ascending[9999])
print(descending.len())
print(descending[9999])"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["10000", "29990", "10000", "-4"]);
}

#[test]
fn builtin_range_direction_mismatch_is_empty_diff() {
    let outcome = run_both("print(range(5, 0, 1))\nprint(range(0, 5, -1))", &standard());
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(outcome.prints, ["[]", "[]"]);
}

#[test]
fn builtin_range_full_i64_span_avoids_cardinality_overflow_diff() {
    let outcome = run_both(
        r#"let min = -9223372036854775807 - 1
let max = 9223372036854775807
print(range(min, max, max))
print(range(max, min, min))"#,
        &standard(),
    );
    assert!(outcome.is_ok(), "unexpected error: {:?}", outcome.error);
    assert_eq!(
        outcome.prints,
        [
            "[-9223372036854775808, -1, 9223372036854775806]",
            "[9223372036854775807, -1]"
        ]
    );
}

#[test]
fn builtin_range_zero_step_remains_non_fatal_diff() {
    for engine in ["tree-walker", "bytecode vm"] {
        let mut host = RecordHost::new();
        let result = if engine == "tree-walker" {
            nybl::run("let values = range(0, 10, 0)", &mut host, &standard())
        } else {
            nybl_vm::run("let values = range(0, 10, 0)", &mut host, &standard())
        };
        assert!(host.prints.borrow().is_empty(), "{engine} printed");
        let err = result.unwrap_err();
        assert_eq!(err.message, "range step can't be 0", "{engine} message");
        assert_eq!(err.line, Some(1), "{engine} line");
        assert!(!err.is_fatal, "{engine} returned a fatal error");
    }

    let code = r#"let result = try_call(fn() { return range(0, 10, 0) })
print(result.is_err())"#;
    assert_eq!(say(code), "true");
}

#[test]
fn builtin_range_one_past_limit_is_fatal_diff() {
    let code = r#"let result = try_call(fn() {
    return range(10001)
})
print("unreachable")"#;
    for engine in ["tree-walker", "bytecode vm"] {
        let mut host = RecordHost::new();
        let result = if engine == "tree-walker" {
            nybl::run(code, &mut host, &standard())
        } else {
            nybl_vm::run(code, &mut host, &standard())
        };
        assert!(host.prints.borrow().is_empty(), "{engine} printed");
        let err = result.unwrap_err();
        assert_eq!(
            err.message,
            nybl::builtins::RANGE_LIMIT_ERROR_MESSAGE,
            "{engine} message"
        );
        assert_eq!(
            err.friendly_hint.as_deref(),
            Some(nybl::builtins::RANGE_LIMIT_HINT),
            "{engine} hint"
        );
        assert_eq!(err.line, Some(2), "{engine} line");
        assert!(err.is_fatal, "{engine} returned a catchable error");
    }
}

#[test]
fn builtin_range_limit_uses_stepped_cardinality_diff() {
    for code in [
        "let values = range(0, 20002, 2)",
        "let values = range(20000, -2, -2)",
    ] {
        let err = run_err(code);
        assert_eq!(err, nybl::builtins::RANGE_LIMIT_ERROR_MESSAGE);
    }
}

#[test]
fn builtin_str() {
    assert_eq!(say(r#"print(42.to_str())"#), "42");
    assert_eq!(say(r#"print(true.to_str())"#), "true");
}

#[test]
fn builtin_int() {
    assert_eq!(say("print(3.7.to_int())"), "3");
    assert_eq!(say("print((-2.9).to_int())"), "-2");
}

#[test]
fn builtin_type() {
    // Phase 6 split numeric types.
    assert_eq!(say("print(42.type())"), "int");
    assert_eq!(say("print(42.0.type())"), "number");
    assert_eq!(say(r#"print("hi".type())"#), "string");
    assert_eq!(say("print(true.type())"), "bool");
    assert_eq!(say("print(none.type())"), "none");
    assert_eq!(say("print([].type())"), "array");
}

#[test]
fn builtin_abs_min_max() {
    assert_eq!(say("print((-5).abs())"), "5");
    assert_eq!(say("print(3.min(7))"), "3");
    assert_eq!(say("print(3.max(7))"), "7");
}

#[test]
fn builtin_len() {
    assert_eq!(say(r#"print("hello".len())"#), "5");
    assert_eq!(say("print([1, 2, 3].len())"), "3");
}

#[test]
fn builtin_inspect() {
    assert_eq!(say(r#"print("hi".inspect())"#), r#""hi""#);
    assert_eq!(say("print(42.inspect())"), "42");
}

#[test]
fn builtin_print_multi_args() {
    let out = run_both(r#"print("a", "b", "c")"#, &standard());
    assert!(out.is_ok());
    assert_eq!(out.prints.as_slice(), &["a b c"]);
}

#[test]
fn builtin_rand_deterministic() {
    // Both engines start `rand_state` at 0, so the runs inside the
    // same harness call also agree. This test pins that the same
    // input produces the same output on repeated top-level runs.
    let a = say("print(rand(100))");
    let b = say("print(rand(100))");
    assert_eq!(a, b);
}

// ─── Error cases ──────────────────────────────────────────────────

#[test]
fn error_division_by_zero() {
    assert!(run_err("print(1 / 0)").contains("Division by zero"));
}

#[test]
fn error_type_mismatch_subtract() {
    let msg = run_err(r#"print("a" - 1)"#);
    assert!(msg.contains("Can't use `-`"));
}

#[test]
fn error_unknown_function() {
    assert!(run_err("nope()").contains("not found"));
}

#[test]
fn dict_missing_key_returns_none_diff() {
    // Walker and VM share `ops::index_get` — the soft-lookup
    // behaviour must be identical byte-for-byte.
    set_modules(&[]);
    assert_eq!(
        say(r#"let d = {"x": 1}
print(d["x"])
print(d["y"])
print(d["y"].is_none())"#),
        "true"
    );
}

#[test]
fn is_none_is_some_universal_methods_diff() {
    // Works on every value shape and matches `== none` exactly.
    // Walker and VM must agree for any receiver.
    set_modules(&[]);
    assert_eq!(
        say(r#"print(none.is_none())
print((0).is_some())
print("".is_none())
print([].is_some())"#),
        "true"
    );
}

#[test]
fn is_none_in_match_and_if_diff() {
    // Guard clauses, conditional branches, and optional return
    // values all need to agree between walker and VM.
    set_modules(&[]);
    assert_eq!(
        say(r#"fn maybe(n) {
    if n < 0 { return none }
    return n * 2
}
let out = ""
for x in [-1, 5, -3, 8] {
    let r = maybe(x)
    if r.is_some() { out = out + r.to_str() + "," } else { out = out + "_," }
}
print(out)"#),
        "_,10,_,16,"
    );
}

#[test]
fn ok_err_sugar_matches_result_construction_diff() {
    // `Ok(x)` / `Err(e)` are parser-level sugar — walker and VM
    // must see identical AST, so the printed values should be
    // indistinguishable from `Result::Ok(x)` / `Result::Err(e)`.
    set_modules(&[]);
    assert_eq!(
        say(r#"print(Ok(42))
print(Err("boom"))
print(Ok(1) == Result::Ok(1))
print(Err("x") == Result::Err("x"))"#),
        "true"
    );
}

#[test]
fn ok_err_pattern_sugar_in_match_diff() {
    // Pattern-side desugar has to produce the same match
    // behaviour in both engines.
    set_modules(&[]);
    assert_eq!(
        say(r#"fn classify(r) {
    return match r {
        Ok(v)  => v,
        Err(_) => -1,
    }
}
print(classify(Ok(5)))
print(classify(Err("x")))"#),
        "-1"
    );
}

#[test]
fn many_flat_ok_err_patterns_do_not_exhaust_parse_depth_diff() {
    // Regression for #6: pattern depth is lexical nesting, not a cumulative
    // budget. Each flat match has two shorthand patterns, so this exercises
    // far more than MAX_PARSE_DEPTH patterns in one program.
    set_modules(&[]);
    let mut program = String::from("let result = Ok(7)\nlet total = 0\n");
    for _ in 0..160 {
        program.push_str("total += match result { Ok(value) => value, Err(_) => 0 }\n");
    }
    program.push_str("print(total)");

    assert_eq!(say(&program), "1120");
}

#[test]
fn iter_protocol_array_yields_each_item_diff() {
    // `.iter()` + `.next()` must produce the same Iter::Next /
    // Iter::Done sequence on both engines.
    set_modules(&[]);
    assert_eq!(
        say(r#"let it = [1, 2, 3].iter()
print(it.next())
print(it.next())
print(it.next())
print(it.next())"#),
        "Iter::Done"
    );
}

#[test]
fn for_over_iter_value_matches_direct_diff() {
    // Walker and VM must treat `for x in arr.iter()` and
    // `for x in arr` identically. Uses a reduce-style running
    // total so the `say` return captures the full result.
    set_modules(&[]);
    assert_eq!(
        say(r#"let total = 0
for x in [1, 2, 3, 4, 5].iter() { total = total + x }
print(total)"#),
        "15"
    );
}

#[test]
fn for_over_user_container_via_iter_diff() {
    // Bag delegates `.iter()` to the backing array. Walker and
    // VM both have to see a Bag, call its `.iter()` method, and
    // iterate the returned Value::Iter transparently.
    set_modules(&[]);
    assert_eq!(
        say(r#"struct Bag { items }
fn bag_of(arr) { return Bag { items: arr } }
fn Bag.iter(self) { return self.items.iter() }

let b = bag_of([10, 20, 30])
let sum = 0
for v in b { sum = sum + v }
print(sum)"#),
        "60"
    );
}

#[test]
fn break_discards_user_iterator_sidecar_diff() {
    set_modules(&[]);
    assert_eq!(
        say(r#"struct Bag { items }
fn Bag.iter(self) { return self.items.iter() }
let bag = Bag { items: [10, 20, 30] }
let seen = []
for outer in [1, 2, 3] {
    for value in bag { break }
    seen.push(outer)
}
print(seen)"#),
        "[1, 2, 3]"
    );
}

#[test]
fn for_on_dict_iterates_keys_diff() {
    // `for k in dict` used to error — it now materialises the
    // keys. Both engines must produce the same output.
    set_modules(&[]);
    assert_eq!(
        say(r#"let out = ""
for k in {"a": 1, "b": 2, "c": 3} { out = out + k }
print(out)"#),
        "abc"
    );
}

#[test]
fn panic_builtin_raises_with_message() {
    // `panic(msg)` is the stdlib's shared error-signalling
    // primitive; both engines have to surface the message
    // verbatim so `unwrap` / `assert_eq` / `assert_raises`
    // (and any user code) see the same string.
    let msg = run_err(r#"panic("deliberate")"#);
    assert!(
        msg.contains("deliberate"),
        "expected panic message to appear in error surface: {msg}"
    );
}

#[test]
fn panic_catches_through_try_call() {
    // The outcome of `try_call(fn() { panic(...) })` must match
    // in walker and VM — both should produce
    // `Result::Err(RuntimeError { message: "x", .. })` with
    // identical printed output. `say` runs both and asserts
    // identical output itself, so a return of "x" is enough.
    let out = say(r#"let r = try_call(fn() { panic("x") })
print(match r {
    Result::Ok(_)  => "ok?",
    Result::Err(e) => e.message,
})"#);
    assert_eq!(out, "x");
}

#[test]
fn error_infinite_loop_protection() {
    let msg = run_err("while true { }");
    assert!(msg.contains("too many steps"));
}

#[test]
fn error_break_outside_loop() {
    // Walker catches this at runtime (`line: 0`), VM compiler
    // catches it at compile time (real line). Message text matches
    // either way — that's the contract we care about.
    assert!(run_err("break").contains("outside of a loop"));
}

#[test]
fn error_continue_outside_loop() {
    assert!(run_err("continue").contains("outside of a loop"));
}

// ─── Edge cases ───────────────────────────────────────────────────

#[test]
fn empty_program() {
    let out = run_both("", &standard());
    assert!(out.is_ok());
    assert!(out.prints.is_empty());
}

#[test]
fn trailing_comma_in_array() {
    assert_eq!(say("print([1, 2, 3,])"), "[1, 2, 3]");
}

#[test]
fn trailing_comma_in_dict() {
    assert_eq!(say(r#"print({"a": 1,}.len())"#), "1");
}

#[test]
fn none_value() {
    assert_eq!(say("print(none)"), "none");
    assert_eq!(say("print(none == none)"), "true");
}

#[test]
fn equality_across_types() {
    assert_eq!(say("print(1 == true)"), "false");
    assert_eq!(say(r#"print(0 == "")"#), "false");
    assert_eq!(say("print(none == false)"), "false");
}

#[test]
fn dict_equality() {
    assert_eq!(
        say(r#"print({"a": 1, "b": 2} == {"b": 2, "a": 1})"#),
        "true"
    );
    assert_eq!(say(r#"print({"a": 1} == {"a": 2})"#), "false");
    assert_eq!(say(r#"print({"a": 1} == {"b": 1})"#), "false");
    assert_eq!(say(r#"print({"a": 1} == {"a": 1, "b": 2})"#), "false");
    assert_eq!(say(r#"print({"a": {"x": 1}} == {"a": {"x": 1}})"#), "true");
}

#[test]
fn nested_array_access() {
    assert_eq!(say("let m = [[1, 2], [3, 4]]\nprint(m[1][0])"), "3");
}

#[test]
fn method_chain() {
    assert_eq!(say(r#"print("  HELLO  ".trim().lower())"#), "hello");
}

#[test]
fn comments_in_code() {
    // `//` is the line-comment leader. No integer-division
    // operator in Nybl — `(a / b).to_int()` covers that case.
    assert_eq!(
        say(r#"// this is a comment
let x = 42 // inline comment
print(x)"#),
        "42"
    );
}

// ─── Scope / block isolation ──────────────────────────────────────

#[test]
fn if_block_scope() {
    assert!(
        run_err(
            r#"if true { let inner = 1 }
print(inner)"#
        )
        .contains("not found")
    );
}

#[test]
fn for_loop_var_scoped() {
    assert!(
        run_err(
            r#"for item in [1, 2] { let x = item }
print(item)"#
        )
        .contains("not found")
    );
}

// ─── Complex programs ─────────────────────────────────────────────

#[test]
fn fizzbuzz() {
    assert_eq!(
        say(r#"let result = []
for i in range(1, 16) {
    if i % 15 == 0 {
        result.push("FizzBuzz")
    } else if i % 3 == 0 {
        result.push("Fizz")
    } else if i % 5 == 0 {
        result.push("Buzz")
    } else {
        result.push(i.to_str())
    }
}
print(result.join(", "))"#),
        "1, 2, Fizz, 4, Buzz, Fizz, 7, 8, Fizz, Buzz, 11, Fizz, 13, 14, FizzBuzz"
    );
}

#[test]
fn nested_function_calls() {
    assert_eq!(
        say(r#"fn square(n) { return n * n }
fn sum_squares(a, b) { return square(a) + square(b) }
print(sum_squares(3, 4))"#),
        "25"
    );
}

#[test]
fn array_manipulation_program() {
    assert_eq!(
        say(r#"let data = [5, 2, 8, 1, 9, 3]
data.sort()
let top3 = data.slice(3, 6)
print(top3.join(", "))"#),
        "5, 8, 9"
    );
}

// ─── Truthiness ───────────────────────────────────────────────────

#[test]
fn truthy_values() {
    assert_eq!(say("print(if 1 { \"yes\" } else { \"no\" })"), "yes");
    assert_eq!(say(r#"print(if "x" { "yes" } else { "no" })"#), "yes");
    assert_eq!(say("print(if [1] { \"yes\" } else { \"no\" })"), "yes");
}

#[test]
fn falsy_values() {
    assert_eq!(say("print(if 0 { \"yes\" } else { \"no\" })"), "no");
    assert_eq!(say("print(if false { \"yes\" } else { \"no\" })"), "no");
    assert_eq!(say("print(if none { \"yes\" } else { \"no\" })"), "no");
    assert_eq!(say(r#"print(if "" { "yes" } else { "no" })"#), "no");
}

// ─── Number display ──────────────────────────────────────────────

#[test]
fn display_whole_number_as_int() {
    assert_eq!(say("print(5.0)"), "5");
}

#[test]
fn display_float_with_decimals() {
    assert_eq!(say("print(3.14)"), "3.14");
}

// ─── Safety / resource-limit tests ───────────────────────────────

#[test]
fn safety_infinite_loop_halts() {
    let msg = run_err_with_limits("while true { }", tight());
    assert!(msg.contains("too many steps"), "got: {msg}");
}

#[test]
fn safety_memory_bomb_string_doubling() {
    let msg = run_err_with_limits(
        r#"let s = "aaaaaaaaaa"
repeat 100 { s = s + s }"#,
        tight(),
    );
    assert!(msg.contains("Memory limit"), "got: {msg}");
}

#[test]
fn safety_memory_bomb_array_growth() {
    let (tw, vm) = run_err_loose(
        r#"let arr = []
repeat 1000 {
    arr.push("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
}"#,
        tight(),
    );
    assert_both_resource_limit(&tw, &vm);
}

#[test]
fn safety_deep_recursion_halts() {
    // The VM runs bytecode, but the differential harness
    // executes both engines — the walker's recursive
    // `call_nybl_fn` path eats real Rust stack per frame, and
    // debug-build frame bloat can blow the default ~2 MiB
    // thread stack before the engine's `MAX_CALL_DEPTH = 64`
    // cap fires. Run both engines on a fatter worker thread so
    // we observe the clean sandbox error instead of SIGABRT.
    let handle = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| run_err_with_limits("fn f() { f() }\nf()", tight()))
        .expect("spawn recursion test thread");
    let msg = handle.join().expect("recursion test thread panicked");
    assert!(
        msg.contains("nested function calls") || msg.contains("recursion"),
        "got: {msg}"
    );
}

#[test]
fn safety_runtime_value_nesting_halts_cleanly() {
    assert_both_value_depth_errors(
        r#"let a = [1]
repeat 128 { a = [a] }"#,
        2,
    );
}

#[test]
fn safety_string_repeat_bomb() {
    let msg = run_err_with_limits(r#"let s = "x" * 999999"#, tight());
    assert!(msg.contains("Memory limit"), "got: {msg}");
}

#[test]
fn safety_string_concat_bomb() {
    let msg = run_err_with_limits(
        r#"let s = "x" * 1000
repeat 100 { s = s + s }"#,
        tight(),
    );
    assert!(msg.contains("Memory limit"), "got: {msg}");
}

#[test]
fn safety_array_concat_bomb() {
    let (tw, vm) = run_err_loose(
        r#"let a = range(100)
repeat 50 { a = a + a }"#,
        tight(),
    );
    assert_both_resource_limit(&tw, &vm);
}

#[test]
fn safety_for_in_large_string() {
    let (tw, vm) = run_err_loose(
        r#"let s = "x" * 10000
for c in s { }"#,
        tight(),
    );
    assert_both_resource_limit(&tw, &vm);
}

#[test]
fn safety_demo_limits_step_bound() {
    let msg = run_err_with_limits("let i = 0\nwhile true { i = i + 1 }", NyblLimits::demo());
    assert!(msg.contains("too many steps"), "got: {msg}");
}

#[test]
fn safety_demo_limits_memory_bound() {
    let msg = run_err_with_limits(
        r#"let s = "x" * 1100000
print(s)"#,
        NyblLimits::demo(),
    );
    assert!(msg.contains("Memory limit"), "got: {msg}");
}

#[test]
fn safety_nested_loop_step_bound() {
    let msg = run_err_with_limits("repeat 100 { repeat 100 { let x = 1 } }", tight());
    assert!(msg.contains("too many steps"), "got: {msg}");
}

#[test]
fn safety_range_memory_preflight() {
    let code = "let values = range(10000)";
    for engine in ["tree-walker", "bytecode vm"] {
        let mut host = RecordHost::new();
        let result = if engine == "tree-walker" {
            nybl::run(code, &mut host, &tight())
        } else {
            nybl_vm::run(code, &mut host, &tight())
        };
        assert!(host.prints.borrow().is_empty(), "{engine} printed");
        let err = result.unwrap_err();
        assert_eq!(err.message, "Memory limit exceeded", "{engine} message");
        assert_eq!(err.line, Some(1), "{engine} line");
        assert!(err.is_fatal, "{engine} returned a catchable error");
    }
}

#[test]
fn amplified_string_operations_preflight_without_partial_output() {
    let cases = [
        (
            "print",
            r#"let shared = "x" * 256
let values = [shared, shared, shared, shared, shared, shared, shared, shared]
print(values)"#,
        ),
        (
            "join",
            r#"let shared = "x" * 256
let values = [shared, shared, shared, shared, shared, shared, shared, shared]
let result = values.join("")"#,
        ),
        (
            "replace",
            r#"let shared = "x" * 256
let result = shared.replace("x", "abcdefgh")"#,
        ),
        (
            "split",
            r#"let shared = "x," * 128
let result = shared.split(",")"#,
        ),
    ];
    let limits = NyblLimits {
        max_steps: 1_000,
        max_memory: 1_200,
    };

    for (name, code) in cases {
        for engine in ["tree-walker", "bytecode vm"] {
            let mut host = RecordHost::new();
            let result = if engine == "tree-walker" {
                nybl::run(code, &mut host, &limits)
            } else {
                nybl_vm::run(code, &mut host, &limits)
            };

            assert!(
                host.prints.borrow().is_empty(),
                "{engine} exposed output while preflighting {name}"
            );
            let error = result.unwrap_err();
            assert!(error.is_fatal, "{engine} returned a catchable {name} error");
            assert_eq!(
                error.message, "Memory limit exceeded",
                "{engine} returned the wrong {name} error"
            );
        }
    }
}

// ─── NyblHost extension ────────────────────────────────────────────
//
// Custom hosts can't share state across the walker / VM calls, so
// each test builds both independently and compares afterwards.

struct CustomHost {
    prints: Vec<String>,
}

impl NyblHost for CustomHost {
    fn call(&mut self, name: &str, args: &[Value], line: u32) -> Option<Result<Value, NyblError>> {
        match name {
            "greet" => {
                if args.len() != 1 {
                    return Some(Err(NyblError {
                        line: Some(line),
                        column: None,
                        message: "greet() needs 1 argument".into(),
                        friendly_hint: None,
                        source_context: None,
                        is_fatal: false,
                        is_try_return: false,
                    }));
                }
                Some(Ok(Value::new_str(format!("Hello, {}!", args[0]))))
            }
            _ => None,
        }
    }

    fn on_print(&mut self, message: &str) {
        self.prints.push(message.to_string());
    }

    fn function_hint(&self) -> &str {
        "Available: greet(name)"
    }
}

#[test]
fn host_custom_builtin() {
    let code = r#"print(greet("world"))"#;
    let mut tw_host = CustomHost { prints: vec![] };
    nybl::run(code, &mut tw_host, &standard()).unwrap();
    let mut vm_host = CustomHost { prints: vec![] };
    nybl_vm::run(code, &mut vm_host, &standard()).unwrap();
    assert_eq!(tw_host.prints, vm_host.prints);
    assert_eq!(tw_host.prints, vec!["Hello, world!"]);
}

#[test]
fn host_function_hint() {
    let code = "unknown()";
    let mut tw_host = CustomHost { prints: vec![] };
    let tw_err = nybl::run(code, &mut tw_host, &standard()).unwrap_err();
    let mut vm_host = CustomHost { prints: vec![] };
    let vm_err = nybl_vm::run(code, &mut vm_host, &standard()).unwrap_err();
    assert_eq!(tw_err.message, vm_err.message);
    assert!(tw_err.message.contains("not found"));
}

#[derive(Default)]
struct OpaqueHost {
    prints: Vec<String>,
    method_calls: usize,
}

impl NyblHost for OpaqueHost {
    fn call(&mut self, name: &str, args: &[Value], _line: u32) -> Option<Result<Value, NyblError>> {
        if name != "make_counter" {
            return None;
        }
        let initial = match args {
            [Value::Int(value)] => *value,
            _ => return Some(Err(NyblError::runtime("make_counter() needs one int", 0))),
        };
        Some(Ok(Value::new_host("counter", Rc::new(Cell::new(initial)))))
    }

    fn call_method(
        &mut self,
        receiver: &HostValue,
        method: &str,
        args: &[Value],
        line: u32,
    ) -> Option<Result<Value, NyblError>> {
        self.method_calls += 1;
        let state = receiver
            .downcast_ref::<Rc<Cell<i64>>>()
            .expect("counter payload");
        match (method, args) {
            ("add", [Value::Int(amount)]) => {
                state.set(state.get() + amount);
                Some(Ok(Value::Int(state.get())))
            }
            ("same", [Value::Host(other)]) => Some(Ok(Value::Bool(receiver.ptr_eq(other)))),
            // Common methods must win before host dispatch reaches this arm.
            ("type", []) => Some(Ok(Value::new_str("host override".to_string()))),
            ("add", _) | ("same", _) => Some(Err(NyblError::runtime(
                "invalid host method arguments",
                line,
            ))),
            _ => None,
        }
    }

    fn on_print(&mut self, message: &str) {
        self.prints.push(message.to_string());
    }
}

#[test]
fn opaque_host_methods_match_walker_dispatch_and_common_precedence() {
    let code = r#"let counter = make_counter(3)
print(counter.type())
print(counter.add(4))
print(counter.same(counter))"#;
    let mut walker = OpaqueHost::default();
    nybl::run(code, &mut walker, &standard()).unwrap();
    let mut vm = OpaqueHost::default();
    nybl_vm::run(code, &mut vm, &standard()).unwrap();
    assert_eq!(vm.prints, walker.prints);
    assert_eq!(vm.prints, ["counter", "7", "true"]);
    assert_eq!(walker.method_calls, 2);
    assert_eq!(vm.method_calls, 2);
}

#[test]
fn opaque_host_methods_are_value_only_and_use_normal_unknown_errors() {
    let ref_source = r#"let counter = make_counter(1)
let value = 2
counter.add(ref value)"#;
    let mut walker = OpaqueHost::default();
    let walker_ref = nybl::run(ref_source, &mut walker, &standard()).unwrap_err();
    let mut vm = OpaqueHost::default();
    let vm_ref = nybl_vm::run(ref_source, &mut vm, &standard()).unwrap_err();
    assert_eq!(vm_ref.message, walker_ref.message);
    assert_eq!(walker.method_calls, 0);
    assert_eq!(vm.method_calls, 0);

    let unknown_source = "make_counter(1).missing()";
    let mut walker = OpaqueHost::default();
    let walker_unknown = nybl::run(unknown_source, &mut walker, &standard()).unwrap_err();
    let mut vm = OpaqueHost::default();
    let vm_unknown = nybl_vm::run(unknown_source, &mut vm, &standard()).unwrap_err();
    assert_eq!(vm_unknown.message, walker_unknown.message);
    assert_eq!(
        vm_unknown.message,
        "counter doesn't have a .missing() method"
    );
}

#[test]
fn opaque_host_handles_survive_persistent_instance_calls() {
    let mut host = OpaqueHost::default();
    let mut instance = NyblInstance::load(
        r#"let counter = make_counter(10)
pub fn bump() { return counter.add(1) }
pub fn same() { return counter.same(counter) }"#,
        &mut host,
        &standard(),
    )
    .unwrap();
    assert_eq!(
        instance.call("bump", &[], &mut host).unwrap().inspect(),
        "11"
    );
    assert_eq!(
        instance.call("bump", &[], &mut host).unwrap().inspect(),
        "12"
    );
    assert_eq!(
        instance.call("same", &[], &mut host).unwrap().inspect(),
        "true"
    );
}

// ─── Fuzzer ───────────────────────────────────────────────────────
//
// Random programs from a constrained grammar. Every generated
// program is run through `run_both`, which panics on any divergence.
// Seeds are deterministic so a failure is trivially reproducible:
// the panic prints the generated source.

/// Deterministic xorshift64* RNG. Kept inline — the harness doesn't
/// want a dep on `rand` just for a smoke-fuzz.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn pick(&mut self, n: u32) -> u32 {
        debug_assert!(n > 0);
        (self.next_u64() % u64::from(n)) as u32
    }
}

/// Program generator. Builds statement-level code with a bounded
/// expression tree. Grammar intentionally avoids `while`, `fn`, and
/// methods — each widens the divergence surface beyond what this
/// smoke fuzz is trying to stress-test (and `while` can't be bounded
/// for guaranteed termination without also generating a decrementing
/// counter, which the walker-vs-VM comparison doesn't care about).
struct Generator {
    rng: Rng,
    vars: Vec<String>,
}

impl Generator {
    fn new(seed: u64) -> Self {
        Self {
            rng: Rng::new(seed),
            vars: Vec::new(),
        }
    }

    fn gen_program(&mut self, stmt_count: usize) -> String {
        let mut out = String::new();
        for _ in 0..stmt_count {
            self.gen_stmt(&mut out, 0);
        }
        out
    }

    fn gen_stmt(&mut self, out: &mut String, nest: u32) {
        // Bias toward `let` and `print` so generated programs
        // actually do something observable to compare.
        let pick = self.rng.pick(8);
        if pick < 2 || (pick == 2 && self.vars.is_empty()) {
            let name = format!("v{}", self.vars.len());
            let expr = self.gen_expr(3);
            out.push_str(&format!("let {name} = {expr}\n"));
            self.vars.push(name);
        } else if pick == 2 {
            let idx = self.rng.pick(self.vars.len() as u32) as usize;
            let name = self.vars[idx].clone();
            let expr = self.gen_expr(3);
            out.push_str(&format!("{name} = {expr}\n"));
        } else if pick == 3 || pick == 4 {
            let expr = self.gen_expr(3);
            out.push_str(&format!("print({expr})\n"));
        } else if pick == 5 && nest < 2 {
            let cond = self.gen_expr(2);
            out.push_str(&format!("if {cond} {{\n"));
            self.gen_stmt(out, nest + 1);
            out.push_str("} else {\n");
            self.gen_stmt(out, nest + 1);
            out.push_str("}\n");
        } else if pick == 6 && nest < 2 {
            // `repeat n { ... }` with n clamped to 0..=8 so nested
            // loops can't blow the step budget.
            let n = self.rng.pick(9);
            out.push_str(&format!("repeat {n} {{\n"));
            self.gen_stmt(out, nest + 1);
            out.push_str("}\n");
        } else {
            // fallback: bind a fresh temp so subsequent stmts have
            // more idents to pick from.
            let expr = self.gen_expr(2);
            let name = format!("t{}", self.vars.len());
            out.push_str(&format!("let {name} = {expr}\n"));
            self.vars.push(name);
        }
    }

    fn gen_expr(&mut self, depth: u32) -> String {
        if depth == 0 || self.rng.pick(3) == 0 {
            return self.gen_leaf();
        }
        match self.rng.pick(4) {
            0 => {
                let l = self.gen_expr(depth - 1);
                let r = self.gen_expr(depth - 1);
                let op = match self.rng.pick(10) {
                    0 => "+",
                    1 => "-",
                    2 => "*",
                    3 => "%",
                    4 => "==",
                    5 => "!=",
                    6 => "<",
                    7 => ">",
                    8 => "&&",
                    _ => "||",
                };
                format!("({l} {op} {r})")
            }
            1 => {
                let e = self.gen_expr(depth - 1);
                if self.rng.pick(2) == 0 {
                    format!("(-{e})")
                } else {
                    format!("(!{e})")
                }
            }
            2 => {
                let n = self.rng.pick(4);
                let mut parts = Vec::new();
                for _ in 0..n {
                    parts.push(self.gen_expr(depth - 1));
                }
                format!("[{}]", parts.join(", "))
            }
            _ => {
                let c = self.gen_expr(depth - 1);
                let t = self.gen_expr(depth - 1);
                let e = self.gen_expr(depth - 1);
                format!("(if {c} {{ {t} }} else {{ {e} }})")
            }
        }
    }

    fn gen_leaf(&mut self) -> String {
        match self.rng.pick(7) {
            0 => format!("{}", self.rng.pick(100) as i32 - 50),
            1 => "true".into(),
            2 => "false".into(),
            3 => "none".into(),
            4 if !self.vars.is_empty() => {
                let idx = self.rng.pick(self.vars.len() as u32) as usize;
                self.vars[idx].clone()
            }
            _ => format!("\"s{}\"", self.rng.pick(10)),
        }
    }
}

#[test]
fn fuzz_smoke_diff() {
    // Fixed seed list. If this flakes, it's a divergence bug, not
    // a fuzz bug — repro by using the printed seed and stmt count.
    for seed in 0..100u64 {
        let mut g = Generator::new(seed.wrapping_add(1));
        let stmt_count = 6 + (seed as usize % 8);
        let program = g.gen_program(stmt_count);
        // `run_both` asserts walker == vm internally. On a diff it
        // panics with the full program printed.
        let _ = run_both(&program, &standard());
    }
}

#[test]
#[ignore] // opt-in: `cargo test --test differential -- --ignored fuzz_extended_diff`
fn fuzz_extended_diff() {
    let seed_base: u64 = std::env::var("NYBL_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xB0B_C0DE_u64);
    for i in 0..10_000u64 {
        let mut g = Generator::new(seed_base.wrapping_add(i).wrapping_add(1));
        let stmt_count = 8 + ((i as usize) % 16);
        let program = g.gen_program(stmt_count);
        let _ = run_both(&program, &standard());
    }
}
