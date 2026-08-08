# Changelog

All notable changes to Nybl are documented here. Versions apply to the
publishable workspace crates unless a section says otherwise.

## Unreleased

## 0.4.3

`0.4.3` improves persistent embedding, cross-engine consistency, shared VM
artifacts, and wasm portability while adding evidence-driven dispatch and
cached-instance benchmarks.

### Embedding

- Add `nybl_vm::CompiledScript`: compile a program once (no host needed, no
  execution) into an immutable `Send + Sync`, Arc-backed artifact, then
  create any number of VM instances from it with
  `NyblInstance::from_compiled(&program, host, limits)`. Instantiation never
  re-parses, re-compiles, or deep-clones — all instances execute the shared
  chunk storage — and `NyblInstance::load` is now exactly `compile` +
  `from_compiled`, so both paths behave identically. Instances stay
  single-threaded (`!Send`); the supported cross-thread pattern is
  create-on-worker from a shared artifact (compile once, clone the artifact
  into N worker threads, instantiate per worker). Determinism, re-entry
  guards, callback affinity, and per-instance limits — including
  `disabled_builtins`, which is enforced per `from_compiled` against usage
  data stored in the artifact — are unchanged. The compiled chunk graph now
  uses `Arc` instead of `Rc` internally (`FnDef::chunk`,
  `InterpRecipe::parts`, `PatternRecipe::pattern`). Plain `compile` preserves
  lazy per-instance module resolution, while the opt-in
  `CompiledScript::compile_with_modules` resolves, parses, compiles, validates,
  and builtin-indexes the complete transitive module graph once. Instances
  share its Arc-backed module chunks without runtime source resolution while
  retaining fresh module globals, imports, callable identity, limits, resource
  accounting, and diagnostics.
- Add `NyblLimits::disabled_builtins` (and `nybl_compile::Options::
  disabled_builtins` for the AOT engine): a host-configured deny list for
  engine builtins, built for deterministic simulation hosts that must route
  all randomness through their own seeded RNG. A definite reference to a
  disabled builtin is a fatal error at load/transpile time; references that
  static analysis cannot prove (a shadowing binding may apply) raise the same
  fatal, `try_call`-proof error the moment they would invoke the builtin.
  Consistent across the walker, VM, and AOT engines; imported modules are
  checked when they load.
- Align callable shadowing across the walker, VM, and AOT engines: an executed
  user function declaration now shadows an engine builtin of the same name,
  while calls before that declaration retain source-ordered builtin behavior.
  Disabled-builtin checks follow the same rule, so a lexical replacement such
  as a host-controlled `rand` function remains valid.
- Add instance-affine `PreparedEntry` handles plus `call_prepared` and
  `call_batch` to the walker and VM embedding APIs. Host batches reuse one live
  engine while resetting step and call-depth accounting per item; script-level
  batch entry points provide the largest measured game-tick improvement while
  preserving ordinary host dispatch and sandbox semantics.
- Benchmark 100 distinct persistent script instances with a consumer-owned,
  entity-binding host. VM instances share one VM-only `CompiledScript`; walker
  instances and all prepared entries are likewise created outside the measured
  frame loop. The host-heavy workload still favors the walker, while an empty
  callback confirms the VM has the lower cached-instance entry floor.
- Make `nybl-sys` clock calls safe on freestanding wasm:
  `unix_time()` and `unix_time_ms()` now return an actionable Nybl runtime
  error on `wasm32-unknown-unknown` instead of trapping in
  `SystemTime::now()`; native and WASI hosts retain real wall-clock time.
- Execute the wasm parity corpus in CI with both the default `std` math
  backend and the deterministic defaults-off `no_std`/pure-Rust `libm`
  backend. Both configurations run through the walker and VM on native and
  `wasm32-wasip1` and are byte-compared; a deliberate-divergence negative
  control proves the comparison can fail.

### Tooling

- Reject unresolved merge-conflict markers in tracked text during CI. The
  guard scans Git-tracked files only, ignores binary data, and carries a
  deliberately invalid Markdown fixture to prove the failure path without
  matching its own implementation.

## 0.4.2

`0.4.2` rounds out the built-in collection surface with mutating dict methods
and in-place array shrinking, consistent across the walker, VM, and AOT
engines.

### Language

- Add dict `.remove(key)`: removes a key and returns its value, or `none` when
  the key is absent. It follows the same transactional write-back, constant
  rejection, and nested-place commit rules as the built-in array mutators
  across all three engines, and removal never raises a memory error — the key
  index shifts in place without allocating.
- Add array `.truncate(n)`: shortens the array to at most `n` elements in
  place. Negative lengths count from the end like a `.slice()` bound;
  already-short arrays are untouched.
- Add `.clear()` on arrays and dicts: removes every element/entry in place
  under the same mutating-method rules, so it also works through `ref`
  parameters and `ref self`, where reassigning the callee's binding would not.

## 0.4.1

`0.4.1` is the first coordinated release after `0.3.0`. It expands Nybl from a
small one-shot scripting runtime into a module-aware language with persistent
embedding APIs across all three execution engines.

### Language

- Replace `import` with four `use` forms: glob, selective, aliased, and
  selective-plus-aliased imports.
- Add module-qualified struct and enum construction/patterns, declaration
  origin tracking, live module bindings, and transitive re-exports.
