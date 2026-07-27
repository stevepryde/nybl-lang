+++
title = "Built-in Functions"
description = "Nybl ships a very small set of built-in functions that are always in scope. They can't be shadowed by user-defined fns. Host-backed builtins (I/O, time) are provided separately by the embedding host — see `nybl-sys`'s `StandardHost` for the reference implementation."
weight = 18
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Operators"
path = "/docs/reference/operators/"
[extra.next]
title = "Methods"
path = "/docs/reference/methods/"
+++

# Built-in Functions

Nybl ships a very small set of built-in functions that are always in scope. They can't be shadowed by user-defined fns. Host-backed builtins (I/O, time) are provided separately by the embedding host — see `nybl-sys`'s `StandardHost` for the reference implementation.

Anything math- or conversion-shaped lives as a [method on a value](/docs/reference/methods/), not as a global — `(-5).abs()`, `"42".to_int()`, `[1, 2, 3].len()`. The global list is deliberately short: variadic (`print`), constructor-shaped (`range`), session-stateful (`rand`), callable-taking (`try_call`), and the shared error-signalling primitive (`panic`).

## `print(args...)`

Prints values to the host's stdout, separated by spaces. Returns `none`.

```nybl
print("hello")           // hello
print("x =", 42)         // x = 42
print(1, "plus", 2)      // 1 plus 2
```

Accepts any number of arguments (including zero). Each argument is converted to its string representation (Display) automatically — you don't need `.to_str()` first.

## `range(n)` / `range(start, end)` / `range(start, end, step)`

Builds an array of `int` values.

```nybl
range(5)           // [0, 1, 2, 3, 4]
range(2, 6)        // [2, 3, 4, 5]
range(0, 10, 2)    // [0, 2, 4, 6, 8]
range(5, 0)        // [5, 4, 3, 2, 1]  (auto-detects direction)
range(10, 0, -3)   // [10, 7, 4, 1]
```

- With 1 arg: `range(n)` → `[0, 1, ..., n-1]`.
- With 2 args: `range(start, end)` auto-detects direction.
- With 3 args: explicit `step` (error if `step == 0`).
- All arguments must be `int` — floats are rejected.
- Maximum 10,000 elements. Bigger ranges raise a fatal resource error before
  allocating, so they cannot be silently truncated or hidden with `try_call`.

## `rand(n)`

Returns a random integer from `0` to `n - 1`, inclusive. `n` must be a positive integer.

```nybl
rand(6)     // 0..=5 (die roll)
rand(2)     // 0 or 1 (coin flip)
```

Uses a deterministic PRNG seeded per-session. The same inputs produce the same sequence — handy for tests, surprising for crypto (don't use it for that).

## `try_call(callable)`

Runs `callable` with no arguments and catches any non-fatal runtime error:

```nybl
let r = try_call(fn() { return 1 / 0 })
print(match r {
  Ok(v)  => v,
  Err(e) => "caught: " + e.message,
})
// caught: Division by zero
```

On success returns `Result::Ok(value)`; on a non-fatal error returns `Result::Err(RuntimeError { message, line })`. The `Ok(v)` / `Err(e)` pattern shorthand desugars to `Result::Ok(v)` / `Result::Err(e)` — see [Error Handling](/docs/errors/#ok-err-shorthand). Fatal errors (step-limit / memory-limit / fn-call-depth) are *not* caught — they propagate past `try_call` unchanged so the sandbox invariant holds.

See [Error Handling](/docs/errors/) for the full story on `try`, `try_call`, `Result`, and `RuntimeError`.

## `panic(message)`

Raise a non-fatal runtime error carrying `message`. Useful inside stdlib helpers (`unwrap`, `expect`, `assert_eq`, …) and user code that needs to bail with a readable error from an expression position where a plain `return` isn't enough.

```nybl
fn take_positive(n) {
  if n <= 0 { panic("n must be positive, got {n}") }
  return n * 2
}

print(take_positive(3))                           // 6
// print(take_positive(-1))                       // runtime error: n must be positive, got -1
```

Non-fatal, so `try_call` catches it:

```nybl
let r = try_call(fn() { panic("deliberate") })
print(match r {
  Ok(_)  => "ok?",
  Err(e) => e.message,
})
// deliberate
```

Non-string arguments are stringified via Display, so `panic(42)` or `panic(RuntimeError { message: "x", line: 0 })` also works without an explicit `.to_str()`.

## Everything else lives on a value

Category | Was | Now
--- | --- | ---
Introspection | `type(x)`, `inspect(x)` | `x.type()`, `x.inspect()`
Conversion | `str(x)`, `int(x)`, `float(x)` | `x.to_str()`, `x.to_int()`, `x.to_float()`
Length | `len(x)` | `x.len()` (arrays, strings, dicts)
Absolute / min / max | `abs(x)`, `min(a, b)`, `max(a, b)` | `x.abs()`, `a.min(b)`, `a.max(b)`
Trig / roots | `sqrt(x)`, `sin(x)`, `cos(x)`, `tan(x)` | `x.sqrt()`, `x.sin()`, `x.cos()`, `x.tan()`
Rounding | `floor(x)`, `ceil(x)`, `round(x)` | `x.floor()`, `x.ceil()`, `x.round()`
Power / log / exp | `pow(b, e)`, `log(x)`, `exp(x)` | `b.pow(e)`, `x.log()`, `x.exp()`

See [Methods](/docs/reference/methods/) for the full per-type method catalogue.
