//! Native ↔ wasm execution-parity harness.
//!
//! Runs a curated corpus of Nybl programs through **both** engines
//! (the `nybl-lang` tree-walker and the `nybl-vm` bytecode VM) and
//! prints one canonicalized transcript to stdout: for each program an
//! index header, then every print line and the success/error outcome
//! per engine, byte-stable.
//!
//! CI builds this binary twice — natively and for `wasm32-wasip1`
//! (run under wasmtime) — and byte-compares the two transcripts. Any
//! difference means Nybl execution has drifted between native and
//! wasm (float formatting, libm/platform math results, integer
//! semantics, hashing/ordering, …), which breaks downstream
//! determinism invariants. The same transcript also cross-checks the
//! two engines against each other on every platform it runs on.
//!
//! The corpus is chosen for platform-drift sensitivity: float
//! arithmetic and formatting, transcendental math builtins
//! (sqrt/sin/cos/tan/exp/log/pow), rounding at the i64 boundary,
//! negative division/modulo, string interpolation, dict/array
//! ordering, the deterministic `rand` sequence, error-message text,
//! and composite programs mixing loops/closures/structs/enums.
//! Program syntax mirrors `nybl-vm/tests/differential.rs`.

use nybl::{NyblError, NyblHost, NyblLimits, Value};

/// (name, source) pairs. Every program must be deterministic: no
/// host time, no external input — `rand` is fine because both
/// engines seed it identically on every run.
const CORPUS: &[(&str, &str)] = &[
    (
        "int_arithmetic",
        "print(1 + 2 * 3)\nprint(10 - 4 / 2)\nprint(7 % 3)\nprint((2 + 3) * (7 - 5))",
    ),
    (
        "int_negative_div_mod",
        "print(-7 / 2)\nprint(7 / -2)\nprint(-7 % 2)\nprint(7 % -2)\nprint(-7 % -2)",
    ),
    (
        "int_i64_extremes",
        r#"let max = 9223372036854775807
print(max)
let min = 0 - max - 1
print(min)
print(min / 2)
print(max % 1000000007)
print(max - 1 + 1)"#,
    ),
    (
        "float_repr",
        "print(0.1 + 0.2)\nprint(1.0 / 3.0)\nprint(2.0 / 3.0)\nprint(0.3 - 0.1)\nprint(100.0 / 7.0)",
    ),
    (
        "float_formatting",
        "print(1.0)\nprint(2.5)\nprint(0.000001)\nprint(123456789.123456789)\nprint(1234567890123456789.0)",
    ),
    (
        "float_negative_div_mod",
        "print(-7.5 / 2.5)\nprint(7.5 % 2.0)\nprint(-7.5 % 2.0)\nprint(7.5 % -2.0)",
    ),
    (
        "mixed_int_float",
        "print(1 / 2)\nprint(3 * 1.5)\nprint(2 + 0.5)\nprint(10.0 % 3)\nprint(2.0 == 2)",
    ),
    (
        "math_sqrt_pow",
        "print(2.sqrt())\nprint(3.sqrt())\nprint(16.sqrt())\nprint(2.pow(10))\nprint(2.7.pow(3.3))\nprint(10.pow(-2))",
    ),
    (
        "math_trig",
        "print(1.sin())\nprint(1.cos())\nprint(1.tan())\nprint(0.5.sin())\nprint(100.sin())\nprint(2.25.cos())",
    ),
    (
        "math_exp_log",
        "print(1.exp())\nprint(2.exp())\nprint(0.5.exp())\nprint(10.log())\nprint(2.718281828459045.log())",
    ),
    (
        "math_rounding",
        "print(3.7.floor())\nprint(3.2.ceil())\nprint(2.5.round())\nprint(3.5.round())\nprint((-2.5).round())\nprint((-3.7).floor())\nprint((-3.2).ceil())",
    ),
    (
        // Copied from `rounding_methods_respect_i64_boundaries_diff`
        // in differential.rs — rounding right at the i64 boundary is
        // exactly where a platform could drift.
        "rounding_i64_boundary",
        r#"print(9223372036854775808.0.floor().type(), 9223372036854775808.0.ceil().type(), 9223372036854775808.0.round().type())
print((-9223372036854775808.0).floor(), (-9223372036854775808.0).ceil(), (-9223372036854775808.0).round())
print(9223372036854774784.0.floor(), 9223372036854774784.0.ceil(), 9223372036854774784.0.round())"#,
    ),
    (
        "abs_min_max",
        "print((-5).abs())\nprint((-5.5).abs())\nprint(3.min(7))\nprint(3.5.max(7))\nprint((-0.0).abs())",
    ),
    (
        "string_interpolation",
        r#"let name = "nybl"
let x = 1.0 / 3.0
print("hi {name}, x={x}!")"#,
    ),
    (
        "string_interpolation_shadowing",
        r#"fn greet(name) {
    let punctuation = "!"
    if true {
        let name = "inner"
        return "hi {name}{punctuation}"
    }
    return "unreachable"
}
print(greet("outer"))"#,
    ),
    (
        "string_ops",
        r#"print("val=" + 42)
print("hello".len())
print("a,b,c".split(","))
print("hi".inspect())
print("HeLLo" == "hello")"#,
    ),
    (
        "std_string_module",
        r#"use std.string
print(pad_left("42", 5, " "))
print(reverse("hello"))
print(is_palindrome("racecar"))"#,
    ),
    (
        "std_math_module",
        r#"use std.math.{PI, E, TAU, clamp, sign}
print(PI)
print(E)
print(TAU * 2.0)
print(clamp(42, 0, 10))
print(sign(-7))
print(sign(3.14))"#,
    ),
    (
        "arrays",
        r#"let a = [1, 2, 3]
a.push(4)
print(a)
print(a.len())
print(a.pop())
print(a)
print([1, [2, 3], "x"])"#,
    ),
    (
        "dict_ordering",
        r#"let d = {"a": 1, "b": 2, "c": 3}
print(d.keys())
print(d.values())
print(d["b"])
print(d["zz"])
print(d["zz"].is_none())"#,
    ),
    (
        "dict_iteration",
        r#"let d = {"one": 1, "two": 2, "three": 3}
for key in d.keys() { print(key, d[key]) }"#,
    ),
    (
        // Both engines seed `rand_state` identically, so this pins
        // the builtin LCG sequence across platforms.
        "rand_sequence",
        "repeat 10 { print(rand(1000000)) }\nprint(rand(7))",
    ),
    (
        "fib_recursion",
        r#"fn fib(n) {
    if n <= 1 { return n }
    return fib(n - 1) + fib(n - 2)
}
print(fib(20))"#,
    ),
    (
        "while_break_continue",
        r#"let i = 0
let total = 0
while i < 20 {
    i += 1
    if i % 3 == 0 { continue }
    if i > 15 { break }
    total += i
}
print(i, total)"#,
    ),
    (
        "nested_repeat",
        r#"let n = 0
repeat 5 {
    repeat 3 { n += 1 }
}
print(n)"#,
    ),
    (
        "closures",
        r#"fn helper() { return 7 }
fn call_factory() { return fn() { return helper() } }
fn value_factory() { return fn() { return helper } }
let call_helper = call_factory()
let load_helper = value_factory()
let helper_value = load_helper()
print(call_helper() + helper_value())"#,
    ),
    (
        "structs",
        r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
print(p.x + p.y)
print((p.x * p.x + p.y * p.y).sqrt())"#,
    ),
    (
        "enum_match",
        r#"enum Boxed { Pair(left, right), Record { first, second } }
let values = [Boxed::Pair(3, 4), Boxed::Record { first: 5, second: 6 }]
for value in values {
    let encoded = match value {
        Boxed::Pair(right, left) | Boxed::Record { first: left, second: right } => left * 10 + right,
    }
    print(encoded)
}"#,
    ),
    (
        "match_or_patterns",
        r#"let x = 3
let r = match x {
  1 | 2 | 3 => "low",
  _ => "other",
}
print(r)"#,
    ),
    (
        "ref_params",
        r#"fn update(ref value) {
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
print(value)"#,
    ),
    (
        "error_division_by_zero",
        r#"print("before")
print(1 / 0)"#,
    ),
    ("error_type_mismatch", r#"print("a" - 1)"#),
    (
        "error_variable_not_found",
        r#"print("before")
print(missing_var)"#,
    ),
    (
        "composite_arith_arrays",
        r#"let v0 = 3
let v1 = [1, 2, 3]
let v2 = 0.5
repeat 4 {
    v0 = v0 * 2 - 1
    v1.push(v0)
    v2 = v2 + 0.25 * v0
    if v0 > 10 { print("big", v0) } else { print("small", v0) }
}
print(v1, v2)"#,
    ),
    (
        "composite_string_loop",
        r#"let s = ""
let i = 0
while i < 5 {
    s = s + "{i},"
    i += 1
}
print(s)
print(s.len())"#,
    ),
    (
        "composite_float_accumulation",
        r#"let total = 0.0
let i = 1
while i <= 50 {
    total += 1.0 / i
    i += 1
}
print(total)
print(total.exp())"#,
    ),
    (
        "none_and_truthiness",
        r#"print(none)
print(none.is_none())
print(true && false)
print(true || false)
print(1 == 1, 1 != 2, 3 < 5, 5 >= 6)"#,
    ),
];

