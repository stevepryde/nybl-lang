# Nybl Execution Modes & Compilation Strategy

Status: design / roadmap. Steps 1 (`nybl-sys`), 2 (bytecode VM with
differential harness), and 3 (AOT-Rust transpiler with sandbox
mode and three-way differential harness) have all landed. Nybl now
ships three execution modes: tree-walker, bytecode VM, and AOT
Rust, with a shared differential harness proving they agree.
JIT stays out of scope. `nybl-lang` stays self-contained — the VM
and AOT crates depend on it directly for `Value` / builtins /
operator primitives, rather than a separate runtime crate.

## Summary

Nybl currently ships as a tree-walking interpreter. This document captures
the roadmap for additional execution modes and the supporting crate
reorganisation.

Decisions:

- **Yes** to `nybl-sys`: a separate crate for standard host / OS integration.
- **No** to a separate `nybl-runtime` crate. `nybl-lang` stays
  self-contained (no internal crate deps) so it publishes cleanly on
  crates.io. The "shared runtime surface" the VM and AOT need —
  `Value`, `NyblError`, memory tracking, builtins, methods, operator
  primitives — lives in `nybl-lang`'s public API (exposed through the
  `nybl::value`, `nybl::error`, `nybl::memory`, `nybl::ops`, `nybl::builtins`,
  `nybl::methods` modules). VM / AOT crates depend on `nybl-lang` directly.
- **Yes** to a **Bytecode VM** as its own crate from day one
  (`nybl-vm`), dependency-light, WASM-capable.
- **Yes** to **AOT Rust** transpilation (`nybl-compile`): Nybl source →
  Rust source → native binary.
- **No** to JIT: out of scope.

Package name vs import name: `nybl-lang` is the Cargo package; `nybl` is
the lib name used in `use` paths. Keep both — the `nybl` import name is
shorter and already shipped. Plan text uses "`nybl-lang`" when referring
to the package / crate and "`nybl::`" when referring to imports.

## Execution modes

### 1. Tree-walking interpreter (existing)

Today's implementation. Source → tokens → AST → direct evaluation via
`evaluator::Evaluator`. Stays as the default.

- Small, simple, and good for short scripts, the REPL, and learning.
- Keeps the `no_std` / embedded story clean.
- Resource limits (`NyblLimits`) apply here first and foremost.

Status: shipping. No changes planned beyond bug fixes and new language features.

### 2. Bytecode VM (new)

A stack-based VM that compiles the AST to a compact bytecode and executes
it. Faster than tree-walking for hot loops and longer programs. Useful
when Nybl is embedded in a game loop or serves as a scripting target for
workloads larger than a REPL one-liner.

Constraints:

- **Separate crate** (`nybl-vm`) that depends on `nybl-lang` for both
  the AST and the runtime surface (`Value`, ops, builtins, memory
  tracking). Keeps the tree-walker path unaffected and avoids a
  growing `#[cfg]` forest inside `nybl-lang`.
- **Minimise dependencies.** The VM is pure Rust — no LLVM, no Cranelift,
  no external runtime. At most, small well-vetted crates, and only if
  strictly needed.
- **`no_std` compatible.** Same story as the interpreter: works under
  `alloc` only.
- **WASM support is nice-to-have, not essential.** If a genuine tradeoff
  appears between WASM compatibility and VM performance / clarity, prefer
  performance. WASM should come mostly for free by keeping dependencies
  minimal and avoiding host-specific code in the VM itself.
- **Same resource-limit semantics** as the tree-walker, reinterpreted
  in VM terms:
  - **Step counting**: per bytecode instruction dispatched, with a
    per-op cost table (most ops = 1; calls / long-running ops can
    charge more). A check at every loop backedge and at call entry
    guarantees infinite loops halt. `NyblLimits::max_steps` stays the
    user-facing knob; the VM scales it internally if a source-level
    step maps to several bytecode ops, so `standard()` / `demo()`
    remain meaningful without retuning.
  - **Memory tracking**: all `Value` allocations (strings, arrays,
    dicts) route through `nybl::memory`'s accounting hook, shared with
    the tree-walker. The VM must not create `Value::Str` / `Value::Arr`
    directly — it goes through `Value::new_*` constructors so the
    memory ceiling is enforced uniformly.
  - **Host calls**: `NyblHost::on_tick` fires at the same cadence as in
    the tree-walker (at least once per loop backedge and call entry),
    so timeouts and cancellation work identically.

