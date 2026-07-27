use std::cell::RefCell;

use nybl::{NyblError, NyblHost, NyblLimits, Value};
use nybl_vm::{NyblInstance, run};

#[derive(Default)]
struct Host {
    prints: RefCell<Vec<String>>,
}

impl NyblHost for Host {
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

fn run_ok(source: &str) -> Vec<String> {
    let mut host = Host::default();
    run(source, &mut host, &NyblLimits::standard()).expect("VM program should succeed");
    host.prints.into_inner()
}

fn run_error(source: &str) -> NyblError {
    let mut host = Host::default();
    run(source, &mut host, &NyblLimits::standard()).expect_err("VM program should fail")
}

#[test]
fn explicit_and_implicit_returns_commit_all_ref_parameters() {
    assert_eq!(
        run_ok(
            r#"
fn explicit(ref left, ref right) {
    left = left + 10
    right = right + 20
    return "done"
}
fn implicit(ref value) { value = value + 1 }
let a = 1
let b = 2
print(explicit(ref a, ref b))
implicit(ref a)
print([a, b])
"#,
        ),
        ["done", "[12, 22]"]
    );
}

#[test]
fn aliases_and_forwarding_preserve_transaction_boundaries() {
    assert_eq!(
        run_ok(
            r#"
fn inner(ref value) { value = value + 10 }
fn outer(ref value, should_fail) {
    inner(ref value)
    if should_fail { panic("rollback") }
}
let alias = outer
let value = 1
alias(ref value, false)
print(value)
let caught = try_call(fn() { outer(ref value, true) })
print(value)
print(caught.is_err())
"#,
        ),
        ["11", "11", "true"]
    );
}

#[test]
fn returned_err_values_and_try_short_circuit_are_normal_commits() {
    assert_eq!(
        run_ok(
            r#"
fn return_err(ref value) {
    value = 2
    return Err("ordinary")
}
fn try_err(ref value) {
    value = 3
    return try Err("short")
}
let first = 0
let second = 0
print(return_err(ref first).is_err())
print(try_err(ref second).is_err())
print([first, second])
"#,
        ),
        ["true", "true", "[2, 3]"]
    );
}

#[test]
fn runtime_error_rolls_back_before_try_call_catches_it() {
    assert_eq!(
        run_ok(
            r#"
let value = 1
fn fail(ref target) {
    target = 99
    panic("no commit")
}
fn invoke() { fail(ref value) }
print(try_call(invoke).is_err())
print(value)
"#,
        ),
        ["true", "1"]
    );
}

#[test]
fn preflight_happens_before_argument_side_effects_and_snapshot_follows_values() {
    assert_eq!(
        run_ok(
            r#"
let calls = 0
let target = 1
fn side_effect() {
    calls = calls + 1
    target = 7
    return 5
}
fn needs_ref(ref value) { value = value + 1 }
fn bad() { needs_ref(side_effect()) }
print(try_call(bad).is_err())
print([calls, target])
fn observe(ordinary, ref value) { value = value + ordinary }
observe(side_effect(), ref target)
print([calls, target])
"#,
        ),
        ["true", "[0, 1]", "[1, 12]"]
    );
}

#[test]
fn user_methods_allow_ref_on_non_receiver_parameters() {
    assert_eq!(
        run_ok(
            r#"
struct Counter { amount }
fn Counter.add(self, ref total) {
    total = total + self.amount
}
let counter = Counter { amount: 4 }
let total = 1
counter.add(ref total)
print(total)
"#,
        ),
        ["5"]
    );
}

#[test]
fn mode_and_target_diagnostics_are_actionable() {
    let missing = run_error("fn f(ref x) {}\nlet x = 1\nf(x)");
    assert_eq!(
        missing.message,
        "argument 1 to `f` must be passed with `ref`"
    );
    assert_eq!(
        missing.friendly_hint.as_deref(),
        Some("Write `ref` before argument 1.")
    );

    let extra = run_error("fn f(x) {}\nlet x = 1\nf(ref x)");
    assert_eq!(
        extra.message,
        "argument 1 to `f` is a value parameter and can't use `ref`"
    );
    assert_eq!(
        extra.friendly_hint.as_deref(),
        Some("Remove `ref` from argument 1.")
    );

    let invalid = run_error("fn f(ref x) {}\nf(ref [1])");
    assert_eq!(
        invalid.message,
        "`ref` argument 1 must name a mutable variable"
    );

    let duplicate = run_error("fn f(ref x, ref y) {}\nlet x = 1\nf(ref x, ref x)");
    assert_eq!(
        duplicate.message,
        "the same variable can't be passed to more than one `ref` parameter"
    );
}

#[test]
fn constants_and_captured_targets_are_rejected() {
    let constant = run_error("fn f(ref x) {}\nconst VALUE = 1\nf(ref VALUE)");
    assert_eq!(constant.message, "can't reassign `VALUE` — it's a constant");

    let capture = run_error(
        r#"
fn build(ref value) {
    return fn() { return value }
}
let value = 1
build(ref value)
"#,
    );
    assert_eq!(
        capture.message,
        "a `ref` parameter can't be captured by a closure"
    );
}

#[test]
fn captured_target_fence_precedes_ordinary_argument_side_effects() {
    assert_eq!(
        run_ok(
            r#"
let calls = 0
fn side_effect() {
    calls = calls + 1
    return 7
}
fn accept(ref target, ordinary) {}
fn build() {
    let captured = 1
    return fn() { accept(ref captured, side_effect()) }
}
let invoke = build()
print(try_call(invoke).is_err())
print(calls)
"#,
        ),
        ["true", "0"]
    );

    let error = run_error(
        r#"
fn accept(ref target, ordinary) {}
fn build() {
    let captured = 1
    return fn() { accept(ref captured, 0) }
}
build()()
"#,
    );
    assert_eq!(
        error.message,
        "`ref` argument 1 can't target a closure-captured binding"
    );
    assert_eq!(
        error.friendly_hint.as_deref(),
        Some("Pass the binding through an explicit `ref` parameter instead.")
    );
}

#[test]
fn fatal_unwind_discards_staged_values_and_instance_remains_reusable() {
    let mut host = Host::default();
    let limits = NyblLimits {
        max_steps: 40,
        max_memory: 1024 * 1024,
    };
    let mut instance = NyblInstance::load(
        r#"
let value = 1
fn mutate_then_spin(ref target) {
    target = 99
    while true {}
}
pub fn fail() { mutate_then_spin(ref value) }
pub fn read() { return value }
"#,
        &mut host,
        &limits,
    )
    .expect("load");

    let failure = instance.call("fail", &[], &mut host).expect_err("fatal");
    assert!(failure.is_fatal);
    assert_eq!(
        instance
            .call("read", &[], &mut host)
            .expect("instance reusable")
            .inspect(),
        "1"
    );
}

#[test]
fn final_return_memory_check_runs_before_ref_commit() {
    let mut host = Host::default();
    let limits = NyblLimits {
        max_steps: 100,
        max_memory: 32,
    };
    let mut instance = NyblInstance::load(
        r#"
let value = 1
fn allocate_on_return(ref target) {
    target = 99
    return "abcdefghijklmnopqrstuvwxyz0123456789"
}
pub fn fail() { allocate_on_return(ref value) }
pub fn read() { return value }
"#,
        &mut host,
        &limits,
    )
    .expect("load");

    let failure = instance.call("fail", &[], &mut host).expect_err("fatal");
    assert!(failure.is_fatal);
    assert!(failure.message.contains("Memory limit"));
    assert_eq!(
        instance
            .call("read", &[], &mut host)
            .expect("temporary allocation was released")
            .inspect(),
        "1"
    );
}
