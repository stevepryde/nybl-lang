+++
title = "std.test"
description = "Minimal assertion toolkit for sanity checks in Nybl scripts."
weight = 27
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "std.json"
path = "/docs/stdlib/json/"
[extra.next]
title = "Embedding Nybl"
path = "/docs/embedding/"
+++

# std.test

Minimal assertion toolkit for sanity checks in Nybl scripts.

This isn't an xUnit clone — just the assertion primitives you'll reach for when writing quick tests. Assertions fail by routing through the [`panic`](/docs/reference/builtins/#panicmessage) builtin, so the failure detail is surfaced verbatim in `Err(e).message` when caught by `try_call`. Print-based reporting is intentionally out of scope — wrap the assertion in `try_call` if you need "report and continue".

## Import

```nybl
use std.test                                // glob
use std.test.{assert_eq, assert_near}       // selective
use std.test as t                           // aliased
```

## Assertions

### `assert(cond, message)`

Assert that `cond` is truthy. On failure, raises a runtime error — `message` is surfaced in the crash trace.

```nybl
use std.test.{assert}
assert(1 + 1 == 2, "arithmetic still works")
```

### `assert_eq(actual, expected)`

Assert two values are structurally equal (same as `==`). On failure the error includes an `assert_eq failed:` prefix and the `.inspect()` of both sides.

```nybl
use std.test.{assert_eq}
assert_eq([1, 2, 3].len(), 3)
assert_eq("hi".upper(), "HI")
```

### `assert_near(actual, expected, tolerance)`

Assert two floats are within `tolerance` of each other. Use this instead of `assert_eq` when comparing `number` values subject to rounding.

```nybl
use std.test.{assert_near}
assert_near((2).sqrt() * (2).sqrt(), 2, 0.0000001)
```

### `assert_raises(body)`

Assert that `body` — a zero-arg closure — raises a runtime error. On success (no raise), the assertion itself fails. Useful for negative tests.

```nybl
use std.test.{assert_raises}

assert_raises(fn() {
  let _ = 1 / 0     // expected to raise "Division by zero"
})

assert_raises(fn() {
  "not a number".to_int()
})
```

Under the hood, `assert_raises` uses `try_call` to observe whether `body` raised, so the same fatal-vs-non-fatal rules apply — a step-limit exceeded inside `body` propagates past the assertion instead of being caught.

## Putting it together

```nybl
use std.test.{assert_eq, assert_raises}

fn normalize(arr) {
  if arr.len() == 0 { return none[0] }
  let total = 0
  for x in arr { total = total + x }
  let m = total / arr.len()
  let out = []
  for x in arr { out.push(x - m) }
  return out
}

assert_eq(normalize([1, 2, 3]), [-1, 0, 1])
assert_raises(fn() { normalize([]) })      // raises on empty
print("normalize: all checks passed")
```