Explicitly out of scope (for v1):

- Register-based VM (stick with stack-based — simpler).
- NaN-boxing / fancy value representation.
- Stable on-disk bytecode format. Bytecode is an implementation detail;
  users should not rely on a `.nyblc` file they can ship separately.
- Debugger protocols, profilers, inline caches. Nice to have, not in scope.

### 3. AOT Rust transpiler (new)

A separate tool that converts a Nybl program into **Rust source code**. The
user then compiles that Rust with `rustc` / `cargo` to produce a
standalone native binary.

Why Rust output (and not C, LLVM IR, WASM, etc.):

- Lets Nybl target anywhere Rust already targets, with no new backend work.
- After `rustc` gets hold of it, hot code gets real optimisation.
- No runtime VM to embed — the output is just Rust that links against
  `nybl-lang` for language-level builtins and `nybl-sys` for host calls
  (when the program uses them).
- The toolchain assumption (`cargo`/`rustc` installed) is acceptable
  because this path is for users who want a native binary; they're
  already in Rust territory.

Shape:

- Lives in a new crate, `nybl-compile`, invoked by `nybl-cli` (e.g.
  `nybl build foo.nybl`).
- Emits **human-readable Rust**, not obfuscated codegen — easier to
  debug, easier to audit, easier to hand-tune if someone wants to.
- Generated code depends on `nybl-lang` for `Value`, operators, and
  built-in methods (so we don't reimplement `range`, `str`, `split`,
  etc. twice), and on `nybl-sys` for host-backed builtins when the
  program uses them.
- Resource limits become optional at this layer, off by default — the
  hot path should be clean. Behind a `--sandbox` flag, the transpiler
  emits step-count checks at loop backedges / function entry and
  `nybl::memory`'s allocation hooks enforce the memory ceiling. Without
  `--sandbox`, the output is straight-line Rust with no accounting
  overhead.

Non-goals for AOT v1:

- Emitting C or C++. Rust only.
- Direct native codegen (skipping the `rustc` step). That's JIT-adjacent
  and out of scope.
- Cross-compiling for the user. They drive `cargo build --target=…`
  themselves.

### 4. JIT — rejected

Explicitly out of scope. A JIT would require either:

- Embedding LLVM (huge dependency, slow compile, complex build), or
- Cranelift (smaller, but still a sizeable dep and platform-specific).

Neither is worth it for Nybl's intended use cases. AOT-Rust covers the "I
want native speed" story. The bytecode VM covers "I want faster than
tree-walking but still embeddable." JIT sits awkwardly between the two,
carrying a large dependency cost for modest incremental value.

If the need ever becomes real, reopen the discussion. For now: no.

## The `nybl-sys` crate

A new crate alongside `nybl-lang` that provides standard host / OS
integration: file I/O, env vars, time, stdin, etc. — the things the core
language deliberately stays agnostic of.

Status: crate split implemented. `nybl-sys` now provides the standard
stdout-backed host used by `nybl-cli`, plus stdin, file, env, and time host
functions.

Rationale:

- Keeps `nybl-lang` core **pure**: no I/O deps, no platform assumptions,
  stays clean for `no_std` and embedded use.
- Gives the AOT-Rust output a well-defined runtime surface to link
  against, instead of inlining builtins into generated code.
- Gives embedders a clear "give me the standard host" import without
  dragging I/O and std assumptions into the interpreter crate.

Shape:

- Implements `NyblHost` with a standard set of builtins (print, readline,
  file ops, time, env…).
- Intentionally not feature-flagged internally. Embedded/minimal use cases
  depend on `nybl-lang` directly; users that choose `nybl-sys` get the full
  standard host.
- Re-used by `nybl-cli` so the `nybl` binary keeps its current behaviour
  without duplicating code.

## Target crate layout

```
nybl-lang     core: lexer, parser, AST, tree-walking evaluator, NyblHost trait,
             Value, operator primitives (nybl::ops), builtins, methods,
             memory accounting. Self-contained — no internal crate deps.
nybl-vm       bytecode compiler + stack VM  (depends on nybl-lang for AST,
             Value, ops, memory tracking; dep-light, no_std-capable)
nybl-sys      standard host: I/O, time, env, stdin  (implements NyblHost; std-only)
nybl-compile  AOT: Nybl source → Rust source  (emits code that depends on
             nybl-lang and optionally nybl-sys)
nybl-cli      user-facing CLI: run, repl, build
```

