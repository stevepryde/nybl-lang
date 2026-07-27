//! Three-way differential tests: tree-walker vs bytecode VM vs AOT.
//!
//! For a curated corpus of programs, every engine must produce
//! identical prints and agree on success/error. This is the final
//! piece of step 3 — once this harness is green, the transpiled
//! path is proven to match the reference semantics on the same
//! programs the walker and VM already agree on.
//!
//! # Why it's `#[ignore]`'d
//!
//! The AOT leg spins up a single release-mode `cargo run` per test
//! invocation. Even with a batched driver, compiling every corpus
//! program in both native and sandboxed output shapes takes minutes
//! on a typical development machine, which is too slow for every
//! `cargo test` pass. Run it with
//!
//! ```text
//! cargo test -p nybl-compile --test three_way -- --ignored
//! ```
//!
//! when verifying AOT or before a release.
//!
//! # Batching
//!
//! Each program is transpiled into native and sandboxed `pub mod`
//! variants via `Options::module_name`. All modules plus a small
//! driver `fn main()` are concatenated into one `src/main.rs`. The
//! driver runs each program sequentially in both modes, captures its
//! prints into a buffered host, and emits a delimited envelope on
//! stdout. The harness parses that envelope back into
//! `(Vec<String>, Result)` outcomes and compares them against the
//! walker / VM.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use nybl::{NyblError, NyblHost, NyblLimits, Value};
use nybl_compile::{Options, transpile};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AotMode {
    Native,
    Sandbox,
}

impl AotMode {
    const ALL: [Self; 2] = [Self::Native, Self::Sandbox];

    fn sandbox(self) -> bool {
        matches!(self, Self::Sandbox)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Sandbox => "sandbox",
        }
    }
}

// ─── Shared test host ─────────────────────────────────────────────

struct BufHost {
    prints: RefCell<Vec<String>>,
    modules: std::collections::HashMap<String, String>,
    case_name: &'static str,
}

impl BufHost {
    fn new(case_name: &'static str, modules: &[(&str, &str)]) -> Self {
        Self {
            prints: RefCell::new(Vec::new()),
            modules: modules
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            case_name,
        }
    }
}

impl NyblHost for BufHost {
    fn call(
        &mut self,
        name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        match (self.case_name, name) {
            ("named_function_host_precedence", "root_fn") => Some(Ok(Value::Int(99))),
            ("named_function_host_precedence", "module_fn") => Some(Ok(Value::Int(98))),
            ("active_value_site_preserves_host_precedence_with_dead_ref_site", "f") => {
                Some(Ok(Value::Int(99)))
            }
            _ => None,
        }
    }

    fn on_print(&mut self, message: &str) {
        self.prints.borrow_mut().push(message.to_string());
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        if let Some(src) = self.modules.get(name) {
            return Some(Ok(src.clone()));
        }
        nybl::stdlib::resolve(name).map(|s| Ok(s.to_string()))
    }
}

/// Normalised per-engine outcome the harness compares across
/// walker / VM / AOT.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    prints: Vec<String>,
    error: Option<String>,
}

fn walker_outcome(case_name: &'static str, code: &str, modules: &[(&str, &str)]) -> Outcome {
    let mut host = BufHost::new(case_name, modules);
    let result = nybl::run(code, &mut host, &NyblLimits::standard());
    Outcome {
        prints: host.prints.borrow().clone(),
        error: result.err().map(|e| e.message),
    }
}

fn vm_outcome(case_name: &'static str, code: &str, modules: &[(&str, &str)]) -> Outcome {
    let mut host = BufHost::new(case_name, modules);
    let result = nybl_vm::run(code, &mut host, &NyblLimits::standard());
    Outcome {
        prints: host.prints.borrow().clone(),
        error: result.err().map(|e| e.message),
    }
}

// ─── AOT batched runner ───────────────────────────────────────────
//
// The expensive bit: transpile every corpus program, stitch them
// into one `src/main.rs` wrapped under per-program modules, write a
// scratch cargo project, `cargo run`, parse the delimited output
// back into `Outcome`s.

fn workspace_root() -> PathBuf {
    let crate_dir: PathBuf = env!("CARGO_MANIFEST_DIR").into();
    crate_dir.parent().unwrap().to_path_buf()
}

fn scratch_dir(name: &str) -> PathBuf {
    let mut p = workspace_root();
    p.push("target");
    p.push("nybl-compile-three-way");
    p.push(name);
    p
}

/// One test in the three-way corpus. `modules` is optional — empty
/// slice means the program has no imports.
struct CorpusEntry {
    name: &'static str,
    source: &'static str,
    modules: &'static [(&'static str, &'static str)],
}

/// Construct an AOT `ModuleResolver` that looks up corpus-local
/// overrides first, then falls back to `nybl::stdlib::resolve` so
/// `use std.*` works without every test having to redeclare
/// the stdlib. Entries with no imports at all still receive a
/// resolver — it's never called for them, so the extra
/// allocation is cheap.
fn build_resolver(overrides: &[(&str, &str)]) -> Option<nybl_compile::ModuleResolver> {
    let map: std::collections::HashMap<String, String> = overrides
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    Some(std::rc::Rc::new(std::cell::RefCell::new(
        move |name: &str| {
            if let Some(src) = map.get(name) {
                return Some(Ok(src.clone()));
            }
            nybl::stdlib::resolve(name).map(|s| Ok(s.to_string()))
        },
    )))
}

/// Build the single-file AOT driver that runs every program in the
/// corpus and emits per-program envelopes on stdout.
fn build_driver(programs: &[CorpusEntry]) -> String {
    let mut out = String::new();
    out.push_str(DRIVER_HEADER);

    // One pub mod per program and AOT mode. The native mode is the
    // shape emitted by `nybl compile`; keeping it in this differential
    // harness ensures fixes cannot accidentally cover only the
    // persistent sandbox runtime. The curated corpus excludes
    // runaway programs, so executing its native variants is safe.
    for mode in AotMode::ALL {
        for entry in programs {
            // Resolver: entry-local modules first, then nybl-std
            // stdlib as a fallback. Each transpilation gets its own
            // resolver because module resolution is stateful.
            let resolver = build_resolver(entry.modules);
            let mod_src = transpile(
                entry.source,
                &Options {
                    emit_main: false,
                    use_nybl_sys: false,
                    sandbox: mode.sandbox(),
                    module_name: Some(format!("p_{}_{}", mode.label(), entry.name)),
                    module_resolver: resolver,
                },
            )
            .unwrap_or_else(|e| {
                panic!(
                    "transpile failed for {} ({}): {}",
                    entry.name,
                    mode.label(),
                    e.message
                )
            });
            out.push_str(&mod_src);
            out.push('\n');
        }
    }

    // Driver: iterate through modes and programs, capture prints,
    // and emit an envelope for each pair. We inline the calls rather
    // than build a Vec of trait objects — each generated `run` is
    // generic over H and can't be trivially type-erased.
    out.push_str("fn main() {\n");
    out.push_str("    let mut out = ::std::string::String::new();\n");
    for mode in AotMode::ALL {
        for entry in programs {
            let mode_label = mode.label();
            let name = entry.name;
            let run = if mode.sandbox() {
                format!("p_{mode_label}_{name}::run(&mut host, &limits)")
            } else {
                format!("p_{mode_label}_{name}::run(&mut host)")
            };
            writeln!(
                out,
                concat!(
                    "    {{\n",
                    // Driver-side BufHost, defined in DRIVER_HEADER —
                    // distinct from the harness-side one, no modules
                    // map (AOT resolves at compile time).
                    "        let mut host = BufHost::new({name:?});\n",
                    "        let limits = ::nybl::NyblLimits::standard();\n",
                    "        let result = {run};\n",
                    "        out.push_str(\"<<TEST>>{mode_label}/{name}\\n\");\n",
                    "        for p in &host.prints {{\n",
                    "            out.push_str(\"<<PRINT>>\");\n",
                    "            out.push_str(p);\n",
                    "            out.push_str(\"<<END>>\\n\");\n",
                    "        }}\n",
                    "        match result {{\n",
                    "            Ok(()) => out.push_str(\"<<OK>>\\n\"),\n",
                    "            Err(e) => {{\n",
                    "                out.push_str(\"<<ERR>>\");\n",
                    "                out.push_str(&e.message);\n",
                    "                out.push_str(\"<<END>>\\n\");\n",
                    "            }}\n",
                    "        }}\n",
                    "    }}\n",
                ),
                run = run,
                mode_label = mode_label,
                name = name
            )
            .unwrap();
        }
    }
    out.push_str("    ::std::print!(\"{}\", out);\n");
    out.push_str("}\n");
    out
}

const DRIVER_HEADER: &str = r#"// Auto-generated by three_way.rs for the AOT leg of the
// tree-walker / VM / AOT differential harness.
#![allow(dead_code, unused_imports, unused_variables, clippy::all)]

pub struct BufHost {
    pub prints: ::std::vec::Vec<String>,
    case_name: &'static str,
}

impl BufHost {
    pub fn new(case_name: &'static str) -> Self {
        Self { prints: ::std::vec::Vec::new(), case_name }
    }
}

impl ::nybl::NyblHost for BufHost {
    fn call(
        &mut self,
        name: &str,
        _args: &[::nybl::value::Value],
        _line: u32,
    ) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> {
        match (self.case_name, name) {
            ("named_function_host_precedence", "root_fn") => {
                Some(Ok(::nybl::value::Value::Int(99)))
            }
            ("named_function_host_precedence", "module_fn") => {
                Some(Ok(::nybl::value::Value::Int(98)))
            }
            (
                "active_value_site_preserves_host_precedence_with_dead_ref_site",
                "f",
            ) => Some(Ok(::nybl::value::Value::Int(99))),
            _ => None,
        }
    }

    fn on_print(&mut self, message: &str) {
        self.prints.push(message.to_string());
    }
}

"#;

fn write_driver_project(driver_src: &str) -> PathBuf {
    let root = workspace_root();
    let dir = scratch_dir("driver");
    let src_dir = dir.join("src");
    std::fs::create_dir_all(&src_dir).expect("create scratch src dir");

    let nybl_path = root.join("nybl");
    let manifest = format!(
        r#"[package]
name = "nybl-three-way-driver"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
nybl = {{ path = "{nybl}", package = "nybl-lang" }}

[[bin]]
name = "driver"
path = "src/main.rs"

[workspace]
"#,
        nybl = nybl_path.display()
    );
    std::fs::write(dir.join("Cargo.toml"), manifest).expect("write Cargo.toml");
    std::fs::write(src_dir.join("main.rs"), driver_src).expect("write main.rs");
    dir
}

fn run_aot_batch(programs: &[CorpusEntry]) -> Vec<(String, Outcome)> {
    let driver_src = build_driver(programs);
    let dir = write_driver_project(&driver_src);
    let output = Command::new("cargo")
        .arg("run")
        .arg("--quiet")
        .arg("--release")
        // Cargo gives this precedence over RUSTFLAGS. Remove an
        // inherited value so the warning-denial contract below
        // cannot be bypassed (or poisoned) by the parent shell.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env("RUSTFLAGS", "-D warnings")
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run cargo");
    if !output.status.success() {
        panic!(
            "cargo run failed in {}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- generated ---\n{}",
            dir.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            &driver_src,
        );
    }
    parse_envelope(&String::from_utf8_lossy(&output.stdout))
}

/// Split the driver's stdout into per-program outcomes. Uses the
/// sentinel markers emitted by `build_driver`: each program starts
/// with `<<TEST>>name\n`, prints are framed `<<PRINT>>msg<<END>>`,
/// and the program ends with either `<<OK>>` or
/// `<<ERR>>msg<<END>>`.
fn parse_envelope(stdout: &str) -> Vec<(String, Outcome)> {
    let mut out = Vec::new();
    let mut lines = stdout.lines().peekable();
    while let Some(line) = lines.next() {
        let name = match line.strip_prefix("<<TEST>>") {
            Some(n) => n.to_string(),
            None => continue,
        };
        let mut prints = Vec::new();
        let mut error = None;
        for next in lines.by_ref() {
            if let Some(p) = next.strip_prefix("<<PRINT>>") {
                let p = p.strip_suffix("<<END>>").unwrap_or(p);
                prints.push(p.to_string());
            } else if next == "<<OK>>" {
                break;
            } else if let Some(err) = next.strip_prefix("<<ERR>>") {
                let err = err.strip_suffix("<<END>>").unwrap_or(err);
                error = Some(err.to_string());
                break;
            }
            // Anything else is treated as garbage and skipped.
        }
        out.push((name, Outcome { prints, error }));
    }
    out
}

// ─── The corpus ───────────────────────────────────────────────────
//
// A focused set of programs spanning every feature the AOT supports.
// Keep additions intentional: every entry is emitted twice, and generated
// Rust release compilation dominates the ignored harness's runtime.
//
// Safety / tight-limit tests are deliberately *not* included here:
// the walker, VM, and AOT sandbox each measure "steps" differently
// (per-statement vs per-bytecode vs per-tick-point), so they reach
// resource limits at slightly different points even though all
// three halt. That divergence is already documented in the 2c
// harness's `assert_both_resource_limit`, and extending it to three
// engines would muddy the strict equality guarantee this harness
// offers on the happy path.

