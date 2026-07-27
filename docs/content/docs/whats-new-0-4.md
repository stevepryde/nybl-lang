+++
title = "What's new in Nybl 0.4"
description = "Nybl 0.4 adds persistent instances, typed Rust value conversions, a complete module system, Result and lazy iterator protocols, a stateful REPL, richer diagnostics, and full walker/VM/AOT parity."
weight = 2
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Introduction"
path = "/docs/"
[extra.next]
title = "Syntax"
path = "/docs/basics/syntax/"
+++

# What's new in Nybl 0.4

Nybl 0.4 is the first coordinated release after 0.3. All five crates move
together to `0.4.0`:

```toml
[dependencies]
nybl = { package = "nybl-lang", version = "0.4" }
nybl-vm = "0.4"       # optional bytecode engine
nybl-compile = "0.4"  # optional AOT transpiler
nybl-sys = "0.4"      # optional OS-backed host
```

The release makes Nybl substantially more useful as an embedded plugin
language: programs can stay alive across host calls, modules have explicit
namespaces and type identities, Rust values cross the host boundary through
checked conversions, and the walker, VM, and AOT engines expose the same
language.

## Persistent programs

The walker and VM now expose `NyblInstance`. Load a program once, inspect its
direct root-level `pub fn` entries, then call those entries without resetting
program state:

```nybl
let total = 0

pub fn add(value) {
  total += value
  return total
}
```

Globals, loaded modules, functions, closures, returned callbacks, user types,
methods, and random-number state all remain live. Sandboxed AOT library output
generates the same `load`, `entry_points`, `call`, and `call_value` surface.
Callbacks are bound to the instance that created them, instances reject
re-entry, and failed calls do not reset the program.

See [Stateful instances](/docs/embedding/instances/) for the complete
walker, VM, and AOT lifecycle.

## Checked Rust value conversions

Hosts no longer need to hand-match every nested `Value`.
`Value::to_rust`, `FromValue`, and `IntoValue` cover strict numeric scalars,
borrowed and owned strings, `Vec<T>`, `Option<T>`, `Result<T, E>`, and
deterministic `BTreeMap<String, T>` dictionaries. Conversion errors report the
nested path that failed, such as `$[0]["stats"]["hp"]`.

The fallible `nybl_value!` macro builds JSON-like arrays and dictionaries while
preserving Nybl's maximum value-depth invariant:

```rust
let request = nybl::nybl_value!({
    "name": "Ada",
    "scores": [10, 20, 30],
    "nickname": none,
})?;
```

