# Language and runtime contract

## Purpose

This specification owns the observable contract shared by Nybl's tree-walker,
bytecode VM, AOT compiler, CLI, and embedding APIs.

## Requirements

- **RUN-001 — Embeddable core.** `nybl-lang` must provide an embeddable,
  dynamically typed language whose ambient capabilities are limited to those
  explicitly exposed by `NyblHost`.
- **RUN-002 — Sandbox termination.** A script that exceeds a runtime resource
  boundary or exercises adversarial input must halt with a Nybl diagnostic; it
  must not hang, panic, overflow the native stack, or abort the host process.
- **RUN-003 — Resource accounting.** Walker and VM execution, plus AOT output
  emitted with sandboxing enabled, must enforce their applicable step,
  tracked-memory, and call-depth boundaries. AOT sandboxing is opt-in; the CLI's
  compiled binaries are currently emitted without runtime limits. Operations
  that can amplify shared values into formatted strings or substring
  collections must preflight their exact bytes/cardinality before
  materialization and before exposing host output.
- **RUN-004 — Engine parity.** For the same source and host behaviour, the three
  engines must agree on language-visible values, output, mutations, errors, and
  module semantics, except for explicitly documented engine API differences.
  Resource checkpoints may occur at engine-specific times, but enabled limits
  must terminate cleanly rather than changing results or terminating the host.
- **RUN-005 — Core isolation.** Core language execution must not perform
  filesystem, network, clock, environment, or other OS I/O except through a
  host capability.
- **RUN-006 — Portable core.** `nybl-lang` and `nybl-vm` must remain usable in
  supported `no_std` and `wasm32-unknown-unknown` embeddings.
- **RUN-007 — Stable diagnostics.** Invalid syntax and runtime failures must
  produce actionable Nybl errors with source context when available; equivalent
  engine failures should retain the same error shape and helpful hints.
  Diagnostics originating in imported modules must identify the deepest owning
  module and render only that module's source; if its source is unavailable,
  they must omit the snippet rather than substituting root source.
- **RUN-008 — General-purpose language semantics.** Functions and closures,
  collections, user-defined types, pattern matching, control flow, iterators,
  and modules must compose according to the documented grammar and reference
  material.
- **RUN-009 — Correctness over silent truncation.** Resource guards and engine
  limitations must surface an error rather than silently changing a program's
  result.
- **RUN-010 — No silent mutation loss.** A mutating method must not report
  success when an unsupported receiver place would silently discard the
  mutation. Index and field receivers that cannot yet be written back must
  raise an actionable runtime error; genuine by-value temporaries may still be
  mutated and discarded intentionally.
- **RUN-011 — Container value semantics.** Assignment, argument passing,
  capture, and return preserve independent value semantics for arrays, dicts,
  structs, and enum payloads. Implementations may share backing storage until
  mutation, but changing one value must not change another. Iterators are the
  deliberate exception: cloned iterator handles share their cursor.
- **RUN-012 — Constant bindings are immutable assignment roots.** No assignment
  target whose base binding is a constant may mutate that value, including
  direct, index, field, compound, grouped, and syntactically nested place
  forms. A built-in mutating method cannot mutate an Array or Dict through a
  named constant receiver either. Reads through a constant, pure user-defined methods
  (including names that collide with built-in mutators), and writes through
  lowercase mutable bindings remain valid. A new `const` declaration is a
  declaration, not an assignment target.
- **RUN-013 — Exact signed integer literals.** Decimal integer source must map
  to exact signed 64-bit values without floating-point fallback. The minimum
  value `-9223372036854775808` is valid in expression and literal-pattern
  contexts; positive or larger magnitudes are rejected with source context,
  and arithmetic at either boundary remains checked.
- **RUN-014 — Typed embedding conversions.** The public Rust API must support
  documented, `no_std`-compatible conversions between `Value` and common Rust
  scalar and collection types. Fallible conversions must preserve integer
  range and Nybl type distinctions, enforce tracked-constructor depth limits,
  recognize only the canonical built-in `Result` shape, and identify a nested
  failure with expected/actual descriptions plus a root-to-leaf path.
- **RUN-015 — Stateful embedding.** Walker, VM, and sandboxed AOT embeddings
  must provide equivalent persistent-instance APIs. Loading runs the top level
  once; later host calls retain authoritative root/module bindings, imports,
  functions, callbacks, types, methods, and RNG state. One-shot APIs remain
  source-compatible and continue to start from fresh state.
- **RUN-016 — Explicit instance ABI.** Only final executed direct-root `pub fn`
  declarations are host-callable by name. Entry metadata must report final
  declaration order and arity; later declarations replace earlier ABI sites,
  and a later private declaration removes that name. Ordinary language lookup
  must not redirect the dedicated host entry table.