const CORPUS: &[(&str, &str)] = &[
    (
        "ref_commit_forward_returned_err_and_runtime_rollback",
        r#"fn inner(ref value) { value += 2 }
fn outer(ref value) { inner(ref value); value *= 3 }
let value = 1
outer(ref value)
print(value)
fn returned_err(ref target) {
    target = 20
    return Result::Err(RuntimeError { message: "ordinary value", line: 8 })
}
print(returned_err(ref value), value)
fn panics(ref target) { target = 99; panic("rollback") }
fn attempt() { panics(ref value) }
print(try_call(attempt), value)"#,
    ),
    (
        "ref_preflight_order_and_target_fences",
        r#"fn take(ref first, ref second, ordinary) {
    first = ordinary
    second = ordinary + 1
}
fn callee() { print("callee"); return take }
fn side() { print("arg"); return 4 }
let other = 0
fn invalid() { callee()(ref [1], ref other, side()) }
print(try_call(invalid))
let first = 1
let second = 2
fn duplicate() { callee()(ref first, ref first, side()) }
print(try_call(duplicate))
fn missing() { callee()(first, ref second, side()) }
print(try_call(missing))
fn capture_case() {
    let captured = 3
    let action = fn() { take(ref captured, ref second, side()) }
    return try_call(action)
}
print(capture_case())
const FIXED = 5
fn constant_case() { take(ref FIXED, ref second, side()) }
print(try_call(constant_case))"#,
    ),
    (
        "mutating_method_receiver_preflight_and_snapshot_order",
        r#"let values = [1]
fn side() { print("arg"); return 2 }
values.push(side())
print(values)
fn bad_arity() { values.push(side(), 3) }
print(try_call(bad_arity))
fn nested_place() { [values][0].push(side()) }
print(try_call(nested_place))
print([10].push(side()))"#,
    ),
    (
        "method_preflight_retains_exact_adapter_across_argument_redeclaration",
        r#"struct Box { value }
fn Box.apply(self, ref output, trigger) { output = self.value }
fn Box.read(self, trigger) { return self.value }
fn replace_apply() {
    fn Box.apply(self, ref output, trigger) { output = 99 }
    return 0
}
fn replace_read() {
    fn Box.read(self, trigger) { return 99 }
    return 0
}
let box = Box { value: 7 }
let output = 0
print(box.read(replace_read()))
box.apply(ref output, replace_apply())
print(output)
"#,
    ),
    (
        "user_ref_receiver_commit_order_rollback_and_explicit_targets",
        r#"struct Counter { value }
fn Counter.add(ref self, amount) {
    self.value += amount
    return self.value
}
fn Counter.add_from(ref self, ref other) {
    self.value += other
    other += 1
    return self.value
}
fn Counter.fail(ref self) {
    self.value = 99
    panic("rollback")
}
enum Switch { Off, On }
fn Switch.turn_on(ref self) {
    self = Switch::On
}
let counter = Counter { value: 1 }
fn side() {
    counter.value = 10
    return 2
}
print(counter.add(side()), counter.value)
let other = 3
print(counter.add_from(ref other), counter.value, other)
fn attempt() { counter.fail() }
print(try_call(attempt), counter.value)
let switch = Switch::Off
switch.turn_on()
print(switch)"#,
    ),
    (
        "captured_implicit_ref_receiver_fences_before_arguments",
        r#"fn side() { print("arg"); return 1 }
fn run() {
    let values = []
    let action = fn() { values.push(side()) }
    return try_call(action)
}
print(run())"#,
    ),
    (
        "branch_local_enum_site_then",
        r#"if true {
    enum Choice { Left }
    print(match Choice::Left { Choice::Left => "left" })
} else {
    enum Choice { Right }
    print(match Choice::Right { Choice::Right => "right" })
}"#,
    ),
    (
        "branch_local_enum_site_else",
        r#"if false {
    enum Choice { Wrong, Extra }
} else if true {
    enum Choice { Right }
    print(match Choice::Right { Choice::Right => "right" })
}"#,
    ),
    (
        "asi_multiline_delimiters",
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
    ),
    (
        "asi_nested_lambda_braces",
        r#"let functions = [
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
print(functions[0]() + wrapped())"#,
    ),
    (
        "const_array_mutation_error",
        r#"fn mutate() {
    const VALUES = [3, 1, 2]
    VALUES.sort()
}
mutate()"#,
    ),
    (
        "const_user_mutator_name_remains_value_method",
        r#"struct Accumulator { total }
fn Accumulator.push(self, value) { return self.total + value }
fn Accumulator.pop(self) { return self.total }
const ACCUMULATOR = Accumulator { total: 7 }
print(ACCUMULATOR.push(5), ACCUMULATOR.pop())"#,
    ),
    (
        "const_non_array_mutator_name_keeps_method_error",
        r#"const LOOKUP = {"n": 1}
LOOKUP.remove("n")"#,
    ),
    (
        "mutable_array_mutators_still_write",
        r#"let values = [3, 1, 2]
values.sort()
values.reverse()
values.insert(1, 4)
values.remove(0)
values.push(5)
values.pop()
print(values)"#,
    ),
    (
        "asi_return_newline",
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
    ),
    ("arithmetic", "print(1 + 2 * 3 - 4)"),
    ("divide_float", "print(7 / 2)"),
    ("modulo", "print(10 % 3)"),
    ("unary_neg", "print(-5)"),
    ("unary_not", "print(!true)"),
    ("string_concat", r#"print("hello" + " " + "world")"#),
    ("string_repeat", r#"print("ab" * 3)"#),
    ("string_auto_coerce", r#"print("val=" + 42)"#),
    (
        "string_interpolation",
        r#"let name = "nybl"
let version = 2
print("hi {name} v{version}!")"#,
    ),
    (
        "aot_identifier_hygiene",
        r#"fn subtract(a, b) { return a - b }
fn yield(crate, super, ctx) { return crate + super + ctx }
struct Holder { n }
fn Holder.read(self) {
    let nybl_self = 40
    return self.n + nybl_self
}
let __t0 = 1
let __t1 = 2
let __l = 4
let ctx = 3
let crate = 5
let super = 6
let x = 10
let __nybl_user_value_78 = 20
let holder = Holder { n: 2 }
print(subtract(__t1, __t0))
print(1 + __l)
print(yield(crate, super, ctx))
print(x, __nybl_user_value_78)
print(holder.read())"#,
    ),
    (
        "string_interpolation_function_locals",
        r#"fn greet(name) {
    let punctuation = "!"
    return "hi {name}{punctuation}"
}
print(greet("nybl"))"#,
    ),
    (
        "string_interpolation_nested_closure_captures",
        r#"fn build(prefix) {
    let local = "local"
    return fn(suffix) {
        return fn() { return "{prefix}:{local}:{suffix}" }
    }
}
print(build("start")("end")())"#,
    ),
    ("equality", "print(1 == 1)\nprint(1 == 2)\nprint(1 != 2)"),
    (
        "ordering",
        "print(3 < 5)\nprint(5 <= 5)\nprint(6 > 5)\nprint(5 >= 6)",
    ),
    (
        "logical",
        // Note: the tree-walker accepts `false && x` / `true || x`
        // with an unbound `x` thanks to short-circuiting — the
        // right side is never evaluated. The AOT path compiles to
        // Rust, which resolves every identifier at compile time
        // regardless of dynamic reachability, so that construct is
        // a legitimate AOT divergence and is intentionally omitted
        // from the three-way corpus.
        "print(true && false)\nprint(true || false)\nprint(false && true)\nprint(true || false)",
    ),
    ("let_and_assign", "let x = 1\nx = 5\nprint(x)"),
    (
        "compound_assign",
        r#"let x = 10
x += 5
x -= 3
x *= 2
x /= 4
x %= 3
print(x)"#,
    ),
    (
        "named_container_assignment_order",
        r#"let values = [1, 2]
values[0] += values.remove(0)
print(values)
let dict = {"n": 4}
dict["n"] += 6
dict["extra"] = 8
print(dict)
struct Counter { n }
let counter = Counter { n: 3 }
counter.n *= 4
print(counter.n)"#,
    ),
    (
        "if_else_if",
        r#"let x = 2
if x == 1 { print("one") } else if x == 2 { print("two") } else { print("other") }"#,
    ),
    (
        "if_expression",
        "let x = if true { 1 } else { 2 }\nprint(x)",
    ),
    (
        "if_expression_multiline_layout",
        r#"let first = if true {
    // Comments and blank lines remain layout.

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
    ),
    (
        "struct_literals_in_condition_delimiters",
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
    ),
    (
        "type_bindings_direct_source_order",
        r#"fn direct() { return Point { value: 1 } }
fn direct_enum() { return Signal::Idle }
fn direct_pattern() {
    return match (Point { value: 5 }) { Point { value } => value, _ => 0 }
}
struct Runner { marker }
fn Runner.build(self) { return Point { value: self.marker } }
let runner = Runner { marker: 7 }
let delayed = fn() { return Point { value: 6 } }
print(try_call(direct).is_err(), try_call(direct_enum).is_err(), try_call(delayed).is_err())
print(try_call(fn() { return runner.build() }).is_err())
struct Point { value }
enum Signal { Idle, Pair(left, right), Named { value } }
print(direct().value, direct_pattern(), delayed().value, runner.build().value)
print(match direct_enum() { Signal::Idle => "idle", _ => "bad" })
print(match Signal::Pair(7, 8) { Signal::Pair(left, right) => left + right, _ => 0 })
print(match (Signal::Named { value: 9 }) { Signal::Named { value } => value, _ => 0 })"#,
    ),
    (
        "nested_type_declarations_execute_and_do_not_leak",
        r#"if true {
    struct Branch { value }
    enum Flag { On, Pair(left, right), Named { value } }
    print(Branch { value: 3 }.value)
    print(match Flag::Pair(4, 5) { Flag::Pair(left, right) => left + right, _ => 0 })
}
if false {
    struct DeadBad { value, value }
    enum DeadWorse { Pair(value, value) }
}
print(try_call(fn() { return Branch { value: 9 } }).is_err())
print(try_call(fn() { return Flag::On }).is_err())"#,
    ),
    (
        "callable_and_lambda_type_declarations_are_runtime_sites",
        r#"fn build(value) {
    struct Local { value }
    enum Wrapped { Value(item) }
    let wrapped = Wrapped::Value(Local { value: value })
    return match wrapped { Wrapped::Value(item) => item, _ => none }
}
let make = fn(value) {
    struct LambdaLocal { value }
    enum LambdaWrapped { Value(item) }
    return match LambdaWrapped::Value(LambdaLocal { value: value }) {
        LambdaWrapped::Value(item) => item,
        _ => none,
    }
}
print(build(2).value, build(3).value)
print(make(4).value, make(5).value)
print(try_call(fn() { return Local { value: 0 } }).is_err())
print(try_call(fn() { return LambdaWrapped::Value(0) }).is_err())"#,
    ),
    (
        "conditional_type_alternatives_register_only_executed_site",
        r#"let choose_left = true
if choose_left {
    struct Choice { left }
    enum Signal { Left(value) }
    print(Choice { left: 7 }.left)
    print(match Signal::Left(8) { Signal::Left(value) => value, _ => 0 })
} else {
    struct Choice { right }
    enum Signal { Right { value } }
    print(Choice { right: 9 }.right)
}"#,
    ),
    (
        "conditional_type_alternative_other_shape",
        r#"if false {
    struct Choice { left }
    enum Signal { Left(value) }
} else {
    struct Choice { right }
    enum Signal { Right { value } }
    print(Choice { right: 9 }.right)
    print(match (Signal::Right { value: 10 }) { Signal::Right { value } => value, _ => 0 })
}"#,
    ),
    (
        "loop_nested_type_declaration_sites",
        r#"let once = true
while once {
    struct WhileType { value }
    print(WhileType { value: 1 }.value)
    once = false
}
repeat 2 {
    struct RepeatType { value }
    enum RepeatSignal { Value(item) }
    print(match RepeatSignal::Value(RepeatType { value: 2 }) {
        RepeatSignal::Value(item) => item.value,
        _ => 0,
    })
}
for value in [3, 4] {
    struct ForType { value }
    print(ForType { value: value }.value)
}"#,
    ),
    (
        "struct_and_enum_names_use_separate_runtime_registries",
        r#"struct Dual { value }
enum Dual { Value(item) }
print(Dual { value: 5 }.value)
print(match Dual::Value(6) { Dual::Value(item) => item, _ => 0 })"#,
    ),
    (
        "failed_call_preserves_definition_but_unwinds_binding",
        r#"fn fail() {
    struct Persist { value }
    let boom = 1 / 0
    return none
}
print(try_call(fail).is_err())
print(try_call(fn() { return Persist { value: 1 } }).is_err())
fn recover() {
    struct Persist { value }
    return Persist { value: 2 }
}
print(recover().value)"#,
    ),
    (
        "block_type_definition_persists_without_lexical_binding_leak",
        r#"if true { struct Hidden { value } }
fn build() { return Hidden { value: 1 } }
print(try_call(build).is_err())"#,
    ),
    (
        "executed_conflicting_type_sites_error_at_second_execution",
        r#"let turn = 0
repeat 2 {
    if turn == 0 { struct Flip { left } }
    else { struct Flip { right } }
    turn += 1
}"#,
    ),
    (
        "enum_tuple_names_are_validated_but_not_identity",
        r#"enum Item { Value(first) }
enum Item { Value(second) }
print(match Item::Value(6) { Item::Value(value) => value, _ => 0 })"#,
    ),
    (
        "executed_duplicate_tuple_field_is_rejected",
        "enum Invalid { Pair(value, value) }",
    ),
    (
        "enum_variant_order_is_runtime_shape",
        r#"enum Ordered { First, Second }
enum Ordered { Second, First }"#,
    ),
    ("while_loop", "let i = 0\nwhile i < 5 { i += 1 }\nprint(i)"),
    (
        "while_break",
        "let i = 0\nwhile true { i += 1\nif i == 3 { break } }\nprint(i)",
    ),
    (
        "while_continue",
        r#"let sum = 0
let i = 0
while i < 10 {
    i += 1
    if i % 2 == 0 { continue }
    sum += i
}
print(sum)"#,
    ),
    (
        "for_over_array",
        r#"let sum = 0
for x in [10, 20, 30] { sum += x }
print(sum)"#,
    ),
    (
        "for_over_range",
        "let sum = 0\nfor i in range(5) { sum += i }\nprint(sum)",
    ),
    (
        "for_over_string",
        r#"let out = ""
for ch in "abc" { out += ch + "-" }
print(out)"#,
    ),
    ("repeat_loop", "let n = 0\nrepeat 4 { n += 1 }\nprint(n)"),
    ("repeat_zero", "let n = 99\nrepeat 0 { n = 0 }\nprint(n)"),
    (
        "fn_basic",
        "fn double(x) { return x * 2 }\nprint(double(5))",
    ),
    (
        "fn_recursion",
        r#"fn fib(n) {
    if n <= 1 { return n }
    return fib(n - 1) + fib(n - 2)
}
print(fib(10))"#,
    ),
    (
        "nested_fn_calls",
        r#"fn square(n) { return n * n }
fn sum_squares(a, b) { return square(a) + square(b) }
print(sum_squares(3, 4))"#,
    ),
    (
        "array_literal_index",
        "let a = [10, 20, 30]\nprint(a[1])\nprint(a[-1])",
    ),
    (
        "array_mutation",
        r#"let a = [1, 2]
a.push(3)
a.push(4)
print(a.len())
print(a)
let last = a.pop()
print(last)
print(a)"#,
    ),
    (
        "array_mutation_value_semantics",
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
    ),
    (
        "array_large_append_loop",
        r#"let values = []
let next = 0
repeat 2048 {
    values.push(next)
    next += 1
}
print(values.len())
print(values[0])
print(values[-1])"#,
    ),
    (
        "array_mutation_methods_and_returns",
        r#"let values = [4, 1, 3]
print(values.push(2))
print(values.insert(1, 5))
print(values.remove(2))
print(values.pop())
values.sort()
values.reverse()
print(values)"#,
    ),
    (
        "array_mutation_errors_are_atomic",
        r#"let values = [1, 2, 3]
print(try_call(fn() { return values.push() }).is_err())
print(try_call(fn() { return values.insert(99, 4) }).is_err())
print(try_call(fn() { return values.remove(99) }).is_err())
print(values)"#,
    ),
    (
        "dynamic_struct_method_named_push",
        r#"struct Accumulator { total }
fn Accumulator.push(self, value) { return self.total + value }
let accumulator = Accumulator { total: 7 }
print(accumulator.push(5))"#,
    ),
    (
        "array_sort_reverse",
        r#"let a = [3, 1, 2]
a.sort()
print(a)
a.reverse()
print(a)"#,
    ),
    (
        "array_index_assign",
        r#"let a = [1, 2, 3]
a[0] = 99
a[1] += 10
a[-1] *= 2
print(a)"#,
    ),
    (
        "dict_basics",
        r#"let d = {"name": "nybl", "hp": 100}
print(d["name"])
d["hp"] = 50
d["mp"] = 20
print(d["hp"])
print(d.keys())"#,
    ),
    (
        "string_methods",
        r#"print("Hello".upper())
print("HI".lower())
print("  trim  ".trim())
print("hello".len())
print("a,b,c".split(","))
print(["x", "y", "z"].join("-"))"#,
    ),
    (
        "fizzbuzz",
        r#"let result = []
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
print(result.join(", "))"#,
    ),
    (
        "builtin_str_int_type",
        "print(42.to_str())\nprint(3.7.to_int())\nprint([].type())",
    ),
    (
        "builtin_abs_min_max",
        "print((-5).abs())\nprint(3.min(7))\nprint(3.max(7))",
    ),
    (
        "builtin_range",
        "print(range(5))\nprint(range(2, 5))\nprint(range(0, 10, 3))",
    ),
    (
        "range_limit_boundary",
        "let values = range(10000)\nprint(values.len())\nprint(values[9999])",
    ),
    (
        "range_limit_error",
        r#"let result = try_call(fn() {
    return range(10001)
})
print("unreachable")"#,
    ),
    (
        "builtin_len_inspect",
        r#"print("hello".len())
print([1, 2, 3].len())
print("hi".inspect())"#,
    ),
    (
        "signed_index_methods",
        r#"let values = [10, 20, 30, 40]
print(values.remove(-1))
print(values.insert(-1, 25))
print(values)
print(values.slice(-3, -1))
print("a🙂é界"[-1])
print("a🙂é界".slice(-3, -1))
print(values[1.9])"#,
    ),
    (
        "signed_index_failures_are_nonfatal",
        r#"let values = [10, 20, 30]
print(try_call(fn() { return values.remove(-4) }).is_err())
print(try_call(fn() { return values.insert(-4, 0) }).is_err())
print(try_call(fn() { values[-4] = 0 }).is_err())
print(values)"#,
    ),
    (
        "nested_array_mutation_is_catchable",
        r#"struct Holder { items }
let indexed = {"items": [1]}
let fielded = Holder { items: [1, 2] }
let index_result = try_call(fn() {
    indexed["items"].push(2)
})
let field_result = try_call(fn() {
    fielded.items.pop()
})
print(match index_result { Result::Err(e) => e.message, _ => "missing" })
print(match index_result { Result::Err(e) => e.line, _ => -1 })
print(match field_result { Result::Err(e) => e.message, _ => "missing" })
print(match field_result { Result::Err(e) => e.line, _ => -1 })"#,
    ),
    (
        "nested_array_mutation_index_error",
        r#"let indexed = {"items": [1]}
indexed["items"].push(2)"#,
    ),
    (
        "nested_array_mutation_field_error",
        r#"struct Holder { items }
let fielded = Holder { items: [1, 2] }
fielded.items.pop()"#,
    ),
    (
        "temporary_array_mutation_and_dynamic_method_fallback",
        r#"fn make_array() { return [7] }
print([1].push(2))
print(make_array().pop())
struct Gadget { n }
fn Gadget.push(self, amount) { return self.n + amount }
struct Wrapper { item }
let wrapper = Wrapper { item: Gadget { n: 10 } }
let dynamic = {"item": Gadget { n: 20 }}
print(wrapper.item.push(2))
print(dynamic["item"].push(3))"#,
    ),
    (
        "signed_index_i64_extremes",
        r#"let min = -9223372036854775807 - 1
let max = 9223372036854775807
let values = [1, 2]
print(values.slice(min, max))
print(values.slice(max, min))
print(try_call(fn() { return values[min] }).is_err())
print(try_call(fn() { return values.remove(min) }).is_err())
print(try_call(fn() { return values.insert(max, 3) }).is_err())
print(values)"#,
    ),
    ("signed_index_bounds_error", "[1, 2].remove(-3)"),
    ("signed_index_type_error", r#"[1].remove("0")"#),
    (
        "nested_array_access",
        "let m = [[1, 2], [3, 4]]\nprint(m[1][0])",
    ),
    ("method_chain", r#"print("  HELLO  ".trim().lower())"#),
    (
        "truthiness",
        r#"print(if 0 { "t" } else { "f" })
print(if "" { "t" } else { "f" })
print(if [1] { "t" } else { "f" })
print(if [] { "t" } else { "f" })"#,
    ),
    (
        "number_display",
        "print(5.0)\nprint(3.14)\nprint(0.1 + 0.2)",
    ),
    ("error_division_by_zero", "print(1 / 0)"),
    ("error_type_mismatch", r#"print("a" - 1)"#),
    ("error_unknown_fn", "nope()"),
    (
        // `panic` is a builtin; in walker / VM / AOT it has to
        // produce the same `Err` payload when caught by
        // `try_call`, matching down to the error message.
        "builtin_panic_via_try_call",
        r#"let r = try_call(fn() { panic("nope") })
print(match r {
    Result::Ok(_)  => "ok?",
    Result::Err(e) => e.message,
})"#,
    ),
    // NOTE: `print(nope)` — an undefined identifier — is *not*
    // included: the walker raises "Variable `nope` not found" at
    // runtime, but the AOT emits `nope.clone()` which rustc
    // rejects at compile time with a different message. Both halt
    // with a useful error; the three-way harness just can't
    // phrase the assertion as "same message text".
    ("error_array_oob", "let a = [1]\nprint(a[5])"),
    ("error_bare_panic", "panic(\"top level\")"),
    // ─── Closures / first-class fns (phase 1) ─────────────────
    (
        "closure_basic_lambda",
        r#"let double = fn(x) { return x * 2 }
print(double(5))
print(double(21))"#,
    ),
    (
        "lambda_parameter_binding_semantics",
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
    ),
    (
        "duplicate_parameter_ref_semantics",
        r#"fn ref_value(ref value, value) { value += 2 }
fn value_ref(value, ref value) { value += 3 }
fn ref_ref(ref value, ref value) { value += 4 }
struct Holder { n }
fn Holder.update(self, ref value, value) { value += self.n }
fn Holder.replace(ref self, self) { self = Holder { n: self } }
let first = 1
let second = 1
let third = 1
let fourth = 2
let method_target = 1
let receiver = Holder { n: 1 }
ref_value(ref first, 7)
value_ref(7, ref second)
ref_ref(ref third, ref fourth)
Holder { n: 6 }.update(ref method_target, 9)
receiver.replace(12)
print([first, second, third, fourth, method_target, receiver.n])"#,
    ),
    (
        "duplicate_lambda_parameter_ref_semantics",
        r#"let ref_value = fn(ref value, value) { value += 5 }
let value_ref = fn(value, ref value) { value += 6 }
let ref_ref = fn(ref value, ref value) { value += 7 }
let first = 2
let second = 2
let third = 2
let fourth = 3
ref_value(ref first, 8)
value_ref(8, ref second)
ref_ref(ref third, ref fourth)
let rollback_target = 4
let fail = fn(ref value, value) {
    value = 99
    panic("rollback")
}
fn attempt() { fail(ref rollback_target, 8) }
print(try_call(attempt).is_err())
print([first, second, third, fourth, rollback_target])"#,
    ),
    (
        "duplicate_lambda_ref_target_rejection",
        r#"let both = fn(ref value, ref value) { value = 9 }
let target = 1
both(ref target, ref target)"#,
    ),
    (
        "closure_captures_value",
        r#"let n = 5
let add_n = fn(x) { return x + n }
print(add_n(3))
n = 100
print(add_n(3))"#,
    ),
    (
        "closure_factory",
        r#"fn make_adder(n) { return fn(x) { return x + n } }
let add5 = make_adder(5)
let add10 = make_adder(10)
print(add5(3))
print(add10(3))"#,
    ),
    (
        "named_fn_as_first_class_value",
        r#"fn double(x) { return x * 2 }
let f = double
print(f(7))"#,
    ),
    (
        "nested_named_fn_assigned_and_called",
        r#"fn make() {
    fn increment(value) { return value + 1 }
    let assigned = increment
    return assigned
}
let callable = make()
print(callable(41))"#,
    ),
    (
        "nested_named_fn_returned_passed_stored_and_redeclared",
        r#"fn apply(callable, value) { return callable(value) }
fn build(flag) {
    fn transform(value) { return value + 1 }
    let original = transform
    if flag {
        fn transform(value) { return value * 10 }
    }
    return [original, transform]
}
let functions = build(true)
let stored = {"first": functions[0], "second": functions[1]}
print(stored["first"](4))
print(apply(stored["second"], 4))"#,
    ),
    (
        "nested_named_fn_reached_source_order",
        r#"fn before() {
    return missing
    fn missing() { return 1 }
}
fn dead() {
    if false {
        fn missing() { return 2 }
    }
    return missing
}
print(try_call(before))
print(try_call(dead))
if true {
    fn selected() { return 3 }
}
let retained = selected
if true {
    fn selected() { return 4 }
}
print(retained(), selected())"#,
    ),
    (
        "exact_ref_self_recursion_retains_site",
        r#"fn f(ref value, remaining) {
    if remaining == 0 { return }
    value += 1
    f(ref value, remaining - 1)
}
let retained = f
fn f(ref value, remaining) { value = 99 }
let value = 0
retained(ref value, 2)
print(value)"#,
    ),
    (
        "exact_value_self_recursion_ignores_active_ref_site",
        r#"fn f(value) {
    if value == 0 { return 7 }
    return f(value - 1)
}
let retained = f
fn f(ref value) { value = 99 }
print(retained(2))"#,
    ),
    (
        "active_function_site_controls_ref_mode_preflight",
        r#"fn f(ref value) { value = 2 }
if false {
    fn f(value) { return value }
}
fn side_effect() { print("evaluated"); return 3 }
let value = 1
let attempt = fn() { f(side_effect()) }
print(try_call(attempt), value)"#,
    ),
    (
        "exact_value_self_wrong_arity_precedes_argument_side_effects",
        r#"fn side() { print("ARG"); return 1 }
fn f(value) { return f(side(), side()) }
let attempt = fn() { return f(1) }
print(try_call(attempt))"#,
    ),
    (
        "exact_value_self_wrong_mode_precedes_argument_side_effects",
        r#"fn side() { print("ARG"); return 1 }
fn f(first, second) {
    let target = 1
    return f(ref target, side())
}
let attempt = fn() { return f(1, 2) }
print(try_call(attempt))"#,
    ),
    (
        "mixed_active_value_wrong_arity_precedes_argument_side_effects",
        r#"fn f(value) { return value }
if false {
    fn f(ref value) { value = 2 }
}
fn side() { print("ARG"); return 1 }
let attempt = fn() { return f(side(), side()) }
print(try_call(attempt))"#,
    ),
    (
        "mixed_active_ref_wrong_arity_precedes_argument_side_effects",
        r#"fn f(ref value) { value = 2 }
if false {
    fn f(value) { return value }
}
fn side() { print("ARG"); return 1 }
let attempt = fn() { return f(side(), side()) }
print(try_call(attempt))"#,
    ),
    (
        "method_bare_name_resolves_named_function",
        r#"struct S {}
fn m() { return 7 }
fn S.m(self) { return m() }
let value = S {}
print(value.m())"#,
    ),
    (
        "active_value_site_preserves_host_precedence_with_dead_ref_site",
        r#"fn f() { return 1 }
if false {
    fn f(ref value) { value = 2 }
}
print(f())"#,
    ),
    (
        "higher_order_apply",
        r#"fn apply(f, x) { return f(x) }
fn square(n) { return n * n }
print(apply(square, 4))
print(apply(fn(n) { return n + 1 }, 4))"#,
    ),
    (
        "fn_in_array_indexed_call",
        r#"fn add(x, y) { return x + y }
fn mul(x, y) { return x * y }
let ops = [add, mul]
print(ops[0](2, 3))
print(ops[1](2, 3))"#,
    ),
    ("iife_value_call", "print((fn(x) { return x * 3 })(4))"),
    ("type_of_fn_is_fn", "fn f() { }\nprint(f.type())"),
    // ─── Structs / enums / user methods (phase 3) ───────────────
    (
        "struct_basic",
        r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
print(p.x + p.y)
print(p)"#,
    ),
    (
        "struct_field_assign",
        r#"struct Counter { n }
let c = Counter { n: 10 }
c.n += 5
c.n *= 2
print(c.n)"#,
    ),
    (
        "struct_equality",
        r#"struct P { x, y }
let a = P { x: 1, y: 2 }
let b = P { x: 1, y: 2 }
print(a == b)"#,
    ),
    (
        "enum_unit_and_tuple",
        r#"enum E { A, B(n) }
print(E::A == E::A)
print(E::B(1) == E::B(1))
print(E::B(1) == E::B(2))"#,
    ),
    (
        "enum_struct_variant",
        r#"enum Shape { Rect { w, h } }
let r = Shape::Rect { w: 4, h: 3 }
print(r.w * r.h)
print(r)"#,
    ),
    (
        "method_on_struct",
        r#"struct Point { x, y }
fn Point.sum(self) { return self.x + self.y }
let p = Point { x: 3, y: 4 }
print(p.sum())"#,
    ),
    (
        "method_chain_user",
        r#"struct Adder { n }
fn Adder.then(self, m) { return Adder { n: self.n + m } }
let r = Adder { n: 1 }.then(2).then(3).then(4)
print(r.n)"#,
    ),
    (
        "method_on_enum",
        r#"enum Shape { Circle(r), Rect { w, h } }
fn Shape.label(self) { return "shape" }
print(Shape::Circle(5).label())
print(Shape::Rect { w: 4, h: 3 }.label())"#,
    ),
    (
        "method_overrides_builtin",
        r#"struct Wrapper { data }
fn Wrapper.len(self) { return 99 }
let w = Wrapper { data: [1, 2, 3] }
print(w.len())"#,
    ),
    (
        "method_declarations_follow_direct_and_branch_source_order",
        r#"struct Box { value }
let box = Box { value: 7 }
print(try_call(fn() { return box.read() }).is_err())
fn Box.read(self) { return self.value }
print(box.read())
if false { fn Box.dead(self) { return 1 } }
print(try_call(fn() { return box.dead() }).is_err())
if true { fn Box.live(self) { return self.value + 1 } }
print(box.live())"#,
    ),
    (
        "method_declarations_in_loops_overwrite_body_and_arity",
        r#"struct Box { value }
let box = Box { value: 5 }
let turn = 0
repeat 2 {
    if turn == 0 { fn Box.pick(self) { return 10 } }
    else { fn Box.pick(self, extra) { return self.value + extra } }
    turn += 1
}
print(try_call(fn() { return box.pick() }).is_err())
print(box.pick(4))
for ignored in [1] { fn Box.from_for(self) { return self.value + 6 } }
print(box.from_for())
fn Box.zero() { return 99 }
print(try_call(fn() { return box.zero() }).is_err())"#,
    ),
    (
        "callable_lambda_and_nested_method_sites_install_on_execution",
        r#"struct Box { value }
let box = Box { value: 3 }
fn install_named() { fn Box.named(self) { return self.value + 1 } }
let install_lambda = fn() { fn Box.lambda(self) { return self.value + 2 } }
print(try_call(fn() { return box.named() }).is_err(), try_call(fn() { return box.lambda() }).is_err())
install_named()
install_lambda()
print(box.named(), box.lambda())
fn Box.install_inner(self) { fn Box.inner(self) { return self.value + 3 } }
print(try_call(fn() { return box.inner() }).is_err())
box.install_inner()
print(box.inner())
fn Box.replace_self(self) {
    fn Box.replace_self(self) { return 9 }
    return 8
}
print(box.replace_self(), box.replace_self())
fn returns_early() { return none
    fn Box.after_return(self) { return 100 }
}
returns_early()
print(try_call(fn() { return box.after_return() }).is_err())"#,
    ),
    (
        "method_sites_do_not_capture_installer_values_and_survive_caught_error",
        r#"struct Box { value }
let box = Box { value: 2 }
fn install_bad(secret) { fn Box.bad(self) { return secret } }
install_bad(9)
print(try_call(fn() { return box.bad() }).is_err())
fn install_then_fail() {
    fn Box.kept(self) { return self.value + 5 }
    let boom = 1 / 0
}
print(try_call(install_then_fail).is_err())
print(box.kept())"#,
    ),
    (
        "methods_register_before_type_and_read_self_by_value",
        r#"fn Later.bump(self) {
    return self.value + 1
}
struct Later { value }
let value = Later { value: 4 }
print(value.bump(), value.value)"#,
    ),
    (
        "active_zero_parameter_method_reports_exact_arity_error",
        r#"struct Empty { }
fn Empty.zero() { return none }
Empty { }.zero()"#,
    ),
    (
        "method_installer_values_are_exactly_uncaptured",
        r#"struct Empty { }
fn install(secret) { fn Empty.bad(self) { return secret } }
install(9)
Empty { }.bad()"#,
    ),
    (
        "struct_and_enum_same_identity_share_method_key",
        r#"struct Dual { value }
enum Dual { Wrapped { value } }
fn Dual.read(self) { return self.value }
print(Dual { value: 6 }.read(), (Dual::Wrapped { value: 7 }).read())"#,
    ),
    (
        "active_user_methods_precede_common_and_iterator_protocol",
        r#"struct Token { value }
fn Token.type(self) { return "user-type" }
fn Token.len(self) { return 41 }
fn Token.iter(self) { return self }
fn Token.next(self) { return Iter::Done }
let token = Token { value: 1 }
print(token.type(), token.len())
for item in token { print(item) }
print(token.next())"#,
    ),
    (
        "method_call_arguments_evaluate_before_receiver_on_active_arity_error",
        r#"struct Box { value }
let box = Box { value: 1 }
fn Box.only_self(self) { return self.value }
fn make_arg() { print("arg")
    return 2
}
fn make_receiver(value) { print("receiver")
    return value
}
print(try_call(fn() { return make_receiver(box).only_self(make_arg()) }).is_err())"#,
    ),
    // ─── Pattern matching (phase 4) ─────────────────────────────
    (
        "match_literal_number",
        r#"let x = 2
print(match x {
  1 => "one",
  2 => "two",
  _ => "other",
})"#,
    ),
    (
        "match_wildcard_catches_all",
        r#"let x = 42
print(match x {
  0 => "zero",
  _ => "big",
})"#,
    ),
    (
        "match_binding_captures",
        r#"let x = 7
print(match x { n => n * 2 })"#,
    ),
    (
        "match_guard_selects_arm",
        r#"let x = 10
print(match x {
  n if n < 5 => "small",
  n if n < 20 => "medium",
  _ => "big",
})"#,
    ),
    (
        "match_or_pattern",
        r#"let x = 3
print(match x {
  1 | 2 | 3 => "low",
  _ => "other",
})"#,
    ),
    (
        "match_or_pattern_complex_bindings",
        r#"struct Node { left, right }
enum Boxed { Pair(left, right), Record { first, second } }
let values = [
    Node { left: 1, right: 2 },
    Boxed::Pair(3, 4),
    Boxed::Record { first: 5, second: 6 },
]
for value in values {
    print(match value {
        Node { left, right } | Boxed::Pair(right, left) | Boxed::Record { first: left, second: right } => left * 10 + right,
    })
}
enum Packet { Values(items, marker) }
let packets = [Packet::Values([1, 2, 3], 9), Packet::Values([7], 8)]
for packet in packets {
    print(match packet {
        Packet::Values([_, head, ..tail] | [head, ..tail], marker) => head * 100 + marker * 10 + tail.len(),
    })
}
for items in [[1, 2], [3]] {
    print(match items { [x, x] | [x] => x })
}"#,
    ),
    (
        "match_enum_unit",
        r#"enum Light { Red, Green }
let l = Light::Green
print(match l {
  Light::Red => "stop",
  Light::Green => "go",
})"#,
    ),
    (
        "match_enum_tuple_binds",
        r#"enum Res { Ok(v), Err(m) }
let r = Res::Ok(42)
print(match r {
  Res::Ok(v) => v,
  Res::Err(_) => -1,
})"#,
    ),
    (
        "match_enum_struct_variant_binds",
        r#"enum Shape { Rect { w, h } }
let s = Shape::Rect { w: 4, h: 3 }
print(match s { Shape::Rect { w, h } => w * h })"#,
    ),
    (
        "match_struct_destructure",
        r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
print(match p { Point { x, y } => x + y })"#,
    ),
    (
        "match_struct_partial_rest",
        r#"struct Point { x, y, z }
let p = Point { x: 1, y: 2, z: 3 }
print(match p { Point { y, .. } => y * 10 })"#,
    ),
    (
        "match_nested_enum_struct",
        r#"enum FileError { NotFound(path) }
enum Res { Ok(v), Err(e) }
let r = Res::Err(FileError::NotFound("missing.txt"))
print(match r {
  Res::Ok(_) => "ok",
  Res::Err(FileError::NotFound(p)) => p,
})"#,
    ),
    (
        "match_array_exact",
        r#"let xs = [1, 2, 3]
print(match xs {
  [a, b, c] => a + b + c,
  _ => -1,
})"#,
    ),
    (
        "match_array_with_rest",
        r#"let xs = [10, 20, 30, 40]
print(match xs {
  [first, ..rest] => first,
  _ => -1,
})"#,
    ),
    (
        "match_no_arm_matched_errors",
        r#"let x = 99
match x {
  1 => print("one"),
  2 => print("two"),
}"#,
    ),
    // ─── `try` operator (phase 5) ────────────────────────────────
    (
        "try_unwraps_ok",
        r#"enum Result { Ok(v), Err(e) }
fn doit() {
    let v = try Result::Ok(42)
    return v
}
print(doit())"#,
    ),
    (
        "try_propagates_err",
        r#"enum Result { Ok(v), Err(e) }
fn doit() {
    let v = try Result::Err("boom")
    return Result::Ok(v)
}
let r = doit()
print(match r {
    Result::Ok(v) => v,
    Result::Err(e) => e,
})"#,
    ),
    (
        "try_chains_through_nested_calls",
        r#"enum Result { Ok(v), Err(e) }
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
})"#,
    ),
    // `try_ok_unit_variant_yields_none` used to live here — it
    // redeclared `Result` with a Unit `Ok` variant and relied on
    // the walker silently accepting it. Now that `Result` is an
    // engine builtin with the canonical `Ok(value)` shape, a
    // redeclaration with a different shape is a hard error in
    // all three engines. The per-engine unit tests cover the new
    // behaviour; there's nothing to diff here.
    (
        "try_inside_lambda_returns_from_lambda",
        r#"enum Result { Ok(v), Err(e) }
let f = fn() {
    let v = try Result::Err("inner")
    return Result::Ok(v)
}
let r = f()
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e,
})"#,
    ),
    (
        "try_in_for_loop_short_circuits",
        r#"enum Result { Ok(v), Err(e) }
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
})"#,
    ),
    (
        "try_on_non_result_errors",
        r#"fn doit() {
    let v = try 42
    return v
}
doit()"#,
    ),
    (
        "try_top_level_on_err_errors",
        r#"enum Result { Ok(v), Err(e) }
let r = try Result::Err("boom")"#,
    ),
    // ─── Integer type (phase 6) ─────────────────────────────────
    (
        "int_literal_type",
        r#"print(42.type())
print(42.0.type())
print((-3).type())"#,
    ),
    (
        "int_arithmetic_stays_int",
        r#"print(1 + 2)
print((1 + 2).type())
print(10 - 4)
print(3 * 4)"#,
    ),
    (
        "division_always_number_int_via_cast",
        r#"print(10 / 3)
print((10 / 3).type())
print((10 / 3).to_int())
print((10 / 3).to_int().type())
print((-7 / 2).to_int())"#,
    ),
    (
        "int_number_mixed_widens",
        r#"print(1 + 2.0)
print((1 + 2.0).type())
print(3 * 0.5)"#,
    ),
    (
        "int_number_equality_is_numeric",
        r#"print(1 == 1.0)
print(2 > 1.5)"#,
    ),
    ("division_by_zero_errors", "print(10 / 0)"),
    ("int_overflow_add_errors", "print(9223372036854775807 + 1)"),
    (
        "int_builtin_and_float_builtin",
        r#"print(3.7.to_int())
print(3.7.to_int().type())
print(42.to_float())
print(42.to_float().type())"#,
    ),
    (
        "len_returns_int",
        r#"print("hi".len().type())
print([1, 2, 3].len().type())"#,
    ),
    (
        "range_int_elements",
        r#"let r = range(3)
print(r[0].type())"#,
    ),
    (
        "int_match_literal",
        r#"let x = 2
print(match x {
    1 => "one",
    2 => "two",
    _ => "other",
})"#,
    ),
    (
        "repeat_accepts_int",
        r#"let n = 0
repeat 5 { n = n + 1 }
print(n)"#,
    ),
    // ─── `try_call` builtin ─────────────────────────────────────
    (
        "try_call_wraps_ok",
        r#"let r = try_call(fn() { return 42 })
print(match r {
    Result::Ok(v) => v,
    Result::Err(_) => -1,
})"#,
    ),
    (
        "try_call_wraps_non_fatal_err",
        r#"let r = try_call(fn() { return 1 / 0 })
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e.message,
})"#,
    ),
    (
        "try_call_err_carries_line",
        r#"let r = try_call(fn() {
    let x = 1
    return x / 0
})
print(match r {
    Result::Ok(_) => -1,
    Result::Err(e) => e.line,
})"#,
    ),
    (
        "try_call_composes_with_try_operator",
        r#"fn risky(x) {
    let arr = [1, 2]
    return arr[x]
}
let r = try_call(fn() { return risky(5) })
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e.message,
})"#,
    ),
    ("try_call_wrong_arg_count_errors", "try_call()"),
    ("try_call_non_function_errors", "try_call(42)"),
    (
        "try_call_nested_outer_catches_inner_err_as_ok",
        r#"let r = try_call(fn() {
    let inner = try_call(fn() { return 1 / 0 })
    return inner
})
print(match r {
    Result::Ok(Result::Err(e)) => e.message,
    Result::Ok(Result::Ok(_)) => "inner ok?",
    Result::Err(_) => "outer caught",
})"#,
    ),
];

