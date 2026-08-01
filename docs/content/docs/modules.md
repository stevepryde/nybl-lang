+++
title = "Modules"
description = "A Nybl program can be split across multiple files — or in-memory source strings, asset bundles, anywhere the embedding host can return Nybl source. The `use` statement pulls another module's public surface into the current scope."
weight = 14
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Reference Parameters"
path = "/docs/functions/reference-parameters/"
[extra.next]
title = "Error Handling"
path = "/docs/errors/"
+++

# Modules

A Nybl program can be split across multiple files — or in-memory source strings, asset bundles, anywhere the embedding host can return Nybl source. The `use` statement pulls another module's public surface into the current scope.

## The four forms of `use`

```nybl
use path                    // glob:        everything public
use path.{a, b, Type}       // selective:   just the listed items
use path as m               // aliased:     binds `m` as a Value::Module
use path.{a, b} as m        // aliased + selective
```

Paths are dot-joined identifiers: `std.math`, `game.entity.player`. How the host resolves a path is up to the embedder — `nybl-sys`'s `StandardHost::with_module_root` maps `foo.bar` to `<root>/foo/bar.nybl`, in-memory hosts can look up a string table, a web host can fetch a URL. See [Embedding](/docs/embedding/#resolve_module-custom-use-resolution).

## Explicit public surfaces

A module can declare an export allow-list with `pub { ... }`:

```nybl
let visible = 1
let implementation_detail = 2
let _explicitly_public = 3
struct Widget { value }
fn helper() { return implementation_detail }

pub { visible, _explicitly_public, Widget, helper }
```

When at least one list appears, only listed values, functions, types, and
re-exports can cross the module boundary. Multiple lists are unioned, and
`pub {}` exports nothing. Listed underscore-prefixed names are public even to a
glob import. Private implementation bindings remain available to exported
functions inside their defining module.

Without a `pub { ... }` list, modules keep the legacy convention described
below: glob imports omit `_` names, while aliases and selective imports may
reach them. This compatibility rule lets existing modules adopt explicit
surfaces incrementally.

`pub { ... }` is separate from root-level `pub fn`: the former controls Nybl
module imports, while the latter declares a host-callable
[`NyblInstance`](/docs/embedding/instances/) entry point. A surface statement
in the directly executed root has no effect.

## Glob `use`

Brings every public export of a module into the current scope as a bare name:

```nybl
use std.math
print(PI)            // constant from std.math
print(factorial(5))  // fn from std.math → 120
```

Names that start with `_` are considered **private by convention** and glob imports skip them:

```nybl
// In module `foo`:
fn _helper() { return 42 }
fn public() { return _helper() }

// Elsewhere:
use foo
print(public())      // 42
// print(_helper())  // error: `_helper` not in scope
```

Glob is idempotent at the injection site — running `use foo` twice in the same scope is a no-op (matches Python's `import foo; import foo`). When two glob imports would introduce the same name, the first wins and the second emits a runtime warning — explicit selective imports are the way to disambiguate.

## Selective `use`

Pick exactly which names you want:

```nybl
use std.math.{PI, factorial}
print(PI)
print(factorial(4))
// print(clamp(1, 0, 10))   // error — not imported
```

In a legacy module without an explicit public surface, selective imports can
reach private names explicitly:

```nybl
use foo.{_helper}
print(_helper())     // ok — explicit opt-in
```

If a listed name doesn't exist in the target module, you get a clear error pointing at the `use` site.

## Aliased `use`

Binds the whole module as a single value under the alias:

```nybl
use std.math as m
print(m.PI)
print(m.factorial(5))
```

`m` is a `Value::Module` — `m.type()` is `"module"`. You access its exports via the `.` operator. Methods on aliased modules (`m.helper(...)`) work the same way they would on a bare imported fn.

Combine with selective to shrink the alias's surface:

```nybl
use std.math.{PI, factorial} as m
print(m.PI)
print(m.factorial(5))
// print(m.clamp(1, 0, 10))   // error — `clamp` wasn't imported
```

## Namespaced types

User-defined `struct` and `enum` types can be constructed and pattern-matched through the alias:

```nybl
// In `paint.nybl`:
enum Color { Red, Green, Blue }
struct Point { x, y }

// In main:
use paint as p
let c = p.Color::Red
let origin = p.Point { x: 0, y: 0 }

print(match c {
  p.Color::Red   => "stop",
  p.Color::Green => "go",
  p.Color::Blue  => "cool",
})
```

The namespace is required — bare `Color::Red` inside the main file wouldn't find the type unless you also imported `paint.{Color}` by bare name.

## Type identity

Types carry their declaring module as part of their identity. Two modules can declare a type with the same name; values from them are **distinct types** — equality is always `false` across the module boundary, and patterns only match values from the module the pattern named.

```nybl
// paint.nybl: enum Color { Red, Blue }
// other.nybl: enum Color { Red, Green, Yellow }

use paint as p
use other as o

let a = p.Color::Red
let b = o.Color::Red

print(a == b)        // false — different `Color` types
print(a == a)        // true
```

A pattern over an aliased module's type only fires for values from that module:

```nybl
fn label(c) {
  return match c {
    p.Color::Red => "paint-red",
    o.Color::Red => "other-red",
    _            => "something else",
  }
}
print(label(p.Color::Red))   // "paint-red"
print(label(o.Color::Red))   // "other-red"
```

This is Nybl's answer to the "same-named type, different shape, in different modules" problem. No renames required.

## Re-exports are transitive

A module re-exports the bindings that its own `use` statements introduce. If `a` does `use b` and `b` declares `fn foo()`, then `use a` in the top-level program makes `foo` visible too. Selective imports re-export only their selected names, including a private name selected explicitly. Aliased imports re-export only the alias: if `a` does `use b as dep`, an importer of `a` can reach `dep`, but does not receive `b`'s exports as bare names. The same shape rules apply to types, and glob privacy filtering is applied at every module boundary.

## Builtin types

`Result` and `RuntimeError` are engine built-ins. They're always in scope — you don't need any `use` to write `Result::Ok(v)` or to match on `RuntimeError { message, line }`. The combinators (`unwrap`, `map`, `and_then`, …) live as [methods on the `Result` type](/docs/reference/methods/#result-methods-result), also always available. See [Error Handling](/docs/errors/).

## Cycles

Circular imports (`a` uses `b` which uses `a`) are detected at load time and raise a clear error naming the cycle path. Restructure the code so the cycle breaks — usually by pulling shared definitions into a third module that neither circular node depends on.

## Inside a function body

Aliased modules and bare-imported types remain visible inside function bodies declared in the same module:

```nybl
use paint as p

fn describe(c) {
  return match c {
    p.Color::Red   => "red",
    p.Color::Blue  => "blue",
    _              => "other",
  }
}
```

The `p` alias doesn't need to be a parameter — module-level aliases persist across function call boundaries so patterns inside fn bodies can resolve them.