The VM and AOT-Rust output share `nybl-lang`'s runtime surface:
`nybl::value::Value`, `nybl::ops` (operator primitives as pure
functions), `nybl::memory` (allocation tracking), `nybl::builtins`, and
`nybl::methods`. Without this single source of truth, each engine would
reinvent coercion rules and drift.

`nybl-vm` is a separate crate from day one, not a feature inside
`nybl-lang`. Rationale: `nybl-lang` stays minimal and `no_std`-clean;
embedders that never want the VM don't pay compile cost for it; the VM
can iterate without touching `nybl-lang`'s public surface. Feature-flag
splits are easy to add later and painful to remove after publication.

## Phasing

Rough order of work (each step is independently shippable):

1. **Split `nybl-sys` out** of `nybl-cli` / `nybl-lang`. Lowest risk;
   unblocks later work. Pure refactor from a user's point of view.
   *Status: done.*

1b. **Expose the runtime surface as public API in `nybl-lang`.** Promote
    the operator primitives (previously inlined in `evaluator::binary_op`)
    into a `nybl::ops` module (`add`, `sub`, `mul`, `div`, `rem`, `eq`,
    `not_eq`, `lt`, `gt`, `lt_eq`, `gt_eq`, `neg`, `not`, `index_get`,
    `index_set`) as pure functions over `Value`. Keep `Value`,
    `NyblError`, `memory`, `builtins`, `methods` where they are —
    `nybl-lang` stays self-contained. The VM and AOT crates will
    depend on `nybl-lang` directly for this surface. `NyblError::runtime`
    is the canonical error constructor.
    *Status: done.*

