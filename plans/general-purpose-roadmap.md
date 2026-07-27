# Nybl → General-Purpose Language Roadmap

Status: design / roadmap. `plans/execution-modes.md` covers how Nybl
programs are *executed*; this document covers how Nybl becomes rich
enough to *write non-trivial programs in*. The execution-modes
roadmap is essentially complete (three engines, shared differential
harness); this one is the next multi-phase effort.

## Goal

Take Nybl from "well-engineered embedded scripting language" to
"general-purpose language you could actually build an app in" —
without breaking the thing that makes it worth embedding.

By the end of this roadmap a user should be able to write:

- A multi-file CLI tool with subcommands, error handling, and a
  stdlib it can lean on (`math`, `iter`, `string`, `json`, `test`).
- A small data-processing script that reads files, transforms
  records, and reports results, using closures and structs to
  model data cleanly.
- A simple web scraper or config parser where the structure of the
  program — imports, modules, user-defined types — looks and feels
  like something written in Python or Lua, not like a shell script.

## The non-negotiable invariant

> **`nybl-lang` (the `nybl` crate) stays zero-dep and embeddable.**

Every phase below is gated by that constraint. Anything that needs
filesystem access, network, a package registry, OS time, FFI, or a
Rust dependency beyond `core`/`alloc` goes into a separate crate
(`nybl-sys`, `nybl-std`, `nybl-pkg`, …) or behind a `NyblHost` hook
that the embedder supplies.

If a proposed feature genuinely cannot be implemented without
violating that invariant, the feature gets deferred or redesigned.
This is a hard line, not a soft preference.

### How the constraint expresses itself

- Core never reads files, opens sockets, or touches the clock.
- Core never pulls in a Rust crate outside the workspace (no
  `serde`, no `regex`, no `indexmap`). `alloc` only; 
- Language features that need I/O (imports, stdlib with OS access)
  are implemented as `NyblHost` trait methods, with OS-backed
  default impls living in `nybl-sys`.
- Standard library content that can be written in Nybl itself
  (iterator helpers, math, assertions) lives in `nybl-std` as
  bundled source strings, loaded through the module system —
  not compiled into core.

Practical test when adding a phase: *could a `no_std` embedder
pull just `nybl` and a tiny custom host and still use this
feature?* If not, the feature needs to sit outside core.

## Target crate landscape

Current:

```
nybl         core language: lexer, parser, AST, tree-walker, Value,
            ops, builtins, methods, memory tracking, NyblHost trait.
            Zero deps, no_std-capable.
nybl-vm      bytecode compiler + stack VM (depends on nybl)
nybl-sys     standard host: stdio, fs, env, time (std-only)
nybl-compile AOT Nybl → Rust transpiler (depends on nybl)
nybl-cli     CLI driver (run, repl; build coming with nybl-compile CLI)
```

Additions this roadmap calls for:

```
nybl-std     standard library written in Nybl source; bundled as
            string assets, loaded through the module system.
            Zero OS dependencies — the embedder chooses whether
            to load it via NyblHost::resolve_module.
nybl-pkg     (far future) package manager CLI + registry client.
            Separate plan doc when it lands.
```

## Phases

Phases are listed in dependency order. Each is independently
shippable, differential-tested across walker / VM / AOT, and
documented in `plans/execution-modes.md`-style "status: done" blocks
once it lands.

A useful mental split:

- **Phases 1–7** constitute the "MVP general-purpose" set. After
  phase 7 lands, Nybl is a small-but-real general-purpose language.
- **Phases 8–9** are ecosystem (package manager) and continuous
  polish (docs, REPL, performance).

### Phase 1 — Closures and first-class functions

*Status: done. Value gains an `Rc<NyblFn>` variant with an
engine-opaque `FnBody` (`Ast` for the walker; `Compiled(Rc<dyn
Any>)` for the VM and AOT). Lambdas parse as `fn(params) {
body }` at expression position; the walker snapshots visible
scope into the closure's captures, the VM's `MakeLambda` opcode
does the same at runtime while carrying a pre-compiled chunk,
and the AOT emits Rust `move` closures with compile-time
free-variable analysis. First-class named fns work in all three
via synthesised wrappers / `LoadVar` fallback / module-scope
`__nybl_fn_value_<name>` helpers. Non-Ident callees (`funcs[0](x)`,
`make_adder(5)(3)`) go through a `CallValue` opcode in the VM
and a `__nybl_call_value` helper in AOT. 12 walker tests + 10
differential tests + 6 AOT e2e tests + 8 three-way corpus
entries all green.*

