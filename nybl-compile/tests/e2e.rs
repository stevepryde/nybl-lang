//! End-to-end differential tests for the AOT transpiler.
//!
//! Each test:
//!
//! 1. Runs the Nybl program through the tree-walker to get the
//!    reference output.
//! 2. Transpiles the same program to Rust via `nybl-compile`.
//! 3. Drops the generated Rust into a scratch `cargo` project under
//!    `target/nybl-compile-e2e/<test-name>/`, pointing at the
//!    workspace `nybl` / `nybl-sys` crates by path.
//! 4. Runs `cargo run` and captures stdout.
//! 5. Asserts the AOT output matches the tree-walker's.
//!
//! These are marked `#[ignore]` because each test spins up a full
//! `cargo build` — cheap per-test (~1s warm cache) but too heavy for
//! every `cargo test` run. Opt in with
//!
//! ```text
//! cargo test -p nybl-compile --test e2e -- --ignored
//! ```
//!
//! The scratch dir is reused across invocations, so the second run
//! is markedly faster than the first (dep tree compiled once).

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use nybl::{NyblError, NyblHost, NyblLimits, Value};
use nybl_compile::{Options, modules_from_map, transpile};

// ─── Tree-walker reference ────────────────────────────────────────

struct RecordHost {
    prints: RefCell<Vec<String>>,
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
}

fn walker_output(code: &str) -> String {
    let host = RecordHost {
        prints: RefCell::new(Vec::new()),
    };
    let mut host = host;
    nybl::run(code, &mut host, &NyblLimits::standard()).expect("tree-walker failed on e2e program");
    host.prints.borrow().join("\n")
}

// ─── AOT scratch project ──────────────────────────────────────────

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at the crate under test; the
    // workspace root is one level up.
    let crate_dir: PathBuf = env!("CARGO_MANIFEST_DIR").into();
    crate_dir.parent().unwrap().to_path_buf()
}

fn scratch_dir(test_name: &str) -> PathBuf {
    let mut p = workspace_root();
    p.push("target");
    p.push("nybl-compile-e2e");
    p.push(test_name);
    p
}

fn write_scratch_project(test_name: &str, rust_src: &str) -> PathBuf {
    let root = workspace_root();
    let dir = scratch_dir(test_name);
    let src_dir = dir.join("src");
    std::fs::create_dir_all(&src_dir).expect("create scratch src dir");

    let nybl_path = root.join("nybl");
    let nybl_sys_path = root.join("nybl-sys");
    let nybl_vm_path = root.join("nybl-vm");
    let nybl_vm_dependency = if rust_src.contains("::nybl_vm") {
        format!(
            "nybl-vm = {{ path = {:?} }}\n",
            nybl_vm_path.to_string_lossy()
        )
    } else {
        String::new()
    };
    let manifest = format!(
        r#"[package]
name = "nybl-e2e-{name}"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
nybl = {{ path = "{nybl}", package = "nybl-lang" }}
nybl-sys = {{ path = "{nybl_sys}" }}
{nybl_vm_dependency}

[[bin]]
name = "program"
path = "src/main.rs"

[workspace]
"#,
        name = test_name,
        nybl = nybl_path.display(),
        nybl_sys = nybl_sys_path.display(),
    );
    std::fs::write(dir.join("Cargo.toml"), manifest).expect("write Cargo.toml");
    std::fs::write(src_dir.join("main.rs"), rust_src).expect("write main.rs");
    dir
}

fn run_aot_with_opts(code: &str, test_name: &str, opts: &Options) -> AotRun {
    let rust_src = transpile(code, opts).expect("transpile");
    let dir = write_scratch_project(test_name, &rust_src);
    let output = Command::new("cargo")
        .arg("run")
        .arg("--quiet")
        .arg("--release")
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run cargo");
    AotRun {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        rust_src,
    }
}

fn run_generated_source(test_name: &str, rust_src: String) -> AotRun {
    let dir = write_scratch_project(test_name, &rust_src);
    let output = Command::new("cargo")
        .arg("run")
        .arg("--quiet")
        .arg("--release")
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run cargo");
    AotRun {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        rust_src,
    }
}

#[cfg(unix)]
fn run_aot_with_closed_stdout(code: &str, test_name: &str, opts: &Options) -> AotRun {
    use std::io::Write;

    let rust_src = transpile(code, opts).expect("transpile");
    let dir = write_scratch_project(test_name, &rust_src);
    let build = Command::new("cargo")
        .arg("build")
        .arg("--quiet")
        .arg("--release")
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("build generated program");
    assert!(
        build.status.success(),
        "generated program failed to build:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    let mut child = Command::new(dir.join("target/release/program"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn generated program");
    drop(child.stdout.take().expect("generated stdout"));
    let mut stdin = child.stdin.take().expect("generated stdin");
    stdin.write_all(b"\n").expect("release generated program");
    drop(stdin);
    let output = child
        .wait_with_output()
        .expect("wait for generated program");

    AotRun {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        rust_src,
    }
}

fn run_aot_with_modules_and_opts(
    code: &str,
    test_name: &str,
    modules: &[(&str, &str)],
    opts: &Options,
) -> AotRun {
    let mut opts = opts.clone();
    opts.module_resolver = Some(modules_from_map(modules.iter().map(|(k, v)| (*k, *v))));
    run_aot_with_opts(code, test_name, &opts)
}

struct AotRun {
    status: Option<i32>,
    stdout: String,
    stderr: String,
    rust_src: String,
}

fn run_aot(code: &str, test_name: &str) -> String {
    let run = run_aot_with_opts(code, test_name, &Options::default());
    if run.status != Some(0) {
        panic!(
            "cargo run failed for {}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- generated ---\n{}",
            test_name, run.stdout, run.stderr, run.rust_src,
        );
    }
    run.stdout
}

fn assert_aot_modes_match_walker(code: &str, test_name: &str) {
    let expected = walker_output(code);
    assert_eq!(run_aot(code, &format!("{test_name}_native")), expected);
    let sandbox = run_aot_with_opts(
        code,
        &format!("{test_name}_sandbox"),
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        sandbox.status,
        Some(0),
        "sandbox stderr:\n{}",
        sandbox.stderr
    );
    assert_eq!(sandbox.stdout, expected);
}

#[test]
#[ignore]
fn e2e_ref_parameters_commit_rollback_and_dynamic_dispatch() {
    let source = r#"
fn update(ref left, ref right, delta) {
  left = left + delta
  right = right + left
}
let a = 1
let b = 10
let f = update
f(ref a, ref b, 2)
print(a, b)

fn fail(ref value) {
  value = 99
  panic("rollback")
}
fn attempted() { fail(ref a) }
print(try_call(attempted), a)

fn inner(ref value) { value = value + 5 }
fn outer(ref value) {
  inner(ref value)
  value = value * 2
}
outer(ref a)
print(a)
"#;
    let expected = "3 13\nResult::Err(RuntimeError { message: \"rollback\", line: 14 }) 3\n16";
    assert_eq!(run_aot(source, "ref_parameters_native"), expected);
    let sandbox = run_aot_with_opts(
        source,
        "ref_parameters_sandbox",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        sandbox.status,
        Some(0),
        "sandbox stderr:\n{}",
        sandbox.stderr
    );
    assert_eq!(sandbox.stdout, expected);
}

#[test]
#[ignore]
fn e2e_ref_preflight_and_snapshot_order() {
    let source = r#"
fn side() { print("arg"); return 7 }
fn target(ref value, ordinary) { print(value); value = ordinary }
fn make() { print("callee"); return target }
fn invalid_target() { make()(ref [1], side()) }
print(try_call(invalid_target))
fn missing_marker() { make()(side(), side()) }
print(try_call(missing_marker))

let value = 1
fn ordinary() { value = 2; return 9 }
target(ref value, ordinary())
print(value)
"#;
    let output = run_aot(source, "ref_preflight_order_native");
    assert_eq!(
        output,
        concat!(
            "callee\n",
            "Result::Err(RuntimeError { message: \"`ref` argument 1 must name a mutable variable\", line: 5 })\n",
            "callee\n",
            "Result::Err(RuntimeError { message: \"argument 1 to `target` must be passed with `ref`\", line: 7 })\n",
            "2\n",
            "9",
        )
    );
}

#[test]
#[ignore]
fn e2e_sandbox_ref_memory_limit_rolls_back_all_targets() {
    let source = r#"
let left = 0
let right = ""
fn allocate(ref a, ref b) {
  a = 7
  b = "x" * 4000
}
pub fn trigger() { allocate(ref left, ref right) }
pub fn read_left() { return left }
pub fn read_right() { return right }
"#;
    let mut rust_src = transpile(
        source,
        &Options {
            emit_main: false,
            use_nybl_sys: false,
            sandbox: true,
            ..Options::default()
        },
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct Host;
impl ::nybl::NyblHost for Host {
    fn call(&mut self, _name: &str, _args: &[::nybl::value::Value], _line: u32) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> {
        None
    }
}
fn main() {
    let limits = ::nybl::NyblLimits { max_steps: 100, max_memory: 1_024 };
    let mut host = Host;
    let mut instance = NyblInstance::load(&mut host, &limits).unwrap();
    let error = instance.call("trigger", &[], &mut host).unwrap_err();
    assert!(error.is_fatal && error.message.contains("Memory limit exceeded"));
    assert!(matches!(instance.call("read_left", &[], &mut host).unwrap(), ::nybl::value::Value::Int(0)));
    match instance.call("read_right", &[], &mut host).unwrap() {
        ::nybl::value::Value::Str(value) => assert!(value.is_empty()),
        other => panic!("expected string, got {}", other.type_name()),
    }
    println!("ok");
}
"#,
    );
    let run = run_generated_source("sandbox_ref_memory_rollback", rust_src);
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed:\n{}",
        run.stderr
    );
    assert_eq!(run.stdout, "ok");
}

#[test]
#[ignore]
fn e2e_user_method_explicit_ref_parameters() {
    let source = r#"
struct Counter { amount }
fn Counter.add_to(self, ref destination, delta) {
  destination = destination + self.amount + delta
  return destination
}
let counter = Counter { amount: 3 }
let value = 4
print(counter.add_to(ref value, 2))
print(value)
"#;
    assert_eq!(run_aot(source, "method_ref_native"), "9\n9");
    let sandbox = run_aot_with_opts(
        source,
        "method_ref_sandbox",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        sandbox.status,
        Some(0),
        "sandbox stderr:\n{}",
        sandbox.stderr
    );
    assert_eq!(sandbox.stdout, "9\n9");
}

#[test]
#[ignore]
fn e2e_ref_method_receiver_commit_order_and_rollback() {
    let source = r#"
struct Counter { amount }
fn Counter.add(ref self, amount) {
  self.amount = self.amount + amount
  return self.amount
}
fn Counter.fail(ref self) {
  self.amount = 99
  panic("stop")
}
fn Counter.add_from(ref self, ref other, extra) {
  self.amount = self.amount + other + extra
  other = other + 1
  return self.amount
}
fn Counter.push(ref self, amount) {
  self.amount += amount
}
let counter = Counter { amount: 1 }
fn side() {
  counter.amount = 10
  return 2
}
print(counter.add(side()), counter.amount)
fn attempt() { counter.fail() }
print(try_call(attempt), counter.amount)
let other = 3
print(counter.add_from(ref other, 1), counter.amount, other)
counter.push(1)
print(counter.amount)
"#;
    assert_eq!(
        run_aot(source, "method_receiver_ref_native"),
        "12 12\nResult::Err(RuntimeError { message: \"stop\", line: 9 }) 12\n16 16 4\n17"
    );
    let sandbox = run_aot_with_opts(
        source,
        "method_receiver_ref_sandbox",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        sandbox.status,
        Some(0),
        "sandbox stderr:\n{}",
        sandbox.stderr
    );
    assert_eq!(
        sandbox.stdout,
        "12 12\nResult::Err(RuntimeError { message: \"stop\", line: 9 }) 12\n16 16 4\n17"
    );
}

#[test]
#[ignore]
fn e2e_module_export_ref_parameters_preflight_commit_and_rollback() {
    let source = r#"
use dep as api
let value = 1
api.bump(ref value)
print(value)
fn attempted() { api.fail(ref value) }
print(try_call(attempted), value)
fn side_effect() { print("ARG-RAN"); return 0 }
fn missing_marker() { api.bump(side_effect()) }
print(try_call(missing_marker), value)
"#;
    let modules = [(
        "dep",
        r#"
fn bump(ref value) { value = value + 1 }
fn fail(ref value) { value = 99; panic("rollback") }
"#,
    )];
    let expected = concat!(
        "2\n",
        "Result::Err(RuntimeError { message: \"rollback\", line: 3 }) 2\n",
        "Result::Err(RuntimeError { message: \"argument 1 to `bump` must be passed with `ref`\", line: 9 }) 2",
    );
    let native = run_aot_with_modules_and_opts(
        source,
        "module_export_ref_native",
        &modules,
        &Options::default(),
    );
    assert_eq!(native.status, Some(0), "native stderr:\n{}", native.stderr);
    assert_eq!(native.stdout, expected);
    assert!(!native.stdout.contains("ARG-RAN"));

    let sandbox = run_aot_with_modules_and_opts(
        source,
        "module_export_ref_sandbox",
        &modules,
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        sandbox.status,
        Some(0),
        "sandbox stderr:\n{}",
        sandbox.stderr
    );
    assert_eq!(sandbox.stdout, expected);
    assert!(!sandbox.stdout.contains("ARG-RAN"));
}

#[test]
#[ignore]
fn e2e_method_redeclaration_preflight_uses_live_site_modes() {
    let source = r#"
struct Box { amount }
if true {
  fn Box.apply(self, ref output) { output = self.amount }
} else {
  fn Box.apply(self, output) { panic("unreached") }
}
let box = Box { amount: 7 }
let value = 1
box.apply(ref value)
print(value)
"#;
    assert_eq!(run_aot(source, "method_ref_redeclaration_native"), "7");
    let sandbox = run_aot_with_opts(
        source,
        "method_ref_redeclaration_sandbox",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        sandbox.status,
        Some(0),
        "sandbox stderr:\n{}",
        sandbox.stderr
    );
    assert_eq!(sandbox.stdout, "7");
}

#[test]
#[ignore]
fn e2e_method_preflight_retains_exact_adapter_across_argument_redeclaration() {
    let source = r#"
struct Box { value }
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
"#;
    assert_eq!(run_aot(source, "method_stable_adapter_native"), "7\n7");
    let sandbox = run_aot_with_opts(
        source,
        "method_stable_adapter_sandbox",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        sandbox.status,
        Some(0),
        "sandbox stderr:\n{}",
        sandbox.stderr
    );
    assert_eq!(sandbox.stdout, "7\n7");
}