- Add explicit `pub { ... }` module export allow-lists while preserving legacy
  underscore/selective behavior for modules that do not declare one.
- Add `const` and enforce lowercase value names, ALL_CAPS constants, and
  UpperCamel-style type/variant names at parse time.
- Add explicit second-class `ref` parameters to user-defined functions,
  method parameters, and method receivers. `ref self` updates a mutable
  field/index place rooted in a `let` binding; ordinary `self` is read-only and
  mutation through it is a parse error. Calls use transactional
  copy-in/copy-out: distinct mutable
  targets commit together after a normal return and roll back together on
  runtime or resource errors.
- Add final `..rest` parameters to named functions, closures, methods, and
  variadic persistent entry points.
- Extend assignment, explicit `ref`, built-in array mutation, and `ref self`
  write-back through arbitrarily nested field/index places.
- Restore `//` line comments. Integer division now uses
  `(left / right).to_int()` because `/` always returns `number`.
- Move introspection, conversion, collection, string, and math operations to
  methods such as `.type()`, `.to_str()`, `.to_int()`, `.len()`, and `.sqrt()`.
- Add the built-in `Result` and `Iter` types, `Ok(...)` / `Err(...)`
  shorthand, Result combinators, `try_call`, `panic`, universal
  `.is_none()` / `.is_some()`, and the lazy `.iter()` / `.next()` protocol.
- Make a missing dictionary key evaluate to `none`.
- Support multiline expressions inside `()` and `[]`, leading-dot
  continuations, first-class closures, match guards, namespaced patterns, and
  declaration-aware exhaustiveness diagnostics.
- Accept the full signed `i64` literal range, including
  `-9223372036854775808`.

### Embedding and Rust APIs

- Add `nybl::NyblInstance` and `nybl_vm::NyblInstance`. A program is loaded once,
  then direct root-level `pub fn` entries can be called while globals,
  modules, callbacks, types, methods, and RNG state remain live.
- Add the equivalent generated `NyblInstance` API to sandboxed AOT library
  output.
- Add strict, path-aware Rust ↔ `Value` conversion through `IntoValue`,
  `FromValue`, `Value::to_rust`, and `nybl_value!`.
- Add opaque, identity-based `HostValue` handles and host method dispatch via
  `NyblHost::call_method`.
- Add in-memory module helpers in `nybl::host`.
- Move the Nybl standard library into `nybl-lang` behind the default `nybl-std`
  feature. The old standalone `nybl-std` crate is no longer needed.
- Make Rust standard-library integration an additive-safe default `std`
  feature. Genuine no_std builds use `default-features = false` with
  `features = ["no_std"]`; if Cargo unifies both features, `std` wins.
- Replace ambient engine memory accounting with an explicit per-instance
  context across the walker, VM, modules, and generated sandbox runtime.
  Legacy `nybl_memory_*` hooks are now std-only; custom no_std integrations
  must use the context-aware internal runtime APIs. Host-created `Value`s are
  untracked until an engine mutation adopts their backing allocation.
- Improve imported-module diagnostics so parse and runtime failures render
  against the source and module that owns the error.

### Engines and tools

- Bring the bytecode VM and AOT transpiler to language parity with the
  tree-walker, covered by an expanded three-engine differential suite.
- Set Rust 1.88 as the minimum supported Rust version for the complete
  workspace, including `nybl-cli`.
- Add public bytecode validation through `nybl_vm::validate_chunk`.
- Add copy-on-write runtime containers, in-place VM mutation paths, compact
  instruction pools, safe superinstructions, and allocation reductions.
- Add a persistent, multiline REPL with expression echo, tab completion,
  history, `:vars`, `:reset`, and non-TTY transcript support.
- Keep `nybl run` on the VM by default, with `--novm` for the tree-walker, and
  support native builds or Rust-source emission through `nybl compile`.

### Safety and diagnostics

- Add source columns and caret rendering to more parse/runtime diagnostics.
- Add targeted naming, match-arm, range, module, shadowing, and `try` hints.
- Harden parser/runtime nesting, range sizes, copy-on-write mutation,
  constant containers, VM scope unwinding, jump targets, and malformed
  bytecode handling.
- Align warnings, resource-limit failures, module errors, and observable
  behavior across walker, VM, and AOT execution.

### Migration from 0.3

- Change dependency requirements from `"0.3"` to `"0.4"`.
- Replace `import path` with `use path`.
- Replace `# comment` with `// comment`.
- Replace removed global helpers such as `type(x)`, `str(x)`, `int(x)`,
  `float(x)`, and `len(x)` with methods on the value.
- Replace `a // b` integer division with `(a / b).to_int()`.
- Remove a standalone `nybl-std` dependency; enable `nybl-lang`'s `nybl-std`
  feature instead (it is enabled by default).

### Publishing order

The crates use `0.4.1` requirements for workspace dependencies and should be
published in dependency order:

1. `nybl-lang`
2. `nybl-sys`
3. `nybl-vm`
4. `nybl-compile`
5. `nybl-cli`

Wait for each package to become available in the crates.io index before
publishing a dependent package.

Before publishing, verify that the pinned release dependency graph builds on
the minimum supported toolchain and that every crate archive includes both
canonical license texts:

```sh
cargo +1.88.0 check --workspace --all-targets --locked
bun run release:check-licenses
```