Known limitation: let-bound lambdas can't self-reference
(`let f = fn() { f() }` fails because captures snapshot `f`
before it's bound). Named-fn declarations (`fn name(...)`) work
for recursion via `self_name` seeding.

**Why first.** Everything downstream assumes functions are values.
Iterators, callbacks, the stdlib's `map` / `filter` / `reduce`,
event handlers in embedders, higher-order patterns in user code —
none of it works cleanly without closures. Closures are also the
single biggest reason the current language feels toy-like.

**Scope.**

- New `Value::Fn` variant carrying `params`, compiled body, and a
  captured-environment snapshot.
- Function declarations become sugar: `fn foo(x) { ... }` lowers
  to `let foo = <fn-value>`. Named-only calls (`foo(x)`) keep
  working; value-calls (`let g = foo; g(x)`) start working too.
- Lambda syntax. Prefer `fn(x) { ... }` for consistency with
  declared fns (no new token types). Reconsider `|x| x * 2` later
  if the noise bothers anyone.
- Lexical capture: a function captures the variables it references
  from the enclosing scope at the moment it's constructed, by
  value (clone). Mutations to captured vars inside the closure
  don't propagate back out — same call-by-value model Nybl already
  uses for function parameters.
- Call dispatch is extended: the `Call { callee, args }` path now
  accepts any expression that evaluates to `Value::Fn`, not just a
  named identifier. Host and builtin dispatch still resolve by
  name as today.

**Where it lives.** `nybl-lang` core. `nybl-vm` needs a new opcode
(`MakeClosure(fn_idx)`) plus a call-indirect dispatch. `nybl-compile`
emits a Rust closure (`Box<dyn Fn(...) -> Result<Value, NyblError>>`
or a generated fn per lambda).

**Non-goals.** No currying, no partial application, no rest
parameters. `self` and method binding stay tied to method-call
syntax.

**Risks.** The VM's `Rc<Chunk>` model interacts with capturing —
a closure holds its body and an env snapshot; cloning the Value
must be cheap (envs use `Rc` internally).

### Phase 2 — Modules and imports

*Status: done across all three engines + nybl-sys.
`NyblHost::resolve_module(name) -> Option<Result<String,
NyblError>>` added to the core trait with a `None`-returning
default. `import foo.bar.baz` is now a statement; the walker and
VM each run the module in a sub-engine that inherits the parent's
import cache, memory ceiling, and step budget but runs with a
fresh scope so module code can't see the importer's locals.
Re-imports of the same path in one `run` hit the cache and are
no-ops at the injection site. Circular imports surface as a clean
error via a `Loading` sentinel in the cache. Named fns imported
from a module come back as engine-compatible `Value::Fn`s —
walker gets `FnBody::Ast`, VM gets `FnBody::Compiled(Rc<Chunk>)`.
nybl-sys's `StandardHost::with_module_root(path)` maps
`foo.bar.baz` to `<root>/foo/bar/baz.nybl` with path-traversal
guards. The AOT transpiler takes a compile-time
`Options::module_resolver` callback, pre-resolves the entire
module graph with DFS cycle detection, and emits each module as a
`__mod_<slug>__load` fn + `__mod_<slug>__Exports` struct + a
shared `Ctx::module_cache` that memoises loaded modules and
detects cycles at runtime via a `__ModuleLoading` sentinel. Cross-
module refs unpack the exports struct into local Nybl bindings at
each import site, matching the walker's flat-injection semantics
(with transitive re-exports). 9 walker tests + 8 differential
tests + 3 nybl-sys tests + 5 AOT e2e tests + 5 three-way corpus
entries. All green.*

**Why second.** Multi-file programs are table stakes. Closures
without modules would still leave users writing 2000-line single
files.

**Scope.**

- Extension to `NyblHost`:
  ```rust
  fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>>;
  ```
  Default impl returns `None`. Embedders supply their own — file
  system, embedded string assets, network fetch, whatever fits.
- Core parser: `import foo` / `import foo.bar` / `from foo import x`.
  Names are identifier-dotted strings.
- Module caching: repeated imports of the same name resolve once.
- Modules are Nybl source files; loading one parses it, evaluates
  its top-level statements in a fresh module scope, and exposes
  its public bindings under the module name.
- What "public" means: start simple — every top-level `let` and
  `fn` in a module is public. Add a `pub` / `priv` distinction
  later if we need it (probably we will).
- `nybl-sys` adds a filesystem resolver that maps `foo.bar` →
  `./foo/bar.nybl` relative to a configurable root.

**Where it lives.** Parser + evaluator changes go in `nybl-lang`.
The `NyblHost` trait (also in `nybl-lang`) gains the new method —
core stays zero-dep because it doesn't *implement* resolution, it
just declares the hook. `nybl-sys` gets the filesystem impl.

**Non-goals.** No circular import magic (error out); no version
selection (that's the package manager); no re-exports (can add
later).

**Risks.** Module scoping intersects with closures — a module-
level `fn` captures module-level state. Design must be clear that
module scope is its own lexical environment.

### Phase 3 — User-defined types (structs and enums)

*Status: done across all three engines.* `struct Name { f1, f2 }`
and `enum Shape { Empty, Circle(r), Rect { w, h } }` declare
product and sum types respectively. Construction is strict
(missing / extra / duplicate fields all error at emit or runtime).
Enums ship unit / tuple / struct variants; `::` is the path
separator (`Shape::Circle(5)`, `Shape::Rect { w: 4, h: 3 }`).
Field access is `obj.field` for structs and struct-shaped enum
payloads; field assignment (`obj.f = v`, `obj.f += v`) works on
bare-ident targets. Methods declared via `fn Type.method(self,
...) { body }` live in a per-engine registry keyed by
(type_name, method_name); enum methods dispatch by enum type,
not variant, so all variants of `Shape` share `fn Shape.area`.
User methods win over builtin method dispatch of the same name.
`self` is passed by value (matching how params work elsewhere);
idiomatic mutation is `c = c.bump()`, not in-place.

Walker, VM, and AOT all implement the same semantics:

- Walker: `struct_defs`, `enum_defs`, `methods` BTreeMaps on
  `Evaluator`; `Value::Struct(NyblStruct)` and
  `Value::EnumVariant(NyblEnumVariant)` use compact `Rc`-backed
  copy-on-write payloads, keeping the enum small while making
  pass/assign/return constant-time.
- VM: new `DefineStruct` / `DefineEnum` / `DefineMethod` /
  `ConstructStruct` / `ConstructEnum` / `FieldGet` / `FieldSetInPlace`
  opcodes; `chunk.struct_defs` / `chunk.enum_defs` pools;
  `vm.user_methods` registry; `CallMethod` checks user methods
  before the built-in dispatch.
- AOT: compile-time `TypeRegistry` collected across the root +
  every transitively-imported module; each user method emits as
  a mangled Rust fn (`nybl_fn_<prefix>method_<Type>__<name>`); a
  generated `__nybl_try_user_method` dispatcher runtime-matches
  `(type_name, method_name)` and returns `Some(Value)` or falls
  through to the builtin. `self` is remapped to `nybl_self`
  because Rust reserves `self` for inherent methods on trait
  impls.

Known AOT divergence: user types are globally visible regardless
of where the `import` appeared, because the AOT resolves types
statically; walker / VM gate visibility on import. In practice,
well-formed multi-module programs behave identically — the
divergence only shows for "use before import" patterns that
would be bugs anyway.

Tests: 46 new walker tests, 15 new walker↔VM differential tests,
8 new AOT e2e tests, 9 new three-way corpus entries (68 total).
All green.

**Why third.** Dicts-with-convention simulate structs badly, and
tag-field workarounds simulate tagged unions even worse. Real data
modeling wants named fields, a type identity, and — for sum types
— variant dispatch that's checked by the language rather than by
convention. `Result` (phase 5) is a sum type, so enums land here;
pattern matching (phase 4) destructures them; the stdlib (phase 7)
leans on both heavily.

**Scope.**

- **Structs** — product types with named fields:
  ```
  struct Point { x, y }

  fn Point.distance(self, other) {
      let dx = self.x - other.x
      let dy = self.y - other.y
      return math.sqrt(dx * dx + dy * dy)
  }
  ```
  Construction: `Point { x: 1, y: 2 }`. Positional `Point(1, 2)`
  can come later.
- **Enums** — sum types with named variants. Each variant is one of:
  unit (`Empty`), tuple (`Circle(radius)`), or struct
  (`Rectangle { width, height }`):
  ```
  enum Shape {
      Circle(radius),
      Rectangle { width, height },
      Empty,
  }

  fn Shape.area(self) {
      match self {
          Shape::Circle(r) => math.pi * r * r,
          Shape::Rectangle { width, height } => width * height,
          Shape::Empty => 0,
      }
  }
  ```
- **Construction**. Structs: `Point { x: 1, y: 2 }`. Enum
  variants: `Shape::Circle(5)`, `Shape::Rectangle { width: 4,
  height: 3 }`, `Shape::Empty`. Fully qualified paths avoid
  ambiguity with free functions of the same name.
- **Field access** via `.`: distinct from dict indexing. `foo.x`
  resolves by name; `foo["x"]` works only for dicts.
- **Method resolution**: `foo.bar(args)` looks up `bar` on the
  value's type first (struct or enum), then falls back to the
  built-in method dispatch (arrays / strings / dicts).
- **New `Value` variants**: `Value::Struct { type_name, fields }`
  and `Value::EnumVariant { type_name, variant, payload }`. The
  payload is `Unit`, `Tuple(Vec<Value>)`, or `Struct(Vec<(String,
  Value)>)` depending on the variant's shape.

**Where it lives.** Core — all three engines.

**Non-goals.**

- No inheritance. No traits or interfaces. No generics.
- No exhaustiveness checking at declaration time (that becomes a
  pattern-matching concern — see phase 4).
- No `impl` blocks. Methods are free `fn TypeName.method(...)`
  definitions, same pattern for structs and enums.

**Risks.** Enum variant identity is the critical correctness
property — equality checks must compare `type_name` +
`variant_name`, not just payload. Pattern matching (phase 4)
depends on this working.

### Phase 4 — Pattern matching ✅

**Why fourth.** Sum types without variant destructuring are
unusable. Phase 3 introduces enums; phase 4 gives users the
primitive for taking them apart. It also lands before `Result`
(phase 5) because `try` is just sugar for a specific `match`
pattern, and `try_call`'s output is pattern-matched.

**Scope.**

- `match expr { pattern => expr, ... }` as an expression.
- Patterns supported in v1:
  - Literals: `1`, `"foo"`, `true`, `none`.
  - Wildcard: `_`.
  - Variable binding: `x` (binds the scrutinee's value to `x`).
  - Enum variants: `Result::Ok(v)`, `Shape::Rectangle { width,
    height }`, `Option::None`, with nested patterns allowed —
    `Err(FileError::NotFound(path))` works.
  - Struct destructuring: `Point { x, y }` or
    `Point { x, .. }`.
  - Array destructuring: `[a, b, c]` or `[head, ..rest]`.
  - Or-patterns: `1 | 2 | 3`.
  - Guards: `x if x > 0 => ...`.
- Compiled in the walker to nested `if`/`else`; in the VM to a
  small decision-tree opcode; in the AOT to a straight Rust
  `match`.

**Where it lives.** Core — all three engines.

**Non-goals.**

- No exhaustiveness checking in v1. A dynamic language can get
  away with "no matching arm → runtime error". Revisit in
  phase 9.
- No custom matcher protocol, no deref patterns, no range
  patterns (`1..10`). Those are phase-9 polish.

**Delivered.**

- Lexer gains `match`, `=>`, `|`, `..` tokens; parser grows
  `ExprKind::Match` + the `Pattern` AST (literals, wildcard,
  binding, enum variants with tuple/struct payloads, struct
  destructure with `..`, array destructure with `..rest`/`..`,
  or-patterns, guard clauses).
- Walker adds `eval_match` plus a shared `pattern_matches`
  helper (re-exported as `nybl::pattern_matches`) so every engine
  runs the same structural matcher.
- VM adds a `patterns: Vec<Pattern>` pool on `Chunk` plus two
  new instructions: `MatchFail { pattern, on_fail }` (pops the
  candidate, applies bindings on match, jumps on miss) and
  `MatchExhausted` (raises the runtime error). Each arm
  compiles to a `PushScope` / `Dup` / `MatchFail` / guard /
  body / `PopScope` sequence with explicit fall-through.
- AOT emits a Rust block expression that constructs each
  `nybl::parser::Pattern` inline and dispatches through
  `nybl::pattern_matches`, extracting bindings into Rust locals
  before the guard and body.
- **Tests**: 20 walker tests, 16 VM differential tests, 14
  three-way corpus programs exercising every pattern kind plus
  guards, or-patterns, nested variants, and the
  "no-arm-matched" runtime error. All three engines agree on
  every program.

### Phase 5 — Error handling (Result + `try`) ✅

**Why fifth.** Programs past a few hundred lines need a way for
libraries to signal recoverable failures without either aborting
the whole program or forcing every caller to pre-check every
input. Nybl rejects exception machinery in favour of a Result-based
model built on the enum type from phase 3 and the pattern matcher
from phase 4 — a lighter touch that maps cleanly onto all three
engines.

**Two-tier semantics.** The model splits runtime failure into two
distinct categories that don't get confused at the language level:

1. **Unwinding errors** — the existing `NyblError` mechanism. Raised
   by the runtime for type mismatches, division by zero, index
   out of bounds, "function not found", and — crucially — resource-
   limit violations (`too many steps`, `Memory limit`). These
   unwind to the engine boundary and halt the program. **User
   code cannot catch them at the statement level.** This is the
   load-bearing property that makes `NyblLimits` a real sandbox:
   a script can't swallow a step-limit error and loop anyway.
2. **Result values** — ordinary `enum Result { Ok(value),
   Err(error) }` instances. Libraries that can fail in a
   recoverable way return these; callers inspect, propagate, or
   destructure them with `match` (phase 4) like any other value.
   No control-flow magic.

**Scope.**

- `Result` ships in the stdlib as `enum Result { Ok(value),
  Err(error) }` with helper functions (`is_ok`, `is_err`,
  `unwrap`, `unwrap_or`, `map`, `and_then`). Written in Nybl;
  lives in `nybl-std` (phase 7). Core stays agnostic about
  error representation.
- **`try <expr>` operator** — parses as a prefix expression. Pure
  sugar for a specific `match`:
  ```
  // This
  let text = try read_file(path)

  // Desugars to
  let text = match read_file(path) {
      Ok(value) => value,
      Err(e) => return Err(e),
  }
  ```
  If `<expr>` evaluates to a non-Result value, `try` raises a
  runtime error. Same rule as Rust's `?`.
- **`try_call(f)` builtin** — Lua's `pcall`, renamed. Calls `f`
  (a zero-arg closure) and catches any *non-fatal* unwinding
  `NyblError`, returning
  `Result::Ok(return_value)` on success or
  `Result::Err(RuntimeError { message, line })` on unwind.
  Resource-limit errors — flagged with `is_fatal = true` on
  `NyblError` — bypass `try_call` entirely so the sandbox
  invariant can't be undone by a wrapping `fn` { ... }`.
- **`RuntimeError` struct** lives in `nybl-std` alongside `Result`
  so pattern-matching the payload is idiomatic:
  ```
  match try_call(fn() { return risky(input) }) {
      Ok(v) => v,
      Err(RuntimeError { message, line }) => {
          log("crashed at {line}: {message}")
          fallback()
      },
  }
  ```

**Where it lives.** `try` is a parser + codegen change in
`nybl-lang`; the three engines each lower it to their existing
match / branching machinery (walker: `Signal::Return`-style
propagation on `Err`; VM: a new `TryUnwrap` opcode that inspects
the top-of-stack Result variant; AOT: emits a Rust `match` with
`return Err(e)` in the `Err` arm). `try_call` is a builtin in
`nybl-lang` that wraps a runtime call in its engine's
error-trapping primitive. `NyblError` gains an `is_fatal: bool`
field.

`Result` and `RuntimeError` live in `nybl-std` — core stays
zero-dep, and nothing forces embedders to load `nybl-std` if
they have their own error conventions.

**Non-goals.**

- No `try { ... } catch { ... }` block form. If you need
  multi-statement error handling, use `match`.
- No automatic conversion between Result variants (no `From` /
  `Into` chain). Explicit `map_err` or similar in the stdlib.
- No stack traces in `Err` payloads for v1.
- `try_call` is deliberately clunky. It exists; it isn't
  idiomatic. Most code uses `Result` + `try` and never touches
  it.

**Risks / open points.**

- `try`'s interaction with top-level code. At program scope
  (outside a function) there's nothing to return from. Treat
  top-level `try` on an `Err` as a runtime error — either the
  user converts to a value with `match` or they put it in a
  function.
- `try_call` in the AOT must not swallow resource-limit errors.
  The emitted Rust inspects `NyblError::is_fatal` before
  wrapping.
- Naming of Result's variants inside `nybl-std`. Rust-style
  `Result::Ok` / `Result::Err` is one option; making `Ok` and
  `Err` top-level names (imported by default) saves typing at
  the cost of polluting the global namespace. Lean
  default-imported like Rust's prelude; embedders who want it
  strict can skip loading `nybl-std`.

**Delivered.**

- Lexer gains the `try` keyword; parser grows
  `ExprKind::Try(Box<Expr>)` as a prefix expression that binds
  tighter than binary ops and looser than postfix operators
  (mirrors Rust's `?`).
- **Shape-based recognition**: `try` accepts any enum value
  whose variant is named `Ok` or `Err`. `Result` itself will
  live in `nybl-std` (phase 7); until then user code declares
  its own `enum Result { Ok(v), Err(e) }` (or any two-variant
  enum with the same naming) and `try` works on it.
- Walker: `eval_try` unwraps `Ok(v)` to `v`, raises for
  malformed `Ok`/`Err`-on-non-Result values, and uses a
  sentinel `NyblError` + `pending_try_return` field on the
  evaluator to carry the `Err` variant back up to the
  enclosing `call_nybl_fn`, which converts it to a regular
  `Signal::Return`. Top-level `try` on `Err`
  (`call_depth == 0`) surfaces as a real runtime error with a
  friendly hint.
- VM: new `TryUnwrap` instruction. Pops the candidate,
  unwraps `Ok` to the stack, fast-returns on `Err` via the
  existing `do_return` path, raises at the top-level frame,
  and bails on non-Result-shaped inputs.
- AOT: `try <expr>` emits a Rust `match` over the `Value`'s
  `EnumVariant` / `EnumPayload`. An emitter-state flag
  (`in_top_level`) picks between `return Ok(err_value)`
  (inside user fns / lambdas, which return
  `Result<Value, NyblError>`) and `return Err(NyblError::…)`
  (inside `run_program`, which returns `Result<(), NyblError>`).
- **`NyblError::is_fatal: bool`** added. Resource-limit paths
  (`too many steps`, `Memory limit exceeded`) now go through
  `error_fatal_with_hint`; every other error stays non-fatal.
  Fatal errors bypass `try_call`'s catch — the sandbox
  invariant holds.
- **`try_call(f)` builtin** in all three engines. Invokes a
  zero-arg callable and wraps the outcome:
  - normal return → `Result::Ok(value)`
  - non-fatal `NyblError` → `Result::Err(RuntimeError
    { message, line })`
  - fatal `NyblError` → re-raise unchanged
  Both `Result::Ok/Err` and `RuntimeError` are constructed by
  shared helpers (`nybl::builtins::make_try_call_ok` / `_err`)
  so they produce the same shape regardless of whether the
  program declared the types itself — pattern matching still
  works because the matcher compares type-name strings.
  - Walker: synchronous call through `call_nybl_fn`, wraps the
    result directly.
  - VM: a new `try_call_wrapper` field on `Frame` plus an
    `unwind_to_try_call` helper in the main dispatch loop.
    Normal `Return` through a wrapper frame wraps the value
    in `Ok`; a non-fatal error propagates through frames until
    it hits the wrapper, which wraps in `Err` and pushes for
    the caller. Fatal errors bypass the wrapper entirely.
  - AOT: a `__nybl_try_call` runtime helper in both (sandbox
    and non-sandbox) preambles. The call site at `try_call(f)`
    emits a direct call into the helper. Compiled closures
    downcast through the `AotClosure` body and the helper
    inspects `NyblError::is_fatal` before wrapping.
- **Tests**: 19 walker tests (10 on `try`, 9 on `try_call`),
  16 VM differential tests (8 + 8), and 15 three-way corpus
  programs. All three engines agree on every case, including
  the fatal-step-limit-is-uncatchable invariant that protects
  `NyblLimits`.

**Still pending (deferred to phase 7 — `nybl-std`).**

- `Result` enum + helper fns (`is_ok`, `is_err`, `unwrap`,
  `unwrap_or`, `map`, `map_err`, `and_then`) — written in Nybl,
  shipped with the standard library. The structural
  recognition means `try_call` already produces values that
  match this shape; declaring the type in `nybl-std` just makes
  it available without the user writing it themselves.
- `RuntimeError` struct — likewise. The fields
  (`message: Str`, `line: Number`) are already set by
  `make_try_call_err`.

### Phase 6 — Integer type ✅

**Why sixth.** `f64`-only arithmetic bites real use cases: bit
twiddling, array indices past 2^53, any domain where
`3.0000000001` surprises you. Orthogonal to everything above and
below, so it can move in parallel, but cheaper to do after the
semantic surface stabilises so we only refit `ops` / `methods` /
`builtins` once.

**Scope.**

- New `Value::Int(i64)` variant alongside existing
  `Value::Number(f64)`.
- Numeric literals: `42` → `Int`, `42.0` → `Number`.
- Ops:
  - `Int op Int` → `Int` (overflow → wrapping? panic? error? —
    pick one and document; lean toward `NyblError`).
  - `Int op Number` and `Number op Int` → `Number` (Int widens).
  - Integer division: new `//` operator, or have `/` split
    (`/` always returns Number, `//` returns Int). Pick the
    latter — matches Python.
- Builtins: `int()` keeps truncating semantics; new `float()`
  coerces up. `type()` returns `"int"` or `"number"` accordingly.
- String parsing: `int("42")` returns Int; `float("3.14")` returns
  Number.

**Where it lives.** Core. Breaks tree-walker tests that assert
`type(42) == "number"`; fine, those get updated as part of the
phase.

**Non-goals.** No `i32` / `u64` / unsigned variants. No bigints.
No arbitrary precision. One integer type.

**Risks.** Every single `f64` pattern match in `ops.rs`,
`methods.rs`, `builtins.rs` needs an `Int` arm. Tedious; no
architectural risk.

**Delivered.**

- `Value::Int(i64)` variant. Eq / Clone / Drop / Display /
  `type_name` / `is_truthy` all extended.
- Literals: integer-shaped tokens (`42`, `-3`, `0`) lex to
  `Token::Int`; anything with a decimal (`42.0`, `3.14`) stays
  `Token::Number`. Out-of-range integer literals surface as a
  lex-time error rather than silently downgrading to float.
- Lexer: new `Token::Int(i64)`, new `Token::SlashSlash`. **Line
  comments switched from `//` to `#`** so `//` can claim the
  integer-division slot — the only breaking-source-compat
  change phase 6 lands.
- Parser: `ExprKind::Int`, `LiteralPattern::Int`,
  `BinOp::IntDiv`. Pattern `-42` is an `Int` literal (the
  negation is checked so `-i64::MIN` as a literal errors
  rather than wraps).
- Ops: Int/Int arithmetic with `checked_add`/`sub`/`mul`/`rem`
  — overflow → `NyblError`. Cross-type Int/Number widens to
  Number. `/` is always Number (Python rule); `//` is always
  Int, truncating toward zero. `%` follows the same widening
  rule. Comparisons are exact for Int/Int (sidestepping
  f64 precision loss past 2^53) and widen for mixed pairs.
  `neg(i64::MIN)` → `NyblError`.
- Indexing: both `arr[0]` (Int) and `arr[0.0]` (Number-via-
  cast) keep working, so legacy code composes unchanged.
- Builtins: `int()` returns Int (direct for Int input, `as
  i64` for Number, integer-first parse for strings with a
  float-then-truncate fallback). New `float()` companion for
  the reverse coercion. `len` / `range` / `array.len` /
  `array.index_of` / `string.len` / `string.index_of` /
  `dict.len` all return Int. `type()` distinguishes
  `"int"` vs `"number"`. `min` / `max` preserve the input
  shape (Int/Int → Int, Number/Number → Number, mixed →
  Number).
- Walker: `eval_expr` handles `ExprKind::Int`; repeat
  statement accepts both Int and Number; pattern-match
  literal comparison does cross-type equality.
- VM: new `Constant::Int(i64)`, `Instr::IntDiv`. Disasm
  renders them. `MakeRepeatCount` accepts Int.
- AOT: emits `Value::Int(42i64)` for Int literals,
  `::nybl::ops::int_div` for `//`. The `repeat` lowering
  accepts both variants. `float` wired into the builtin
  dispatcher.
- **Tests**: 16 walker tests, 13 VM differential tests, 12
  three-way corpus programs. Overflow, div-by-zero, cross-
  type arithmetic, `//` truncation toward zero, pattern
  matching, repeat with Int — all three engines agree. The
  existing `comments_in_code` / `builtin_type` / snapshot
  tests were updated to match the new token + type names.

### Phase 7 — Standard library (`nybl-std`) ✅

**Why seventh.** Everything above enables it, and it's the thing
that turns "Nybl can do it" into "Nybl ships with it". Phases 1–6
produce the language; phase 7 produces the library.

**Scope.**

- New crate `nybl-std`. Contents are Nybl source files bundled as
  `&'static str` constants (or loaded via `include_str!`).
- The crate exposes one function: roughly
  ```rust
  pub fn resolve(name: &str) -> Option<&'static str>;
  ```
  which maps module names (`"std.iter"`, `"std.math"`, …) back
  to the bundled source.
- Embedders opt in by chaining `nybl-std::resolve` into their
  `NyblHost::resolve_module` impl. `nybl-sys`'s default host adds
  this chain so `nybl-cli` users get stdlib imports for free.
- Proposed modules for v1:
  - `std.math` — `sqrt`, `sin`, `cos`, `abs`, `pi`, `e`, `floor`,
    `ceil`, `round`, `pow`. (Some delegate to built-ins that
    wrap `f64::*` — those built-ins live in core.)
  - `std.iter` — `map`, `filter`, `reduce`, `take`, `drop`,
    `zip`, `enumerate`, `sum`, `product`, `any`, `all`, `find`.
    Operates on arrays; returns arrays (no lazy iterator
    protocol in v1).
  - `std.string` — `pad_left`, `pad_right`, `repeat`, `chars`,
    helpers that don't fit the method-on-string pattern.
  - `std.collections` — `Set`, `Queue`, `Stack` as struct types
    wrapping arrays/dicts.
  - `std.test` — `assert`, `assert_eq`, `assert_near`, and a
    tiny `test("name") { ... }` runner. Enables Nybl programs to
    self-test.
  - `std.json` — `parse(str)` and `stringify(value)`. Needs
    thought once structs land.

**Where it lives.** `nybl-std` crate. Zero Rust deps beyond
`nybl-lang` (and only for the resolver signature — no runtime
dep). Core stays untouched.

**Non-goals.** No I/O in `nybl-std` (goes through nybl-sys hooks).
No date/time (nybl-sys or a new nybl-time crate). No regex (probably
its own crate — regex engines are non-trivial).

**Risks.** None to core. The main risk is bikeshedding naming and
APIs; enforce "write idiomatic Nybl first, Rust library influence
second".

**Delivered.**

- New `nybl-std` crate — zero runtime dependencies, modules
  bundled via `include_str!`. Exposes `pub fn resolve(name:
  &str) -> Option<&'static str>` plus a `MODULES` listing.
- **Cross-engine import-type-transfer.** Before this phase,
  `import` only transferred top-level `let` and `fn` bindings
  — struct / enum / method declarations stayed in the
  sub-evaluator. That made stdlib modules that declare types
  (like `std.result`) unusable. Walker and VM now both
  propagate type declarations and `fn_decls` (so cross-fn
  calls inside a module resolve via the parent's
  `functions` table). AOT already transferred types via its
  `TypeRegistry` pre-pass.
- **Core math builtins** — `sqrt`, `sin`, `cos`, `tan`,
  `floor`, `ceil`, `round`, `pow`, `log`, `exp`. Always
  available; no import needed. `floor`/`ceil`/`round` return
  `Int` when the result fits in `i64` (so `arr[floor(i)]`
  works without a wrapping `int()`), else `Number`.
- Also added a `float()` builtin as the Int-to-Number
  companion to `int()`.
- Shipped stdlib modules:
  - **`std.result`** — `enum Result { Ok, Err }`, `struct
    RuntimeError { message, line }`, and combinators
    `is_ok` / `is_err` / `unwrap` / `unwrap_or` / `expect`
    / `map` / `map_err` / `and_then`.
  - **`std.math`** — `pi`, `e`, `tau` constants plus
    `clamp`, `sign`, `factorial`, `gcd`, `lcm`, `sum`,
    `mean`.
  - **`std.iter`** — `map`, `filter`, `reduce`, `take`,
    `drop`, `zip`, `enumerate`, `all`, `any`, `count`,
    `find`, `find_index`, `flatten`, `sum`, `product`,
    `min_array`, `max_array`.
  - **`std.string`** — `pad_left`, `pad_right`, `center`,
    `chars`, `reverse`, `is_palindrome`, `count`, `join`.
  - **`std.test`** — `assert`, `assert_eq`, `assert_near`,
    `assert_raises`.
  - **`std.collections`** — `Stack`, `Queue`, and `Set`
    struct types with value-semantic methods (caller rebinds
    the result: `s = s.push(v)`). Set algebra operations —
    `union`, `intersect`, `difference`. Factory fns `stack()`,
    `queue()`, `set()`, `set_of(arr)`.
  - **`std.json`** — `stringify(value)` and `parse(text)` in
    pure Nybl. Parse errors raise a runtime error that
    `try_call` surfaces; design documented in the module
    header. Known gaps: `\b`, `\f`, `\uXXXX` escapes not
    supported (documented).
- **nybl-sys integration** — `StandardHost::resolve_module`
  now tries `nybl_std::resolve` first, then falls back to
  filesystem resolution. `nybl-cli` users get `import
  std.math` working with no extra config.
- **Tests**: 24 in-crate stdlib smoke tests (walker) + 8 for
  `std.collections` + 14 for `std.json`, 13 VM differential
  tests (stdlib + import type-transfer), 9 three-way corpus
  programs. All three engines agree on every program.
- **Lexer gap closed for std.json.** Added `\r` to the string
  escape set so `stringify` / `parse` can round-trip
  Windows / HTTP line endings. Previously only `\n` and `\t`
  were supported.

**Deferred.**

- `test("name") { ... }` block-level syntax sugar — `assert*`
  primitives are enough to start, and the block form would
  need parser-level changes.

### — Checkpoint: "MVP general purpose" reached —

After phase 7 Nybl has: closures, modules, structs + enums,
pattern matching, Result-based error handling, an integer type,
and a standard library. A competent developer can write a
non-trivial program in it. The core crate is still zero-dep
embeddable. The remaining phases are tooling and polish.

### Phase 8 — Package manager (`nybl-pkg`) — parked

*Explicitly deferred; no active work.* The `nybl-std`
bundled-source approach (phase 7) already covers the stdlib
story without a package manager, and external-dependency
management is a separate product concern.

Left here as a future-work sketch. Its own plan doc when the
time comes. Rough shape:

- `nybl-pkg` CLI: `nybl install foo`, `nybl publish`.
- `nybl.toml` manifest at the root of a project: name, version,
  dependencies.
- Start with a single git-based registry, no central index.
- Install puts sources in `./.nybl/modules/`; `nybl-sys`'s fs
  resolver checks there first, then the project root.

None of this touches core. It's tooling plus convention.

### Phase 9 — Polish

Ongoing, not a discrete phase:

- Documentation — tutorial, reference, `nybl-std` API docs, a
  landing page. Without docs, none of the above matters to
  anyone who isn't already in the codebase.
- Better parse / runtime errors (line spans, source pointers).
- REPL: multi-line input, history, tab completion.
- Language server (far future; may never happen).
- Debugging: tracebacks on caught exceptions, a `debug()` hook
  for embedders.
- Performance pass once the feature set is stable: VM dispatch
  tuning, Value cloning reduction, inline caches.

**Landed.**

- ✅ **Source-pointer error rendering.** Lexer tracks column;
  `SpannedToken` carries it; `NyblError::render(source)` prints
  an `error:` header, `--> line N[:col]` location, source
  snippet with gutter, a carat under the column (when known),
  and a trailing `hint:` line. Tab-aware carat padding.
  `nybl-cli` uses it for both file runs and REPL.
- ✅ **"Did you mean?" suggestions.** New `nybl::suggest`
  module (Wagner–Fischer Levenshtein + length prune +
  per-target edit budget). Walker, VM, and AOT all populate
  hints on `Variable X not found`, `Function X not found`,
  `Struct X has no field Y` (construction + access), and
  `Enum X has no variant Y`. Shared
  `suggest::CORE_CALLABLE_BUILTINS` list keeps builtin
  suggestions consistent across engines.
- ✅ **Match exhaustiveness warnings.** New `nybl::check`
  module + `NyblWarning` type + `nybl::parse_with_warnings`
  entry point. Flags enum matches that miss variants when
  there's no catch-all; conservative (guards don't count;
  imported enums are opaque). `nybl-cli` prints warnings
  before running.

**Still open.**

- **Runtime errors don't yet carry column.** Would require
  adding `column` to `Expr` / `Stmt` — 47 constructor sites.
  The renderer falls back cleanly and parse errors (where
  the carat matters most) already have column info.
- **REPL multi-line input, history, tab completion.** Needs
  a line-editor dep (rustyline or similar) in `nybl-cli`.
- **Exhaustiveness check doesn't follow `import`s.** Imported
  enums are opaque to `nybl::check`; a second pass that walks
  the import graph would close that gap.
- **Documentation.** Tutorial + language reference + stdlib
  API docs. Nothing formally written yet beyond this roadmap.
- **Performance pass.** See "Known tech debt" below.
- **Language server / debugger hook / tracebacks.** Far-future
  items kept in the original list for reference.

## Ship-it readiness tracker

A running tally of the "what's missing before 1.0" items from
the codebase walkthrough. Linked to the phase that delivered
(or will deliver) each one.

| Item | Status | Notes |
|------|--------|-------|
| **Diagnostics** — column info, source snippets, carat, "did you mean?" | ✅ done | Phase 9 landed across walker / VM / AOT. |
| **`std.collections`** (Set / Queue / Stack) | ✅ done | Shipped as struct types with value-semantic methods + `union`/`intersect`/`difference`. |
| **`std.json`** (parse / stringify) | ✅ done | Pure Nybl implementation; parse raises on malformed input and `try_call` surfaces the error. `\b` / `\f` / `\uXXXX` escapes documented as known gaps. |
| **Match exhaustiveness checking** | ✅ done | `nybl::check` + `NyblWarning`, phase 9. Imported enums still opaque. |
| **Performance** | ✅ meaningfully faster | VM now runs **2.5×–3.1× faster** than the tree-walker on micro-benchmarks (combined: 2.6×; fib(28): 2.5×; 500k-iter loop: 3.1×). Earned via compile-time slot resolution + capture analysis + peephole superinstructions. Makes the VM a genuinely useful tier for embedders: walker for simple/portable cases, VM for "I need speed but can't bring rustc to the target machine," AOT for max speed. |
| **Documentation** (tutorial, reference, API docs) | ❌ open | No user-facing docs beyond this roadmap and inline `///` comments. |
| **Packaging** (`nybl install`, dependency manifest) | ⏸ deferred | Phase 8 in the plan; explicitly parked for now. `nybl-std` bundled-source approach handles stdlib without needing a package manager. |

## Known tech debt

Items worth fixing when convenient — none of them blocks
shipping, but each one hurts maintenance or future work.

- ~~**AOT preamble duplication.**~~ ✅ Fixed. The two
  preamble string literals (`RUNTIME_PREAMBLE` +
  `RUNTIME_PREAMBLE_SANDBOX`) were refactored into four
  composable pieces — `RUNTIME_HEADER`, `CTX_BASE` /
  `CTX_SANDBOX`, `RUNTIME_SHARED`, and `TICK_HELPER` — that
  `emit_runtime_preamble` stitches together. New runtime
  helpers now land once in `RUNTIME_SHARED` instead of
  twice; `emit.rs` lost ~170 lines. `PUBLIC_ENTRY` and
  `MAIN_FN` still have two variants (15 and 8 lines each)
  since the `run()` signature genuinely differs — collapsing
  them wouldn't pay for itself.
- ~~**Walker's `try` unwinding uses a sentinel error.**~~ ✅
  Fixed. `NyblError` gained an `is_try_return: bool` flag;
  `try` builds one via a new `NyblError::try_return_signal`
  `pub(crate)` constructor and the fn-call boundary checks
  the flag instead of comparing `.message` against a magic
  string. The `"__nybl_try_return_signal__"` constant is
  gone. A regression test
  (`try_sentinel_uses_flag_not_message_string`) pins the
  invariant that a real runtime error never carries
  `is_try_return: true`. The value still lives on
  `Evaluator::pending_try_return` — moving it onto the
  error itself would cycle `nybl::error` ↔ `nybl::value`,
  which isn't worth it.
- ~~**AOT `TypeRegistry` is flat, not module-scoped.**~~ ✅
  Fixed by detecting clashes at transpile time instead of
  scoping. Walker rejects cross-module type redeclarations
  with different shapes; AOT used to silently pick whichever
  module was seen last. `collect_type_registry` now returns
  `Result<TypeRegistry, NyblError>` and raises when two
  modules declare a struct/enum with the same name but
  different fields / variants. Same-shape redeclarations
  stay idempotent (mirrors the walker's re-import behaviour
  for a module imported via two paths). Error message names
  both declaration sites with line numbers. Full module
  scoping would have required renaming every type reference
  in the emitted Rust — much more invasive for no semantic
  win, since walker and VM both reject clashes anyway.
  Methods remain last-wins, matching the walker's
  permissive method-import path.
- ~~**Error paths have subtly different wording across engines.**~~
  ✅ Fixed. A new `nybl::error_messages` module hosts format
  helpers for the 12 messages that previously had 2+ copies
  across engines (`variable_not_found`, `function_not_found`,
  `struct_has_no_field`, `variant_has_no_field`,
  `struct_not_declared`, `enum_not_declared`,
  `enum_has_no_variant`, `cant_read_field`,
  `cant_assign_field`, `cant_call_a`, `cant_iterate_over`,
  `no_such_method`). Walker, VM, AOT-compile-time, and AOT's
  emitted runtime helpers all call the same functions, so a
  wording change now lands once. 4 unit tests in
  `error_messages::tests` pin the canonical output — a regex
  rewrite can't silently drift the text. One-off per-engine
  messages (e.g. `"VM: stack underflow"`) stay with their
  engine since there's nothing to deduplicate.
- ~~**`suggest::leak_name` leaks a `&'static str` per match
  analysis.**~~ ✅ Fixed. The check pass now threads an
  owned `Option<String>` through `gather_variants` instead of
  a `&mut Option<&'static str>`. The extra allocation is one
  `String::clone()` per first-enum-variant arm — negligible
  against the rest of the pass — and the `leak_name` fn
  (along with its no_std fallback stub) is gone. No behaviour
  change; all 11 `check::tests` still pass.
- ~~**VM hot-path string allocations.**~~ ✅ First pass
  landed. On a fib(25) + 2×100 000-iter loop benchmark the
  VM used to run ~30–40% slower than the tree-walker;
  profiling pointed at per-instruction `String` allocations
  and a per-tick TLS lookup. Fixes:
  - `Instr` / `EnumConstructShape` now derive `Copy` so the
    dispatch loop's per-step `.clone()` compiles to a
    register-sized memcpy rather than a `Clone::clone` match.
  - `Instr::LoadVar` / `Instr::StoreVar` / `call()` /
    `call_method()` no longer allocate a `String` per
    invocation to look up their target name; they split-borrow
    the current frame (`frame.chunk` + `frame.scopes`) and
    read the name as `&str` straight from the chunk's name
    pool. `set_existing` writes through `get_mut` instead of
    re-inserting so the scope map's existing key allocation
    stays put.
  - The memory-limit check in `tick()` ran two TLS loads per
    instruction. Batched to once every 256 ticks (masked
    with `TICK_MEMCHECK_MASK`), plus a final
    `nybl_memory_exceeded()` check at the end of `run()` so
    programs that allocate past the cap and then terminate
    in fewer than 256 remaining instructions still trap
    (regression-guarded by `safety_range_hard_cap`).

  Net: VM overhead vs the walker on the benchmark closed
  from ~30–40% to ~15%.
- ~~**VM slower than the walker on call-heavy workloads.**~~
  ✅ Second-pass fixes brought the VM to parity or slightly
  ahead. Split-benchmark profile pointed the finger at the
  `Instr::Call` dispatch — fib(28) was ~26% slower than the
  walker while the 500k-iter tight loop was only ~5% slower.
  The fixes:
  - `FnEntry` (params `Vec<String>` + chunk `Rc`) is now
    wrapped in an outer `Rc<FnEntry>` so the per-call
    `self.functions.get(name).cloned()` is a single
    refcount bump instead of cloning a `Vec<String>` + the
    chunk handle. The same wrapping runs through
    `user_methods` and `ModuleArtifacts::fn_decls` /
    `methods` so cross-module imports don't deep-clone
    either.
  - New `enter_user_fn` fast-path in `Vm::call` pops the
    args directly off the value stack into the new
    frame's parameter scope via `mem::replace` +
    `truncate`. The previous code allocated a
    `Vec<Value>` via `pop_n_values` just to immediately
    drain it into the scope map — ~500 000 small heap
    allocations per `fib(28)` run.
  - Reordered `call()` so the user-fn branch runs before
    the builtin / host dispatch matches. Safe because
    lexical shadowing is checked first (same as the
    walker) and `self.functions` never contains builtin
    names — `DefineFn` is the only writer and it's
    driven purely by user `fn` declarations. The reorder
    lets the hot path skip the 20-way `match name` and
    the host vtable call entirely.
  - `#[inline]` on `fetch` / `tick` / `dispatch` /
    `push_value` / `pop_value` / `peek_value`. All are
    one-line accessors where manual inlining hints move
    the needle for LLVM.

  Combined delta on the original fib + 2×100k-iter
  benchmark: walker ~68 ms, VM ~71 ms (1.04×). On an
  isolated fib(28) the VM is now ~2% faster than the walker.

  Further gains — scope `BTreeMap` → hashed / slotted map,
  `Value::clone` reduction, inline caches, compile-time
  slot resolution, superinstructions — would need a real
  profiler session rather than static reasoning.
- ~~**VM only at parity with the walker — no reason for embedders to use it.**~~
  ✅ Third pass landed the structural changes that were
  hinted at in the previous entry. VM is now 2.5×–3.1× faster
  than the tree-walker on call-heavy + loop-heavy
  micro-benchmarks. The point of a bytecode VM is to *beat*
  the AST walker — otherwise it's just a second
  implementation of the same semantics. Now it earns its
  keep, especially in the embedding use case (Rust apps
  that want to hand scripts to AI / users at runtime
  without shipping rustc to the target machine).

  What moved the needle:
  - **Compile-time slot resolution.** `LocalResolver` in
    the compiler assigns every parameter, `let`, and
    `for-in` variable a numeric slot index at compile time.
    Inside a function body, identifier references emit
    `LoadLocal(slot)` / `StoreLocal(slot)` — direct
    `Vec<Value>` indexing, no `BTreeMap<String, Value>`
    lookup, no `String` hashing, no scope-stack walk.
    Function frames carry a flat `slots: Vec<Value>`
    alongside (now usually empty) `scopes: Vec<BTreeMap>`.
    Module top-level keeps the BTreeMap path so `import`
    can still inject names dynamically.
  - **Compile-time capture analysis.** Lambda bodies track
    their free variables during compilation; each one
    becomes a `CaptureSource::ParentSlot(slot)` or
    `CaptureSource::ParentScope(name)` on the emitted
    `FnDef`. `MakeLambda` at runtime reads exactly those
    sources from the defining frame — no over-capture of
    out-of-scope slots (would have changed semantics) and
    no full `BTreeMap` flatten. Nested-lambda
    capture-of-capture propagates by re-noting the name in
    the parent's free-var list.
  - **Slot vec freelist.** ~500 000 `Vec::with_capacity`
    allocations in `fib(28)` (one per call frame) collapse
    into a ~64-entry freelist that recycles the backing
    buffers across calls. `enter_user_fn` / `call_closure`
    / user-method dispatch / return / unwind all route
    through `take_slots` / `return_slots`.
  - **Peephole superinstructions.** The compiler collapses
    hot trailing sequences into single opcodes at emit
    time (safe because the rewrite window is always the
    tail — no jump target can land in it):
    - `LoadLocal(a) + LoadLocal(b) + Add` → `AddLocals(a, b)`
    - `LoadLocal(a) + LoadLocal(b) + Lt`  → `LtLocals(a, b)`
    - `LoadLocal(s) + LoadConst(Int k) + Add` → `LoadLocalAddInt(s, k)`
      (and `Sub` with negated k goes here too, so `n - 1`
      is one opcode)
    - `LoadLocal(s) + LoadConst(Int k) + Lt` → `LtLocalInt(s, k)`
    - `LoadLocal(s) + LoadConst(Int k) + Add + StoreLocal(s)`
      → `IncLocalInt(s, k)` — the `i = i + 1` idiom.
    All five opcodes have an Int→Int fast path in the VM
    and fall back to generic `ops::add` / `ops::lt` on
    non-Int operands, so semantics stay identical.
  - **`AssignBack` enum** on `Instr::CallMethod` lets
    mutating methods (`arr.push(v)`) write back to either
    a slot or a named scope binding depending on where the
    receiver came from. Required once the receiver could
    be a slot-resolved local; before slots landed, all
    receivers were name-scoped.

  Benchmarks (release, best-of-5, Apple silicon):

  ```text
                      walker       VM    speedup
  fib(28)            180 ms     72 ms    2.5×
  count(500k)         72 ms     23 ms    3.1×
  combined (fib 25   70 ms     27 ms    2.6×
   + 2×100k loops)
  ```

  All 219 VM differential tests, 338 core walker tests,
  the three-way corpus, and the compile-roundtrip / stdlib
  smoke suites stay green. Nothing in the walker or AOT
  changed — this is purely additive VM work.
- ~~**`no_std` builds broken in `nybl-lang`.**~~ ✅ Fixed. The
  `no_std` path had drifted: 27 compile errors from missing
  `alloc::{format, string::ToString, vec}` imports in
  `check.rs` / `suggest.rs` / `error.rs` / `parser.rs` /
  `lib.rs`, plus every `f64` math method (`sqrt`, `sin`,
  `cos`, `tan`, `floor`, `ceil`, `round`, `trunc`, `powf`,
  `ln`, `exp`) — all of those live in `std::f64`, not
  `core::f64`. Fixes:
  - New `nybl::math` module wraps each `f64` method with a
    one-liner that dispatches to the native method under
    `std` and to `libm` under the `no_std` feature. Every
    builtin call site goes through the wrapper so the rest
    of the code stays `#[cfg]`-free.
  - New `no_std` feature (opt-in, forwarded from `nybl-vm`)
    pulls in the tiny pure-Rust `libm` crate under the
    hood. Named after the user intent ("I'm targeting
    no_std"), not the implementation detail (libm). Gated
    as an `optional = true` dep so std users never see it
    in their dep graph — verified via `cargo tree -p
    nybl-lang` (empty) vs `cargo tree -p nybl-lang
    --no-default-features --features no_std` (one line:
    `libm v0.2.16`).
  

  End-to-end proof the `no_std` surface works: a
  `wasm32-unknown-unknown` `cdylib` that `#![no_std]`s,
  uses `lol_alloc` for the global allocator, and exposes
  both `nybl::run` and `nybl_vm::run` to JS builds clean at
  **355 KB** stripped (walker + VM + libm + allocator) — `no_std` is
  slightly smaller because there's no std runtime lib.
  Either is shippable to a browser / edge / embedded
  target.

## Deliberately out of scope

Keeping this list is as important as the roadmap itself.

- **Static typing.** Nybl is dynamic. If static typing becomes
  interesting, it's an entirely separate language; don't bolt it
  on.
- **Macros / metaprogramming.** Adds complexity; the niche Nybl
  fills doesn't need it.
- **JIT.** Already rejected in `plans/execution-modes.md`. AOT
  covers "I want native speed".
- **Concurrency / async / threading.** Out of scope at the
  language level. Embedders already drive the host; if they want
  concurrency, they run multiple engines on multiple threads.
- **FFI to arbitrary C.** The `NyblHost` trait *is* the FFI. If
  you want to call C, expose it through a host method.
- **GC beyond reference counting.** The current Value model
  (Clone/Drop with tracked allocations) is fine for the
  workloads Nybl targets.
- **Breaking the embeddable invariant for convenience.** Ever.

## Open questions

- **Closure representation in the VM.** `Value::Fn` needs to
  carry the captured env. `Rc<Vec<(String, Value)>>` works but
  clones the whole env on every `Fn` value construction. Consider
  a flat slot vec indexed at compile time for speed.
- **Integer vs Number default for comparison.** Should
  `1 == 1.0` be `true` or `false`? Current Nybl says `1 == "1"`
  is `false`; consistency argues `Int(1) == Number(1.0)` is also
  `false`. Users might expect `true`. Decide before shipping
  phase 5.
- **Enum variant resolution.** `Shape::Circle(5)` is the fully
  qualified form. Should bare `Circle(5)` work when the parser
  can tell it's an enum variant by context? Rust allows
  `use Shape::*;` to make variants callable unqualified; Nybl
  could follow suit via `import Shape.*` or by auto-importing
  the variants of an enum declared in the current module. Pick
  auto-import-in-scope for ergonomics.
- **Which `NyblError`s are fatal.** Resource-limit errors must stay
  uncatchable by `try_call` — that's the sandbox invariant. Type
  errors, division by zero, index OOB probably should be catchable
  since user code has a reasonable interest in recovering from
  parse-like failures. Needs an explicit `is_fatal` bit on
  `NyblError`, populated at construction.
- **Module resolution timing.** Parse-time (all imports resolved
  before any code runs) or run-time (imports resolved when
  executed)? Python does the latter, which enables lazy /
  conditional imports but makes error diagnostics worse. Lean
  toward parse-time for clarity; revisit if it bites.
- **Should `nybl-std` modules be in `nybl-sys`'s default resolver
  chain, or opt-in?** Default-on is friendlier; opt-in is purer.
  Lean default-on in `nybl-cli`, opt-in for direct embedders.

## Dependency graph between phases

```
Phase 1 (closures)
        │
        ├──> Phase 2 (modules)
        │
        └──> Phase 3 (structs + enums) ──> Phase 4 (pattern matching)
                                                   │
                                                   └──> Phase 5 (Result + try)

Phase 6 (integer type)   (orthogonal, slot in anywhere)

Phase 7 (stdlib) wants 1–6 green
    │
    └──> Phase 8 (package manager, if/when it happens)

Phase 9 (polish) is continuous
```

Structs + enums (phase 3) are a prerequisite for phase 4 — you
can't match on variants that don't exist. Pattern matching
(phase 4) is a prerequisite for phase 5 — `try` is sugar for a
specific `match` on `Result`, and `try_call`'s output is
consumed by `match`. Phase 6 (integer type) is orthogonal and
can land anytime. Phase 7 (stdlib) wants phases 1–6 green so it
can be written against the full language surface, including
`Result` in `std.result` and `RuntimeError` in `std.error`.

Phase 8 (package manager) depends on phase 2 (modules) and the
existence of a stdlib (phase 7), but is otherwise orthogonal.

## How each phase gets shipped

Follow the pattern the execution-modes roadmap established:

1. Design note in this document (update the phase's scope block
   with concrete decisions).
2. Land the feature in the tree-walker first, with walker-only
   tests in `nybl/src/lib.rs`.
3. Extend the VM and the AOT transpiler.
4. Add the 2c differential test cases so walker + VM agree.
5. Add 3-way corpus entries where feasible.
6. Update `plans/general-purpose-roadmap.md` with a "Status: done"
   block under the phase (same pattern as `plans/execution-modes.md`).
7. Single commit to `main`, following the repo's direct-push
   style.

Safety / resource-limit semantics carry over unchanged — every new
feature must respect `NyblLimits` and go through the existing
tick / memory hooks in all three engines.