#[test]
#[ignore]
fn e2e_captured_implicit_and_optional_import_ref_targets_are_fenced() {
    let source = r#"
use dep
fn side() { print("ARG-RAN"); return 1 }
fn take(ref value) { value = 9 }
fn local_capture() {
  let values = []
  let action = fn() { values.push(side()) }
  return try_call(action)
}
fn optional_implicit() {
  use optional
  let action = fn() { values.push(side()) }
  return try_call(action)
}
fn optional_explicit() {
  use optional
  let action = fn() { take(ref values) }
  return try_call(action)
}
print(local_capture())
print(optional_implicit())
print(optional_explicit())
"#;
    let modules = [("dep", "let ready = true"), ("optional", "let values = []")];
    for (name, opts) in [
        ("captured_ref_fences_native", Options::default()),
        (
            "captured_ref_fences_sandbox",
            Options {
                sandbox: true,
                ..Options::default()
            },
        ),
    ] {
        let run = run_aot_with_modules_and_opts(source, name, &modules, &opts);
        assert_eq!(run.status, Some(0), "{name} stderr:\n{}", run.stderr);
        assert!(!run.stdout.contains("ARG-RAN"), "{name}: {}", run.stdout);
        assert_eq!(
            run.stdout
                .matches("can't target a closure-captured binding")
                .count(),
            3,
            "{name}: {}",
            run.stdout
        );
    }
}

#[test]
#[ignore]
fn e2e_sandbox_host_value_abi_rejects_ref_parameters_before_execution() {
    let source = r#"
let executions = 0
pub fn target(ref value) { executions += 1; value = 99 }
pub fn callback() { return target }
pub fn execution_count() { return executions }
"#;
    let mut rust_src = transpile(
        source,
        &Options {
            emit_main: false,
            use_nybl_sys: false,
            sandbox: true,
            ..Options::default()
        },
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct Host;
impl ::nybl::NyblHost for Host {
    fn call(&mut self, _name: &str, _args: &[::nybl::value::Value], _line: u32) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> { None }
}
fn main() {
    let mut host = Host;
    let mut instance = NyblInstance::load(&mut host, &::nybl::NyblLimits::standard()).unwrap();
    let wrong_arity = instance.call("target", &[], &mut host).unwrap_err();
    assert!(wrong_arity.message.contains("expects 1 argument, but got 0"), "{}", wrong_arity.message);
    let direct = instance.call("target", &[::nybl::value::Value::Int(1)], &mut host).unwrap_err();
    assert!(direct.message.contains("must be passed with `ref`"), "{}", direct.message);
    let callback = instance.call("callback", &[], &mut host).unwrap();
    let indirect = instance.call_value(&callback, &[::nybl::value::Value::Int(1)], &mut host).unwrap_err();
    assert!(indirect.message.contains("must be passed with `ref`"), "{}", indirect.message);
    assert!(matches!(instance.call("execution_count", &[], &mut host).unwrap(), ::nybl::value::Value::Int(0)));
    println!("ok");
}
"#,
    );
    let run = run_generated_source("sandbox_ref_host_value_abi", rust_src);
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed:\n{}",
        run.stderr
    );
    assert_eq!(run.stdout, "ok");
}

#[test]
#[ignore]
fn e2e_sandbox_function_sites_preserve_redeclaration_and_exact_self() {
    let opts = Options {
        sandbox: true,
        ..Options::default()
    };
    let cases = [
        (
            "pub fn f() { return 1 } pub fn f() { return 2 }\nprint(f())",
            "sandbox_same_line_function_sites",
            "2",
        ),
        (
            "fn outer() { fn f() { return 1 } fn f() { return 2 } return f() }\nprint(outer())",
            "sandbox_nested_function_sites",
            "2",
        ),
        (
            "fn outer() { fn inner() { return 1 } return inner() }\npub fn later() { return 3 }\nprint(later())",
            "sandbox_nested_before_later_persistent_site",
            "3",
        ),
        (
            "if true { fn h() { return 7 } }\nprint(h())",
            "sandbox_reached_block_function_persists",
            "7",
        ),
        (
            "fn install() { fn h() { return 8 } }\ninstall()\nprint(h())",
            "sandbox_called_function_installs_global_function",
            "8",
        ),
        (
            "let install = fn() { fn h() { return 9 } }\ninstall()\nprint(h())",
            "sandbox_lambda_installs_global_function",
            "9",
        ),
        (
            "let install = match 1 { 1 => fn() { fn h() { return 10 } }, _ => fn() { fn h() { return 11 } } }\ninstall()\nprint(h())",
            "sandbox_match_arm_lambda_installs_global_function",
            "10",
        ),
        (
            "fn f() { return 1 }\nif true { fn f() { return 2 } }\nprint(f())",
            "sandbox_reached_nested_redeclaration_updates_ordinary_lookup",
            "2",
        ),
        (
            "fn f() { return 1 }\nif false { fn f() { return 2 } }\nprint(f())",
            "sandbox_dead_nested_redeclaration_preserves_ordinary_lookup",
            "1",
        ),
        (
            "fn f() { return 1 }\nfn get() { return f }\nfn f() { return 2 }\nprint(get()())",
            "sandbox_non_self_function_values_resolve_active_site",
            "2",
        ),
        (
            "struct S {}\nfn S.install(self) { fn h() { return 14 } }\nlet s = S {}\ns.install()\nprint(h())",
            "sandbox_method_body_installs_global_function",
            "14",
        ),
        (
            "fn f(n) { if n == 0 { return 1 } return f(n - 1) }\nlet old = f\nfn f(n) { return 9 }\nprint(old(2))",
            "sandbox_retained_self_recursion",
            "1",
        ),
        (
            "fn f() { return f }\nlet old = f\nfn f(x) { return x }\nlet again = old()\nprint(again())",
            "sandbox_retained_self_value",
            "<fn f>",
        ),
    ];
    for (source, name, expected) in cases {
        let run = run_aot_with_opts(source, name, &opts);
        assert_eq!(
            run.status,
            Some(0),
            "generated program failed: {}",
            run.stderr
        );
        assert_eq!(run.stdout, expected);
    }
}

#[test]
#[ignore]
fn e2e_sandbox_unreached_nested_functions_are_not_statically_callable() {
    let opts = Options {
        sandbox: true,
        ..Options::default()
    };
    for (source, name) in [
        (
            "let g = fn() { if true { f(); fn f() {} } }\nprint(try_call(g))",
            "sandbox_nested_call_before_declaration",
        ),
        (
            "let g = fn() { if false { fn f() {} } f() }\nprint(try_call(g))",
            "sandbox_nested_call_outside_dead_branch",
        ),
    ] {
        let expected = walker_output(source);
        let run = run_aot_with_opts(source, name, &opts);
        assert_eq!(
            run.status,
            Some(0),
            "generated program failed: {}",
            run.stderr
        );
        assert_eq!(run.stdout, expected);
    }
}

#[test]
#[ignore]
fn e2e_sandbox_abi_uses_final_reached_public_declaration_sites() {
    let source = "pub fn first() { return 1 }\npub fn hidden() { return 2 }\nfn hidden() { return 3 }\npub fn first(x) { return x }\nfn install() { fn first(x) { return 99 } }\ninstall()\nreturn\npub fn skipped() { return 4 }";
    let mut rust_src = transpile(
        source,
        &Options {
            emit_main: false,
            use_nybl_sys: false,
            sandbox: true,
            ..Options::default()
        },
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct Host;
impl ::nybl::NyblHost for Host {
    fn call(&mut self, _name: &str, _args: &[::nybl::value::Value], _line: u32) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> { None }
}
fn main() {
    let limits = ::nybl::NyblLimits::standard();
    let mut host = Host;
    let memory = ::nybl::memory::MemoryContext::__new(limits.max_memory);
    let mut state = __nybl_load_state(&mut host, &limits, memory.clone()).unwrap();
    let entries = __nybl_instance_entry_points(&state);
    println!("{}", entries.iter().map(|entry| format!("{}/{}", entry.name(), entry.arity())).collect::<Vec<_>>().join(","));
    let site = state.abi_declarations.iter().copied().find(|site| __NYBL_FUNCTION_SITES[*site].name == "first" && __NYBL_FUNCTION_SITES[*site].is_public).unwrap();
    let mut ctx = Ctx { host: &mut host, state: &mut state, memory, steps: 0, call_depth: 0, max_steps: limits.max_steps };
    println!("{}", __nybl_call_function_site(&mut ctx, site, vec![::nybl::value::Value::Int(7)], 0).unwrap());
    println!("{}", __nybl_call_active_function(&mut ctx, "<root>", "first", vec![::nybl::value::Value::Int(7)], 0).unwrap());
}
"#,
    );
    let run = run_generated_source("sandbox_exact_final_abi", rust_src);
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed: {}",
        run.stderr
    );
    assert_eq!(run.stdout, "first/1\n7\n99");
}

#[test]
#[ignore]
fn e2e_sandbox_generated_instance_retains_state_and_callbacks() {
    let source = r#"init()
let count = 0
pub fn next(delta) { count += delta; return count }
fn private() { return 99 }
pub fn callback() {
    return fn(delta) { count += delta; return count }
}
pub fn recurse(n) {
    if n == 0 { return 0 }
    return recurse(n - 1)
}
pub fn recurse_value() { return recurse }
pub fn attempt(f) { return try_call(f) }
pub fn invoke(f) { return f() }
pub fn map_callback(f) { return Result::Ok(1).map(f) }
pub fn reenter() { return host_reenter() }"#;
    let mut rust_src = transpile(
        source,
        &Options {
            emit_main: false,
            use_nybl_sys: false,
            sandbox: true,
            ..Options::default()
        },
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct HostA { init_calls: usize }
impl ::nybl::NyblHost for HostA {
    fn call(&mut self, name: &str, _args: &[::nybl::value::Value], _line: u32) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> {
        if name == "init" {
            self.init_calls += 1;
            Some(Ok(::nybl::value::Value::None))
        } else {
            None
        }
    }
}
struct HostB;
impl ::nybl::NyblHost for HostB {
    fn call(&mut self, name: &str, _args: &[::nybl::value::Value], _line: u32) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> {
        (name == "init").then_some(Ok(::nybl::value::Value::None))
    }
}
fn expect_int(value: ::nybl::value::Value, expected: i64) {
    assert!(matches!(value, ::nybl::value::Value::Int(actual) if actual == expected));
}
fn main() {
    let limits = ::nybl::NyblLimits::standard();
    let mut host_a = HostA { init_calls: 0 };
    let mut instance = NyblInstance::load(&mut host_a as &mut dyn ::nybl::NyblHost, &limits).unwrap();
    assert_eq!(host_a.init_calls, 1);
    drop(host_a);

    let entries = instance.entry_points().iter().map(|entry| format!("{}/{}", entry.name(), entry.arity())).collect::<Vec<_>>();
    assert_eq!(entries, ["next/1", "callback/0", "recurse/1", "recurse_value/0", "attempt/1", "invoke/1", "map_callback/1", "reenter/0"]);
    let mut host_b = HostB;
    expect_int(instance.call("next", &[::nybl::value::Value::Int(1)], &mut host_b).unwrap(), 1);
    expect_int(instance.call("next", &[::nybl::value::Value::Int(2)], &mut host_b).unwrap(), 3);
    assert!(instance.call("private", &[], &mut host_b).unwrap_err().message.contains("Public entry point"));
    assert!(instance.call("next", &[], &mut host_b).unwrap_err().message.contains("expects 1 argument"));

    let callback = instance.call("callback", &[], &mut host_b).unwrap();
    expect_int(instance.call_value(&callback, &[::nybl::value::Value::Int(4)], &mut host_b).unwrap(), 7);
    expect_int(instance.call_value(&callback, &[::nybl::value::Value::Int(5)], &mut host_b).unwrap(), 12);
    expect_int(nybl_entry_points::__nybl_entry_6e657874(&mut instance, &[::nybl::value::Value::Int(1)], &mut host_b).unwrap(), 13);

    expect_int(instance.call("recurse", &[::nybl::value::Value::Int(63)], &mut host_b).unwrap(), 0);
    let boundary = instance.call("recurse", &[::nybl::value::Value::Int(64)], &mut host_b).unwrap_err();
    assert!(boundary.message.contains("nested function calls"), "{}", boundary.message);
    let deep = instance.call("recurse", &[::nybl::value::Value::Int(100)], &mut host_b).unwrap_err();
    assert!(deep.message.contains("nested function calls"), "{}", deep.message);
    expect_int(instance.call("recurse", &[::nybl::value::Value::Int(1)], &mut host_b).unwrap(), 0);

    let recurse_value = instance.call("recurse_value", &[], &mut host_b).unwrap();
    expect_int(instance.call_value(&recurse_value, &[::nybl::value::Value::Int(63)], &mut host_b).unwrap(), 0);
    assert!(instance.call_value(&recurse_value, &[::nybl::value::Value::Int(64)], &mut host_b).unwrap_err().message.contains("nested function calls"));
    expect_int(instance.call_value(&recurse_value, &[::nybl::value::Value::Int(1)], &mut host_b).unwrap(), 0);

    let mut second = NyblInstance::load(&mut host_b, &limits).unwrap();
    assert!(second.call_value(&callback, &[], &mut host_b).unwrap_err().message.contains("different Nybl engine instance"));
    let second_callback = second.call("callback", &[], &mut host_b).unwrap();
    let foreign_attempt = instance.call("attempt", &[second_callback.clone()], &mut host_b).unwrap();
    assert!(matches!(foreign_attempt, ::nybl::value::Value::EnumVariant(value) if value.variant() == "Err"));
    assert!(instance.call("invoke", &[second_callback.clone()], &mut host_b).unwrap_err().message.contains("different Nybl engine instance"));
    assert!(instance.call("map_callback", &[second_callback], &mut host_b).unwrap_err().message.contains("different Nybl engine instance"));
    let arity_attempt = instance.call("attempt", &[recurse_value.clone()], &mut host_b).unwrap();
    assert!(matches!(arity_attempt, ::nybl::value::Value::EnumVariant(value) if value.variant() == "Err"));
    expect_int(instance.call("next", &[::nybl::value::Value::Int(1)], &mut host_b).unwrap(), 14);

    let mut walker = ::nybl::NyblInstance::load(
        "pub fn callback() { return fn() { return 1 } }",
        &mut host_b,
        &limits,
    ).unwrap();
    let foreign = walker.call("callback", &[], &mut host_b).unwrap();
    assert!(instance.call_value(&foreign, &[], &mut host_b).unwrap_err().message.contains("different Nybl engine instance"));
    assert!(instance.call_value(&::nybl::value::Value::Int(1), &[], &mut host_b).unwrap_err().message.contains("expected function"));
    let external_ast = ::nybl::value::Value::new_fn(Vec::new(), Vec::new(), Vec::new(), None);
    assert!(instance.call_value(&external_ast, &[], &mut host_b).unwrap_err().message.contains("wasn't compiled for the AOT"));
    let external_body: ::std::rc::Rc<dyn ::core::any::Any + 'static> = ::std::rc::Rc::new(1usize);
    let external_compiled = ::nybl::value::Value::new_compiled_fn(Vec::new(), Vec::new(), external_body, None);
    assert!(instance.call_value(&external_compiled, &[], &mut host_b).unwrap_err().message.contains("wasn't compiled by the AOT"));

    instance.in_operation.set(true);
    assert!(instance.call("missing", &[::nybl::value::Value::None], &mut host_b).unwrap_err().message.contains("cannot be re-entered"));
    instance.in_operation.set(false);
    expect_int(instance.call("next", &[::nybl::value::Value::Int(1)], &mut host_b).unwrap(), 15);
    println!("ok");
}
"#,
    );
    let run = run_generated_source("sandbox_generated_instance_state_callbacks", rust_src);
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed: {}",
        run.stderr
    );
    assert_eq!(run.stdout, "ok");
}