2. **Bytecode VM** as a new `nybl-vm` crate. Must not change tree-walker
   behavior. Ships in three sub-steps, each independently mergeable:

   - **2a. Compiler.** AST (from `nybl-lang`) → bytecode. Stable
     instruction set, stack-based, documented in the crate. No
     execution yet — round-trip tests (compile → disassemble) only.
     *Status: done. Crate: [`nybl-vm`](../nybl-vm). Instruction set
     and pool layout documented in `nybl-vm/src/lib.rs`. 25
     round-trip tests in `nybl-vm/tests/compile_roundtrip.rs` pin the
     emitted shape for every language feature (literals, variables,
     operators, short-circuit, if/else chains, while/for/repeat with
     break/continue, methods with mutating back-assign, functions,
     nested functions, string interpolation, dicts).*
   - **2b. VM + limits.** Dispatch loop, step counting per-op with a
     cost table, loop-backedge / call-entry checks, `on_tick` hooks,
     memory tracking routed through `nybl::memory`. At the end of 2b
     the VM passes the full `nybl-lang` test suite on its own.
     *Status: done. Implementation: [`nybl-vm/src/vm.rs`](../nybl-vm/src/vm.rs).
     Stack-based dispatch with per-frame scopes, `Rc<Chunk>` for
     function sharing, and iteration/repeat slots kept inline on the
     value stack. `nybl::memory` tracks allocations automatically via
     `Value`'s `Clone` / `Drop`, so no VM-specific bookkeeping is
     needed for `max_memory`. `max_steps` is scaled internally
     (`STEP_SCALE = 8`) so source-level budgets survive the 1-op-per-
     instruction expansion. `nybl::builtins` and `nybl::methods` are
     promoted to public modules so the VM can share the tree-walker's
     builtin / method implementations. Tested in
     [`nybl-vm/tests/semantics.rs`](../nybl-vm/tests/semantics.rs)
     (117 tests mirroring `nybl-lang`'s suite, including safety /
     resource-limit cases).*
   - **2c. Differential harness.** Every test in the suite runs
     against both engines; outputs and error messages must match.
     Fuzzing layered on top (random programs from a constrained
     grammar, compare outputs). Promoted from "nice to have" — this
     is how we keep the engines from drifting. Required before
     shipping the VM.
     *Status: done. Lives in
     [`nybl-vm/tests/differential.rs`](../nybl-vm/tests/differential.rs).
     Every corpus program runs through both `nybl::run` and
     `nybl_vm::run`; the harness asserts strict print equality and
     error-message equality (line numbers legitimately diverge, so
     they're excluded). A constrained-grammar fuzzer layered on top
     (`let` / assign / `print` / `if` / `repeat` / arrays /
     arithmetic / logic; no `while`, `fn`, or methods) runs 100
     deterministic programs on every build and 10k under
     `cargo test -- --ignored fuzz_extended_diff`. One loosened
     check, `run_err_loose` / `assert_both_resource_limit`, covers
     the handful of safety tests where the engines legitimately
     halt on different limit classes (step vs memory).*

3. **AOT-Rust transpiler** (`nybl-compile`). Depends on `nybl-lang` for
   the AST and runtime surface. Start with a subset (numbers, strings,
   arrays, dicts, functions, control flow, common builtins). Extend
   the differential harness from 2c to run tests through the
   transpiled path too; grow the subset until it reaches parity.
   *Status: v1 complete. Crate:
   [`nybl-compile`](../nybl-compile). `transpile(source, opts) ->
   String` parses Nybl via `nybl-lang` and emits Rust that depends
   on `nybl` for `Value` / `ops` / `builtins` / `methods` and (when
   `Options::emit_main` is set) on `nybl-sys` for the standard
   host. Supported: all literals, binary/unary ops (including
   short-circuit `&&` / `||`), plain-variable `let` / assign /
   compound-assign, `if` / `if-expr`, `while`, `repeat`, `for-in`
   (arrays / ranges / strings), `break` / `continue`, user
   functions with recursion, built-in calls, arrays, dicts, index
   reads, method calls with mutation back-assign, string
   interpolation, and indexed writes. Resource limits are opt-in
   via `Options::sandbox`: when enabled, the emitted code
   enforces `NyblLimits` by emitting a `__nybl_tick(ctx, line)?`
   checkpoint at every loop iteration and function entry, wiring
   `max_memory` into `nybl::memory`'s allocation tracker, and
   firing `NyblHost::on_tick` at every checkpoint. 29 snapshot
   tests pin the emitted shape; 16 `#[ignore]`-gated e2e tests
   run the transpiled Rust through `cargo run` and assert either
   output matches the tree-walker (non-sandbox) or the program
   exits with the expected limit-violation message (sandbox).
   A three-way differential harness
   ([`nybl-compile/tests/three_way.rs`](../nybl-compile/tests/three_way.rs))
   runs every corpus program through all three engines — walker,
   VM, AOT — and asserts strict agreement on prints and on error
   messages. AOT programs are batched into a single Rust driver
   file via `Options::module_name` so one `cargo run` covers the
   entire corpus; `#[ignore]`-gated to keep default `cargo test`
   fast (the AOT leg adds ~1–2 s per invocation). Safety tests
   that trip resource limits are deliberately excluded from the
   three-way corpus — the three engines measure "steps"
   differently and halt at slightly different points, a
   divergence already accepted by 2c's `assert_both_resource_limit`.
   Two other intentional AOT divergences are documented inline in
   the corpus: short-circuit with unbound idents
   (`false && x`) and undefined-variable lookups (`print(nope)`)
   — both halt with a useful error in all three engines, but the
   message text only matches for the first two since the AOT
   catches unbound names at rustc time.*

JIT is not on the roadmap. If that changes, open a new design doc.

## Non-goals

- Breaking the tree-walker API. The current public surface (`run`,
  `NyblHost`, `NyblLimits`, `Value`, `NyblError`) stays stable.
- Stable on-disk bytecode format. Bytecode is internal.
- Multi-language AOT targets. Rust output only for v1. C / C++ / WASM
  could come later but are not on the near-term plan.
- `no_std` support for `nybl-sys`. `nybl-sys` is std-only by design —
  it exists to provide OS-backed I/O, time, and env. `no_std`
  embedders depend on `nybl-lang` (and later `nybl-vm`) directly and
  supply their own `NyblHost`.
- JIT. See above.

## Open questions

- What's the smallest useful subset of the language for AOT v1? Probably:
  numbers, strings, arrays, dicts, functions, control flow, the common
  builtins. Closures can come later if the surface allows them cleanly.
- Exact VM per-op cost table. Straw-man: most ops = 1, `call` = 1 +
  arg-count, allocation ops charged on the value they build. Needs
  calibration against the tree-walker so `NyblLimits::standard()` keeps
  roughly the same script wall-clock ceiling.
- How does `nybl::memory` expose accounting to external engines
  without leaking internals? Today it uses thread-local / static
  counters via free functions (`nybl_memory_init`, `nybl_alloc`,
  `nybl_dealloc`). Revisit if we want per-engine isolation: likely a
  small `MemoryTracker` type that both the tree-walker and VM hold a
  reference to.