/// Programs that exercise the `use` surface. Each entry
/// pairs source with a module map the walker, VM, and AOT all
/// resolve against. AOT's compile-time resolver is seeded from
/// this same map via `modules_from_map`.
type ImportCase = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

const IMPORTS_CORPUS: &[ImportCase] = &[
    (
        "incremental_type_publication_preserves_source_order_and_module_context",
        r#"fn make_later() { return Later {} }
print(try_call(make_later).is_err())
struct Later {}
print(type(make_later()))
use schema
use schema as api
let direct = make(4)
let namespaced = api.make(5)
print(direct.value, namespaced.value)
print(match signal(6) { Signal::Item(value) => value, _ => 0 })
print(match api.signal(7) { api.Signal::Item(value) => value, _ => 0 })"#,
        &[
            ("helper", "fn increment(value) { return value + 1 }"),
            (
                "schema",
                "use helper as dep\nstruct Box { value }\nenum Signal { Item(value) }\nfn make(value) { return Box { value: dep.increment(value) } }\nfn signal(value) { return Signal::Item(dep.increment(value)) }",
            ),
        ],
    ),
    (
        "shared_imported_named_functions",
        r#"use funcs as funcs
let callback = funcs.call
print(funcs.call(5), callback(6))"#,
        &[(
            "funcs",
            "fn twice(n) { return n * 2 }\nfn call(n) { return twice(n) + 1 }",
        )],
    ),
    (
        "optional_import_capture_explicit_and_implicit_ref_fences",
        r#"fn side() { print("arg"); return 1 }
fn take(ref value) { value = 9 }
fn implicit() {
    use dep
    let action = fn() { values.push(side()) }
    return try_call(action)
}
fn explicit() {
    use dep
    let action = fn() { take(ref values) }
    return try_call(action)
}
print(implicit())
print(explicit())"#,
        &[("dep", "let values = []")],
    ),
    (
        "module_export_ref_commit_preflight_and_rollback",
        r#"use dep as api
let value = 1
api.bump(ref value)
print(value)
fn failed() { api.fail(ref value) }
print(try_call(failed), value)
fn side() { print("arg"); return 0 }
fn missing() { api.bump(side()) }
print(try_call(missing), value)"#,
        &[(
            "dep",
            "fn bump(ref value) { value += 1 }\nfn fail(ref value) { value = 99; panic(\"rollback\") }",
        )],
    ),
    (
        "module_methods_keep_full_receiver_identity",
        r#"use left as left
use right as right
let a = left.Same { value: 2 }
let b = right.Same { value: 3 }
print(a.read(), b.read())"#,
        &[
            (
                "left",
                "struct Same { value }\nfn Same.read(self) { return self.value + 10 }",
            ),
            (
                "right",
                "struct Same { value }\nfn Same.read(self) { return self.value + 20 }",
            ),
        ],
    ),
    (
        "selective_and_aliased_use_execute_module_method_sites",
        r#"use direct.{Direct}
use aliased as api
let a = Direct { value: 4 }
let b = api.Aliased { value: 5 }
print(a.read(), b.read())"#,
        &[
            (
                "direct",
                "struct Direct { value }\nfn Direct.read(self) { return self.value + 1 }",
            ),
            (
                "aliased",
                "struct Aliased { value }\nfn Aliased.read(self) { return self.value + 2 }",
            ),
        ],
    ),
    (
        "failed_parent_module_keeps_dependency_methods",
        r#"fn load_bad() { use bad }
print(try_call(load_bad).is_err())
print(try_call(load_bad).is_err())
use dep
let value = Shared { value: 8 }
print(value.read())"#,
        &[
            (
                "dep",
                "print(\"dep-loaded\")\nstruct Shared { value }\nfn Shared.read(self) { return self.value + 30 }",
            ),
            (
                "bad",
                r#"use dep
struct Probe { }
let leaked = try_call(fn() { return Probe { }.mark() }).is_ok()
if leaked {
    print("leaked")
} else {
    fn Probe.mark(self) { return true }
    let boom = 1 / 0
}"#,
            ),
        ],
    ),
    (
        "import_basic_let",
        r#"use math
print(pi)"#,
        &[("math", "let pi = 3")],
    ),
    (
        "import_named_fn",
        r#"use math
print(square(7))"#,
        &[("math", "fn square(n) { return n * n }")],
    ),
    (
        "module_fn_reads_and_mutates_module_bindings",
        r#"use counter
print(next())
print(next())"#,
        &[(
            "counter",
            r#"const STEP = 3
let value = 4
fn next() {
    value += STEP
    return value
}"#,
        )],
    ),
    (
        "named_fns_call_bare_and_transitive_imports",
        r#"use outer
fn root_call(n) { return increment(n) }
print(root_call(10))
print(transitive(10))"#,
        &[
            (
                "outer",
                r#"use inner
fn transitive(n) { return increment(increment(n)) }"#,
            ),
            ("inner", "fn increment(n) { return n + 1 }"),
        ],
    ),
    (
        "reexported_type_origins_two_hop_facade",
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
        &[
            (
                "leaf",
                "struct Point { value }\nenum Signal { Idle, Count(value), Named { value } }\nfn make_point(value) { return Point { value: value } }\nfn make_named(value) { return Signal::Named { value: value } }",
            ),
            ("middle", "use leaf"),
            ("top", "use middle"),
            ("other", "struct Point { other }"),
        ],
    ),
    (
        "reexported_type_origins_diamond_and_first_win",
        r#"use diamond as diamond
use order_ab as ab
use order_ba as ba
let shared = diamond.Shared { value: 1 }
let first = ab.Same { a: 2 }
let second = ba.Same { b: 3 }
print(shared.value, first.a, second.b)"#,
        &[
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
        ],
    ),
    (
        "reexported_type_origins_private_and_local_overwrite",
        r#"use leaf as direct
use selective as selected
use local_before as before
use local_after as after
print(direct._Hidden { value: 1 }.value)
print(selected._Hidden { value: 2 }.value)
print(before.Public { local: 3 }.local, after.Public { local: 4 }.local)"#,
        &[
            ("leaf", "struct Public { value }\nstruct _Hidden { value }"),
            ("selective", "use leaf.{_Hidden}"),
            ("local_before", "struct Public { local }\nuse leaf"),
            ("local_after", "use leaf\nstruct Public { local }"),
        ],
    ),
    (
        "reexported_types_callable_context_and_method_identity",
        r#"use facade as api
use callers as calls
let value = api.make(4)
let aliased = calls.make_alias(6)
print(value.bump(), api.matcher()(value))
print(aliased.bump(), calls.alias_matcher()(aliased))"#,
        &[
            ("helper", "fn increment(value) { return value + 1 }"),
            (
                "leaf",
                "use helper as dep\nstruct Box { value }\nfn Box.bump(self) { return dep.increment(self.value) }",
            ),
            (
                "facade",
                "use leaf\nfn make(value) { return Box { value: value } }\nfn matcher() { return fn(value) { return match value { Box { value: found } => found, _ => 0 } } }",
            ),
            (
                "callers",
                "use facade as dep\nfn make_alias(value) { return dep.Box { value: value } }\nfn alias_matcher() { return fn(value) { return match value { dep.Box { value: found } => found, _ => 0 } } }",
            ),
        ],
    ),
    (
        "reexported_module_aliases_surface_and_callable_context",
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
        &[
            (
                "dep",
                "struct Point { value }\nenum Signal { Idle, Count(value), Named { value } }\nfn make(value) { return Point { value: value } }\nfn hidden() { return 99 }",
            ),
            ("wrapper", "use dep.{Point, Signal, make} as api"),
            ("middle", "use wrapper"),
            (
                "top",
                "use middle.{api}\nstruct Runner { offset }\nfn Runner.run(self, value) { return api.make(value + self.offset) }\nfn build(value) { let point = api.Point { value: value }; return match point { api.Point { value: found } => api.make(found + 1), _ => none } }\nfn matcher() { return fn(value) { return match value { api.Point { value: found } => found, _ => 0 } } }",
            ),
        ],
    ),
    (
        "reexported_module_aliases_same_name_isolation",
        "use wa.{make_a}\nuse wb.{make_b}\nprint(make_a(2).a, make_b(3).b)",
        &[
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
        ],
    ),
    (
        "reexported_module_aliases_timing_and_fn_winner",
        r#"use timing
use alias_before_fn
print(before.is_err(), after.is_ok(), after.unwrap().value)
print(build().value, api.Point { value: 8 }.value)"#,
        &[
            ("dep", "struct Point { value }"),
            ("wrapper", "use dep as api"),
            (
                "timing",
                "fn timed() { return api.Point { value: 9 } }\nlet before = try_call(timed)\nuse wrapper\nlet after = try_call(timed)",
            ),
            (
                "alias_before_fn",
                "use dep as api\nfn api() { return 99 }\nfn build() { return api.Point { value: 7 } }",
            ),
        ],
    ),
    (
        "reexported_module_aliases_copy_assignment_and_flat_fn_order",
        r#"use copies.{from_copy, from_reassigned}
use flat_fn_first.{result} as first
use flat_module_first.{result} as second
print(from_copy(1).value, from_reassigned(2).value)
print(first.result(), second.result().value)
use module_fn_then_value.{api} as final_value
print(final_value.api)"#,
        &[
            ("a", "struct Point { value }"),
            ("b", "struct Other { value }"),
            ("wrapper", "use a as api"),
            (
                "copies",
                "use a as api\nuse b as other\nlet copy = api\napi = other\nfn from_copy(value) { return copy.Point { value: value } }\nfn from_reassigned(value) { return api.Other { value: value } }",
            ),
            (
                "flat_fn_first",
                "fn api() { return 21 }\nuse wrapper\nfn result() { return api() }",
            ),
            (
                "flat_module_first",
                "use wrapper\nfn api() { return 22 }\nfn result() { return api.Point { value: 23 } }",
            ),
            (
                "module_fn_then_value",
                "use a as api\nfn api() { return 24 }\nlet api = 25",
            ),
        ],
    ),
    (
        "reexported_module_aliases_private_and_first_win",
        r#"use private_selected.{_api}
use module_first.{make}
use value_first.{api} as value_holder
print(_api.Point { value: 3 }.value, make().value, value_holder.api)"#,
        &[
            ("dep", "struct Point { value }"),
            ("wrapper", "use dep as api"),
            ("private", "use dep as _api"),
            ("private_selected", "use private.{_api}"),
            ("values", "let api = 11"),
            (
                "module_first",
                "use wrapper\nuse values\nfn make() { return api.Point { value: 4 } }",
            ),
            ("value_first", "use values\nuse wrapper"),
        ],
    ),
    (
        "root_named_fn_declaration_alias_context",
        r#"use types as t
fn build(value) {
    let direct = t.Point { value: value }
    let called = t.make(direct.value + 1)
    return match called { t.Point { value: found } => found, _ => 0 }
}
print(build(41))"#,
        &[(
            "types",
            "struct Point { value }\nfn make(value) { return Point { value: value } }",
        )],
    ),
    (
        "declaration_alias_is_shadowed_by_parameter",
        r#"use types as t
fn build(t) { return t.Point { value: 42 } }
print(build(1))"#,
        &[("types", "struct Point { value }")],
    ),
    (
        "future_alias_struct_namespace_precedes_payload",
        r#"fn invalid() { return dep.Stack { items: panic("payload") } }
invalid()
use types as dep"#,
        &[("types", "struct Stack { items }")],
    ),
    (
        "future_alias_enum_tuple_namespace_precedes_payload",
        r#"fn invalid() { return dep.Maybe::Some(panic("payload")) }
invalid()
use types as dep"#,
        &[("types", "enum Maybe { Some(value) }")],
    ),
    (
        "future_alias_enum_struct_namespace_precedes_payload",
        r#"fn invalid() { return dep.Maybe::Named { value: panic("payload") } }
invalid()
use types as dep"#,
        &[("types", "enum Maybe { Named { value } }")],
    ),
    (
        "future_alias_method_arguments_precede_receiver",
        r#"fn invalid() { return dep.nope(panic("payload")) }
invalid()
use types as dep"#,
        &[("types", "fn exported(value) { return value }")],
    ),
    (
        "ordinary_method_arguments_precede_receiver",
        r#"fn invalid() { return "receiver".nope(panic("payload")) }
invalid()"#,
        &[],
    ),
    (
        "non_ident_call_arguments_precede_callee",
        r#"fn invalid() { return dep["exported"](panic("payload")) }
invalid()
use types as dep"#,
        &[("types", "fn exported(value) { return value }")],
    ),
    (
        "dynamic_struct_namespace_without_declaration_seed",
        r#"use second as other
fn make(module) { return module.Point { second: 2 } }
fn is_second(value) {
    return match value { other.Point { second: found } => found == 2, _ => false }
}
print(is_second(make(other)))"#,
        &[("second", "struct Point { second }")],
    ),
    (
        "dynamic_struct_namespace_shadow_uses_runtime_identity_and_shape",
        r#"use first as dep
use second as other
fn make(dep) { return dep.Point { second: 2 } }
fn is_second(value) {
    return match value { other.Point { second: found } => found == 2, _ => false }
}
print(is_second(make(other)))"#,
        &[
            ("first", "struct Point { first }"),
            ("second", "struct Point { second }"),
        ],
    ),
    (
        "dynamic_enum_namespace_unit_tuple_and_struct",
        r#"use second as other
fn unit(module) { return module.State::Ready }
fn tuple(module) { return module.TupleState::Item(2) }
fn named(module) { return module.NamedState::Item { second: 3 } }
print(match unit(other) { other.State::Ready => true, _ => false })
print(match tuple(other) { other.TupleState::Item(value) => value, _ => 0 })
print(match named(other) { other.NamedState::Item { second: value } => value, _ => 0 })"#,
        &[(
            "second",
            "enum State { Ready }\nenum TupleState { Item(value) }\nenum NamedState { Item { second } }",
        )],
    ),
    (
        "dynamic_enum_namespace_shadow_uses_runtime_shape",
        r#"use first as dep
use second as other
fn tuple(dep) { return dep.TupleState::Item(2) }
fn named(dep) { return dep.NamedState::Item { second: 3 } }
print(match tuple(other) { other.TupleState::Item(value) => value, _ => 0 })
print(match named(other) { other.NamedState::Item { second: value } => value, _ => 0 })"#,
        &[
            (
                "first",
                "enum TupleState { Item(left, right) }\nenum NamedState { Item { first } }",
            ),
            (
                "second",
                "enum TupleState { Item(value) }\nenum NamedState { Item { second } }",
            ),
        ],
    ),
    (
        "dynamic_constructor_shape_precedes_payload",
        r#"use types as module
fn invalid_struct(ns) { return ns.Stack { wrong: panic("payload") } }
invalid_struct(module)"#,
        &[("types", "struct Stack { items }")],
    ),
    (
        "dynamic_enum_variant_and_arity_precede_payload",
        r#"use types as module
fn invalid_enum(ns) { return ns.Maybe::Some(panic("payload"), 2) }
invalid_enum(module)"#,
        &[("types", "enum Maybe { Some(value) }")],
    ),
    (
        "reassigned_declaration_alias_constructs_runtime_struct_identity",
        r#"use first as dep
use second as other
fn make() {
    dep = other
    return dep.Point { second: 2 }
}
print(match make() { other.Point { second: value } => value, _ => 0 })"#,
        &[
            ("first", "struct Point { first }"),
            ("second", "struct Point { second }"),
        ],
    ),
    (
        "reassigned_local_alias_constructs_runtime_enum_identity",
        r#"use second as other
fn make() {
    use first as dep
    dep = other
    return dep.State::Item { second: 3 }
}
print(match make() { other.State::Item { second: value } => value, _ => 0 })"#,
        &[
            ("first", "enum State { Item { first } }"),
            ("second", "enum State { Item { second } }"),
        ],
    ),
    (
        "loop_carried_alias_mutation_uses_runtime_struct_and_enum_shapes",
        r#"use first as dep
use second as other
let i = 0
while i < 2 {
    if i == 1 {
        let point = dep.Point { second: 2 }
        let state = dep.State::Item { second: 3 }
        print(match point { other.Point { second: value } => value, _ => 0 })
        print(match state { other.State::Item { second: value } => value, _ => 0 })
    }
    dep = other
    i += 1
}"#,
        &[
            (
                "first",
                "struct Point { first }\nenum State { Item { first } }",
            ),
            (
                "second",
                "struct Point { second }\nenum State { Item { second } }",
            ),
        ],
    ),
    (
        "compound_assignment_rhs_precedes_future_alias_target",
        r#"fn invalid() { dep += panic("payload") }
invalid()
use types as dep"#,
        &[("types", "struct Point { value }")],
    ),
    (
        "index_compound_assignment_rhs_precedes_future_alias_target",
        r#"fn invalid() { dep[panic("index")] += panic("payload") }
invalid()
use types as dep"#,
        &[("types", "struct Point { value }")],
    ),
    (
        "field_compound_assignment_rhs_precedes_future_alias_target",
        r#"fn invalid() { dep.value += panic("payload") }
invalid()
use types as dep"#,
        &[("types", "struct Point { value }")],
    ),
    (
        "declaration_alias_assignment_creates_call_local_overlay",
        r#"use types as dep
fn replace() {
    dep = 1
    return dep
}
print(replace(), dep.Stack { items: [] }.items.len())"#,
        &[("types", "struct Stack { items }")],
    ),
    (
        "declaration_alias_compound_assignment_uses_local_overlay",
        r#"use types as dep
fn increment() {
    dep = 1
    dep += 2
    return dep
}
fn invalid() { dep += 1 }
print(increment(), try_call(invalid).is_err(), dep.Stack { items: [] }.items.len())"#,
        &[("types", "struct Stack { items }")],
    ),
    (
        "declaration_alias_compound_assignment_writes_through",
        r#"use types as dep
fn increment() {
    dep = 1
    dep += 2
    return dep
}
print(increment(), dep)"#,
        &[("types", "struct Stack { items }")],
    ),
    (
        "declaration_alias_mutating_method_writes_through",
        r#"use types as dep
fn mutate() {
    dep = []
    dep.push(1)
    return dep.len()
}
print(mutate(), dep.len())"#,
        &[("types", "struct Stack { items }")],
    ),
    (
        "declaration_alias_interpolation_call_and_local_shadow",
        r#"use holder as holder
print(holder.show(), holder.Holder { value: 0 }.show())
print(holder.shadow("local"), holder.call_push())"#,
        &[
            (
                "types",
                "struct Point { value }\nfn push(value) { return value + 1 }",
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
        ],
    ),
    (
        "declaration_alias_bare_call_is_non_callable",
        r#"use types as dep
fn invoke() { return dep() }
invoke()"#,
        &[("types", "struct Point { value }")],
    ),
    (
        "future_declaration_alias_does_not_shadow_earlier_call",
        r#"fn before() { print("before") }
before()
use types as print"#,
        &[("types", "struct Point { value }")],
    ),
    (
        "declaration_alias_reads_are_source_order_lazy",
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
        &[("types", "struct Stack { items }")],
    ),
    (
        "declaration_alias_overlay_crosses_nested_lambdas",
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
        &[
            ("first", "struct Point { value }"),
            ("second", "struct Point { value }"),
        ],
    ),
    (
        "declaration_alias_mutable_places_need_overlay",
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
        &[("types", "struct Point { value }")],
    ),
    (
        "declaration_alias_unoverlaid_index_error_is_canonical",
        r#"use types as dep
fn invalid() { dep["value"] = 1 }
invalid()"#,
        &[("types", "struct Point { value }")],
    ),
    (
        "declaration_alias_unoverlaid_field_error_is_canonical",
        r#"use types as dep
fn invalid() { dep.value = 1 }
invalid()"#,
        &[("types", "struct Point { value }")],
    ),
    (
        "future_declaration_alias_simple_assignment_error",
        r#"fn invalid() { dep = 1 }
invalid()
use types as dep"#,
        &[("types", "struct Point { value }")],
    ),
    (
        "future_declaration_alias_compound_assignment_error",
        r#"fn invalid() { dep += 1 }
invalid()
use types as dep"#,
        &[("types", "struct Point { value }")],
    ),
    (
        "nested_named_function_uses_declaration_not_outer_local",
        r#"use types as dep
fn outer() {
    let dep = 1
    fn inner() { return dep.Point { value: 4 }.value }
    return inner()
}
print(outer())"#,
        &[("types", "struct Point { value }")],
    ),
    (
        "declaration_alias_overlay_survives_nested_blocks",
        r#"use types as dep
fn replace() {
    if true {
        if true { dep = 1 }
    }
    return dep
}
print(replace(), dep.Stack { items: [] }.items.len())"#,
        &[("types", "struct Stack { items }")],
    ),
    (
        "declaration_alias_overlay_survives_loop_for_compound_write",
        r#"use types as dep
fn increment() {
    if true { dep = 1 }
    repeat 1 { dep += 2 }
    return dep
}
print(increment(), dep.Stack { items: [] }.items.len())"#,
        &[("types", "struct Stack { items }")],
    ),
    (
        "declaration_alias_is_source_ordered_at_call_time",
        r#"fn build() { return t.Point { value: 42 } }
let before = try_call(build)
use types as t
print(before.is_err(), build().value)"#,
        &[("types", "struct Point { value }")],
    ),
    (
        "pattern_only_declaration_alias_is_optional_and_source_ordered",
        r#"fn label(value) {
    return match value { dep.Stack { items } => "hit", _ => "miss" }
}
print(label(1))
use types as dep
print(label(dep.Stack { items: [1] }))"#,
        &[("types", "struct Stack { items }")],
    ),
    (
        "pattern_only_lambda_captures_parameter_namespace",
        r#"use types as root_dep
let value = root_dep.Stack { items: [1] }
fn matcher(dep) {
    return fn(value) {
        return match value { dep.Stack { items } => items.len(), _ => 0 }
    }
}
let hit = matcher(root_dep)
let miss = matcher(1)
print(hit(value), miss(value))"#,
        &[("types", "struct Stack { items }")],
    ),
    (
        "namespace_only_lambda_capture_and_pattern_shadow",
        r#"use types as dep
let point = dep.Point { value: 42 }
fn outer(dep) {
    return fn() {
        let made = dep.Point { value: 41 }
        return match made { dep.Point { value: found } => found + 1, _ => 0 }
    }
}
let captured = outer(dep)
fn read(dep, value) {
    return match value { dep.Point { value: found } => found, _ => 0 }
}
print(captured(), read(1, point))"#,
        &[("types", "struct Point { value }")],
    ),
    (
        "pattern_namespace_precedes_same_named_arm_binding",
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
        &[
            ("types_a", "struct Point { dep }"),
            ("types_b", "struct Point { dep }"),
        ],
    ),
    (
        "module_functions_resolve_during_module_load",
        r#"use loading
print(value)"#,
        &[(
            "loading",
            "fn helper(n) { return n + 1 }\nfn recurse(n) { if n == 0 { return 0 } return recurse(n - 1) + 1 }\nfn build() { return helper(40) + recurse(1) }\nlet value = build()",
        )],
    ),
    (
        "imported_fn_method_declaration_alias_context",
        r#"use first_holder as first
use second_holder as second
use second_types as dep
print(first.build(1), first.Holder { value: 2 }.build())
print(second.build(3), second.Holder { value: 4 }.build())"#,
        &[
            (
                "first_types",
                "struct Point { value }\nfn make(value) { return Point { value: value } }",
            ),
            (
                "second_types",
                "struct Point { value }\nfn make(value) { return Point { value: value + 100 } }",
            ),
            (
                "first_holder",
                "use first_types as dep\nstruct Holder { value }\nfn build(value) { let point = dep.make(value) return match point { dep.Point { value: found } => found, _ => 0 } }\nfn Holder.build(self) { return dep.make(self.value).value }",
            ),
            (
                "second_holder",
                "use second_types as dep\nstruct Holder { value }\nfn build(value) { let point = dep.Point { value: value } return match point { dep.Point { value: found } => found, _ => 0 } }\nfn Holder.build(self) { return dep.make(self.value).value }",
            ),
        ],
    ),
    (
        "imported_fn_bare_type_declaration_context",
        r#"use holder as module
print(module.build(42))"#,
        &[
            ("types", "struct Point { value }"),
            (
                "holder",
                "use types.{Point}\nfn build(value) { let point = Point { value: value } return match point { Point { value: found } => found, _ => 0 } }",
            ),
        ],
    ),
    (
        "import_dotted_path",
        r#"use std.math
print(e)"#,
        &[("std.math", "let e = 2")],
    ),
    (
        "import_dot_underscore_slug_collision",
        r#"use a.b as dotted
use a_b as underscored
print(dotted.helper(), dotted.ctx, underscored.helper(), underscored.yield)"#,
        &[
            ("a.b", "let ctx = 10\nfn helper() { return 1 }"),
            ("a_b", "let yield = 20\nfn helper() { return 2 }"),
        ],
    ),
    (
        "import_transitive",
        r#"use a
print(doubled)"#,
        &[("a", "use b\nlet doubled = pi + pi"), ("b", "let pi = 3")],
    ),
    (
        "import_shared_dependency_diamond",
        r#"use left
use right
print(one, two)"#,
        &[
            (
                "shared",
                "print(\"shared init\")\nfn helper(n) { return n + 10 }",
            ),
            ("left", "use shared\nlet one = helper(1)"),
            ("right", "use shared\nlet two = helper(2)"),
        ],
    ),
    (
        "import_shared_dependency_alias_diamond",
        r#"use left as l
use right as r
print(l.one, r.two)"#,
        &[
            (
                "shared",
                "print(\"shared alias init\")\nfn helper(n) { return n + 20 }",
            ),
            ("left", "use shared\nlet one = helper(1)"),
            ("right", "use shared\nlet two = helper(2)"),
        ],
    ),
    (
        "import_alias_only_function_surface",
        r#"use internal as module
print(module.twice(3))
print(module.recurse(4))
let returned = module.twice
print(returned(5))
print(module.closure(6))
print(module.Thing { value: 7 }.bump())
print(module._private)"#,
        &[(
            "internal",
            r#"fn helper(n) { return n + 1 }
fn twice(n) { return helper(n) + helper(n) }
fn recurse(n) {
    if n == 0 { return 0 }
    return 1 + recurse(n - 1)
}
let closure = fn(n) { return helper(n) }
struct Thing { value }
fn Thing.bump(self) { return helper(self.value) }
let _private = 99"#,
        )],
    ),
    (
        "import_alias_rejects_bare_function",
        r#"use internal as module
print(module.helper(1))
print(helper(2))"#,
        &[("internal", "fn helper(n) { return n + 1 }")],
    ),
    (
        "import_nested_alias_rejects_bare_function",
        r#"use wrapper
print(via_alias)"#,
        &[
            ("shared", "fn helper(n) { return n + 1 }"),
            (
                "wrapper",
                "use shared as dep\nlet via_alias = dep.helper(3)\nlet via_bare = helper(4)",
            ),
        ],
    ),
    (
        "import_selective_glob_mix",
        r#"use mixed as module
print(module.total)
print(module._private)
print(module.helper(5))"#,
        &[
            (
                "shared",
                "let public = 10\nlet _private = 2\nfn helper(n) { return n + 1 }",
            ),
            (
                "mixed",
                "use shared.{_private}\nuse shared\nlet total = _private + public",
            ),
        ],
    ),
    (
        "import_alias_of_alias_hygiene",
        r#"use facade as outer
print(outer.layer.ctx.helper(4))"#,
        &[
            ("shared", "fn helper(n) { return n + 1 }"),
            ("layer", "use shared as ctx"),
            ("facade", "use layer as layer"),
        ],
    ),
    (
        "import_idempotent_cache",
        r#"use m
use m
print(x)"#,
        &[("m", "let x = 42")],
    ),
    (
        "import_plain_glob_idempotency_is_lexical",
        r#"if true {
    use m
    print(x)
}
if true {
    use m
    print(x)
}
use m
print(x)"#,
        &[("m", "let x = 42")],
    ),
    (
        "import_alias_shadow_restores_outer",
        r#"use first as t
if true {
    use second as t
    print(t.Point { second: 2 }.second)
}
print(t.Point { first: 1 }.first)"#,
        &[
            ("first", "struct Point { first }"),
            ("second", "struct Point { second }"),
        ],
    ),
    (
        "import_selective_function_preserves_named_fn",
        r#"fn pick() { return 1 }
use dep.{pick}
print(pick())"#,
        &[("dep", "fn pick() { return 42 }")],
    ),
    (
        "import_glob_function_preserves_named_fn",
        r#"fn pick() { return 1 }
use dep
print(pick())"#,
        &[("dep", "fn pick() { return 42 }")],
    ),
    (
        "import_glob_value_export_preserves_named_fn",
        r#"fn pick() { return 1 }
use dep
print(pick())"#,
        &[("dep", "let pick = 42")],
    ),
    (
        "import_glob_in_fn_body_preserves_slot_local_and_param",
        r#"fn local_case() {
    let picked = "local"
    use dep
    return picked
}
fn param_case(picked) {
    use dep
    return picked
}
print(local_case())
print(param_case("param"))
print(param_case("again"))"#,
        &[("dep", "fn picked() { return \"imported\" }")],
    ),
    (
        "import_local_callable_shadows_then_restores",
        r#"use outer
fn local() {
    use inner.{helper}
    return helper()
}
print(local())
print(helper())"#,
        &[
            ("outer", "fn helper() { return 1 }"),
            ("inner", "fn helper() { return 2 }"),
        ],
    ),
    (
        "import_local_callable_shadows_named_function",
        r#"fn helper() { return 1 }
fn local() {
    use inner.{helper}
    return helper()
}
print(local())
print(helper())"#,
        &[("inner", "fn helper() { return 2 }")],
    ),
    (
        "import_function_local_callable_does_not_leak",
        r#"fn seed() {
    use inner.{helper}
    return helper()
}
seed()
print(helper())"#,
        &[("inner", "fn helper() { return 2 }")],
    ),
    (
        "import_nested_every_runtime_body",
        r#"fn function_use() {
    use nested_shared as unused_alias
    use nested_shared.{inc}
    return inc(1)
}
fn branch_use(which) {
    if which == 1 {
        use nested_shared
        return inc(2)
    } else if which == 2 {
        use nested_shared.{inc}
        return inc(3)
    } else {
        use nested_shared
        return inc(4)
    }
}
fn loop_uses() {
    let total = 0
    while total == 0 {
        use nested_shared
        total = inc(4)
    }
    repeat 1 {
        use nested_shared.{inc}
        total += inc(5)
    }
    for item in [6] {
        use nested_shared
        total += inc(item)
    }
    return total
}
struct Holder { value }
fn Holder.load(self) {
    use nested_shared
    return inc(self.value)
}
let lambda_use = fn() {
    use nested_shared.{inc}
    return inc(7)
}
let match_lambda_use = match 1 {
    1 => fn() {
        use nested_shared
        return inc(8)
    },
    _ => fn() { return 0 },
}
print(function_use())
print(branch_use(1), branch_use(2), branch_use(3))
print(loop_uses())
print(Holder { value: 6 }.load())
print(lambda_use(), match_lambda_use())"#,
        &[("nested_shared", "fn inc(n) { return n + 1 }")],
    ),
    (
        "import_nested_inside_imported_module",
        r#"use wrapper as wrapper_module
print(wrapper_module.run())"#,
        &[
            ("wrapper", "fn run() { use leaf.{value}; return value }"),
            ("leaf", "let value = 42"),
        ],
    ),
    (
        "import_nested_alias_type_construction",
        r#"fn build_and_read() {
    use nested_types as types
    let point = types.Point { value: 41 }
    return match point {
        types.Point { value } => value + 1,
        _ => 0,
    }
}
print(build_and_read())"#,
        &[("nested_types", "struct Point { value }")],
    ),
    (
        "import_nested_alias_shadows_outer_alias",
        r#"use first_types as types
if true {
    use second_types as types
    let point = types.Point { second: 42 }
    print(point.second)
}"#,
        &[
            ("first_types", "struct Point { first }"),
            ("second_types", "struct Point { second }"),
        ],
    ),
    (
        "import_selective_value_shadowing_is_lexical_and_first_win",
        r#"let value = 1
if true {
    use shadow_values.{value}
    print(value)
}
if true {
    let value = 10
    use shadow_values.{value}
    print(value)
}"#,
        &[("shadow_values", "let value = 2")],
    ),
    (
        "import_glob_value_shadowing_is_lexical_and_first_win",
        r#"let value = 1
if true {
    use shadow_values
    print(value)
}
if true {
    let value = 10
    use shadow_values
    print(value)
}"#,
        &[("shadow_values", "let value = 2")],
    ),
    (
        "import_bare_type_conflicts_are_first_win_per_scope",
        r#"if true {
    use first_types.{Point}
    use second_types.{Point}
    let selected = Point { first: 41 }
    print(selected.first + 1)
}
if true {
    use first_types
    use second_types
    let globbed = Point { first: 40 }
    print(globbed.first + 2)
}"#,
        &[
            ("first_types", "struct Point { first }"),
            ("second_types", "struct Point { second }"),
        ],
    ),
    (
        "import_selective_alias_pattern_excludes_unselected_type",
        r#"use alias_types as all
use alias_types.{A} as narrowed
let value = all.B { value: 1 }
print(match value {
    narrowed.B { value } => "matched",
    _ => "missed",
})"#,
        &[("alias_types", "struct A { value }\nstruct B { value }")],
    ),
    (
        "import_lazy_edge_is_not_eager_cycle",
        r#"use lazy_a as a
print(a.value)
print(a.load())"#,
        &[
            (
                "lazy_a",
                "let value = 10\nfn load() { use lazy_b; return answer() }",
            ),
            ("lazy_b", "use lazy_a as parent\nfn answer() { return 32 }"),
        ],
    ),
    // ─── Result methods (engine-level, no `use` required) ────
    (
        "result_method_helpers",
        r#"print(Result::Ok(1).is_ok())
print(Result::Err("x").is_err())
print(Result::Err("x").unwrap_or(42))"#,
        &[],
    ),
    (
        "result_method_map",
        r#"let r = Result::Ok(5).map(fn(x) { return x * 2 })
print(match r { Result::Ok(v) => v, Result::Err(_) => -1 })"#,
        &[],
    ),
    (
        "result_method_map_err",
        r#"let r = Result::Err("bad").map_err(fn(e) { return e + "!" })
print(match r { Result::Err(v) => v, Result::Ok(_) => "ok?" })"#,
        &[],
    ),
    (
        "result_method_and_then",
        r#"fn halve(x) {
    if x % 2 == 0 { return Result::Ok((x / 2).to_int()) }
    return Result::Err("odd")
}
let r = Result::Ok(8).and_then(halve).and_then(halve)
print(match r { Result::Ok(v) => v, Result::Err(_) => -1 })"#,
        &[],
    ),
    // ─── Dict missing-key soft lookup ─────────────────────────
    (
        "dict_missing_key_returns_none",
        // AOT emits `ops::index_get` inline, so the soft-lookup
        // result has to match walker / VM exactly.
        r#"let d = {"hp": 10}
print(d["hp"])
print(d["missing"])
print(d["missing"].is_none())"#,
        &[],
    ),
    // ─── is_none / is_some universal methods ─────────────────
    (
        "is_none_basic_dispatch",
        // Has to work uniformly across every receiver shape so
        // walker / VM / AOT must agree per-type.
        r#"print(none.is_none())
print((0).is_none())
print("".is_none())
print([].is_none())
print(false.is_none())
print(none.is_some())
print((42).is_some())"#,
        &[],
    ),
    (
        "is_none_with_optional_return",
        r#"fn maybe(n) {
    if n < 0 { return none }
    return n
}
let a = maybe(-1)
let b = maybe(7)
if a.is_none() { print("a is none") }
if b.is_some() { print("b = " + b.to_str()) }"#,
        &[],
    ),
    // ─── Ok / Err shorthand ───────────────────────────────────
    (
        "ok_err_shorthand_expression",
        // Parser-level sugar — the resulting `Value` must match
        // `Result::Ok(...)` / `Result::Err(...)` byte-for-byte
        // across walker / VM / AOT.
        r#"print(Ok(1))
print(Err("x"))
print(Ok(1) == Result::Ok(1))
print(Err("x") == Result::Err("x"))"#,
        &[],
    ),
    (
        "ok_err_shorthand_pattern",
        r#"fn describe(r) {
    return match r {
        Ok(v)  => "ok: " + v.to_str(),
        Err(e) => "err: " + e,
    }
}
print(describe(Ok(5)))
print(describe(Err("stop")))"#,
        &[],
    ),
    // ─── Iterator protocol ────────────────────────────────────
    (
        "iter_basic_next_sequence",
        r#"let it = [10, 20, 30].iter()
print(it.next())
print(it.next())
print(it.next())
print(it.next())"#,
        &[],
    ),
    (
        "iter_for_over_value_iter",
        r#"let total = 0
for x in [1, 2, 3, 4, 5].iter() { total = total + x }
print(total)"#,
        &[],
    ),
    (
        "iter_for_over_user_container",
        // User wrapper delegates `.iter()` to a backing array —
        // exercises the full protocol path in every engine.
        r#"struct Bag { items }
fn bag_of(arr) { return Bag { items: arr } }
fn Bag.iter(self) { return self.items.iter() }

let b = bag_of([10, 20, 30])
let sum = 0
for v in b { sum = sum + v }
print(sum)"#,
        &[],
    ),
    (
        "iter_for_on_dict_yields_keys",
        r#"let out = ""
for k in {"a": 1, "b": 2, "c": 3} { out = out + k }
print(out)"#,
        &[],
    ),
    (
        "iter_string_yields_code_points",
        r#"let it = "nybl".iter()
print(it.next())
print(it.next())
print(it.next())
print(it.next())"#,
        &[],
    ),
    // ─── nybl-std stdlib (phase 7) ─────────────────────────────
    (
        "std_math_factorial",
        r#"use std.math
print(factorial(5))
print(gcd(12, 18))
print(clamp(99, 0, 10))"#,
        &[],
    ),
    (
        "std_iter_functional_helpers",
        r#"use std.iter
let nums = [1, 2, 3, 4, 5]
print(map(nums, fn(x) { return x + 1 }))
print(filter(nums, fn(x) { return x % 2 == 0 }))
print(reduce(nums, 0, fn(a, b) { return a + b }))"#,
        &[],
    ),
    (
        "std_string_reverse_and_pad",
        r#"use std.string
print(reverse("hello"))
print(pad_left("7", 3, "0"))
print(is_palindrome("racecar"))"#,
        &[],
    ),
    (
        "core_math_builtins_no_import",
        r#"print(16.sqrt())
print(3.7.floor())
print(3.2.ceil())
print(2.pow(10))"#,
        &[],
    ),
    (
        "rounding_methods_respect_i64_boundaries",
        r#"print(9223372036854775808.0.floor().type(), 9223372036854775808.0.ceil().type(), 9223372036854775808.0.round().type())
print((-9223372036854775808.0).floor(), (-9223372036854775808.0).ceil(), (-9223372036854775808.0).round())
print(9223372036854774784.0.floor(), 9223372036854774784.0.ceil(), 9223372036854774784.0.round())"#,
        &[],
    ),
    (
        "imported_fn_calls_sibling_fn",
        r#"use helpers
print(quadruple(3))"#,
        &[(
            "helpers",
            r#"fn double(x) { return x * 2 }
fn quadruple(x) { return double(double(x)) }"#,
        )],
    ),
    (
        "imported_struct_type_in_caller",
        r#"use shapes
let p = Point { x: 3, y: 4 }
print(p.x + p.y)"#,
        &[("shapes", r#"struct Point { x, y }"#)],
    ),
    (
        "imported_enum_type_in_caller",
        r#"use shapes
let s = Shape::Rect { w: 4, h: 3 }
print(match s {
    Shape::Circle(r) => r,
    Shape::Rect { w, h } => w * h,
})"#,
        &[("shapes", r#"enum Shape { Circle(r), Rect { w, h } }"#)],
    ),
    (
        "type_bindings_selective_and_glob_source_order",
        r#"fn imported() { return Imported { value: 2 } }
fn imported_enum() { return ImportedSignal::Pair(3, 4) }
print(try_call(imported).is_err(), try_call(imported_enum).is_err())
use types.{Imported}
print(imported().value)
use types
print(match imported_enum() { ImportedSignal::Pair(left, right) => left + right, _ => 0 })"#,
        &[(
            "types",
            r#"struct Imported { value }
enum ImportedSignal { Idle, Pair(left, right), Named { value } }"#,
        )],
    ),
    (
        "type_bindings_dead_branch_import_does_not_publish",
        r#"fn build() { return Point { value: 14 } }
if false {
    use types.{Point}
    print(build().value)
}
print(try_call(build).is_err())
use types.{Point}
print(build().value)"#,
        &[("types", "struct Point { value }")],
    ),
    (
        "type_bindings_module_init_and_retry_cleanup",
        r#"use timing
fn load_bad() { use bad }
print(try_call(load_bad).is_err())
print(try_call(load_bad).is_err())"#,
        &[
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
        ],
    ),
    (
        "module_callable_nested_types_execute_without_exporting",
        r#"use maker
print(build(12).value, build(13).value)
print(try_call(fn() { return Hidden { value: 0 } }).is_err())"#,
        &[(
            "maker",
            r#"fn build(value) {
    struct Hidden { value }
    enum Wrapped { Value(item) }
    return match Wrapped::Value(Hidden { value: value }) {
        Wrapped::Value(item) => item,
        _ => none,
    }
}"#,
        )],
    ),
    (
        "failed_module_load_rolls_back_own_type_defs_only",
        r#"fn load_bad() { use bad }
print(try_call(load_bad).is_err())
print(try_call(load_bad).is_err())
use dep
print(Shared { value: 14 }.value)"#,
        &[
            ("dep", "struct Shared { value }"),
            (
                "bad",
                r#"use dep
struct Own { value }
enum OwnSignal { Value(item) }
let boom = 1 / 0"#,
            ),
        ],
    ),
    (
        "aliased_module_member_reads_are_live_after_module_fn_mutation",
        r#"use counter as c
print(c.value)
c.next()
print(c.value)
print(c.next(), c.value)"#,
        &[(
            "counter",
            "let value = 1\nfn next() {\n    value += 1\n    return value\n}",
        )],
    ),
    (
        "aliased_module_member_read_inside_fn_and_fn_member_value",
        r#"use counter as c
let bump = c.next
print(bump())
fn reader() {
    return c.value
}
c.next()
print(reader(), c.step)"#,
        &[(
            "counter",
            "let step = 5\nlet value = 1\nfn next() {\n    value += 1\n    return value\n}",
        )],
    ),
    (
        "aliased_facade_member_reads_follow_reexport_origin",
        r#"use facade as f
print(f.value)
f.next()
print(f.value)"#,
        &[
            ("facade", "use counter"),
            (
                "counter",
                "let value = 1\nfn next() {\n    value += 1\n    return value\n}",
            ),
        ],
    ),
    (
        "aliased_selective_member_reads_resolve_live_bindings",
        r#"use counter.{next} as c
print(c.value)
c.next()
print(c.value)"#,
        &[(
            "counter",
            "let value = 1\nfn next() {\n    value += 1\n    return value\n}",
        )],
    ),
    (
        "aliased_module_missing_member_error_is_canonical",
        r#"use counter as c
print(c.value)
print(c.missing)"#,
        &[(
            "counter",
            "let value = 1\nfn next() {\n    value += 1\n    return value\n}",
        )],
    ),
    (
        "named_function_host_precedence",
        r#"fn root_fn() { return 1 }
use dep.{invoke}
print(root_fn())
print(invoke())"#,
        &[(
            "dep",
            "fn module_fn() { return 2 }\nfn invoke() { return module_fn() }",
        )],
    ),
    (
        "failed_module_load_rolls_back_retained_function_reachability",
        r#"use stash.{get}
fn load_bad() { use bad }
print(try_call(load_bad))
let retained = get()
print(try_call(retained))"#,
        &[
            (
                "stash",
                "let saved = none\nfn save(value) { saved = value }\nfn get() { return saved }",
            ),
            (
                "bad",
                "use stash.{save}\nfn helper() { return 7 }\nfn expose() { return helper() }\nsave(expose)\nmissing()",
            ),
        ],
    ),
];

// ─── The actual three-way test ────────────────────────────────────

#[test]
#[ignore]
fn three_way_diff() {
    // Unify the flat CORPUS (no imports) and IMPORTS_CORPUS into
    // a single list of `CorpusEntry`. The empty-slice for
    // `modules` on flat entries is load-bearing — it's what lets
    // us skip threading a resolver through to simple programs.
    let mut entries: Vec<CorpusEntry> = CORPUS
        .iter()
        .map(|(name, source)| CorpusEntry {
            name,
            source,
            modules: &[],
        })
        .collect();
    for (name, source, modules) in IMPORTS_CORPUS {
        entries.push(CorpusEntry {
            name,
            source,
            modules,
        });
    }
    assert_three_way(&entries);
}

#[test]
#[ignore]
fn three_way_rounding_methods_respect_i64_boundaries() {
    let entries = IMPORTS_CORPUS
        .iter()
        .filter(|(name, _, _)| *name == "rounding_methods_respect_i64_boundaries")
        .map(|(name, source, modules)| CorpusEntry {
            name,
            source,
            modules,
        })
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1);
    assert_three_way(&entries);
}

#[test]
#[ignore]
fn three_way_incremental_type_publication_semantics() {
    const NAMES: &[&str] = &[
        "incremental_type_publication_preserves_source_order_and_module_context",
        "type_bindings_module_init_and_retry_cleanup",
    ];
    let entries = IMPORTS_CORPUS
        .iter()
        .filter(|(name, _, _)| NAMES.contains(name))
        .map(|(name, source, modules)| CorpusEntry {
            name,
            source,
            modules,
        })
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), NAMES.len());
    assert_three_way(&entries);
}