- **RUN-017 — Instance isolation and calls.** An instance borrows a host for one
  operation and never retains it. Function values are callable only by the
  instance that created them; same-instance re-entry is rejected, while a host
  may nest a different instance without crossing state or accounting.
- **RUN-018 — Persistent limits and failure state.** Step and call-depth state
  reset for each instance operation, while tracked memory belongs to the
  instance across calls. Errors unwind transient frames but do not roll back
  completed mutations. Step/depth failures leave an instance reusable; memory
  exhaustion leaves an instance reusable when unwinding releases the transient
  peak, but calls fail fast while retained charged storage remains over budget.
  Releasing enough charged storage to return to budget makes the instance
  callable again.
- **RUN-019 — Executed declarations.** Struct, enum, and method declarations
  take effect when execution reaches their source site. Dead sites have no
  runtime effect, nested type declarations obey lexical scope, and the last
  executed method declaration determines dispatch body and arity. Advisory
  match exhaustiveness follows source-ordered lexical type bindings and
  suppresses diagnostics when identity or control-flow provenance is
  ambiguous.
- **RUN-020 — Transactional `ref` parameters.** A user function may declare a
  second-class `ref` parameter and callers must mark the corresponding argument
  with `ref`. The target is one mutable field/index place rooted in an
  uncaptured `let` binding; constants, temporary roots, and duplicate root
  identities are rejected before the callee runs. Ordinary arguments evaluate
  left-to-right before place indexes are resolved and ref leaves are
  snapshotted in parameter order. Each ref
  value is staged in the callee and all targets commit only after a normal
  return and pending resource checks; a runtime or fatal error rolls every
  target back. Forwarding updates the outer staged local, while a ref parameter
  cannot be captured. Repeated parameter spellings denote one language binding:
  positional values rebind it in source order, every ref position reads its
  final value for commit, and any ref position makes that binding ineligible
  for closure capture. A normally returned language `Result::Err` is still a
  successful return and commits. Host-to-script `NyblInstance::call` and
  `call_value` remain value-only and reject ref-bearing callables before
  execution because host values do not identify Nybl bindings.
- **RUN-021 — Unified mutating receivers.** A built-in mutating method (the
  Array mutators and Dict `.remove` / `.clear`) on a
  mutable field/index place rooted in a `let` binding uses the same snapshot/commit model
  implicitly, with method arguments evaluated before the receiver snapshot.
  True temporary receivers mutate and discard their owned value while
  preserving the method's ordinary result. Nested places rebuild and commit
  their candidate root atomically. A user-defined method opts into the same transactional receiver
  model with `ref self`; method syntax supplies the call-site reference
  implicitly. Its receiver must be a mutable place, joins explicit ref
  arguments in one atomic transaction, and is snapshotted after ordinary
  arguments. An ordinary value receiver is read-only, and mutation through it
  is rejected during parsing with a `ref self` hint.
- **RUN-022 — Rest parameters.** A final `..name` parameter on a named
  function, function expression, or method collects zero or more remaining
  positional values into one tracked array. It is value-only and
  declaration-only; fixed value or ref parameters may precede it. Arity and
  argument modes are validated before expression side effects, and persistent
  entry-point metadata reports the fixed count as a variadic minimum.
- **RUN-023 — Explicit module surfaces.** One or more direct module-root
  `pub { ... }` statements form an export allow-list for values, functions,
  types, aliases, and re-exports. An empty list exports nothing; listed private
  names are public. Private implementation bindings remain lexically available
  to exported callables but are not capabilities of an imported module value.
  Modules with no surface retain legacy underscore/selective/alias behavior,
  and a surface statement in the directly executed root is inert.
- **RUN-024 — Opaque host values.** A host can place an identity-comparable,
  type-named opaque Rust handle in `Value`. Nybl cannot inspect or own its
  payload; display is `<host TYPE>`, ownership depth and tracked memory cost are
  zero, and methods dispatch through `NyblHost::call_method` after universal
  value methods. Arguments are value-only and host-side effects are outside
  Nybl transaction rollback. Persistent instances retain handle values but do
  not extend the lifetime of the host object used for a call.

## Acceptance criteria

- **AC-RUN-001:** A custom host exposing no functions cannot access ambient OS
  facilities, while a host-provided function is callable through `NyblHost`.
- **AC-RUN-002:** Programs that exceed step, memory, call, parse, or safe value
  processing boundaries return `Err(NyblError)` or another documented clean
  termination without terminating the embedding process.
- **AC-RUN-003:** The differential suites cover representative successful and
  failing programs and report no walker/VM/AOT semantic, output, or diagnostic
  drift outside documented resource-checkpoint differences.
- **AC-RUN-004:** Core and VM builds succeed for the supported standard,
  `no_std`, and WASM configurations documented by the project.