#[test]
#[ignore]
fn e2e_sandbox_generated_instance_scopes_limits_and_host_memory() {
    let mut source = r#"boot()
let mutation = 0
pub fn external_value() { return host_value() }
pub fn detach_value() {
    let value = host_value()
    value.push("script-owned-abcdefghijklmnopqrstuvwxyz0123456789")
    return value
}
pub fn print_it() { print("hello") }
pub fn hint_it() { return missing_host_function() }
pub fn spin(n) { repeat n { } return n }
pub fn call_other() { return nested_other() }
pub fn make_result() {
    let value = []
    repeat 8 {
        value.push("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789")
    }
    return value
}
pub fn mutate_then_fail() { mutation += 1; panic("boom") }
pub fn mutate_then_spin() { mutation += 1; repeat 200 { } }
pub fn read_mutation() { return mutation }
"#
    .to_string();
    let oversized_items = vec!["0"; 256].join(",");
    source.push_str("\npub fn transient_peak() { return [");
    source.push_str(&oversized_items);
    source.push_str("] }\npub fn retain_over_limit() { retain_charged([");
    source.push_str(&oversized_items);
    source.push_str("]) }\n");
    let mut rust_src = transpile(
        &source,
        &Options {
            emit_main: false,
            use_nybl_sys: false,
            sandbox: true,
            ..Options::default()
        },
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct InnerHost;
impl ::nybl::NyblHost for InnerHost {
    fn call(&mut self, name: &str, _args: &[::nybl::value::Value], _line: u32) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> {
        (name == "boot").then_some(Ok(::nybl::value::Value::None))
    }
}
struct Host {
    retained: ::std::cell::RefCell<Vec<::nybl::value::Value>>,
    charged: Option<::nybl::value::Value>,
    other: Option<(NyblInstance, InnerHost)>,
}
impl Host {
    fn retain_external(&self) -> ::nybl::value::Value {
        let value = ::nybl::value::Value::new_array(vec![
            ::nybl::value::Value::new_str("host-owned-abcdefghijklmnopqrstuvwxyz0123456789".to_string()),
        ]);
        self.retained.borrow_mut().push(value.clone());
        value
    }
}
impl ::nybl::NyblHost for Host {
    fn call(&mut self, name: &str, args: &[::nybl::value::Value], _line: u32) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> {
        if name == "boot" {
            self.retain_external();
            return Some(Ok(::nybl::value::Value::None));
        }
        if name == "host_value" {
            return Some(Ok(self.retain_external()));
        }
        if name == "retain_charged" {
            self.charged = Some(args[0].clone());
            return Some(Ok(::nybl::value::Value::None));
        }
        if name == "nested_other" {
            let (mut other, mut inner_host) = self.other.take().expect("nested instance installed");
            let result = other.call("make_result", &[], &mut inner_host);
            self.other = Some((other, inner_host));
            return Some(result);
        }
        None
    }
    fn on_print(&mut self, _message: &str) {
        self.retain_external();
    }
    fn on_tick(&mut self) -> Result<(), ::nybl::error::NyblError> {
        self.retain_external();
        Ok(())
    }
    fn function_hint(&self) -> &str {
        self.retain_external();
        "host hint"
    }
}
fn main() {
    let limits = ::nybl::NyblLimits { max_steps: 100, max_memory: 1_600 };
    let mut host = Host {
        retained: ::std::cell::RefCell::new(Vec::new()),
        charged: None,
        other: None,
    };
    let mut instance = NyblInstance::load(&mut host, &limits).unwrap();
    let baseline = instance.memory.__used();
    assert_eq!(baseline, 0, "top-level host allocations must not enter the instance account");
    let mut inner_host = InnerHost;
    let other = NyblInstance::load(&mut inner_host, &limits).unwrap();
    let other_baseline = other.memory.__used();
    assert_eq!(other_baseline, 0);
    host.other = Some((other, inner_host));

    let external = instance.call("external_value", &[], &mut host).unwrap();
    assert_eq!(instance.memory.__used(), baseline);
    let nested = instance.call("call_other", &[], &mut host).unwrap();
    assert_eq!(instance.memory.__used(), baseline);
    assert!(host.other.as_ref().unwrap().0.memory.__used() > other_baseline);
    drop(nested);
    assert_eq!(host.other.as_ref().unwrap().0.memory.__used(), other_baseline);
    instance.call("print_it", &[], &mut host).unwrap();
    let hint = instance.call("hint_it", &[], &mut host).unwrap_err();
    assert_eq!(hint.friendly_hint.as_deref(), Some("host hint"));
    assert_eq!(instance.memory.__used(), baseline);
    drop(external);

    let detached = instance.call("detach_value", &[], &mut host).unwrap();
    assert!(instance.memory.__used() > baseline);
    drop(detached);
    assert_eq!(instance.memory.__used(), baseline);

    let steps = instance.call("spin", &[::nybl::value::Value::Int(200)], &mut host).unwrap_err();
    assert!(steps.is_fatal && steps.message.contains("too many steps"));
    assert!(matches!(instance.call("spin", &[::nybl::value::Value::Int(1)], &mut host).unwrap(), ::nybl::value::Value::Int(1)));
    assert_eq!(instance.memory.__used(), baseline);

    let ordinary = instance.call("mutate_then_fail", &[], &mut host).unwrap_err();
    assert!(!ordinary.is_fatal && ordinary.message.contains("boom"));
    assert!(matches!(instance.call("read_mutation", &[], &mut host).unwrap(), ::nybl::value::Value::Int(1)));
    let fatal = instance.call("mutate_then_spin", &[], &mut host).unwrap_err();
    assert!(fatal.is_fatal && fatal.message.contains("too many steps"));
    assert!(matches!(instance.call("read_mutation", &[], &mut host).unwrap(), ::nybl::value::Value::Int(2)));

    let held = instance.call("make_result", &[], &mut host).unwrap();
    let held_memory = instance.memory.__used();
    assert!(held_memory > baseline);
    let nested_while_held = instance.call("call_other", &[], &mut host).unwrap();
    assert_eq!(instance.memory.__used(), held_memory);
    assert!(host.other.as_ref().unwrap().0.memory.__used() > other_baseline);
    drop(nested_while_held);
    assert_eq!(instance.memory.__used(), held_memory);
    assert_eq!(host.other.as_ref().unwrap().0.memory.__used(), other_baseline);
    let memory = instance.call("make_result", &[], &mut host).unwrap_err();
    assert!(memory.is_fatal && memory.message.contains("Memory limit exceeded"));
    drop(held);
    assert_eq!(instance.memory.__used(), baseline);
    let released = instance.call("make_result", &[], &mut host).unwrap();
    drop(released);
    assert_eq!(instance.memory.__used(), baseline);

    instance.in_operation.set(true);
    let reentry = instance.call("missing", &[::nybl::value::Value::None], &mut host).unwrap_err();
    instance.in_operation.set(false);
    assert!(reentry.message.contains("cannot be re-entered"));
    assert!(matches!(instance.call("spin", &[::nybl::value::Value::Int(1)], &mut host).unwrap(), ::nybl::value::Value::Int(1)));

    let mut transient = NyblInstance::load(&mut host, &limits).unwrap();
    let transient_baseline = transient.memory.__used();
    assert_eq!(transient_baseline, 0);
    let peak = transient.call("transient_peak", &[], &mut host).unwrap_err();
    assert!(peak.is_fatal && peak.message.contains("Memory limit exceeded"));
    assert_eq!(transient.memory.__used(), transient_baseline);
    assert!(matches!(
        transient.call("spin", &[::nybl::value::Value::Int(1)], &mut host).unwrap(),
        ::nybl::value::Value::Int(1)
    ));

    let mut retained = NyblInstance::load(&mut host, &limits).unwrap();
    let retained_error = retained.call("retain_over_limit", &[], &mut host).unwrap_err();
    assert!(retained_error.is_fatal && retained_error.message.contains("Memory limit exceeded"));
    assert!(retained.memory.__used() > limits.max_memory);
    let retained_again = retained.call("spin", &[::nybl::value::Value::Int(1)], &mut host).unwrap_err();
    assert!(retained_again.is_fatal && retained_again.message.contains("Memory limit exceeded"));
    drop(host.charged.take());
    assert_eq!(retained.memory.__used(), 0);
    assert!(matches!(
        retained.call("spin", &[::nybl::value::Value::Int(1)], &mut host).unwrap(),
        ::nybl::value::Value::Int(1)
    ));
    assert!(matches!(instance.call("spin", &[::nybl::value::Value::Int(1)], &mut host).unwrap(), ::nybl::value::Value::Int(1)));
    println!("ok");
}
"#,
    );
    let run = run_generated_source("sandbox_generated_instance_limits_memory", rust_src);
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed: {}",
        run.stderr
    );
    assert_eq!(run.stdout, "ok");
}

#[test]
#[ignore]
fn e2e_sandbox_generated_instance_retains_modules_types_methods_and_rng() {
    let root = r#"use wrapper as api
pub fn next() { return api.next() }
pub fn make(value) {
    let point = api.Point { value: value }
    return point.bump()
}
pub fn random() { return rand(1000000000) }"#;
    let modules = [
        (
            "dep",
            r#"let count = 0
struct Point { value }
fn Point.bump(self) { return self.value + 1 }
fn next() { count += 1; return count }"#,
        ),
        ("wrapper", "use dep"),
    ];
    let mut rust_src = transpile(
        root,
        &Options {
            emit_main: false,
            use_nybl_sys: false,
            sandbox: true,
            module_resolver: Some(modules_from_map(modules)),
            ..Options::default()
        },
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct Host;
impl ::nybl::NyblHost for Host {
    fn call(&mut self, _name: &str, _args: &[::nybl::value::Value], _line: u32) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> { None }
}
fn expect_int(value: ::nybl::value::Value, expected: i64) {
    assert!(matches!(value, ::nybl::value::Value::Int(actual) if actual == expected));
}
fn int(value: ::nybl::value::Value) -> i64 {
    match value { ::nybl::value::Value::Int(value) => value, other => panic!("expected int, got {}", other.type_name()) }
}
fn main() {
    let limits = ::nybl::NyblLimits::standard();
    let mut host = Host;
    let mut first = NyblInstance::load(&mut host, &limits).unwrap();
    expect_int(first.call("next", &[], &mut host).unwrap(), 1);
    expect_int(first.call("next", &[], &mut host).unwrap(), 2);
    expect_int(first.call("make", &[::nybl::value::Value::Int(41)], &mut host).unwrap(), 42);
    let first_random = int(first.call("random", &[], &mut host).unwrap());
    let second_random = int(first.call("random", &[], &mut host).unwrap());
    assert_ne!(first_random, second_random, "successive calls must advance retained RNG state");
    expect_int(first.call("next", &[], &mut host).unwrap(), 3);

    let mut second = NyblInstance::load(&mut host, &limits).unwrap();
    assert_eq!(int(second.call("random", &[], &mut host).unwrap()), first_random);
    expect_int(second.call("next", &[], &mut host).unwrap(), 1);
    expect_int(first.call("next", &[], &mut host).unwrap(), 4);
    println!("ok");
}
"#,
    );
    let run = run_generated_source("sandbox_generated_instance_modules_types_rng", rust_src);
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed: {}",
        run.stderr
    );
    assert_eq!(run.stdout, "ok");
}

#[test]
#[ignore]
fn e2e_sandbox_failed_module_load_rolls_back_reached_nested_function_sites() {
    let mut opts = Options {
        emit_main: false,
        use_nybl_sys: false,
        sandbox: true,
        ..Options::default()
    };
    opts.module_resolver = Some(modules_from_map([
        (
            "bad",
            "use leaf\nlet own = value\nif true { fn leaked() { return 1 } }\nmissing()",
        ),
        ("leaf", "let value = 1"),
    ]));
    let mut rust_src =
        transpile("let attempt = fn() { use bad }\ntry_call(attempt)", &opts).unwrap();
    rust_src.push_str(
        r#"
struct Host;
impl ::nybl::NyblHost for Host {
    fn call(&mut self, _name: &str, _args: &[::nybl::value::Value], _line: u32) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> { None }
}
fn main() {
    let limits = ::nybl::NyblLimits::standard();
    let mut host = Host;
    let memory = ::nybl::memory::MemoryContext::__new(limits.max_memory);
    let state = __nybl_load_state(&mut host, &limits, memory).unwrap();
    assert!(!state.active_function_sites.keys().any(|(module, _)| module == "bad"));
    assert!(!state.bindings.contains_key("bad"));
    assert!(!state.binding_origins.contains_key("bad"));
    assert!(!state.binding_claims.contains_key("bad"));
    println!("clean");
}
"#,
    );
    let run = run_generated_source("sandbox_failed_module_nested_site_rollback", rust_src);
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed: {}",
        run.stderr
    );
    assert_eq!(run.stdout, "clean");
}

