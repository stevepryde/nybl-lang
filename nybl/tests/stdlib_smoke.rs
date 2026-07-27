//! Smoke tests for the bundled stdlib modules. Each module is
//! resolved through `nybl::stdlib::resolve` and executed against the
//! tree-walker with a minimal host that:
//!
//! - captures `print` output so tests can assert on it
//! - routes `use std.*` back through `nybl::stdlib::resolve` so
//!   stdlib modules importing each other work the same way
//!   `StandardHost` handles them.
//!
//! Keeping these smoke tests in `nybl-lang` itself means refactors to the
//! `.nybl` sources break cleanly inside this crate rather than
//! trickling into `nybl-cli` or embedder tests downstream.

use std::cell::RefCell;

use nybl::{NyblError, NyblHost, NyblLimits, Value};

struct BufHost {
    prints: RefCell<Vec<String>>,
}

impl BufHost {
    fn new() -> Self {
        Self {
            prints: RefCell::new(Vec::new()),
        }
    }

    fn last(&self) -> String {
        self.prints
            .borrow()
            .last()
            .cloned()
            .expect("expected at least one print")
    }
}

impl NyblHost for BufHost {
    fn call(
        &mut self,
        _name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        None
    }

    fn on_print(&mut self, msg: &str) {
        self.prints.borrow_mut().push(msg.to_string());
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        nybl::stdlib::resolve(name).map(|src| Ok(src.to_string()))
    }
}

fn run(code: &str) -> BufHost {
    let mut host = BufHost::new();
    nybl::run(code, &mut host, &NyblLimits::standard())
        .unwrap_or_else(|e| panic!("program errored: {e}\n\n{code}"));
    host
}

// ─── Result methods ───────────────────────────────────────────
//
// `Result` is a built-in enum; its combinators are engine-level
// methods dispatched via `methods::result_method` and the per-
// engine callable helpers. No `use std.result` required.