- **AC-RUN-005:** Parser, runtime, and CLI errors identify the real failure and
  do not replace I/O, binding, or limit failures with misleading results.
  Walker, VM, AOT, and CLI diagnostics for direct and transitive module parse
  failures identify the owning module, retain line/column/hints, and never
  clamp a caret against root-file text.
- **AC-RUN-006:** Cloning a container handle does not charge a second backing
  allocation; unique mutation does not copy its backing storage; the first
  mutation of shared storage detaches exactly once; and tracked storage is
  released when its last owner drops.
- **AC-RUN-007:** Parser and cross-engine tests reject direct and compound
  Array, Dict, and Struct writes rooted at constants with the canonical
  constant diagnostic and hint, including grouped/nested targets. Cross-engine
  and native AOT tests reject built-in Array mutators and the mutating Dict
  methods on
  named constants after receiver-aware dispatch, preserve pure user-defined
  name collisions and ordinary non-collection method errors, and keep the
  corresponding mutable-binding programs identical.
- **AC-RUN-008:** Reserved-word binding diagnostics derive from the lexer's
  current keyword vocabulary, including `const`, while keyword-shaped text in
  strings and comments remains ordinary source content. The compatibility
  `precheck::check` API retains its narrow `let` / named-`fn` contract without
  maintaining a second keyword list.
- **AC-RUN-009:** Lexer/parser, walker/VM differential, and native AOT tests
  cover `i64::MIN` expressions and patterns, exact integer type/value
  preservation, unary-minus and subtraction precedence, checked overflow, and
  rejection of one-beyond magnitudes on both signs with line and column.
- **AC-RUN-010:** Public conversion tests cover scalar boundaries, borrowed and
  owned extraction, recursive arrays/options/results/deterministic maps,
  canonical built-in `Result` identity, nested error paths, macro hygiene,
  depth-limit failure, and a real `NyblHost` call. Standard, `no_std`, and WASM
  checks compile the same conversion surface.
- **AC-RUN-011:** Walker, VM, and sandboxed native AOT tests load the same
  public-entry program, report identical entry names/arities, preserve state
  across named and callback calls, and reject wrong-instance functions and
  same-instance re-entry with equivalent diagnostics.
- **AC-RUN-012:** Instance tests prove that hosts are borrowed per operation,
  different instances can nest safely, step/call-depth budgets restart,
  memory accounts remain independent and persistent, transient frames unwind,
  transient memory peaks do not poison later calls, retained over-budget
  receipts fail fast until released, and mutations before ordinary or fatal
  failures remain visible.
- **AC-RUN-013:** Parser and cross-engine tests cover root-only `pub fn`, final
  public/private redeclaration rules, top-level early return, and immunity of
  the public entry table to ordinary-name reassignment.
- **AC-RUN-014:** Cross-engine declaration tests cover direct, branch, loop,
  function, lambda, and dead-code type/method sites, while checker tests cover
  source order, lexical frames, imported identities, shadowing, and ambiguous
  control flow without false-positive exhaustiveness warnings.
- **AC-RUN-015:** Plain-glob value/function collisions remain first-definition
  wins and emit the same runtime warning on stderr in the walker, VM, and both
  native AOT modes. Selective and aliased imports, private names, absent
  conditional exports, stdout, and binding/source order are unaffected.
- **AC-RUN-016:** Parser, checker, walker/VM differential, and native AOT tests
  cover explicit marker agreement through direct and first-class callables,
  deterministic preflight/evaluation order, mutable/grouped target acceptance,
  const/index/field/captured/duplicate target rejection, forwarding and
  no-capture rules, multi-target commit, and rollback on ordinary/fatal
  `NyblError` paths including errors caught by `try_call`. The engines also
  agree that a normally returned `Result::Err` commits and that pending
  resource-limit failures happen before commit. Walker, VM, and sandboxed AOT
  instance APIs reject ref-bearing entries and callback values consistently
  before executing them.
- **AC-RUN-017:** Cross-engine tests cover named and grouped implicit-ref
  mutating receivers, argument-before-snapshot order, true-temporary method
  results, the line-aware unsupported index/field receiver diagnostic, and
  user-defined `ref self` commit/rollback, invalid and duplicate receiver
  fences, receiver-plus-ref-argument atomicity, and value-receiver mutation
  diagnostics.

## Design notes

The grammar reference and user documentation under `docs/content/docs/` remain the
canonical teaching material. This file owns cross-engine guarantees rather
than duplicating syntax documentation.

An inert or custom `NyblHost` is capability-sandboxed by default. `nybl-sys` and
the CLI deliberately grant selected OS capabilities and therefore are not an
ambient-authority sandbox, even when language resource limits are enabled.