#[test]
#[ignore]
fn e2e_sandbox_dynamic_module_function_exports_follow_runtime_presence() {
    let opts = Options {
        sandbox: true,
        ..Options::default()
    };
    let reached = "if true { fn h() { return 12 } }";
    for (source, name) in [
        ("use m.{h}\nprint(h())", "sandbox_dynamic_export_selective"),
        ("use m\nprint(h())", "sandbox_dynamic_export_glob"),
        ("use m as x\nprint(x.h())", "sandbox_dynamic_export_alias"),
    ] {
        let run = run_aot_with_modules_and_opts(source, name, &[("m", reached)], &opts);
        assert_eq!(
            run.status,
            Some(0),
            "generated program failed: {}",
            run.stderr
        );
        assert_eq!(run.stdout, "12");
    }

    let run = run_aot_with_modules_and_opts(
        "use facade.{h}\nprint(h())",
        "sandbox_dynamic_export_facade",
        &[("m", reached), ("facade", "use m")],
        &opts,
    );
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed: {}",
        run.stderr
    );
    assert_eq!(run.stdout, "12");

    let dead = "if false { fn h() { return 13 } }";
    for (source, name) in [
        (
            "let load = fn() { use m.{h} }\nprint(try_call(load))",
            "sandbox_dead_dynamic_export_selective",
        ),
        (
            "use m\nlet invoke = fn() { return h() }\nprint(try_call(invoke))",
            "sandbox_dead_dynamic_export_glob",
        ),
        (
            "use m as x\nlet invoke = fn() { return x.h() }\nprint(try_call(invoke))",
            "sandbox_dead_dynamic_export_alias",
        ),
    ] {
        let run = run_aot_with_modules_and_opts(source, name, &[("m", dead)], &opts);
        assert_eq!(
            run.status,
            Some(0),
            "generated program failed: {}",
            run.stderr
        );
        assert!(
            run.stdout.contains("Err("),
            "unexpected output: {}",
            run.stdout
        );
    }
}

#[test]
#[ignore]
fn e2e_sandbox_optional_imports_preserve_presence_and_lambda_snapshots() {
    let opts = Options {
        sandbox: true,
        ..Options::default()
    };
    let module = "if true { fn h() { return 12 } }";
    for (source, name, expected) in [
        (
            "fn make() { use m.{h}\nlet saved = fn() { return h() }\nh = 3\nreturn saved }\nprint(make()())",
            "sandbox_local_import_lambda_snapshot",
            "12",
        ),
        (
            "use m\nlet saved = fn() { return h() }\nh = 3\nprint(saved())",
            "sandbox_persistent_import_lambda_snapshot",
            "12",
        ),
        (
            "use m\nlet saved = fn() { return h() }\nfn h() { return 20 }\nprint(saved())\nprint(h())",
            "sandbox_import_before_function_stays_bound_and_captured",
            "12\n12",
        ),
        (
            "fn h() { return 20 }\nuse m\nprint(h())",
            "sandbox_function_before_import_blocks_binding",
            "20",
        ),
    ] {
        let run = run_aot_with_modules_and_opts(source, name, &[("m", module)], &opts);
        assert_eq!(
            run.status,
            Some(0),
            "generated program failed: {}",
            run.stderr
        );
        assert_eq!(run.stdout, expected);
    }

    let run = run_aot_with_modules_and_opts(
        "fn make() { use m\nreturn fn() { return h() } }\nlet saved = make()\nprint(try_call(saved))",
        "sandbox_absent_import_lambda_falls_back_at_call_time",
        &[("m", "if false { fn h() { return 13 } }")],
        &opts,
    );
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed: {}",
        run.stderr
    );
    assert!(
        run.stdout.contains("Err("),
        "unexpected output: {}",
        run.stdout
    );
}

#[test]
#[ignore]
fn e2e_sandbox_dynamic_value_exports_follow_early_return_presence() {
    let opts = Options {
        sandbox: true,
        ..Options::default()
    };
    let module = "let before = 1\nreturn\nlet after = 2";
    let present = run_aot_with_modules_and_opts(
        "use m.{before}\nprint(before)",
        "sandbox_early_return_value_export_present",
        &[("m", module)],
        &opts,
    );
    assert_eq!(
        present.status,
        Some(0),
        "generated program failed: {}",
        present.stderr
    );
    assert_eq!(present.stdout, "1");

    let absent_alias = run_aot_with_modules_and_opts(
        "use facade\nprint(1)",
        "sandbox_early_return_module_alias_absent",
        &[
            ("dep", "struct Point { value }"),
            ("wrapper", "return\nuse dep as api"),
            ("facade", "use wrapper"),
        ],
        &opts,
    );
    assert_eq!(
        absent_alias.status,
        Some(0),
        "generated program failed: {}",
        absent_alias.stderr
    );
    assert_eq!(absent_alias.stdout, "1");

    for (source, name, modules) in [
        (
            "let load = fn() { use m.{after} }\nprint(try_call(load))",
            "sandbox_early_return_value_selective_absent",
            vec![("m", module)],
        ),
        (
            "use m\nlet read = fn() { return after }\nprint(try_call(read))",
            "sandbox_early_return_value_glob_absent",
            vec![("m", module)],
        ),
        (
            "use m as x\nlet read = fn() { return x.after }\nprint(try_call(read))",
            "sandbox_early_return_value_alias_absent",
            vec![("m", module)],
        ),
        (
            "use facade\nlet read = fn() { return after }\nprint(try_call(read))",
            "sandbox_early_return_value_facade_absent",
            vec![("m", module), ("facade", "use m")],
        ),
    ] {
        let run = run_aot_with_modules_and_opts(source, name, &modules, &opts);
        assert_eq!(
            run.status,
            Some(0),
            "generated program failed: {}",
            run.stderr
        );
        assert!(
            run.stdout.contains("Err("),
            "unexpected output: {}",
            run.stdout
        );
    }
}

#[test]
#[ignore]
fn e2e_sandbox_callable_shadows_preserve_module_alias_context() {
    let source = r#"use types as t
fn clobber() {
    let t = 0
    t = 1
}
fn clobber_import() {
    use wrapper
    t = 3
}
fn clobber_param(t) { t = 2 }
fn probe(value) {
    return match value {
        t.Point { value: found } => found,
        _ => 0,
    }
}
let point = t.Point { value: 42 }
clobber()
clobber_import()
clobber_param(0)
print(probe(point))"#;
    let run = run_aot_with_modules_and_opts(
        source,
        "sandbox_callable_shadow_preserves_module_alias_context",
        &[
            ("types", "struct Point { value }"),
            ("wrapper", "use types as t"),
        ],
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed: {}",
        run.stderr
    );
    assert_eq!(run.stdout, "42");
}

#[test]
#[ignore]
fn e2e_sandbox_reassigned_alias_patterns_read_authoritative_binding() {
    let source = r#"use first as dep
use second as other
fn probe(value) {
    return match value {
        dep.Point { value: found } => found,
        _ => 0,
    }
}
fn select_second() { dep = other }
let first_value = dep.Point { value: 1 }
print(probe(first_value))
select_second()
let second_value = dep.Point { value: 2 }
print(probe(second_value))
print(match second_value {
    dep.Point { value: found } => found,
    _ => 0,
})"#;
    let run = run_aot_with_modules_and_opts(
        source,
        "sandbox_reassigned_alias_patterns_read_authoritative_binding",
        &[
            ("first", "struct Point { value }"),
            ("second", "struct Point { value }"),
        ],
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed: {}",
        run.stderr
    );
    assert_eq!(run.stdout, "1\n2\n2");

    let module_body = run_aot_with_modules_and_opts(
        "use switched\nprint(result)",
        "sandbox_reassigned_alias_module_body_pattern_reads_authoritative_binding",
        &[
            ("first", "struct Point { value }"),
            ("second", "struct Point { value }"),
            (
                "switched",
                r#"use first as dep
use second as other
fn select_second() { dep = other }
select_second()
let value = dep.Point { value: 3 }
let result = match value {
    dep.Point { value: found } => found,
    _ => 0,
}"#,
            ),
        ],
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        module_body.status,
        Some(0),
        "generated program failed: {}",
        module_body.stderr
    );
    assert_eq!(module_body.stdout, "3");
}

#[test]
#[ignore]
fn e2e_sandbox_local_flat_import_tracks_module_alias_presence() {
    let opts = Options {
        sandbox: true,
        ..Options::default()
    };
    let dep = r#"struct Point { value }
enum State { Item(value), Empty }
fn make(value) { return Point { value: value } }"#;
    let wrapper = "use dep as api";
    let present = run_aot_with_modules_and_opts(
        r#"fn build() {
    use wrapper
    let point = api.make(7)
    let state = api.State::Item(point.value)
    return match state {
        api.State::Item(value) => value,
        _ => 0,
    }
}
fn maker() {
    use wrapper
    return fn() {
        let point = api.Point { value: 11 }
        return match point {
            api.Point { value: found } => found,
            _ => 0,
        }
    }
}
print(build())
let saved = maker()
print(saved())"#,
        "sandbox_local_flat_import_module_alias_present",
        &[("dep", dep), ("wrapper", wrapper)],
        &opts,
    );
    assert_eq!(
        present.status,
        Some(0),
        "generated program failed: {}",
        present.stderr
    );
    assert_eq!(present.stdout, "7\n11");

    let absent = run_aot_with_modules_and_opts(
        "fn build() { use wrapper\nreturn api.Point { value: 1 } }\nprint(try_call(build))",
        "sandbox_local_flat_import_module_alias_absent",
        &[("dep", dep), ("wrapper", "return\nuse dep as api")],
        &opts,
    );
    assert_eq!(
        absent.status,
        Some(0),
        "generated program failed: {}",
        absent.stderr
    );
    assert!(
        absent.stdout.contains("Err("),
        "unexpected output: {}",
        absent.stdout
    );
    assert!(
        absent.stdout.contains("isn't a module alias in scope"),
        "unexpected output: {}",
        absent.stdout
    );
}

#[test]
#[ignore]
fn e2e_sandbox_local_imported_value_beats_same_named_host_function() {
    let mut opts = Options {
        emit_main: false,
        use_nybl_sys: false,
        sandbox: true,
        ..Options::default()
    };
    opts.module_resolver = Some(modules_from_map([("m", "fn h() { return 12 }")]));
    let mut rust_src = transpile(
        "fn invoke() { use m.{h}\nreturn h() }\nprint(invoke())",
        &opts,
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct Host { calls: usize }
impl ::nybl::NyblHost for Host {
    fn call(&mut self, name: &str, _args: &[::nybl::value::Value], _line: u32) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> {
        if name == "h" { self.calls += 1; Some(Ok(::nybl::value::Value::Int(99))) } else { None }
    }
    fn on_print(&mut self, message: &str) { println!("{}", message); }
}
fn main() {
    let limits = ::nybl::NyblLimits::standard();
    let mut host = Host { calls: 0 };
    run(&mut host, &limits).unwrap();
    println!("calls={}", host.calls);
}
"#,
    );
    let run = run_generated_source("sandbox_local_import_host_precedence", rust_src);
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed: {}",
        run.stderr
    );
    assert_eq!(run.stdout, "12\ncalls=0");
}

#[test]
#[ignore]
fn e2e_sandbox_state_backed_mutation_preserves_receiver_semantics() {
    let source = r#"let a = [1, 2]
a.push(3)
let b = a
a.push(4)
print(a)
print(b)
[8].push(9)
let d = { "a": [1] }
let nested = fn() { d["a"].push(2) }
print(try_call(nested))"#;
    let run = run_aot_with_opts(
        source,
        "sandbox_state_backed_receiver_semantics",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed: {}",
        run.stderr
    );
    assert_eq!(run.stdout.lines().next(), Some("[1, 2, 3, 4]"));
    assert_eq!(run.stdout.lines().nth(1), Some("[1, 2, 3]"));
    assert!(
        run.stdout
            .lines()
            .nth(2)
            .is_some_and(|line| line.contains("Err("))
    );
}

#[test]
#[ignore]
fn e2e_sandbox_exact_self_bypasses_same_named_host_function() {
    let source = "fn f(n) { if n == 0 { return 1 } return f(n - 1) }\nlet old = f\nfn f(n) { return 9 }\nprint(old(2))";
    let mut rust_src = transpile(
        source,
        &Options {
            emit_main: false,
            use_nybl_sys: false,
            sandbox: true,
            ..Options::default()
        },
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct Host { calls: usize }
impl ::nybl::NyblHost for Host {
    fn call(&mut self, name: &str, _args: &[::nybl::value::Value], _line: u32) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> {
        if name == "f" { self.calls += 1; Some(Ok(::nybl::value::Value::Int(99))) } else { None }
    }
    fn on_print(&mut self, message: &str) { println!("{}", message); }
}
fn main() {
    let limits = ::nybl::NyblLimits::standard();
    let mut host = Host { calls: 0 };
    run(&mut host, &limits).unwrap();
    println!("calls={}", host.calls);
}
"#,
    );
    let run = run_generated_source("sandbox_exact_self_host_precedence", rust_src);
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed: {}",
        run.stderr
    );
    assert_eq!(run.stdout, "1\ncalls=0");
}

fn assert_aot_matches(test_name: &str, code: &str) {
    let expected = walker_output(code);
    let actual = run_aot(code, test_name);
    assert_eq!(
        actual, expected,
        "aot output diverged from tree-walker on {test_name}:\n--- tree-walker ---\n{expected}\n--- aot ---\n{actual}",
    );
}