#[test]
fn result_is_ok_and_is_err() {
    let host = run(r#"print(Result::Ok(1).is_ok())
print(Result::Err("boom").is_err())
print(Result::Err("x").is_ok())"#);
    assert_eq!(host.prints.borrow().clone(), vec!["true", "true", "false"]);
}

#[test]
fn result_unwrap_or_returns_default_on_err() {
    let host = run(r#"print(Result::Ok(42).unwrap_or(0))
print(Result::Err("boom").unwrap_or(99))"#);
    assert_eq!(host.prints.borrow().clone(), vec!["42", "99"]);
}

#[test]
fn result_map_applies_only_to_ok() {
    let host = run(r#"let doubled = Result::Ok(5).map(fn(x) { return x * 2 })
print(match doubled {
    Result::Ok(v) => v,
    Result::Err(_) => -1,
})
let passed = Result::Err("stop").map(fn(x) { return x * 2 })
print(match passed {
    Result::Ok(_) => "ok?",
    Result::Err(e) => e,
})"#);
    assert_eq!(host.prints.borrow().clone(), vec!["10", "stop"]);
}

#[test]
fn result_map_err_applies_only_to_err() {
    let host = run(
        r#"let tagged = Result::Err("bad").map_err(fn(e) { return e + "!" })
print(match tagged {
    Result::Ok(_)  => "ok?",
    Result::Err(e) => e,
})
let passed = Result::Ok(5).map_err(fn(e) { return e + "!" })
print(match passed {
    Result::Ok(v)  => v,
    Result::Err(_) => -1,
})"#,
    );
    assert_eq!(host.prints.borrow().clone(), vec!["bad!", "5"]);
}

#[test]
fn result_and_then_chains_fallible_steps() {
    let host = run(r#"fn halve(x) {
    if x % 2 == 0 { return Result::Ok((x / 2).to_int()) }
    return Result::Err("odd")
}
let r = Result::Ok(8).and_then(halve).and_then(halve)
print(match r { Result::Ok(v) => v, Result::Err(e) => e })
let bad = Result::Ok(7).and_then(halve)
print(match bad { Result::Ok(_) => "ok?", Result::Err(e) => e })"#);
    assert_eq!(host.prints.borrow().clone(), vec!["2", "odd"]);
}

#[test]
fn result_unwrap_on_err_raises_with_message() {
    // `unwrap` / `expect` raise a runtime error carrying the
    // inspected payload (or caller-supplied message). `try_call`
    // catches it so the test can observe `e.message` directly.
    let host = run(
        r#"let r = try_call(fn() { return Result::Err("boom").unwrap() })
print(match r {
    Result::Ok(_)  => "unexpected ok",
    Result::Err(e) => e.message,
})
let r2 = try_call(fn() { return Result::Err("bad").expect("custom message") })
print(match r2 {
    Result::Ok(_)  => "unexpected ok",
    Result::Err(e) => e.message,
})"#,
    );
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["unwrap on Err: \"boom\"", "custom message"],
    );
}

#[test]
fn result_methods_work_without_any_use() {
    // The whole point of moving these off `std.result` was to
    // make them always available — no imports, no host resolver
    // involvement, just method calls on the built-in `Result`.
    let host = run(r#"let mapped = Result::Ok(10).map(fn(v) { return v + 1 })
print(mapped)
print(Result::Err("x").is_err())"#);
    assert_eq!(host.prints.borrow().clone(), vec!["Result::Ok(11)", "true"],);
}

// ─── Dict missing-key lookup ──────────────────────────────────
//
// `d[key]` for an absent key returns `none` rather than raising.
// Matches Python / JS / Lua — composes naturally with the
// `.is_none()` / `.is_some()` check, and aligns with the
// language's "any variable can be `none`" story. Callers who
// need strict presence checks use `d.has(key)` explicitly.

#[test]
fn dict_missing_key_returns_none() {
    let host = run(r#"let d = {"hp": 10}
print(d["hp"])
print(d["missing"])
print(d["missing"].is_none())
print(d["missing"].is_some())"#);
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["10", "none", "true", "false"],
    );
}

#[test]
fn dict_missing_key_distinct_from_explicit_none_value() {
    // A key mapped to an actual `none` value is still "present"
    // in the sense that `d.has(k)` reports `true`; the soft
    // lookup can't distinguish the two (both yield `none`),
    // which matches Python's default behaviour and keeps the
    // primitive simple. Callers who care reach for `d.has(k)`.
    let host = run(r#"let d = {"set": none}
print(d["set"])
print(d["set"].is_none())
print(d["absent"].is_none())
print(d.has("set"))
print(d.has("absent"))"#);
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["none", "true", "true", "true", "false"],
    );
}

#[test]
fn dict_assign_to_missing_key_creates_entry() {
    // Regression: the soft-read behaviour must not affect
    // writes. Assigning to an absent key still inserts it.
    let host = run(r#"let d = {}
d["new"] = 42
print(d["new"])
print(d.has("new"))"#);
    assert_eq!(host.prints.borrow().clone(), vec!["42", "true"],);
}

// ─── is_none / is_some universal methods ──────────────────────
//
// Any value can be checked for `none`-ness via `.is_none()` /
// `.is_some()`. They're common methods (alongside `.type()` /
// `.to_str()` / `.inspect()`) because every variable, regardless
// of declared shape, can hold `none` in a dynamically-typed
// language — not just `Option`-shaped receivers.

#[test]
fn is_none_detects_only_the_none_value() {
    // Truthy / falsy values *other* than `none` are not none.
    // `is_none()` is specifically "is this the Value::None
    // variant?", not "is this falsy?".
    let host = run(r#"print(none.is_none())
print((42).is_none())
print("".is_none())
print([].is_none())
print({}.is_none())
print(false.is_none())
print((0).is_none())"#);
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["true", "false", "false", "false", "false", "false", "false"],
    );
}

#[test]
fn is_some_is_the_inverse_of_is_none() {
    let host = run(r#"print(none.is_some())
print((0).is_some())
print("".is_some())
print(false.is_some())"#);
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["false", "true", "true", "true"],
    );
}

#[test]
fn is_none_works_on_optional_return_values() {
    // Motivating use case: a helper that returns `none` for
    // "nothing found" composes cleanly with `.is_none()` at
    // the call site without a `== none` detour.
    let host = run(r#"fn first_or_none(arr) {
    if arr.len() == 0 { return none }
    return arr[0]
}
let a = first_or_none([])
let b = first_or_none([10, 20])
if a.is_none() { print("empty") }
if b.is_some() { print("got: " + b.to_str()) }"#);
    assert_eq!(host.prints.borrow().clone(), vec!["empty", "got: 10"],);
}

#[test]
fn is_none_equivalent_to_equality_check() {
    // `.is_none()` and `== none` must agree on every value.
    let host = run(r#"let vs = [none, 0, "", [], false, 42]
for v in vs { print(v.is_none() == (v == none)) }"#);
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["true", "true", "true", "true", "true", "true"],
    );
}

// ─── Ok / Err shorthand ────────────────────────────────────────
//
// `Ok(x)` / `Err(e)` are parser-level sugar for
// `Result::Ok(x)` / `Result::Err(e)`. Both the expression form
// and the pattern form get the desugar so match arms stay
// readable and returns stay short.

#[test]
fn ok_err_expression_sugar_produces_result_variants() {
    let host = run(r#"print(Ok(42))
print(Err("boom"))
print(Ok("nested").is_ok())
print(Err(123).is_err())"#);
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["Result::Ok(42)", "Result::Err(\"boom\")", "true", "true",],
    );
}

#[test]
fn ok_err_pattern_sugar_matches_result_variants() {
    // Pattern side — `match r { Ok(v) => ..., Err(e) => ... }`
    // desugars to `Result::Ok(v)` / `Result::Err(e)`.
    let host = run(r#"fn describe(r) {
    return match r {
        Ok(v) => "ok: " + v.to_str(),
        Err(e) => "err: " + e,
    }
}
print(describe(Ok(5)))
print(describe(Err("stop")))"#);
    assert_eq!(host.prints.borrow().clone(), vec!["ok: 5", "err: stop"],);
}

#[test]
fn ok_err_shorthand_chains_through_try_call() {
    // The shorthand plays nicely with `try_call` since the
    // desugared value is an actual `Result::Err(RuntimeError
    // { ... })` — but `Err("x")` produces a string payload, so
    // match arms decide per payload shape.
    let host = run(r#"fn fail() { return Err("explicit") }
let r = try_call(fail)
print(match r {
    Ok(inner) => match inner {
        Ok(_)  => "double ok?",
        Err(e) => "inner err: " + e,
    },
    Err(_) => "caught",
})"#);
    // `fail()` returns `Err("explicit")` — a successful return,
    // so `try_call` wraps it in `Result::Ok(Result::Err(...))`.
    // Inner match picks it up as `Err(e)`.
    assert_eq!(host.prints.borrow().clone(), vec!["inner err: explicit"]);
}

// ─── Iterator protocol ─────────────────────────────────────────
//
// `.iter()` on arrays / strings / dicts yields a `Value::Iter`
// whose `.next()` returns `Iter::Next(v)` / `Iter::Done`. User
// types that implement `.iter()` participate in the same
// protocol so `for x in my_container` works out of the box.

#[test]
fn iter_array_next_returns_iter_next_until_done() {
    let host = run(r#"let it = [10, 20, 30].iter()
print(it.type())
print(it.next())
print(it.next())
print(it.next())
print(it.next())
print(it.next())"#);
    assert_eq!(
        host.prints.borrow().clone(),
        vec![
            "iter",
            "Iter::Next(10)",
            "Iter::Next(20)",
            "Iter::Next(30)",
            "Iter::Done",
            "Iter::Done",
        ],
    );
}

#[test]
fn iter_string_yields_code_points() {
    // Strings iterate by Unicode code point — each yielded item
    // is a 1-char string, matching `for c in s` semantics.
    let host = run(r#"let it = "ab".iter()
print(it.next())
print(it.next())
print(it.next())"#);
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["Iter::Next(\"a\")", "Iter::Next(\"b\")", "Iter::Done"],
    );
}

#[test]
fn iter_dict_yields_keys_in_declaration_order() {
    let host = run(r#"let it = {"a": 1, "b": 2}.iter()
print(it.next())
print(it.next())
print(it.next())"#);
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["Iter::Next(\"a\")", "Iter::Next(\"b\")", "Iter::Done"],
    );
}

#[test]
fn iter_is_its_own_iterator() {
    // `iterator.iter()` returns the same iterator — matches the
    // Python / Rust convention so `for x in arr.iter()` doesn't
    // need a special case in the for-loop dispatcher.
    let host = run(r#"let a = [1, 2].iter()
let b = a.iter()
print(a.next())
print(b.next())
print(a.next())"#);
    // `b` shares state with `a` — cloning `Value::Iter` bumps an
    // Rc, it doesn't fork the cursor.
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["Iter::Next(1)", "Iter::Next(2)", "Iter::Done"],
    );
}

#[test]
fn for_in_works_on_value_iter() {
    // `for x in arr.iter()` uses the protocol path, not the
    // eager one. Must produce the same output as `for x in arr`.
    let host = run(r#"for x in [1, 2, 3].iter() { print(x) }"#);
    assert_eq!(host.prints.borrow().clone(), vec!["1", "2", "3"],);
}

#[test]
fn for_in_supports_user_container_via_iter_method() {
    // A user container that delegates `.iter()` to a backing
    // array is the motivating use case for the protocol:
    // structural typing without a trait system. `for v in b`
    // calls `Bag.iter(b)` → backing array iterator → loops.
    let host = run(r#"struct Bag { items }
fn bag_of(arr) { return Bag { items: arr } }
fn Bag.iter(self) { return self.items.iter() }

let b = bag_of(["x", "y", "z"])
for v in b { print(v) }"#);
    assert_eq!(host.prints.borrow().clone(), vec!["x", "y", "z"],);
}

#[test]
fn for_in_on_dict_iterates_keys() {
    // Dict for-loops now materialise the keys up front; the
    // eager fast path stays equivalent to `for k in d.iter()`.
    let host = run(r#"for k in {"alpha": 1, "beta": 2} { print(k) }"#);
    assert_eq!(host.prints.borrow().clone(), vec!["alpha", "beta"],);
}

#[test]
fn for_in_on_primitive_raises_cant_iterate_error() {
    let mut host = BufHost::new();
    let err = nybl::run(
        "for x in 42 { print(x) }",
        &mut host,
        &NyblLimits::standard(),
    )
    .expect_err("expected for-loop over int to fail");
    let msg = err.to_string();
    assert!(msg.contains("Can't iterate over int"), "got: {msg}");
}

// ─── panic builtin ─────────────────────────────────────────────

#[test]
fn panic_raises_non_fatal_runtime_error() {
    // Bare `panic(msg)` raises with the message verbatim, and the
    // resulting error is non-fatal so `try_call` catches it.
    let host = run(r#"let r = try_call(fn() { panic("deliberate") })
print(match r {
    Result::Ok(_)  => "unexpected ok",
    Result::Err(e) => e.message,
})"#);
    assert_eq!(host.prints.borrow().clone(), vec!["deliberate"]);
}

#[test]
fn panic_propagates_when_not_caught() {
    // Outside of `try_call`, a `panic` should bubble up as a
    // top-level program error. The `run` helper's `unwrap_or_else`
    // would blow up on success, so we reach for `nybl::run`
    // directly and assert on the error surface.
    let mut host = BufHost::new();
    let err = nybl::run("panic(\"top-level\")", &mut host, &NyblLimits::standard())
        .expect_err("expected panic to surface as an error");
    assert!(
        err.to_string().contains("top-level"),
        "panic message should appear verbatim in the error: {err}"
    );
}

// ─── std.test — exercises the assertion surface directly ──────

#[test]
fn test_assert_eq_failure_carries_inspected_values() {
    // `assert_eq` (and siblings) now route through `panic` so
    // the user sees a readable message in `Err(e).message`
    // rather than "Can't index none with string".
    let host = run(r#"use std.test
let r = try_call(fn() { assert_eq(1, 2) })
print(match r {
    Result::Ok(_)  => "unexpected ok",
    Result::Err(e) => e.message,
})"#);
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["assert_eq failed: 1 != 2"],
    );
}

// ─── std.math ──────────────────────────────────────────────────

#[test]
fn math_constants_have_expected_precision() {
    let host = run(r#"use std.math
print(PI)
print(E)
print(TAU)"#);
    let prints = host.prints.borrow();
    assert!(prints[0].starts_with("3.14159"));
    assert!(prints[1].starts_with("2.71828"));
    assert!(prints[2].starts_with("6.28318"));
}

#[test]
fn math_clamp_bounds_correctly() {
    let host = run(r#"use std.math
print(clamp(5, 0, 10))
print(clamp(-3, 0, 10))
print(clamp(20, 0, 10))"#);
    assert_eq!(host.prints.borrow().clone(), vec!["5", "0", "10"]);
}

#[test]
fn math_sign_handles_zero_and_negatives() {
    let host = run(r#"use std.math
print(sign(5))
print(sign(-3))
print(sign(0))"#);
    assert_eq!(host.prints.borrow().clone(), vec!["1", "-1", "0"]);
}

#[test]
fn math_factorial_and_gcd_and_lcm() {
    let host = run(r#"use std.math
print(factorial(5))
print(gcd(12, 18))
print(lcm(4, 6))"#);
    assert_eq!(host.prints.borrow().clone(), vec!["120", "6", "12"]);
}

#[test]
fn math_mean_empty_check() {
    // `mean` lives in `std.math`. `sum` used to live here
    // too; it was a duplicate of `std.iter.sum`, so it moved
    // out and `mean` now inlines its own running total.
    let host = run(r#"use std.math
print(mean([2, 4, 6]))
print(mean([10, 20, 30, 40, 50]))"#);
    assert_eq!(host.prints.borrow().clone(), vec!["4", "30"]);
}

// ─── std.iter ──────────────────────────────────────────────────

#[test]
fn iter_map_filter_reduce() {
    let host = run(r#"use std.iter
let nums = [1, 2, 3, 4, 5]
let doubled = map(nums, fn(x) { return x * 2 })
print(doubled)
let evens = filter(nums, fn(x) { return x % 2 == 0 })
print(evens)
let total = reduce(nums, 0, fn(a, b) { return a + b })
print(total)"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "[2, 4, 6, 8, 10]");
    assert_eq!(prints[1], "[2, 4]");
    assert_eq!(prints[2], "15");
}

#[test]
fn iter_take_and_drop() {
    let host = run(r#"use std.iter
print(take([10, 20, 30, 40], 2))
print(drop([10, 20, 30, 40], 2))"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "[10, 20]");
    assert_eq!(prints[1], "[30, 40]");
}

#[test]
fn iter_zip_and_enumerate() {
    let host = run(r#"use std.iter
print(zip([1, 2, 3], ["a", "b", "c"]))
print(enumerate(["x", "y"]))"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "[[1, \"a\"], [2, \"b\"], [3, \"c\"]]");
    assert_eq!(prints[1], "[[0, \"x\"], [1, \"y\"]]");
}

#[test]
fn iter_any_all_count() {
    let host = run(r#"use std.iter
let is_pos = fn(x) { return x > 0 }
print(all([1, 2, 3], is_pos))
print(all([1, -2, 3], is_pos))
print(any([-1, -2, 3], is_pos))
print(count([1, 2, 3, 4], fn(x) { return x % 2 == 0 }))"#);
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["true", "false", "true", "2"]
    );
}

#[test]
fn iter_find_and_find_index() {
    let host = run(r#"use std.iter
print(find([1, 2, 3], fn(x) { return x > 1 }))
print(find_index([1, 2, 3], fn(x) { return x > 1 }))
print(find([1, 2, 3], fn(x) { return x > 99 }))
print(find_index([1, 2, 3], fn(x) { return x > 99 }))"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "2");
    assert_eq!(prints[1], "1");
    assert_eq!(prints[2], "none");
    assert_eq!(prints[3], "-1");
}

#[test]
fn iter_flatten_sum_product() {
    let host = run(r#"use std.iter
print(flatten([[1, 2], [3, 4], [5]]))
print(sum([1, 2, 3, 4]))
print(product([2, 3, 4]))"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "[1, 2, 3, 4, 5]");
    assert_eq!(prints[1], "10");
    assert_eq!(prints[2], "24");
}

#[test]
fn iter_min_and_max() {
    let host = run(r#"use std.iter
print(min_array([5, 3, 8, 1, 4]))
print(max_array([5, 3, 8, 1, 4]))"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "1");
    assert_eq!(prints[1], "8");
}

// ─── std.string ────────────────────────────────────────────────

#[test]
fn string_pad_left_right_and_center() {
    let host = run(r#"use std.string
print(pad_left("42", 5, " "))
print(pad_right("42", 5, "0"))
print(center("hi", 6, "-"))"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "   42");
    assert_eq!(prints[1], "42000");
    assert_eq!(prints[2], "--hi--");
}

#[test]
fn string_chars_reverse_palindrome() {
    let host = run(r#"use std.string
print(chars("abc"))
print(reverse("hello"))
print(is_palindrome("racecar"))
print(is_palindrome("hello"))"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "[\"a\", \"b\", \"c\"]");
    assert_eq!(prints[1], "olleh");
    assert_eq!(prints[2], "true");
    assert_eq!(prints[3], "false");
}

#[test]
fn string_count_substrings() {
    let host = run(r#"use std.string
print(count("banana", "na"))
print(count("aaaa", "aa"))
print(count("empty", ""))"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "2");
    // Non-overlapping: "aaaa" → "aa" matches at 0 and 2, count = 2.
    assert_eq!(prints[1], "2");
    assert_eq!(prints[2], "0");
}

// ─── std.test ──────────────────────────────────────────────────

#[test]
fn test_asserts_pass_silently() {
    // Successful asserts shouldn't produce any output or
    // error out.
    let host = run(r#"use std.test
assert(true, "should pass")
assert_eq(1 + 1, 2)
assert_near(0.1 + 0.2, 0.3, 0.01)
print("all good")"#);
    assert_eq!(host.last(), "all good");
}

#[test]
fn test_assert_eq_failure_raises() {
    let mut host = BufHost::new();
    let err = nybl::run(
        r#"use std.test
assert_eq(1, 2)"#,
        &mut host,
        &NyblLimits::standard(),
    )
    .expect_err("assert_eq should raise");
    assert!(!err.message.is_empty(), "got empty error message");
}

#[test]
fn test_assert_raises_catches_expected_failure() {
    let host = run(r#"use std.test
assert_raises(fn() { return 1 / 0 })
print("caught")"#);
    assert_eq!(host.last(), "caught");
}

// ─── Core math builtins (no use needed) ────────────────────

#[test]
fn core_math_builtins_available_without_import() {
    let host = run(r#"print(16.sqrt())
print(3.7.floor())
print(3.2.ceil())
print(3.5.round())
print(2.pow(10))"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "4");
    assert_eq!(prints[1], "3");
    assert_eq!(prints[2], "4");
    assert_eq!(prints[3], "4");
    assert_eq!(prints[4], "1024");
}

#[test]
fn core_math_floor_ceil_round_return_int_when_possible() {
    let host = run(r#"print(3.7.floor().type())
print(3.2.ceil().type())
print(3.5.round().type())"#);
    assert_eq!(host.prints.borrow().clone(), vec!["int", "int", "int"]);
}

// ─── std.collections ──────────────────────────────────────────

#[test]
fn collections_stack_push_top_pop() {
    let host = run(r#"use std.collections
let s = stack()
s = s.push(1)
s = s.push(2)
s = s.push(3)
print(s.top())
print(s.size())
s = s.pop()
print(s.top())
print(s.size())"#);
    assert_eq!(host.prints.borrow().clone(), vec!["3", "3", "2", "2"]);
}

#[test]
fn collections_stack_pop_empty_is_noop() {
    let host = run(r#"use std.collections
let s = stack()
s = s.pop()
print(s.is_empty())
print(s.top())"#);
    assert_eq!(host.prints.borrow().clone(), vec!["true", "none"]);
}

#[test]
fn collections_queue_fifo_order() {
    let host = run(r#"use std.collections
let q = queue()
q = q.enqueue("a")
q = q.enqueue("b")
q = q.enqueue("c")
print(q.front())
print(q.size())
q = q.dequeue()
print(q.front())
q = q.dequeue()
print(q.front())
q = q.dequeue()
print(q.is_empty())"#);
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["a", "3", "b", "c", "true"]
    );
}

#[test]
fn collections_set_add_remove_has() {
    let host = run(r#"use std.collections
let s = set()
s = s.add(1)
s = s.add(2)
s = s.add(2)  // duplicate, no-op
s = s.add(3)
print(s.size())
print(s.has(2))
print(s.has(99))
s = s.remove(2)
print(s.has(2))
print(s.size())"#);
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["3", "true", "false", "false", "2"]
    );
}

#[test]
fn collections_set_of_handles_duplicates() {
    let host = run(r#"use std.collections
let s = set_of([1, 2, 1, 3, 2])
print(s.size())
print(s.values())"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "3");
    assert_eq!(prints[1], "[1, 2, 3]");
}

#[test]
fn collections_set_union_intersect_difference() {
    let host = run(r#"use std.collections
let a = set_of([1, 2, 3])
let b = set_of([2, 3, 4])
print(a.union(b).values())
print(a.intersect(b).values())
print(a.difference(b).values())"#);
    let prints = host.prints.borrow();
    // Union: [1,2,3,4]. intersect: [2,3]. difference: [1].
    assert_eq!(prints[0], "[1, 2, 3, 4]");
    assert_eq!(prints[1], "[2, 3]");
    assert_eq!(prints[2], "[1]");
}

#[test]
fn collections_remove_preserves_order() {
    let host = run(r#"use std.collections
let s = set_of([1, 2, 3, 4, 5])
s = s.remove(3)
print(s.values())"#);
    assert_eq!(host.prints.borrow()[0], "[1, 2, 4, 5]");
}

// ─── std.json ──────────────────────────────────────────────────

#[test]
fn json_stringify_scalars_and_null() {
    let host = run(r#"use std.json
print(stringify(none))
print(stringify(true))
print(stringify(false))
print(stringify(42))
print(stringify(3.14))
print(stringify("hello"))"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "null");
    assert_eq!(prints[1], "true");
    assert_eq!(prints[2], "false");
    assert_eq!(prints[3], "42");
    assert_eq!(prints[4], "3.14");
    assert_eq!(prints[5], "\"hello\"");
}

#[test]
fn json_stringify_escapes_special_chars() {
    // Source has a literal newline inside a quoted string in
    // Nybl — the `\n` escape is recognised at lex time.
    let host = run(r#"use std.json
print(stringify("a\"b"))
print(stringify("a\\b"))
print(stringify("a\nb"))
print(stringify("a\tb"))"#);
    let prints = host.prints.borrow();
    // Each assertion compares against the literal JSON bytes
    // the stringify helper emits.
    assert_eq!(prints[0], "\"a\\\"b\"");
    assert_eq!(prints[1], "\"a\\\\b\"");
    assert_eq!(prints[2], "\"a\\nb\"");
    assert_eq!(prints[3], "\"a\\tb\"");
}

#[test]
fn json_stringify_array() {
    let host = run(r#"use std.json
print(stringify([1, 2, 3]))
print(stringify([]))
print(stringify(["a", 1, true, none]))"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "[1,2,3]");
    assert_eq!(prints[1], "[]");
    assert_eq!(prints[2], "[\"a\",1,true,null]");
}

#[test]
fn json_stringify_dict() {
    let host = run(r#"use std.json
let d = {"name": "nybl", "version": 1}
print(stringify(d))"#);
    // Dict iteration order is insertion order in Nybl.
    assert_eq!(host.prints.borrow()[0], "{\"name\":\"nybl\",\"version\":1}");
}

#[test]
fn json_parse_scalars() {
    let host = run(r#"use std.json
print(parse("null"))
print(parse("true"))
print(parse("false"))
print(parse("42"))
print(parse("3.14"))
print(parse("-7"))
print(parse("\"hello\""))"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "none");
    assert_eq!(prints[1], "true");
    assert_eq!(prints[2], "false");
    assert_eq!(prints[3], "42");
    assert_eq!(prints[4], "3.14");
    assert_eq!(prints[5], "-7");
    assert_eq!(prints[6], "hello");
}

#[test]
fn json_parse_number_types() {
    // `42` is int; `42.0` / `1e2` are numbers.
    let host = run(r#"use std.json
print(parse("42").type())
print(parse("42.0").type())
print(parse("1e2").type())
print(parse("-3").type())"#);
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["int", "number", "number", "int"]
    );
}

#[test]
fn json_parse_array() {
    let host = run(r#"use std.json
let arr = parse("[1, 2, 3]")
print(arr)
print(arr.len())
let empty = parse("[]")
print(empty.len())"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "[1, 2, 3]");
    assert_eq!(prints[1], "3");
    assert_eq!(prints[2], "0");
}

#[test]
fn json_parse_object_indexes_by_key() {
    let host = run(r#"use std.json
let o = parse("{\"name\": \"nybl\", \"n\": 42}")
print(o["name"])
print(o["n"])"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "nybl");
    assert_eq!(prints[1], "42");
}

#[test]
fn json_parse_nested_structures() {
    let host = run(r#"use std.json
let txt = "{\"users\": [{\"name\": \"a\"}, {\"name\": \"b\"}]}"
let data = parse(txt)
print(data["users"].len())
print(data["users"][0]["name"])
print(data["users"][1]["name"])"#);
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "2");
    assert_eq!(prints[1], "a");
    assert_eq!(prints[2], "b");
}

#[test]
fn json_parse_handles_whitespace() {
    let host = run(r#"use std.json
let txt = "  [  1 ,  2 , 3  ]  "
let arr = parse(txt)
print(arr)"#);
    assert_eq!(host.prints.borrow()[0], "[1, 2, 3]");
}

#[test]
fn json_parse_string_escapes() {
    let host = run(r#"use std.json
let s = parse("\"a\\\"b\\nc\"")
print(s)
print(s.len())"#);
    let prints = host.prints.borrow();
    // a, ", b, \n, c — 5 chars.
    assert_eq!(prints[1], "5");
    assert!(prints[0].contains("\"b"));
}

#[test]
fn json_parse_roundtrip_via_stringify() {
    // stringify(parse(x)) should produce semantically the
    // same data; whitespace may differ.
    let host = run(r#"use std.json
let original = {"name": "nybl", "ok": true, "vals": [1, 2, 3]}
let dumped = stringify(original)
print(dumped)
let roundtrip = parse(dumped)
print(roundtrip["name"])
print(roundtrip["ok"])
print(roundtrip["vals"])"#);
    let prints = host.prints.borrow();
    assert_eq!(
        prints[0],
        "{\"name\":\"nybl\",\"ok\":true,\"vals\":[1,2,3]}"
    );
    assert_eq!(prints[1], "nybl");
    assert_eq!(prints[2], "true");
    assert_eq!(prints[3], "[1, 2, 3]");
}

#[test]
fn json_parse_error_raises_and_try_call_catches() {
    let host = run(r#"use std.json
let r = try_call(fn() { return parse("[1, 2,") })
print(match r {
    Result::Ok(_) => "ok?",
    Result::Err(e) => "caught",
})"#);
    assert_eq!(host.prints.borrow()[0], "caught");
}

#[test]
fn json_parse_empty_input_errors() {
    let host = run(r#"use std.json
let r = try_call(fn() { return parse("   ") })
print(match r {
    Result::Ok(_) => "ok?",
    Result::Err(_) => "err",
})"#);
    assert_eq!(host.prints.borrow()[0], "err");
}

#[test]
fn collections_composes_with_std_iter() {
    // Collections + iter helpers interoperate because a Set's
    // .values() / a Queue's items are ordinary arrays.
    let host = run(r#"use std.collections
use std.iter
let s = set_of([1, 2, 3, 4, 5])
let evens = filter(s.values(), fn(x) { return x % 2 == 0 })
print(evens)"#);
    assert_eq!(host.prints.borrow()[0], "[2, 4]");
}
