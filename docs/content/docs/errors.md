+++
title = "Error Handling"
description = "Nybl uses a `Result`-shaped value model for recoverable errors, with two language features that make it ergonomic:"
weight = 15
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Modules"
path = "/docs/modules/"
[extra.next]
title = "REPL"
path = "/docs/repl/"
+++

# Error Handling

Nybl uses a `Result`-shaped value model for recoverable errors, with two language features that make it ergonomic:

- `try` — unwrap an `Ok(v)` or propagate an `Err(e)` up to the enclosing function.
- `try_call(f)` — run a zero-arg callable, catch any runtime error, and return the outcome as a `Result`.

Both `Result` and `RuntimeError` are **engine built-ins** — always in scope, no import required. The combinators (`is_ok`, `unwrap`, `map`, `and_then`, …) are [methods on the `Result` type](/docs/reference/methods/#result-methods-result), also always available.

## The `Result` type

```
enum Result {
  Ok(value),
  Err(error),
}
```

By convention, `Ok(v)` carries the successful value and `Err(e)` carries whatever describes the failure — a string, a struct, anything. Values from fallible operations are the typical shape:

```nybl
fn parse_positive(s) {
  let n = s.to_int()
  if n <= 0 {
    return Err("must be positive, got {n}")
  }
  return Ok(n)
}

print(parse_positive("42"))    // Result::Ok(42)
print(parse_positive("-3"))    // Result::Err("must be positive, got -3")
```

### `Ok` / `Err` shorthand

`Ok(x)` and `Err(e)` are parser-level sugar for `Result::Ok(x)` and `Result::Err(e)`. The rewrite applies in both expression and pattern position, so you can write:

```nybl
fn classify(n) {
  if n > 0 { return Ok(n) }
  return Err("non-positive")
}

print(match classify(5) {
  Ok(v)  => "ok: {v}",
  Err(e) => "err: {e}",
})
// ok: 5
```

Nybl's case rules already reserve uppercase identifiers for types and variants, so `Ok` and `Err` can't collide with a user fn or variable. The long form (`Result::Ok(v)`, `Result::Err(e)`) still works — pick whichever reads better.

If a different enum happens to have its own `Ok` / `Err` variants, use the qualified `MyEnum::Ok(x)` form for those. The bare sugar always means `Result::Ok` / `Result::Err`.

## The `try` operator

`try expr` evaluates `expr` and:

- If the result is `Result::Ok(v)`, unwraps it to `v`.
- If the result is `Result::Err(e)`, immediately returns `e` from the enclosing function as-is (wrapped in the same `Err` variant the caller will see).
- If the result is anything else (not `Result`-shaped), raises a runtime error.

```nybl
fn pipeline(s) {
  let n = try parse_positive(s)        // Err propagates; Ok unwraps to `n`
  let doubled = try double_checked(n)
  return Ok(doubled)
}

print(pipeline("21"))    // Result::Ok(42)
print(pipeline("-3"))    // Result::Err("must be positive, got -3")
```

Because `try` propagates by returning from the *enclosing function*, it only works inside fn bodies. A top-level `try` that hits an `Err` raises a runtime error — wrap the call site in a fn.

### Unit-Ok

`try Result::Ok` with no payload (or `try Result::Ok` where `Ok` is a unit variant) yields `none`. Mostly relevant for APIs where the success case carries no meaningful value.

## `try_call(f)`

Catch runtime errors from a zero-arg callable. Returns `Result::Ok(value)` on success or `Result::Err(RuntimeError { message, line })` on a caught error.

```nybl
let r = try_call(fn() { return 1 / 0 })

print(match r {
  Result::Ok(v)                      => "got {v}",
  Result::Err(RuntimeError { message, line }) =>
    "failed at line {line}: {message}",
})
// failed at line 1: Division by zero
```

`try_call` is Nybl's answer to exception-like error handling without exceptions. It *only* catches **non-fatal** errors. Fatal conditions — step-budget exhaustion, memory-limit violation, host `on_tick` returning `NyblError::fatal` — are **not** caught. That keeps the sandbox invariant intact: a runaway loop can't wrap itself in `try_call` and keep going.

### `RuntimeError` — the caught error shape

```
struct RuntimeError {
  message,   // string
  line,      // int — 1-indexed source line of the failing expression
}
```

You can construct one explicitly (it's a regular struct), but most of the time you'll see them as the payload inside `Result::Err(...)` returned from `try_call`.

## Combinators — methods on `Result`

Every `Result` value has a small set of always-available methods. No import needed — `Result` is a built-in type and its combinators are engine-level methods.

```nybl
print(Ok(1).is_ok())                     // true
print(Err("oops").is_err())              // true

// unwrap_or — default on Err
print(Ok(10).unwrap_or(0))               // 10
print(Err("fail").unwrap_or(0))          // 0

// map — transform the Ok payload, pass Err through
print(Ok(5).map(fn(n) { return n * n }))      // Result::Ok(25)
print(Err("x").map(fn(n) { return n * n }))   // Result::Err("x")

// and_then — monadic bind (for chaining fallible steps)
fn halve(x) {
  if x % 2 == 0 { return Ok((x / 2).to_int()) }
  return Err("odd")
}
print(Ok(8).and_then(halve).and_then(halve))   // Result::Ok(2)
print(Ok(7).and_then(halve))                    // Result::Err("odd")
```

Available: `is_ok`, `is_err`, `unwrap`, `expect`, `unwrap_or`, `map`, `map_err`, `and_then`. See [Methods → Result](/docs/reference/methods/#result-methods-result) for the full reference.

`unwrap()` and `expect(msg)` raise a runtime error on `Err` — use sparingly, and prefer `try` or pattern matching in production code.

## When to use which

| Situation | Use |
|-----------|-----|
| Writing a fallible function | Return `Result::Ok(v)` / `Result::Err(e)` |
| Chaining several fallible calls | `try` inside a fn, or `r.and_then(f)` |
| Running user-supplied code with a safety net | `try_call(fn() { ... })` |
| Handling every `Err` case explicitly | `match` |
| You know it's `Ok` and want the value | `r.unwrap()` / `r.expect("...")` (sparingly) |
| Supplying a default on `Err` | `r.unwrap_or(default)` |

## Fatal vs non-fatal

- **Non-fatal** (catchable by `try_call`): division by zero, "variable not found", type mismatches, host-raised errors via `NyblError::runtime`, wrong arg count, missing field, etc.
- **Fatal** (not catchable): step-budget exceeded, memory-limit exceeded, fn-call-depth exceeded, host-raised `NyblError::fatal`.

A script can observe whether an error was fatal by inspecting whether `try_call` caught it — fatal errors propagate past `try_call` to the host.