fn cargo_available() -> bool {
    Command::new("cargo")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ─── Tests ────────────────────────────────────────────────────────

#[test]
#[ignore]
fn e2e_hello_world() {
    if !cargo_available() {
        eprintln!("cargo not available — skipping");
        return;
    }
    assert_aot_matches("hello_world", r#"print("hello, world")"#);
}

#[cfg(unix)]
#[test]
#[ignore]
fn e2e_generated_binary_handles_closed_stdout_without_panicking() {
    if !cargo_available() {
        eprintln!("cargo not available — skipping");
        return;
    }

    for sandbox in [false, true] {
        let mode = if sandbox { "sandbox" } else { "native" };
        let run = run_aot_with_closed_stdout(
            "readline()\nprint(\"reader closed\")",
            &format!("broken_pipe_{mode}"),
            &Options {
                sandbox,
                ..Options::default()
            },
        );
        assert_eq!(
            run.status,
            Some(0),
            "{mode} generated program did not terminate gracefully:\n{}",
            run.stderr
        );
        assert_ne!(run.status, Some(101));
        assert!(
            !run.stderr.contains("panicked at") && !run.stderr.contains("stack backtrace"),
            "{mode} generated program leaked a panic/backtrace:\n{}",
            run.stderr
        );
    }
}

#[test]
#[ignore]
fn e2e_arithmetic() {
    assert_aot_matches(
        "arithmetic",
        r#"print(1 + 2)
print(10 - 3)
print(4 * 5)
print(7 / 2)
print(10 % 3)
print(2 + 3 * 4)"#,
    );
}

#[test]
#[ignore]
fn e2e_variables_and_assign() {
    assert_aot_matches(
        "variables",
        r#"let x = 10
print(x)
x = 42
print(x)
x += 8
print(x)
x *= 2
print(x)"#,
    );
}

#[test]
#[ignore]
fn e2e_if_and_while() {
    assert_aot_matches(
        "if_and_while",
        r#"let i = 0
let total = 0
while i < 5 {
    if i % 2 == 0 {
        total = total + i
    }
    i = i + 1
}
print(total)"#,
    );
}

#[test]
#[ignore]
fn e2e_repeat_and_for() {
    assert_aot_matches(
        "repeat_and_for",
        r#"let n = 0
repeat 4 { n = n + 1 }
print(n)

let sum = 0
for x in [10, 20, 30] { sum = sum + x }
print(sum)

let s = 0
for i in range(5) { s = s + i }
print(s)"#,
    );
}

#[test]
#[ignore]
fn e2e_user_fn_with_recursion() {
    assert_aot_matches(
        "recursion",
        r#"fn fib(n) {
    if n <= 1 { return n }
    return fib(n - 1) + fib(n - 2)
}
print(fib(10))"#,
    );
}

#[test]
#[ignore]
fn e2e_truthiness_and_short_circuit() {
    assert_aot_matches(
        "truthiness",
        r#"print(true && false)
print(true || false)
print(false || true)
print(if 0 { "t" } else { "f" })
print(if "" { "t" } else { "f" })
print(if [1] { "t" } else { "f" })"#,
    );
}

#[test]
#[ignore]
fn e2e_method_calls_array_and_string() {
    assert_aot_matches(
        "method_calls",
        r#"let a = [1, 2, 3]
a.push(4)
print(a.len())
print(a)
print("hello world".upper())
print("a,b,c".split(","))
print(["x", "y", "z"].join("-"))
let sorted = [3, 1, 2]
sorted.sort()
print(sorted)"#,
    );
}

#[test]
#[ignore]
fn e2e_array_mutation_fast_path_semantics() {
    let output = run_aot(
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
print(transient_source)

struct Accumulator { total }
fn Accumulator.push(self, value) { return self.total + value }
let accumulator = Accumulator { total: 7 }
print(accumulator.push(5))

let values = []
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
print(changed)

let unchanged = [1, 2, 3]
print(try_call(fn() { return unchanged.push() }).is_err())
print(try_call(fn() { return unchanged.insert(99, 4) }).is_err())
print(try_call(fn() { return unchanged.remove(99) }).is_err())
print(unchanged)"#,
        "array_mutation_fast_path_semantics",
    );
    assert_eq!(
        output,
        concat!(
            "[1, 2, 3]\n",
            "[1, 2]\n",
            "[1, 2]\n",
            "[7]\n",
            "12\n",
            "2048\n",
            "0\n",
            "2047\n",
            "none\n",
            "none\n",
            "1\n",
            "2\n",
            "[5, 4, 3]\n",
            "true\n",
            "true\n",
            "true\n",
            "[1, 2, 3]"
        )
    );
}

#[test]
#[ignore]
fn e2e_array_push_depth_error_is_clean() {
    let run = run_aot_with_opts(
        r#"let deep = none
repeat 64 { deep = [deep] }
let values = []
values.push(deep)"#,
        "array_push_depth_error",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        run.status,
        Some(1),
        "expected a clean Nybl error exit, not an abort; stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains(nybl::value::VALUE_DEPTH_ERROR_MESSAGE),
        "expected value-depth diagnostic; got:\n{}",
        run.stderr
    );
}

#[test]
#[ignore]
fn e2e_signed_indices_across_methods_and_subscripts() {
    let output = run_aot(
        r#"let values = [10, 20, 30, 40]
print(values.remove(-1))
print(values.insert(-1, 25))
print(values)
print(values.slice(-3, -1))
print(values.slice(-99, 99))
print(values.slice(99, -99))
let fractional = [10, 20, 30]
print(fractional[1.9])
fractional[-1.9] = 99
print(fractional.remove(-1.9))
print(fractional.insert(1.9, 15))
print(fractional)
let text = "a🙂é界"
print(text[-1])
print(text.slice(-3, -1))
print(text.slice(-99, 99))
let unchanged = [1, 2, 3]
print(try_call(fn() { return unchanged.remove(-4) }).is_err())
print(try_call(fn() { return unchanged.insert(-4, 0) }).is_err())
print(try_call(fn() { unchanged[-4] = 0 }).is_err())
print(try_call(fn() { return unchanged.remove("0") }).is_err())
print(unchanged)"#,
        "signed_indices_across_methods_and_subscripts",
    );
    assert_eq!(
        output,
        concat!(
            "40\n",
            "none\n",
            "[10, 20, 25, 30]\n",
            "[20, 25]\n",
            "[10, 20, 25, 30]\n",
            "[]\n",
            "20\n",
            "99\n",
            "none\n",
            "[10, 15, 20]\n",
            "界\n",
            "🙂é\n",
            "a🙂é界\n",
            "true\n",
            "true\n",
            "true\n",
            "true\n",
            "[1, 2, 3]"
        )
    );
}

#[test]
#[ignore]
fn e2e_nested_array_mutation_receiver_contract() {
    assert_aot_matches(
        "nested_array_mutation_receiver_contract",
        r#"struct Holder { items }
let indexed = {"items": [1]}
let fielded = Holder { items: [1, 2] }
let index_result = try_call(fn() {
    indexed["items"].push(2)
})
let field_result = try_call(fn() {
    fielded.items.pop()
})
print(index_result.is_err())
print(match index_result { Result::Err(e) => e.message, _ => "missing" })
print(match index_result { Result::Err(e) => e.line, _ => -1 })
print(field_result.is_err())
print(match field_result { Result::Err(e) => e.message, _ => "missing" })
print(match field_result { Result::Err(e) => e.line, _ => -1 })
fn make_array() { return [7] }
print([1].push(2))
print(make_array().pop())
struct Gadget { n }
fn Gadget.push(self, amount) { return self.n + amount }
struct Wrapper { item }
let wrapper = Wrapper { item: Gadget { n: 10 } }
let dynamic = {"item": Gadget { n: 20 }}
print(wrapper.item.push(2))
print(dynamic["item"].push(3))"#,
    );
}

#[test]
#[ignore]
fn e2e_nested_array_mutation_grouped_receivers() {
    assert_aot_matches(
        "nested_array_mutation_grouped_index",
        r#"let indexed = {"items": [1]}
(indexed["items"]).push(2)
print(indexed["items"])"#,
    );
    assert_aot_matches(
        "nested_array_mutation_grouped_field",
        r#"struct Holder { items }
let fielded = Holder { items: [1] }
(fielded.items).push(2)
print(fielded.items)"#,
    );
}

#[test]
#[ignore]
fn e2e_string_interpolation() {
    assert_aot_matches(
        "interpolation",
        r#"let name = "nybl"
let version = 2
print("hi {name}!")
print("nybl v{version} ready")"#,
    );
}

#[test]
#[ignore]
fn e2e_indexed_writes_and_compound() {
    assert_aot_matches(
        "indexed_writes",
        r#"let a = [1, 2, 3]
a[0] = 99
print(a)
a[1] += 10
print(a)
a[-1] *= 2
print(a)
let d = {"hp": 100}
d["hp"] = 50
d["mp"] = 20
print(d["hp"])
print(d["mp"])"#,
    );
}

#[test]
#[ignore]
fn e2e_nested_mutable_places_assignment_ref_and_methods() {
    assert_aot_modes_match_walker(
        r#"struct Bucket { items }
struct State { buckets }
let state = State { buckets: [Bucket { items: [1, 2] }] }
state.buckets[0].items[1] += 5
state.buckets[0].items.push(8)
fn replace(ref value) { value = 11 }
replace(ref state.buckets[0].items[0])
print(state.buckets[0].items)
fn Bucket.add(ref self, amount) { self.items.push(amount) }
state.buckets[0].add(13)
print(state.buckets[0].items)"#,
        "nested_mutable_places",
    );
}

#[test]
#[ignore]
fn e2e_nested_place_receiver_order_and_rollback() {
    assert_aot_modes_match_walker(
        r#"struct Counter { value }
struct Holder { counters }
fn Counter.add(ref self, amount) { self.value += amount; return self.value }
fn Counter.fail(ref self) { self.value = 99; panic("rollback") }
let holder = Holder { counters: [Counter { value: 1 }] }
fn index() { print("index"); return 0 }
fn amount() { print("arg"); return 2 }
print(holder.counters[index()].add(amount()), holder.counters[0].value)
fn fail() { holder.counters[0].fail() }
print(try_call(fail), holder.counters[0].value)"#,
        "nested_place_receiver_order_rollback",
    );
}

#[test]
#[ignore]
fn e2e_fizzbuzz_roundtrip() {
    // Canonical smoke test — uses arrays, method calls, string
    // interpolation indirectly through str(), for/range, if/else
    // chain, and mutation back-assign on `push`.
    assert_aot_matches(
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
    );
}

// ─── Sandbox ───────────────────────────────────────────────────

#[test]
#[ignore]
fn e2e_sandbox_happy_path_matches_walker() {
    // With sandbox on, output for a well-behaved program should
    // still match the tree-walker — ticks / memory checks fire but
    // don't change semantics.
    let code = r#"let sum = 0
for i in range(10) { sum = sum + i }
print(sum)"#;
    let expected = walker_output(code);
    let run = run_aot_with_opts(
        code,
        "sandbox_happy",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(run.status, Some(0), "stderr:\n{}", run.stderr);
    assert_eq!(run.stdout, expected);
}

#[test]
#[ignore]
fn e2e_sandbox_halts_infinite_loop() {
    // Default limits are `NyblLimits::standard()` — 10k steps. A
    // bare `while true { }` burns one tick per iteration and hits
    // the cap. The process should exit non-zero with the
    // canonical "too many steps" message on stderr.
    let run = run_aot_with_opts(
        "while true { }",
        "sandbox_infinite",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_ne!(
        run.status,
        Some(0),
        "expected non-zero exit; stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("too many steps"),
        "expected 'too many steps' in stderr; got:\n{}",
        run.stderr
    );
}

#[test]
#[ignore]
fn e2e_sandbox_halts_memory_bomb() {
    // `"x" * 999999` trips the pre-flight memory check
    // (`check_string_repeat_memory`) since standard limits set
    // max_memory to 10 MB. AOT routes through the same `ops::mul`
    // → builtins path, so the error message is identical.
    let run = run_aot_with_opts(
        r#"let s = "x" * 99999999
print(s.len())"#,
        "sandbox_memory_bomb",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_ne!(
        run.status,
        Some(0),
        "expected non-zero exit; stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("Memory limit"),
        "expected 'Memory limit' in stderr; got:\n{}",
        run.stderr
    );
}

#[test]
#[ignore]
fn e2e_bounded_string_operations_preserve_native_and_sandbox_output() {
    assert_aot_modes_match_walker(
        r#"let nested = ["x", ["y"]]
print(nested, nested)
print([nested, nested].join("|"))
print("ab".replace("", "-"))
print("a,,b".split(","))"#,
        "bounded_string_operation_parity",
    );
}

#[test]
#[ignore]
fn e2e_sandbox_amplified_strings_fail_before_host_output() {
    let source = r#"
pub fn print_amplified() {
  let shared = "x" * 256
  let values = [shared, shared, shared, shared, shared, shared, shared, shared]
  print(values)
}
pub fn join_amplified() {
  let shared = "x" * 256
  let values = [shared, shared, shared, shared, shared, shared, shared, shared]
  return values.join("")
}
pub fn replace_amplified() {
  let shared = "x" * 256
  return shared.replace("x", "abcdefgh")
}
pub fn split_amplified() {
  let shared = "x," * 128
  return shared.split(",")
}
"#;
    let mut rust_src = transpile(
        source,
        &Options {
            emit_main: false,
            use_nybl_sys: false,
            sandbox: true,
            ..Options::default()
        },
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct Host {
    prints: usize,
}
impl ::nybl::NyblHost for Host {
    fn call(&mut self, _name: &str, _args: &[::nybl::value::Value], _line: u32) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> {
        None
    }
    fn on_print(&mut self, _message: &str) {
        self.prints += 1;
    }
}
fn assert_memory_error(result: Result<::nybl::value::Value, ::nybl::error::NyblError>) {
    let error = result.unwrap_err();
    assert!(error.is_fatal);
    assert_eq!(error.message, "Memory limit exceeded");
}
fn main() {
    let limits = ::nybl::NyblLimits { max_steps: 1_000, max_memory: 1_200 };
    let mut host = Host { prints: 0 };
    let mut instance = NyblInstance::load(&mut host, &limits).unwrap();

    for entry in ["print_amplified", "join_amplified", "replace_amplified", "split_amplified"] {
        assert_memory_error(instance.call(entry, &[], &mut host));
        assert_eq!(host.prints, 0, "{entry} exposed host output before failing");
    }
    println!("ok");
}
"#,
    );

    let run = run_generated_source("sandbox_amplified_string_preflight", rust_src);
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed:\n{}",
        run.stderr
    );
    assert_eq!(run.stdout, "ok");
}

#[test]
#[ignore]
fn e2e_sandbox_recursion_halts() {
    // Generated AOT now shares the walker/VM MAX_CALL_DEPTH = 64
    // boundary and reports the canonical recoverable depth error
    // before the native Rust stack is at risk.
    let run = run_aot_with_opts(
        "fn f() { f() }\nf()",
        "sandbox_recursion",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_ne!(
        run.status,
        Some(0),
        "expected non-zero exit; stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("nested function calls"),
        "expected the call-depth diagnostic in stderr; got:\n{}",
        run.stderr
    );
}

#[test]
#[ignore]
fn e2e_sandbox_rejects_deep_value_hidden_in_lambda_without_aborting() {
    // AOT lambdas retain captures inside the generated Rust callable rather
    // than in `NyblFn::captures`. Build a value at depth 63, capture it (making
    // the function depth 64), then try to wrap that fn in an array (depth 65).
    // The generated binary must return the fatal Nybl diagnostic with a normal
    // exit code, not overflow the native stack or abort by signal.
    let run = run_aot_with_opts(
        r#"let value = none
repeat 63 { value = [value] }
let f = fn() { return value }
let too_deep = [f]
print(too_deep)"#,
        "sandbox_deep_opaque_capture",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        run.status,
        Some(1),
        "expected a clean Nybl error exit, not an abort; stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains(nybl::value::VALUE_DEPTH_ERROR_MESSAGE),
        "expected value-depth diagnostic; got:\n{}",
        run.stderr
    );
}

#[test]
#[ignore]
fn e2e_sandbox_counts_namespaced_module_capture_in_lambda_depth() {
    let run = run_aot_with_modules_and_opts(
        "use shapes as s\nlet f = fn() { return s.Box { value: none } }\nprint(f)",
        "sandbox_deep_namespaced_module_capture",
        &[(
            "shapes",
            "struct Box { value }\nlet deep = none\nrepeat 63 { deep = [deep] }",
        )],
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        run.status,
        Some(1),
        "expected a clean depth error exit; stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains(nybl::value::VALUE_DEPTH_ERROR_MESSAGE),
        "expected value-depth diagnostic; got:\n{}",
        run.stderr
    );
}

#[test]
#[ignore]
fn e2e_nested_lambda_param_shadow_does_not_capture_deep_outer_value() {
    assert_aot_matches(
        "nested_lambda_param_shadow_depth",
        r#"let x = none
repeat 64 { x = [x] }
let outer = fn(x) { return fn() { return x } }
print(outer(none)())"#,
    );
}

// ─── Closures / first-class fns ───────────────────────────────

// ─── Imports (phase 2c) ──────────────────────────────────────────

/// Compare AOT output against a walker run that resolves modules
/// from the same in-memory table. Used by the use tests —
/// lets the same map drive both engines so we can assert they
/// produce identical output.
fn walker_output_with_modules(code: &str, modules: &[(&str, &str)]) -> String {
    struct MapHost<'a> {
        prints: std::cell::RefCell<Vec<String>>,
        modules: std::collections::HashMap<String, String>,
        _marker: std::marker::PhantomData<&'a ()>,
    }
    impl<'a> NyblHost for MapHost<'a> {
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
            self.modules.get(name).cloned().map(Ok)
        }
    }
    let mut walker_host = MapHost {
        prints: std::cell::RefCell::new(Vec::new()),
        modules: modules
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        _marker: std::marker::PhantomData,
    };
    nybl::run(code, &mut walker_host, &NyblLimits::standard()).expect("walker failed");
    walker_host.prints.borrow().join("\n")
}

fn assert_aot_matches_with_modules(test_name: &str, code: &str, modules: &[(&str, &str)]) {
    let expected = walker_output_with_modules(code, modules);

    let resolver = modules_from_map(modules.iter().map(|(k, v)| (*k, *v)));
    let rust_src = transpile(
        code,
        &Options {
            module_resolver: Some(resolver),
            ..Options::default()
        },
    )
    .expect("transpile");
    let dir = write_scratch_project(test_name, &rust_src);
    let output = Command::new("cargo")
        .arg("run")
        .arg("--quiet")
        .arg("--release")
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run cargo");
    if !output.status.success() {
        panic!(
            "cargo run failed for {}:\n--- stderr ---\n{}\n--- generated ---\n{}",
            test_name,
            String::from_utf8_lossy(&output.stderr),
            rust_src,
        );
    }
    let actual = String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string();
    assert_eq!(
        actual, expected,
        "AOT output diverged from walker for {test_name}:\n--- walker ---\n{expected}\n--- aot ---\n{actual}",
    );
}

fn assert_aot_modes_match_with_modules(test_name: &str, code: &str, modules: &[(&str, &str)]) {
    let expected = walker_output_with_modules(code, modules);
    for sandbox in [false, true] {
        let run = run_aot_with_modules_and_opts(
            code,
            &format!("{test_name}_{}", if sandbox { "sandbox" } else { "native" }),
            modules,
            &Options {
                sandbox,
                ..Options::default()
            },
        );
        assert_eq!(
            run.status,
            Some(0),
            "generated {test_name} program failed (sandbox={sandbox}):\n{}",
            run.stderr,
        );
        assert_eq!(
            run.stdout, expected,
            "AOT output diverged from walker for {test_name} (sandbox={sandbox})",
        );
    }
}

fn cross_engine_warning_runs(
    test_name: &str,
    code: &str,
    modules: &[(&str, &str)],
) -> Vec<(String, AotRun)> {
    let compile_mode = |module_name: &str, sandbox: bool| {
        transpile(
            code,
            &Options {
                emit_main: false,
                use_nybl_sys: false,
                sandbox,
                module_name: Some(module_name.to_string()),
                module_resolver: Some(modules_from_map(
                    modules.iter().map(|(name, source)| (*name, *source)),
                )),
            },
        )
        .unwrap_or_else(|error| {
            panic!(
                "failed to transpile {test_name} ({module_name}): {}",
                error.message
            )
        })
    };
    let native = compile_mode("__aot_native", false);
    let sandbox = compile_mode("__aot_sandbox", true);

    let mut rust_src = String::new();
    rust_src.push_str(&native);
    rust_src.push('\n');
    rust_src.push_str(&sandbox);
    rust_src.push_str(
        r#"
struct WarningHost {
    modules: ::std::collections::HashMap<::std::string::String, ::std::string::String>,
}

impl ::nybl::NyblHost for WarningHost {
    fn call(
        &mut self,
        _name: &str,
        _args: &[::nybl::value::Value],
        _line: u32,
    ) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> {
        None
    }

    fn on_print(&mut self, message: &str) {
        println!("{}", message);
    }

    fn resolve_module(
        &mut self,
        name: &str,
    ) -> Option<Result<::std::string::String, ::nybl::error::NyblError>> {
        self.modules.get(name).cloned().map(Ok)
    }
}

fn main() {
    let engine = ::std::env::args().nth(1).expect("engine argument");
    let mut modules = ::std::collections::HashMap::new();
"#,
    );
    for (name, source) in modules {
        rust_src.push_str(&format!(
            "    modules.insert({name:?}.to_string(), {source:?}.to_string());\n"
        ));
    }
    rust_src.push_str(&format!(
        r#"    let mut host = WarningHost {{ modules }};
    let result = match engine.as_str() {{
        "walker" => ::nybl::run({code:?}, &mut host, &::nybl::NyblLimits::standard()),
        "vm" => ::nybl_vm::run({code:?}, &mut host, &::nybl::NyblLimits::standard()),
        "aot-native" => __aot_native::run(&mut host),
        "aot-sandbox" => __aot_sandbox::run(&mut host, &::nybl::NyblLimits::standard()),
        other => panic!("unknown engine: {{other}}"),
    }};
    result.unwrap();
}}
"#
    ));

    let dir = write_scratch_project(test_name, &rust_src);
    ["walker", "vm", "aot-native", "aot-sandbox"]
        .into_iter()
        .map(|engine| {
            let output = Command::new("cargo")
                .arg("run")
                .arg("--quiet")
                .arg("--release")
                .arg("--")
                .arg(engine)
                .current_dir(&dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .expect("run cross-engine warning driver");
            (
                engine.to_string(),
                AotRun {
                    status: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout)
                        .trim_end_matches('\n')
                        .to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                    rust_src: rust_src.clone(),
                },
            )
        })
        .collect()
}

fn expected_glob_warnings(path: &str, names: &[&str]) -> String {
    names
        .iter()
        .map(|name| {
            format!(
                "warning: {}\n",
                nybl::error_messages::glob_shadow_warning(name, path)
            )
        })
        .collect()
}

#[test]
#[ignore]
fn e2e_glob_shadow_warnings_match_all_engines_and_module_boundaries() {
    let code = r#"use root_first
use root_second
fn nested() {
    use function_first
    use function_second
    return alpha + beta() + delta + gamma()
}
print(alpha + beta() + delta + gamma())
print(nested())
use wrapper as wrapped
print(wrapped.marker)"#;
    let mixed_first =
        "fn gamma() { return 1 }\nlet delta = 1\nfn beta() { return 1 }\nlet alpha = 1";
    let mixed_second =
        "let delta = 2\nfn gamma() { return 2 }\nlet alpha = 2\nfn beta() { return 2 }";
    let modules = &[
        ("root_first", mixed_first),
        ("root_second", mixed_second),
        ("function_first", mixed_first),
        ("function_second", mixed_second),
        ("internal_first", mixed_first),
        ("internal_second", mixed_second),
        (
            "wrapper",
            "use internal_first\nuse internal_second\nlet marker = alpha + beta() + delta + gamma()",
        ),
    ];
    let runs = cross_engine_warning_runs("glob_warning_all_engines", code, modules);
    let expected = [
        expected_glob_warnings("root_second", &["alpha", "beta", "delta", "gamma"]),
        expected_glob_warnings("function_second", &["alpha", "beta", "delta", "gamma"]),
        expected_glob_warnings("internal_second", &["alpha", "beta", "delta", "gamma"]),
    ]
    .concat();
    let expected_stdout = &runs[0].1.stdout;

    for (engine, run) in &runs {
        assert_eq!(
            run.status,
            Some(0),
            "{engine} warning contract program failed:\n{}\n--- generated ---\n{}",
            run.stderr,
            run.rust_src,
        );
        assert_eq!(
            &run.stdout, expected_stdout,
            "{engine} changed stdout while reporting warnings"
        );
        assert_eq!(
            run.stderr, expected,
            "{engine} warning text/count/order diverged"
        );
    }
}

#[test]
#[ignore]
fn e2e_glob_shadow_warnings_cover_claimed_fns_and_slot_locals() {
    // Orderings from issue #117: a named fn declared *before* the
    // glob import claims its binding statically (both against a fn
    // export and a value export of the same name), and a fn-body
    // import clashing with a slot-allocated local / parameter.
    // Every engine must warn identically, once per executed `use`
    // (so twice for the double-called `caller`).
    let code = r#"fn dup() { return 1 }
fn dupval() { return 2 }
use top_module
fn caller(dup) {
    let local = 3
    use fn_module
    return dup + local
}
print(dup() + dupval())
print(caller(40))
print(caller(50))"#;
    let modules = &[
        ("top_module", "fn dup() { return 10 }\nlet dupval = 20"),
        ("fn_module", "let local = 30\nfn dup() { return 40 }"),
    ];
    let runs = cross_engine_warning_runs("glob_warning_claimed_fns_and_slot_locals", code, modules);
    let expected = [
        expected_glob_warnings("top_module", &["dup", "dupval"]),
        expected_glob_warnings("fn_module", &["dup", "local"]),
        expected_glob_warnings("fn_module", &["dup", "local"]),
    ]
    .concat();
    let expected_stdout = &runs[0].1.stdout;

    for (engine, run) in &runs {
        assert_eq!(
            run.status,
            Some(0),
            "{engine} warning contract program failed:\n{}\n--- generated ---\n{}",
            run.stderr,
            run.rust_src,
        );
        assert_eq!(
            &run.stdout, expected_stdout,
            "{engine} changed stdout while reporting warnings"
        );
        assert_eq!(
            run.stderr, expected,
            "{engine} warning text/count/order diverged"
        );
    }
}

#[test]
#[ignore]
fn e2e_non_glob_private_and_absent_exports_are_silent_in_all_engines() {
    let code = r#"use first.{alpha, beta}
use second.{alpha, beta}
use second as module
use private
use absent
print(alpha + beta() + module.alpha)"#;
    let modules = &[
        ("first", "fn beta() { return 1 }\nlet alpha = 1"),
        ("second", "let alpha = 2\nfn beta() { return 2 }"),
        ("private", "let _alpha = 3\nfn _beta() { return 3 }"),
        (
            "absent",
            "if false { let alpha = 4; fn beta() { return 4 } }",
        ),
    ];
    let runs = cross_engine_warning_runs("glob_warning_negative_all_engines", code, modules);
    let expected_stdout = &runs[0].1.stdout;

    for (engine, run) in &runs {
        assert_eq!(
            run.status,
            Some(0),
            "{engine} negative warning program failed:\n{}\n--- generated ---\n{}",
            run.stderr,
            run.rust_src,
        );
        assert_eq!(&run.stdout, expected_stdout, "{engine} stdout diverged");
        assert_eq!(
            run.stderr, "",
            "{engine} warned for selective, aliased, private, or absent exports"
        );
    }
}

fn assert_aot_compiles_without_warnings_with_modules(
    test_name: &str,
    code: &str,
    modules: &[(&str, &str)],
) {
    let resolver = modules_from_map(modules.iter().map(|(k, v)| (*k, *v)));
    let rust_src = transpile(
        code,
        &Options {
            module_resolver: Some(resolver),
            ..Options::default()
        },
    )
    .expect("transpile");
    let dir = write_scratch_project(test_name, &rust_src);
    let output = Command::new("cargo")
        .arg("rustc")
        .arg("--quiet")
        .arg("--release")
        .arg("--")
        .arg("-D")
        .arg("warnings")
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("compile generated Rust with warnings denied");
    assert!(
        output.status.success(),
        "generated Rust failed a native -D warnings compile for {}:\n--- stderr ---\n{}\n--- generated ---\n{}",
        test_name,
        String::from_utf8_lossy(&output.stderr),
        rust_src,
    );
}

#[test]
#[ignore]
fn e2e_match_arms_and_imports_compile_without_rustc_warnings() {
    if !cargo_available() {
        eprintln!("cargo not available — skipping");
        return;
    }
    assert_aot_compiles_without_warnings_with_modules(
        "warning_free_match_arms_and_imports",
        r#"use warning_fixture
let unguarded = match flag { true => value + 1, _ => 0 }
let guarded = match flag { true if value > 0 => value + 2, _ => 0 }
print(unguarded, guarded)"#,
        &[("warning_fixture", "let flag = true\nlet value = 40")],
    );
}

#[test]
#[ignore]
fn e2e_dynamic_method_sites_compile_without_rustc_warnings() {
    if !cargo_available() {
        eprintln!("cargo not available — skipping");
        return;
    }
    assert_aot_compiles_without_warnings_with_modules(
        "warning_free_dynamic_method_sites",
        r#"use methods as api
let value = api.Item { value: 4 }
print(try_call(fn() { return value.read() }).is_err())
install()
print(value.read())"#,
        &[(
            "methods",
            r#"struct Item { value }
fn install() {
    if true { fn Item.read(self) { return self.value + 1 } }
}"#,
        )],
    );
}

#[test]
#[ignore]
fn e2e_dynamic_method_sites_compile_and_run_in_sandbox_mode() {
    let code = r#"struct Item { value }
fn Item.read(self) { return self.value + 1 }
print(Item { value: 4 }.read())"#;
    let run = run_aot_with_opts(
        code,
        "sandbox_dynamic_method_sites",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        run.status,
        Some(0),
        "sandbox method program failed: {}",
        run.stderr
    );
    assert_eq!(run.stdout, walker_output(code));
}

#[test]
#[ignore]
fn e2e_import_basic_let() {
    assert_aot_matches_with_modules(
        "import_basic_let",
        r#"use math
print(pi)"#,
        &[("math", "let pi = 3")],
    );
}

#[test]
#[ignore]
fn e2e_import_named_fn() {
    assert_aot_matches_with_modules(
        "import_named_fn",
        r#"use math
print(square(7))"#,
        &[("math", "fn square(n) { return n * n }")],
    );
}

#[test]
#[ignore]
fn e2e_non_sandbox_named_fn_reads_and_mutates_root_bindings() {
    assert_aot_matches(
        "non_sandbox_named_fn_root_bindings",
        r#"let base = 5
const STEP = 2
let calls = 0
fn calculate(n) {
    calls += 1
    return base + STEP + n + calls
}
print(calculate(3))
print(calculate(3))
print(calls)"#,
    );
}

#[test]
#[ignore]
fn e2e_non_sandbox_module_fn_reads_its_module_bindings() {
    assert_aot_matches_with_modules(
        "non_sandbox_named_fn_module_bindings",
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
    );
}

#[test]
#[ignore]
fn e2e_non_sandbox_named_fns_call_bare_and_transitive_imports() {
    assert_aot_matches_with_modules(
        "non_sandbox_named_fn_bare_imports",
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
    );
}

#[test]
#[ignore]
fn e2e_aot_modes_preserve_root_and_module_function_first_win() {
    assert_aot_modes_match_with_modules(
        "function_first_win_root",
        r#"fn pick() { return 11 }
use b
fn read() { return pick() }
print(read())"#,
        &[("b", "fn pick() { return 22 }")],
    );
    assert_aot_modes_match_with_modules(
        "function_first_win_root_import_first",
        r#"use b
fn pick() { return 11 }
fn read() { return pick() }
print(read())"#,
        &[("b", "fn pick() { return 22 }")],
    );
    assert_aot_modes_match_with_modules(
        "function_first_win_module",
        "use a\nprint(read())",
        &[
            (
                "a",
                r#"fn pick() { return 11 }
use b
fn read() { return pick() }"#,
            ),
            ("b", "fn pick() { return 22 }"),
        ],
    );
    assert_aot_modes_match_with_modules(
        "function_first_win_module_import_first",
        "use a\nprint(read())",
        &[
            (
                "a",
                r#"use b
fn pick() { return 11 }
fn read() { return pick() }"#,
            ),
            ("b", "fn pick() { return 22 }"),
        ],
    );
}

#[test]
#[ignore]
fn e2e_aot_modes_keep_direct_and_facade_imported_values_live() {
    let root = r#"use imported
fn next_and_read() {
    bump()
    return count
}
print(next_and_read())
print(count)
count += 4
print(count)
print(bump())"#;
    let leaf = r#"let count = 0
fn bump() {
    count += 1
    return count
}"#;
    assert_aot_modes_match_with_modules(
        "live_imported_value_direct",
        &root.replace("imported", "leaf"),
        &[("leaf", leaf)],
    );
    assert_aot_modes_match_with_modules(
        "live_imported_value_facade",
        &root.replace("imported", "facade"),
        &[("facade", "use leaf"), ("leaf", leaf)],
    );
}

#[test]
#[ignore]
fn e2e_sandbox_instance_retains_facade_import_origins() {
    let mut rust_src = transpile(
        r#"use facade
pub fn next_and_read() {
    bump()
    return count
}
pub fn read() { return count }"#,
        &Options {
            emit_main: false,
            use_nybl_sys: false,
            sandbox: true,
            module_resolver: Some(modules_from_map([
                ("facade", "use leaf"),
                (
                    "leaf",
                    r#"let count = 0
fn bump() {
    count += 1
    return count
}"#,
                ),
            ])),
            ..Options::default()
        },
    )
    .expect("transpile persistent facade imports");
    rust_src.push_str(
        r#"
struct Host;
impl ::nybl::NyblHost for Host {
    fn call(&mut self, _name: &str, _args: &[::nybl::value::Value], _line: u32) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> { None }
}
fn expect_int(value: ::nybl::value::Value, expected: i64) {
    assert!(matches!(value, ::nybl::value::Value::Int(actual) if actual == expected));
}
fn main() {
    let mut host = Host;
    let limits = ::nybl::NyblLimits::standard();
    let mut instance = NyblInstance::load(&mut host, &limits).unwrap();
    expect_int(instance.call("next_and_read", &[], &mut host).unwrap(), 1);
    expect_int(instance.call("read", &[], &mut host).unwrap(), 1);
    expect_int(instance.call("next_and_read", &[], &mut host).unwrap(), 2);
    expect_int(instance.call("read", &[], &mut host).unwrap(), 2);
    println!("ok");
}
"#,
    );
    let run = run_generated_source("sandbox_persistent_facade_origins", rust_src);
    assert_eq!(
        run.status,
        Some(0),
        "generated program failed: {}",
        run.stderr
    );
    assert_eq!(run.stdout, "ok");
}

#[test]
#[ignore]
fn e2e_import_dotted_path() {
    assert_aot_matches_with_modules(
        "import_dotted_path",
        r#"use std.math
print(e)"#,
        &[("std.math", "let e = 2")],
    );
}

#[test]
#[ignore]
fn e2e_import_transitive() {
    assert_aot_matches_with_modules(
        "import_transitive",
        r#"use a
print(doubled)"#,
        &[("a", "use b\nlet doubled = pi + pi"), ("b", "let pi = 3")],
    );
}

#[test]
#[ignore]
fn e2e_import_idempotent_reload_cache() {
    // Second use shouldn't re-run the module body. The walker
    // caches; the AOT caches via the __mod_*_load fn's
    // module_cache check.
    assert_aot_matches_with_modules(
        "import_idempotent",
        r#"use m
use m
print(x)"#,
        &[("m", "let x = 42")],
    );
}

#[test]
#[ignore]
fn e2e_use_selective_items() {
    // `use m.{a, b}` brings only the listed exports in as locals.
    assert_aot_matches_with_modules(
        "use_selective_items",
        r#"use m.{pi, tau}
print(pi)
print(tau)"#,
        &[("m", "let pi = 3\nlet tau = 6\nlet unused = 99")],
    );
}

#[test]
#[ignore]
fn e2e_use_selective_reaches_private() {
    // Selective form can reach `_`-prefixed names; glob can't.
    assert_aot_matches_with_modules(
        "use_selective_private",
        r#"use m.{_helper}
print(_helper(5))"#,
        &[("m", "fn _helper(n) { return n * 10 }")],
    );
}

#[test]
#[ignore]
fn e2e_use_aliased_glob() {
    // `use m as n` — namespaced binding read + call through alias.
    assert_aot_matches_with_modules(
        "use_aliased_glob",
        r#"use m as n
print(n.pi)
print(n.double(7))"#,
        &[("m", "let pi = 3\nfn double(x) { return x + x }")],
    );
}

#[test]
#[ignore]
fn e2e_use_aliased_selective() {
    // `use m.{double} as n` — only `double` ends up on `n`.
    assert_aot_matches_with_modules(
        "use_aliased_selective",
        r#"use m.{double} as n
print(n.double(21))"#,
        &[("m", "let pi = 3\nfn double(x) { return x + x }")],
    );
}

#[test]
#[ignore]
fn e2e_use_namespaced_struct_construct() {
    // `n.Entity { ... }` constructs the aliased module's type.
    assert_aot_matches_with_modules(
        "use_namespaced_struct",
        r#"use m as n
let p = n.Point { x: 3, y: 4 }
print(p.x + p.y)"#,
        &[("m", "struct Point { x, y }")],
    );
}

#[test]
#[ignore]
fn e2e_two_modules_same_type_name_distinct_identity() {
    // Phase 2b — two modules both declare `enum Color { ... }`
    // with different variants. Under module-qualified types
    // they coexist as distinct runtime types. Equality never
    // fires across module boundaries even when both values are
    // named `Color::Red`.
    assert_aot_matches_with_modules(
        "two_modules_same_type_name",
        r#"use paint as p
use other as o
let a = p.Color::Red
let b = o.Color::Red
print(a == b)
print(a == a)
print(a)
print(b)"#,
        &[
            ("paint", "enum Color { Red, Blue }"),
            ("other", "enum Color { Red, Green, Yellow }"),
        ],
    );
}

#[test]
#[ignore]
fn e2e_namespaced_pattern_picks_correct_module() {
    // Patterns embed the resolved module path in the emitter's
    // per-site resolver closure; `p.Color::Red` only matches
    // values tagged with the paint module.
    assert_aot_matches_with_modules(
        "namespaced_pattern_picks_correct_module",
        r#"use paint as p
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
print(label(p.Color::Blue))"#,
        &[
            ("paint", "enum Color { Red, Blue }"),
            ("other", "enum Color { Red, Green }"),
        ],
    );
}

#[test]
#[ignore]
fn e2e_use_namespaced_enum_construct() {
    // `n.Color::Red` and `n.Result::Ok(v)` via alias.
    assert_aot_matches_with_modules(
        "use_namespaced_enum",
        r#"use m as n
print(n.Color::Red)
print(n.Result::Ok(42))"#,
        &[(
            "m",
            "enum Color { Red, Green, Blue }\nenum Result { Ok(v), Err(e) }",
        )],
    );
}

// ─── Structs / enums / user methods ──────────────────────────────

#[test]
#[ignore]
fn e2e_struct_basic() {
    assert_aot_matches(
        "struct_basic",
        r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
print(p.x + p.y)
print(p)"#,
    );
}

#[test]
#[ignore]
fn e2e_struct_field_assign() {
    assert_aot_matches(
        "struct_field_assign",
        r#"struct Counter { n }
let c = Counter { n: 10 }
c.n += 5
c.n *= 2
print(c.n)"#,
    );
}

#[test]
#[ignore]
fn e2e_enum_variants() {
    assert_aot_matches(
        "enum_variants",
        r#"enum Shape { Circle(r), Rect { w, h }, Empty }
print(Shape::Circle(3))
print(Shape::Rect { w: 4, h: 3 })
print(Shape::Empty)"#,
    );
}

#[test]
#[ignore]
fn e2e_enum_struct_variant_field_access() {
    assert_aot_matches(
        "enum_struct_access",
        r#"enum Shape { Rect { w, h } }
let r = Shape::Rect { w: 4, h: 3 }
print(r.w * r.h)"#,
    );
}

#[test]
#[ignore]
fn e2e_method_on_struct() {
    assert_aot_matches(
        "method_struct",
        r#"struct Point { x, y }
fn Point.sum(self) { return self.x + self.y }
let p = Point { x: 3, y: 4 }
print(p.sum())"#,
    );
}

#[test]
#[ignore]
fn e2e_method_chain() {
    assert_aot_matches(
        "method_chain",
        r#"struct Adder { n }
fn Adder.then(self, m) { return Adder { n: self.n + m } }
let r = Adder { n: 1 }.then(2).then(3).then(4)
print(r.n)"#,
    );
}

#[test]
#[ignore]
fn e2e_method_on_enum() {
    assert_aot_matches(
        "method_enum",
        r#"enum Shape { Circle(r), Rect { w, h } }
fn Shape.label(self) { return "shape" }
print(Shape::Circle(5).label())
print(Shape::Rect { w: 4, h: 3 }.label())"#,
    );
}

#[test]
#[ignore]
fn e2e_method_overrides_builtin() {
    assert_aot_matches(
        "method_override",
        r#"struct Wrapper { data }
fn Wrapper.len(self) { return 99 }
let w = Wrapper { data: [1, 2, 3] }
print(w.len())"#,
    );
}

#[test]
#[ignore]
fn e2e_closure_basic_lambda() {
    assert_aot_matches(
        "closure_basic",
        r#"let double = fn(x) { return x * 2 }
print(double(5))
print(double(21))"#,
    );
}

#[test]
#[ignore]
fn e2e_closure_captures_value() {
    assert_aot_matches(
        "closure_captures",
        r#"let n = 5
let add_n = fn(x) { return x + n }
print(add_n(3))
n = 100
print(add_n(3))"#,
    );
}

#[test]
#[ignore]
fn e2e_closure_factory() {
    assert_aot_matches(
        "closure_factory",
        r#"fn make_adder(n) { return fn(x) { return x + n } }
let add5 = make_adder(5)
let add10 = make_adder(10)
print(add5(3))
print(add10(3))"#,
    );
}

#[test]
#[ignore]
fn e2e_named_fn_as_value() {
    assert_aot_matches(
        "named_fn_as_value",
        r#"fn double(x) { return x * 2 }
let f = double
print(f(7))"#,
    );
}

#[test]
#[ignore]
fn e2e_native_nested_named_functions_are_first_class_values() {
    assert_aot_matches(
        "native_nested_named_functions_are_first_class_values",
        r#"fn apply(callable, value) { return callable(value) }
fn build(flag) {
    fn transform(value) { return value + 1 }
    let assigned = transform
    if flag {
        fn transform(value) { return value * 10 }
    }
    return [assigned, transform]
}
let functions = build(true)
let stored = {"first": functions[0], "second": functions[1]}
print(stored["first"](4))
print(apply(stored["second"], 4))"#,
    );
}

#[test]
#[ignore]
fn e2e_native_nested_function_sites_are_reached_in_source_order() {
    assert_aot_matches(
        "native_nested_function_sites_are_reached_in_source_order",
        r#"fn read_before_declaration() {
    return missing
    fn missing() { return 1 }
}
fn read_dead_declaration() {
    if false {
        fn missing() { return 2 }
    }
    return missing
}
print(try_call(read_before_declaration))
print(try_call(read_dead_declaration))
if true {
    fn selected() { return 3 }
}
let retained = selected
if true {
    fn selected() { return 4 }
}
print(retained(), selected())"#,
    );
}