fn assert_three_way(entries: &[CorpusEntry]) {
    // Step 1: compute walker + VM outcomes up-front so we can
    // compare against AOT after the slow compile.
    let mut walker = std::collections::HashMap::new();
    let mut vm = std::collections::HashMap::new();
    for e in entries {
        walker.insert(e.name, walker_outcome(e.name, e.source, e.modules));
        vm.insert(e.name, vm_outcome(e.name, e.source, e.modules));
    }

    // Step 2: run both AOT modes in one batched native process.
    let aot_results = run_aot_batch(entries);
    let expected_aot_results = entries.len() * AotMode::ALL.len();
    let mut aot = std::collections::HashMap::with_capacity(expected_aot_results);
    for (key, outcome) in aot_results {
        assert!(
            aot.insert(key.clone(), outcome).is_none(),
            "AOT produced a duplicate envelope for {key}"
        );
    }
    assert_eq!(
        aot.len(),
        expected_aot_results,
        "AOT produced an unexpected number of result envelopes"
    );

    // Step 3: every program's outcome must agree across the walker,
    // VM, native AOT, and sandboxed AOT.
    let mut failures: Vec<String> = Vec::new();
    for e in entries {
        let w = &walker[e.name];
        let v = &vm[e.name];
        for mode in AotMode::ALL {
            let key = format!("{}/{}", mode.label(), e.name);
            let a = aot.get(&key).unwrap_or_else(|| {
                panic!(
                    "AOT did not produce an envelope for {} ({})",
                    e.name,
                    mode.label()
                );
            });

            if w != v || v != a {
                let mut msg = format!("\n--- {} ({}) ---\n", e.name, mode.label());
                writeln!(msg, "walker: {w:?}").unwrap();
                writeln!(msg, "vm:     {v:?}").unwrap();
                writeln!(msg, "aot:    {a:?}").unwrap();
                failures.push(msg);
            }
        }
    }

    assert!(
        failures.is_empty(),
        "three-way differential failures:\n{}",
        failures.join("")
    );
}