/// Host that records prints and resolves `use std.*` from the
/// bundled stdlib. Same shape as the differential-test harness.
struct RecordHost {
    prints: Vec<String>,
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
        self.prints.push(message.to_string());
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        nybl::stdlib::resolve(name).map(|s| Ok(s.to_string()))
    }
}

fn run_engine(
    label: &str,
    source: &str,
    run: impl Fn(&str, &mut RecordHost, &NyblLimits) -> Result<(), NyblError>,
) {
    // Roomier than `NyblLimits::standard()` so recursive corpus
    // programs (fib) complete; still small enough to bound a bug.
    let limits = NyblLimits {
        max_steps: 5_000_000,
        max_memory: 32 * 1024 * 1024,
        ..NyblLimits::standard()
    };
    let mut host = RecordHost { prints: Vec::new() };
    let result = run(source, &mut host, &limits);
    println!("--- {label}");
    for line in &host.prints {
        println!("{line}");
    }
    match result {
        Ok(()) => println!("outcome: ok"),
        Err(e) => println!("outcome: error: {}", e.message),
    }
}

fn main() {
    for (index, (name, source)) in CORPUS.iter().enumerate() {
        println!("=== [{index}] {name}");
        run_engine("walker", source, |src, host, limits| {
            nybl::run(src, host, limits)
        });
        run_engine("vm", source, |src, host, limits| {
            nybl_vm::run(src, host, limits)
        });
    }
    println!("=== end ({} programs)", CORPUS.len());
}