#[test]
#[ignore]
fn e2e_aot_exact_ref_self_recursion_retains_its_declaration_site() {
    assert_aot_modes_match_walker(
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
        "exact_ref_self_recursion",
    );
}

#[test]
#[ignore]
fn e2e_aot_active_function_site_controls_ref_mode_preflight() {
    assert_aot_modes_match_walker(
        r#"fn f(ref value) { value = 2 }
if false {
    fn f(value) { return value }
}
fn side_effect() { print("evaluated"); return 3 }
let value = 1
let attempt = fn() { f(side_effect()) }
print(try_call(attempt), value)"#,
        "active_function_ref_mode_preflight",
    );
}

#[test]
#[ignore]
fn e2e_aot_exact_value_self_arity_preflight_precedes_argument_side_effects() {
    assert_aot_modes_match_walker(
        r#"fn side() { print("ARG"); return 1 }
fn f(value) { return f(side(), side()) }
let attempt = fn() { return f(1) }
print(try_call(attempt))"#,
        "exact_value_self_arity_preflight",
    );
}

#[test]
#[ignore]
fn e2e_aot_active_value_site_and_module_named_calls_preserve_host_precedence() {
    for sandbox in [false, true] {
        let mut rust_src = transpile(
            r#"fn root_fn() { return 1 }
if false {
    fn root_fn(ref value) { value = 2 }
}
use dep.{invoke}
print(root_fn())
print(invoke())"#,
            &Options {
                emit_main: false,
                use_nybl_sys: false,
                sandbox,
                module_resolver: Some(modules_from_map([(
                    "dep",
                    "fn module_fn() { return 2 }\nfn invoke() { return module_fn() }",
                )])),
                ..Options::default()
            },
        )
        .unwrap();
        let run_call = if sandbox {
            "run(&mut host, &limits)"
        } else {
            "run(&mut host)"
        };
        rust_src.push_str(&format!(
            r#"
struct Host {{ calls: usize }}
impl ::nybl::NyblHost for Host {{
    fn call(&mut self, name: &str, _args: &[::nybl::value::Value], _line: u32) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> {{
        let value = match name {{
            "root_fn" => 99,
            "module_fn" => 98,
            _ => return None,
        }};
        self.calls += 1;
        Some(Ok(::nybl::value::Value::Int(value)))
    }}
    fn on_print(&mut self, message: &str) {{ println!("{{}}", message); }}
}}
fn main() {{
    let limits = ::nybl::NyblLimits::standard();
    let mut host = Host {{ calls: 0 }};
    {run_call}.unwrap();
    println!("calls={{}}", host.calls);
}}
"#
        ));
        let run = run_generated_source(
            if sandbox {
                "sandbox_named_function_host_precedence"
            } else {
                "native_named_function_host_precedence"
            },
            rust_src,
        );
        assert_eq!(
            run.status,
            Some(0),
            "generated program failed: {}",
            run.stderr
        );
        assert_eq!(run.stdout, "99\n98\ncalls=2");
    }
}