#[test]
#[ignore]
fn three_way_nested_named_function_values() {
    const NAMES: &[&str] = &[
        "nested_named_fn_assigned_and_called",
        "nested_named_fn_returned_passed_stored_and_redeclared",
        "nested_named_fn_reached_source_order",
        "exact_ref_self_recursion_retains_site",
        "exact_value_self_recursion_ignores_active_ref_site",
        "active_function_site_controls_ref_mode_preflight",
        "exact_value_self_wrong_arity_precedes_argument_side_effects",
        "exact_value_self_wrong_mode_precedes_argument_side_effects",
        "mixed_active_value_wrong_arity_precedes_argument_side_effects",
        "mixed_active_ref_wrong_arity_precedes_argument_side_effects",
        "method_bare_name_resolves_named_function",
        "active_value_site_preserves_host_precedence_with_dead_ref_site",
        "named_function_host_precedence",
        "failed_module_load_rolls_back_retained_function_reachability",
    ];
    let mut entries = CORPUS
        .iter()
        .filter(|(name, _)| NAMES.contains(name))
        .map(|(name, source)| CorpusEntry {
            name,
            source,
            modules: &[],
        })
        .collect::<Vec<_>>();
    entries.extend(
        IMPORTS_CORPUS
            .iter()
            .filter(|(name, _, _)| NAMES.contains(name))
            .map(|(name, source, modules)| CorpusEntry {
                name,
                source,
                modules,
            }),
    );
    assert_eq!(entries.len(), NAMES.len());
    assert_three_way(&entries);
}
