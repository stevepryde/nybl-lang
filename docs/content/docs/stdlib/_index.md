+++
title = "Overview"
description = "`nybl-lang` bundles a small standard library behind its default `nybl-std` feature. The modules are written in Nybl, live under `std.*`, and can be resolved by `nybl-sys::StandardHost` or a custom host."
weight = 21
template = "docs/section.html"
page_template = "docs/page.html"
[extra.previous]
title = "Grammar"
path = "/docs/reference/grammar/"
[extra.next]
title = "std.math"
path = "/docs/stdlib/math/"
+++

# Standard Library — overview

`nybl-lang` bundles a small set of modules written in Nybl itself behind the
default `nybl-std` Cargo feature. They live under the `std.*` namespace.
`nybl-sys::StandardHost` resolves them automatically before its filesystem
fallback; custom hosts can delegate `std.*` names to
`nybl::stdlib::resolve`.

The stdlib is deliberately thin. Core math and `Result` operations are [methods on values](/docs/reference/methods/) (`(-5).abs()`, `(9).sqrt()`, `r.unwrap_or(0)`, `r.map(f)`) — they don't need a module. The stdlib covers what's left: constants, higher-order helpers on arrays, data-structure types, string formatting, JSON, test assertions.

## Modules

| Module | What it gives you |
|--------|-------------------|
| [`std.math`](/docs/stdlib/math/) | `PI`, `E`, `TAU`, `clamp`, `sign`, `factorial`, `gcd`, `lcm`, `mean` |
| [`std.iter`](/docs/stdlib/iter/) | `map`, `filter`, `reduce`, `take`, `drop`, `zip`, `enumerate`, `all`, `any`, `count`, `find`, `find_index`, `flatten`, `sum`, `product`, `min_array`, `max_array` |
| [`std.collections`](/docs/stdlib/collections/) | `Stack`, `Queue`, `Set` as value-semantics structs |
| [`std.string`](/docs/stdlib/string/) | `pad_left`, `pad_right`, `center`, `chars`, `reverse`, `is_palindrome`, `count`, `join` |
| [`std.json`](/docs/stdlib/json/) | `parse`, `stringify` (RFC-8259, pure Nybl) |
| [`std.test`](/docs/stdlib/test/) | `assert`, `assert_eq`, `assert_near`, `assert_raises` |

## Using the stdlib

`std` modules work with every [`use` form](/docs/modules/):

```nybl
use std.math                   // glob — `PI`, `clamp`, etc. available bare
use std.iter.{map, filter}     // selective
use std.json as j              // aliased
```

The modules are plain Nybl source — you can find the implementations in `nybl/src/modules/*.nybl` if you want to see how a helper is wired.

## Things you might expect to find here

- **`Result` combinators** — `is_ok`, `is_err`, `unwrap`, `expect`, `unwrap_or`, `map`, `map_err`, `and_then` used to live in `std.result`. They're now **methods on the built-in `Result` type** and always available without any import. See [Methods → Result](/docs/reference/methods/#result-methods-result).
- **`print`, `range`, `rand`, `try_call`, `panic`** — always-in-scope [built-in functions](/docs/reference/builtins/), not stdlib.
- **Math on numbers** — `abs`, `sqrt`, `sin`, `cos`, `floor`, `ceil`, `round`, `pow`, `log`, `exp`, `min`, `max`, `to_int`, `to_float` are [methods on `int` / `number`](/docs/reference/methods/#numeric-methods-int-and-number), not stdlib.

## Hosts without the stdlib

Disable default Cargo features to omit the bundled source:

```toml
nybl = { package = "nybl-lang", version = "0.4", default-features = false, features = ["std"] }
```

Embedders that keep the feature still choose whether to expose it: a custom
host must call `nybl::stdlib::resolve` from `NyblHost::resolve_module`.
Conversely, a host can bundle or load its own `std.*` source even when the
feature is disabled. Nothing in the core language depends on the stdlib.
The `std` feature shown above controls Rust standard-library integration; it is
separate from the bundled Nybl stdlib.