#[test]
#[ignore]
fn e2e_aot_failed_module_load_rolls_back_retained_function_reachability() {
    let source = r#"use stash.{get}
fn load_bad() { use bad }
print(try_call(load_bad))
let retained = get()
print(try_call(retained))"#;
    let modules = [
        (
            "stash",
            "let saved = none\nfn save(value) { saved = value }\nfn get() { return saved }",
        ),
        (
            "bad",
            "use stash.{save}\nfn helper() { return 7 }\nfn expose() { return helper() }\nsave(expose)\nmissing()",
        ),
    ];
    for sandbox in [false, true] {
        let run = run_aot_with_modules_and_opts(
            source,
            if sandbox {
                "sandbox_failed_module_reached_site_rollback"
            } else {
                "native_failed_module_reached_site_rollback"
            },
            &modules,
            &Options {
                sandbox,
                ..Options::default()
            },
        );
        assert_eq!(
            run.status,
            Some(0),
            "generated program failed: {}",
            run.stderr
        );
        let lines = run.stdout.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2, "unexpected output: {}", run.stdout);
        assert!(
            lines[0].contains("Function `missing` not found"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("Function `helper` not found"),
            "{}",
            lines[1]
        );
    }
}

#[test]
#[ignore]
fn e2e_higher_order_apply() {
    assert_aot_matches(
        "higher_order",
        r#"fn apply(f, x) { return f(x) }
fn square(n) { return n * n }
print(apply(square, 4))
print(apply(fn(n) { return n + 1 }, 4))"#,
    );
}