See [Typed `Value` conversions](/docs/embedding/#typed-value-conversions).

## A complete module system

`use` replaces the old `import` keyword and has four forms:

```nybl
use app.config
use app.config.{HOST, port}
use app.config as config
use app.config.{HOST, port} as config
```

Aliased modules expose live value bindings, callable exports, and namespaced
types. Types retain the identity of the module that declared them, including
through re-exports, so two same-shaped structs from different modules remain
distinct. Glob collisions produce warnings and keep the first binding;
selective and aliased forms make deliberate conflicts explicit.

Imported parse and runtime errors now render against the source module that
owns the failure, including through transitive calls. Read [Modules](/docs/modules/)
for resolution, visibility, namespaced construction and patterns, type
identity, re-exports, and cycles.

## Constants and naming rules

`const` creates a binding that cannot be reassigned or mutated through a
container:

```nybl
const MAX_RETRIES = 3
const DEFAULTS = ["safe", "fast"]
```

Name shapes are checked at parse time:

- values, functions, parameters, fields, aliases, loop variables, and pattern
  bindings start lowercase or with `_`;
- constants are `ALL_CAPS`;
- structs, enums, and variants start uppercase.

Diagnostics suggest the corrected spelling. See
[Variables → Constants](/docs/basics/variables/#constants) and
[Name shapes](/docs/basics/variables/#name-shapes-are-checked).

## Transactional `ref` parameters

User-defined functions can explicitly update mutable caller variables. The
`ref` marker is required in both the parameter declaration and the call:

```nybl
fn swap(ref left, ref right) {
  let old = left
  left = right
  right = old
}

let first = 1
let second = 2
swap(ref first, ref second)
print([first, second])    // [2, 1]
```

References are second-class parameters rather than general aliasing values.
Each target must be a distinct mutable plain variable; constants, expressions,
indexes, fields, and captured bindings are rejected. The callee works on staged
copies, then commits every target together only after a normal return. Runtime
and fatal sandbox errors roll the call back, including errors caught by
`try_call`; a returned `Result::Err` is an ordinary return and commits.

Reference parameters work through first-class function aliases, may be
forwarded into another ref call, and are supported by the walker, VM, and AOT
engines. Built-in and host functions remain value-only, as do Rust
`NyblInstance::call` and `call_value` arguments. User-defined methods can declare
`ref self` to update a mutable plain-variable receiver and can place explicit
refs after it. Ordinary `self` receivers are read-only, so attempting to mutate
one is a parse error rather than a silently discarded change. Built-in array
mutators use the same transaction model implicitly for a named receiver.

Read [Reference Parameters](/docs/functions/reference-parameters/) for target
rules, evaluation order, forwarding, rollback, methods, and embedding
boundaries.

## Methods replace utility globals

Operations that belong to a value now use method syntax:

```nybl
value.type()
value.to_str()
text.to_int()
items.len()
(-5).abs()
(9).sqrt()
```

The complete tables cover universal, numeric, boolean, string, array, dict,
`Result`, `Iter`, user-type, and module methods. See
[Methods](/docs/reference/methods/).

## Built-in `Result` and recoverable errors

`Result` and `RuntimeError` are engine built-ins, so they work without an
stdlib import. `Ok(value)` and `Err(error)` are shorthand in both expressions
and patterns. Results support `.is_ok()`, `.is_err()`, `.unwrap()`,
`.expect()`, `.unwrap_or()`, `.map()`, `.map_err()`, and `.and_then()`.

`try` propagates an `Err` from a function. `try_call(callback)` catches a
non-fatal runtime error as `Err(RuntimeError)`, while `panic(message)` raises
one deliberately. Resource-limit failures remain fatal and cannot be caught.

See [Error Handling](/docs/errors/).

## `none`, dictionaries, and lazy iteration

Two universal helpers make optional values easy to read:
`value.is_none()` and `value.is_some()`. Looking up a missing dictionary key
now returns `none`, so optional data can be queried without raising.

Arrays, strings, dictionaries, and iterators implement a lazy protocol:

```nybl
let it = [10, 20, 30].iter()
print(it.next()) // Iter::Next(10)
print(it.next()) // Iter::Next(20)
```

`.next()` returns `Iter::Next(value)` or `Iter::Done`, and `for` uses the same
protocol. User-defined types can participate by defining `.iter()` and, for a
stateful iterator, `.next()`. See [Iter methods](/docs/reference/methods/#iter-methods-iter).

## Multiline expressions and comments

Newlines inside `()` and `[]` no longer end a statement, and a line beginning
with `.` continues the previous value. This makes calls, conditions, arrays,
indexes, and method chains readable across lines.

`//` is the line-comment marker. The old integer-division spelling is gone:
`/` always produces a `number`; use `(a / b).to_int()` when truncating integer
division is intended. A leading `#` is now an error.

See [Syntax](/docs/basics/syntax/) and
[Automatic semicolons](/docs/reference/grammar/#automatic-semicolons).

## First-class functions and matching

Function expressions can be stored, passed, returned, and captured as
closures. Match expressions support literals, bindings, structs, enum
variants, namespaced types, arrays with rests, or-patterns, and guards.
Exhaustiveness diagnostics understand local and imported enum declarations.

See [Defining Functions](/docs/functions/defining-functions/) and
[Pattern Matching](/docs/control-flow/match/).

## Stateful REPL and CLI

The REPL now retains declarations between submissions, echoes bare
expressions, accepts multiline input, completes keywords and live bindings,
and persists command history. `:vars`, `:reset`, `:help`, and `:quit` manage
the session. Piped input uses the same submission model and keeps processing
after a recoverable error.

`nybl run` uses the VM by default (`--novm` selects the walker), while
`nybl compile` builds a native executable or emits Rust source. See
[REPL](/docs/repl/) and [Command-line interface](/docs/cli/).

## Engine parity, diagnostics, and safety

The VM and AOT transpiler now cover the same public language as the walker.
The differential suite compares all three engines across modules, closures,
methods, matching, iteration, errors, warnings, and limits.

Rust-facing additions include bytecode validation before VM execution,
copy-on-write container values, safer mutation rules for nested and constant
containers, bounded ranges and parser nesting, and hardened VM scope/control
flow handling. Diagnostics now carry source columns more consistently, offer
targeted hints for names, ranges, match arms, `try`, and shadowing, and retain
the correct source context across module calls.

## Migrating from 0.3

Make these mechanical changes:

| 0.3 | 0.4 |
|-----|-----|
| `import foo` | `use foo` |
| `# comment` | `// comment` |
| `type(x)` | `x.type()` |
| `str(x)` | `x.to_str()` |
| `int(x)` / `float(x)` | `x.to_int()` / `x.to_float()` |
| `len(x)` | `x.len()` |
| `a // b` | `(a / b).to_int()` |
| standalone `nybl-std` dependency | `nybl-lang`'s default `nybl-std` feature |

The repository's `CHANGELOG.md` also lists runtime hardening and the
crates.io publishing order.