#[test]
#[ignore]
fn e2e_iife() {
    assert_aot_matches("iife", "print((fn(x) { return x * 3 })(4))");
}

#[test]
#[ignore]
fn e2e_aot_rest_parameters_cover_named_lambda_method_and_ref_self_calls() {
    assert_aot_modes_match_walker(
        r#"struct Box { value }
fn Box.join(self, prefix, ..items) { return [self.value, prefix, items] }
struct Counter { value }
fn Counter.add(ref self, ..items) { self.value += items.len(); return items }
fn collect(first, ..items) { return [first, items] }
let gather = fn(..items) { return items }
let box = Box { value: 7 }
let counter = Counter { value: 10 }
print(collect(1), collect(1, 2, 3))
print(gather(), gather("a", "b"))
print(box.join("x", 8, 9))
print(counter.add(1, 2, 3), counter.value)
let calls = 0
fn tick() { calls += 1; return calls }
fn invalid_rest_ref() { let target = 0; return collect(0, tick(), ref target) }
print(try_call(invalid_rest_ref), calls)"#,
        "rest_named_lambda_method_ref_self",
    );
}

#[test]
#[ignore]
fn e2e_aot_place_indices_run_before_root_snapshot() {
    assert_aot_modes_match_walker(
        r#"let values = [0, 1]
fn assign() { values[values.pop()] = 9 }
print(try_call(assign))
print(values)"#,
        "place_index_before_root_snapshot",
    );
}

#[test]
#[ignore]
fn e2e_aot_explicit_public_surfaces_filter_and_reexport_all_import_forms() {
    let source = r#"use leaf as leaf
use leaf
use facade as facade
print(leaf.visible, visible, _shown, leaf.read_hidden(), facade.visible, facade.read_hidden())
print(leaf.Visible { value: 4 }, Visible { value: 5 })
print(facade.gather(6, 7))"#;
    let modules = [
        (
            "leaf",
            r#"let visible = 1
let hidden = 2
let _shown = 3
struct Visible { value }
struct Hidden { value }
fn read_hidden() { return hidden }
fn gather(..items) { return items }
pub { visible, _shown, read_hidden, gather, Visible }"#,
        ),
        (
            "facade",
            "use leaf.{visible, read_hidden, gather}\npub { visible, read_hidden, gather }",
        ),
    ];
    for sandbox in [false, true] {
        let run = run_aot_with_modules_and_opts(
            source,
            if sandbox {
                "sandbox_explicit_public_surfaces"
            } else {
                "native_explicit_public_surfaces"
            },
            &modules,
            &Options {
                sandbox,
                ..Options::default()
            },
        );
        assert_eq!(
            run.status,
            Some(0),
            "generated program failed: {}",
            run.stderr
        );
        assert_eq!(
            run.stdout,
            "1 1 3 2 1 2\nVisible { value: 4 } Visible { value: 5 }\n[6, 7]"
        );

        let denied = transpile(
            "use leaf.{hidden}\nprint(hidden)",
            &Options {
                sandbox,
                module_resolver: Some(modules_from_map(modules)),
                ..Options::default()
            },
        )
        .unwrap_err();
        assert!(
            denied.message.contains("has no export `hidden`"),
            "{denied}"
        );

        let legacy = run_aot_with_modules_and_opts(
            "use legacy.{_private}\nprint(_private)",
            if sandbox {
                "sandbox_legacy_selective_private"
            } else {
                "native_legacy_selective_private"
            },
            &[("legacy", "let _private = 9")],
            &Options {
                sandbox,
                ..Options::default()
            },
        );
        assert_eq!(legacy.status, Some(0), "{}", legacy.stderr);
        assert_eq!(legacy.stdout, "9");
    }
}

#[test]
#[ignore]
fn e2e_sandbox_variadic_instance_entry_and_callback_pack_tracked_arrays() {
    let mut rust_src = transpile(
        r#"pub fn variadic(first, ..items) { return [first, items] }
pub fn callback() { return fn(..items) { return items } }"#,
        &Options {
            sandbox: true,
            emit_main: false,
            use_nybl_sys: false,
            ..Options::default()
        },
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct Host;
impl ::nybl::NyblHost for Host {
    fn call(&mut self, _name: &str, _args: &[::nybl::Value], _line: u32) -> Option<Result<::nybl::Value, ::nybl::NyblError>> { None }
}
fn main() {
    let limits = ::nybl::NyblLimits::standard();
    let mut host = Host;
    let mut instance = NyblInstance::load(&mut host, &limits).unwrap();
    let entry = instance.entry_points().iter().find(|entry| entry.name() == "variadic").unwrap();
    println!("{} {} {}", entry.arity(), entry.is_variadic(), entry.accepts_arity(3));
    let value = instance.call("variadic", &[::nybl::Value::Int(1), ::nybl::Value::Int(2), ::nybl::Value::Int(3)], &mut host).unwrap();
    println!("{}", value.inspect());
    let callback = instance.call("callback", &[], &mut host).unwrap();
    let value = instance.call_value(&callback, &[::nybl::Value::Int(4), ::nybl::Value::Int(5)], &mut host).unwrap();
    println!("{}", value.inspect());
    println!("{}", instance.call("variadic", &[], &mut host).unwrap_err().message);
}
"#,
    );
    let run = run_generated_source("sandbox_variadic_instance", rust_src);
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    assert_eq!(
        run.stdout,
        "1 true true\n[1, [2, 3]]\n[4, 5]\n`variadic` expects at least 1 argument, but got 0"
    );
}

#[test]
#[ignore]
fn e2e_aot_opaque_host_values_dispatch_custom_and_common_methods() {
    for sandbox in [false, true] {
        let mut rust_src = transpile(
            r#"let widget = make_widget()
print(widget.type(), widget.inspect(), widget.bump(1, 2))
fn missing() { return widget.missing() }
print(try_call(missing))"#,
            &Options {
                sandbox,
                emit_main: false,
                use_nybl_sys: false,
                ..Options::default()
            },
        )
        .unwrap();
        let run_call = if sandbox {
            "run(&mut host, &limits)"
        } else {
            "run(&mut host)"
        };
        rust_src.push_str(&format!(
            r#"
struct Host {{ method_calls: usize }}
impl ::nybl::NyblHost for Host {{
    fn call(&mut self, name: &str, _args: &[::nybl::Value], _line: u32) -> Option<Result<::nybl::Value, ::nybl::NyblError>> {{
        (name == "make_widget").then(|| Ok(::nybl::Value::new_host("widget", 40_i64)))
    }}
    fn call_method(&mut self, receiver: &::nybl::HostValue, method: &str, args: &[::nybl::Value], _line: u32) -> Option<Result<::nybl::Value, ::nybl::NyblError>> {{
        if method != "bump" {{ return None; }}
        self.method_calls += 1;
        let start = *receiver.downcast_ref::<i64>().unwrap();
        let sum = args.iter().map(|value| match value {{ ::nybl::Value::Int(value) => *value, _ => 0 }}).sum::<i64>();
        Some(Ok(::nybl::Value::Int(start + sum)))
    }}
    fn on_print(&mut self, message: &str) {{ println!("{{}}", message); }}
}}
fn main() {{
    let limits = ::nybl::NyblLimits::standard();
    let mut host = Host {{ method_calls: 0 }};
    {run_call}.unwrap();
    println!("calls={{}}", host.method_calls);
}}
"#
        ));
        let run = run_generated_source(
            if sandbox {
                "sandbox_opaque_host_methods"
            } else {
                "native_opaque_host_methods"
            },
            rust_src,
        );
        assert_eq!(run.status, Some(0), "{}", run.stderr);
        let lines = run.stdout.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], "widget <host widget> 43");
        assert!(
            lines[1].contains("doesn't have a .missing() method"),
            "{}",
            lines[1]
        );
        assert_eq!(lines[2], "calls=1");
    }
}

#[test]
#[ignore]
fn e2e_builtins_str_int_type() {
    assert_aot_matches(
        "builtins",
        r#"print(42.to_str())
print(3.7.to_int())
print("hi".type())
print(42.type())
print((-7).abs())
print(3.min(7))
print(3.max(7))
print([1, 2, 3].len())"#,
    );
}

#[test]
#[ignore]
fn e2e_range_boundaries_steps_and_i64_edges() {
    let output = run_aot(
        r#"let boundary = range(10000)
let ascending = range(-7, 29993, 3)
let descending = range(29993, -7, -3)
let min = -9223372036854775807 - 1
let max = 9223372036854775807
print(boundary.len())
print(boundary[9999])
print(ascending.len())
print(ascending[9999])
print(descending.len())
print(descending[9999])
print(range(5, 0, 1))
print(range(0, 5, -1))
print(range(min, max, max))
print(range(max, min, min))"#,
        "range_boundaries_steps_and_i64_edges",
    );
    assert_eq!(
        output,
        concat!(
            "10000\n",
            "9999\n",
            "10000\n",
            "29990\n",
            "10000\n",
            "-4\n",
            "[]\n",
            "[]\n",
            "[-9223372036854775808, -1, 9223372036854775806]\n",
            "[9223372036854775807, -1]"
        )
    );
}

#[test]
#[ignore]
fn e2e_i64_min_literal_stays_exact_through_native_aot() {
    let output = run_aot(
        r#"let min = -9223372036854775808
print(min)
print(min.type())
print(min + 1)
print(min < -9223372036854775807)
print(match min {
    -9223372036854775808 => "minimum",
    _ => "other",
})"#,
        "i64_min_literal_exact",
    );
    assert_eq!(
        output,
        "-9223372036854775808\nint\n-9223372036854775807\ntrue\nminimum"
    );
}

#[test]
#[ignore]
fn e2e_i64_min_literal_keeps_native_overflow_checks() {
    for (source, name) in [
        ("print(--9223372036854775808)", "i64_min_neg_overflow"),
        ("print(-9223372036854775808 - 1)", "i64_min_sub_overflow"),
    ] {
        let run = run_aot_with_opts(source, name, &Options::default());
        assert_eq!(run.status, Some(1), "stderr:\n{}", run.stderr);
        assert!(run.stdout.is_empty(), "unexpected stdout: {}", run.stdout);
        assert!(
            run.stderr.contains("[line 1] Integer overflow in `-`"),
            "unexpected stderr:\n{}",
            run.stderr
        );
    }
}

#[test]
#[ignore]
fn e2e_range_limit_is_fatal_through_try_call() {
    let run = run_aot_with_opts(
        r#"let result = try_call(fn() {
    return range(10001)
})
print("unreachable")"#,
        "range_limit_is_fatal_through_try_call",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        run.status,
        Some(1),
        "expected a clean Nybl error exit, not an abort; stderr:\n{}",
        run.stderr
    );
    assert!(run.stdout.is_empty(), "unexpected stdout: {}", run.stdout);
    assert!(
        run.stderr.contains(&format!(
            "[line 2] {}",
            nybl::builtins::RANGE_LIMIT_ERROR_MESSAGE
        )),
        "range-limit diagnostic had the wrong message or source line; stderr:\n{}",
        run.stderr
    );
}

#[test]
#[ignore]
fn e2e_top_level_try_renders_the_shared_friendly_hint() {
    let source = r#"enum Result { Ok(value), Err(error) }
let value = try Result::Err("boom")"#;
    let cases = [
        ("standard", Options::default()),
        (
            "sandbox",
            Options {
                sandbox: true,
                ..Options::default()
            },
        ),
    ];

    for (mode, options) in cases {
        let run = run_aot_with_opts(
            source,
            &format!("top_level_try_friendly_hint_{mode}"),
            &options,
        );
        assert_eq!(run.status, Some(1), "{mode} stderr:\n{}", run.stderr);
        assert!(
            run.stdout.is_empty(),
            "{mode} unexpected stdout: {}",
            run.stdout
        );
        assert!(
            run.stderr.contains(&format!(
                "[line 2] {}",
                nybl::error_messages::TOP_LEVEL_TRY_ERROR_MESSAGE
            )),
            "{mode} missing canonical message or source line:\n{}",
            run.stderr
        );
        assert_eq!(
            run.stderr
                .matches(nybl::error_messages::TOP_LEVEL_TRY_ERROR_MESSAGE)
                .count(),
            1,
            "{mode} printed the message more than once:\n{}",
            run.stderr
        );
        assert_eq!(
            run.stderr
                .matches(&format!(
                    "hint: {}",
                    nybl::error_messages::TOP_LEVEL_TRY_HINT
                ))
                .count(),
            1,
            "{mode} did not print exactly one canonical hint:\n{}",
            run.stderr
        );
    }
}
