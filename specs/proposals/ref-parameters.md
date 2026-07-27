# Design record: second-class `ref` parameters

> **Status: implemented.** This record preserves the accepted design and
> implementation criteria. The normative runtime contract is
> `RUN-020`/`AC-RUN-016` in `specs/language-runtime.md`; the grammar and user
> documentation under `docs/content/docs/` own the public syntax and teaching
> material.

Tracking issue: [#40](https://github.com/stevepryde/nybl-lang/issues/40).

## Purpose

This proposal defines explicit, second-class `ref` parameters for callers that
want a function to replace a mutable variable's value. The model is
copy-in/copy-out (call-by-value-result), not observable aliasing. It preserves
Nybl's value semantics while making mutation intent visible at both sides of a
call.

```nybl
fn grow(ref items, n) {
  repeat n { items.push(0) }
}

let xs = []
grow(ref xs, 3)
// xs is [0, 0, 0]
```

The requirements below are the design contract used to implement the feature.
The canonical specifications and teaching documentation summarize the shipped
behaviour.

## Design contract

- **REF-001 — Explicit modes.** A `ref` parameter must be marked with `ref` in
  both the function parameter list and its positional argument. Omitting a
  required marker or adding one for a value parameter must be an actionable
  error. Callable values must retain parameter-mode metadata so this check also
  works through aliases, closures, imports, and other dynamic call paths.
- **REF-002 — Copy-in/copy-out.** Each `ref` parameter receives a staged value
  copied from its caller target. Reads and writes in the callee affect that
  staged local. The caller binding is not aliased during execution. Repeated
  parameter spellings share one language binding and are rebound by positional
  arguments in source order. Every ref position copies the final shared value
  back to its own target, and any ref position marks the shared binding as a
  ref parameter for the closure-capture fence.
- **REF-003 — Transactional return.** A call must commit all of its staged
  `ref` values to their caller targets only after a normal Nybl return, including
  implicit return at the end of a function, and after all pending resource-limit
  checks have passed. A final operation that crosses a step, memory, or other
  sandbox limit turns the return into a `NyblError` before commit. Commit is
  all-target: no target may become observably updated unless every target can be
  committed.
- **REF-004 — Error rollback.** A call that exits with a runtime or fatal
  `NyblError` must discard every staged `ref` value. This rollback must happen
  before a non-fatal error is converted to `Result::Err` by `try_call`; fatal
  errors continue to propagate after rollback. A language-level `Result::Err`
  returned as a value is a normal return and therefore commits. The same is true
  when Nybl's `try` operator normally returns an `Err` value from the function.
- **REF-005 — Deterministic call entry.** A call must use this sequence:

  1. evaluate the callee expression exactly once;
  2. verify that the result is callable and preflight arity, positional modes,
     and every statically classifiable target fence;
  3. evaluate ordinary argument expressions from left to right;
  4. resolve and snapshot `ref` targets in parameter order; and
  5. enter the callee.

  A callee-expression side effect therefore happens even when preflight fails,
  but preflight failure prevents every argument-expression side effect. A
  target resolution or snapshot failure happens after ordinary arguments but
  still enters no callee and commits nothing. Side effects in ordinary
  arguments are visible in the copied-in values.
- **REF-006 — Second-class targets.** Initially, an explicit `ref` argument must
  name one mutable plain-variable binding. Constants, expressions, index
  targets, field targets, and closure-captured bindings are not valid targets.
  Place classification must first remove transparent grouping parentheses, so
  `ref (items)` is the same target as `ref items`, while `ref (items[0])`
  remains an invalid index target. A call must not use the same binding for two
  `ref` positions. Target records and duplicate checks must use the caller's
  stable binding identity (such as frame and slot), never the source spelling.
  These restrictions keep aliasing unobservable and make statically illegal
  calls rejectable before argument expressions run.
- **REF-007 — Forwarding, not capture.** A function may forward one of its own
  `ref` parameters to another `ref` parameter. The inner call commits into the
  outer call's staged local; only the outer normal return commits to the original
  caller. A lambda or nested function must not capture a `ref` parameter.
- **REF-008 — Mutating receivers.** A built-in mutating method receiver uses the
  same model implicitly when its receiver is a supported referenceable place.
  Initially that means a mutable plain variable. Method arguments are evaluated
  left to right before the receiver is snapshotted, matching REF-005. Grouping
  parentheses are removed before classification: `(items).push(value)` targets
  `items`, not a temporary. A true-temporary receiver expression is evaluated
  exactly once during callee resolution, before ordinary method arguments. A
  named implicit-ref receiver is classified without snapshotting its value;
  ordinary method arguments run first, then the receiver value is snapshotted.
- **REF-009 — True temporaries.** A built-in mutating method may still receive a
  true temporary by value. The method mutates that owned temporary, discards the
  mutation, and returns the method's ordinary result. Thus `[1, 2].push(3)`
  returns `none` because `push` ordinarily returns `none`, while
  `[1, 2].pop()` returns `2`; neither writes a container back. This is not an
  implicit `ref` call and has no write-back target. Grouping does not change the
  classification: `([1, 2]).pop()` is still a true-temporary call.
- **REF-010 — Unsupported nested places.** Syntactic index and field receivers
  are not treated as true temporaries. Until places are extended, a built-in
  mutating method on `items[0]` or `record.items` must produce a line-aware,
  non-fatal error catchable by `try_call`. Its message is
  `can't mutate through an index or field receiver yet` and its friendly hint is
  `Assign the value to a variable, mutate that variable, then assign it back.`
  Grouped forms such as `(items[0]).push(value)` and
  `(record.items).push(value)` produce the same error. Tracking issue
  [#43](https://github.com/stevepryde/nybl-lang/issues/43) records delivery; this
  proposal owns the behavior. A future extension may admit those places for
  both explicit `ref` arguments and implicit-ref method receivers as one
  coherent feature.
- **REF-011 — User-defined method receivers.** A user-defined method may
  declare its first parameter as `ref self`. Method-call syntax supplies that
  receiver reference implicitly: `value.update()`, not a call-site `ref`
  marker. The receiver must be a mutable plain-variable binding and joins any
  explicit ref arguments in the same ordered snapshot/atomic commit. Constants,
  temporaries, indexes, fields, captures, and duplicate receiver/argument
  identities fail preflight before ordinary argument evaluation. An ordinary
  value receiver is read-only: assigning to it or through one of its
  fields/indexes is a parse error with a `ref self` hint. User-defined dispatch
  still wins over same-named built-ins.
- **REF-012 — Value-only boundaries.** Built-in functions, built-in method
  arguments, and host functions are value-only. An explicit `ref` marker
  supplied to any of them must fail mode preflight before invocation or
  argument-expression evaluation.

## Implementation acceptance criteria

- **AC-REF-001 — Syntax:** Declaration and call parsing represents `ref`
  positions in `fn f(ref x) { ... }` and `f(ref value)`. It accepts transparent
  grouping in `f(ref (value))` and rejects syntactically misplaced markers with
  line-aware diagnostics. Parsing does not decide callable modes or arity.
- **AC-REF-002 — Validation:** Direct calls, calls through function values,
  closures, module exports, user-defined methods, and forwarding enforce the
  same arity and positional modes. Missing or extra markers produce actionable
  hints. Built-in and host callables reject `ref` before invocation. These
  checks behave identically in the walker, VM, and AOT output.
- **AC-REF-003 — Entry order:** Tests with observable side effects prove the
  callee expression runs exactly once; a non-callable result, bad arity, mode
  mismatch, or statically invalid target prevents argument expressions from
  running; ordinary arguments then run left to right; and ref targets resolve
  and snapshot in parameter order before the callee starts. Mutating built-in
  methods prove a side-effecting true-temporary receiver runs exactly once before
  ordinary arguments, while a named implicit-ref receiver is snapshotted after
  those arguments.
- **AC-REF-004 — Transaction boundary:** A multi-target call commits all final
  staged values after an explicit or implicit normal return. A returned
  `Result::Err` commits, while a runtime error, a fatal sandbox error, and an
  error caught at a surrounding `try_call` boundary each leave every caller
  target unchanged. A regression where the final callee operation crosses the
  tracked-memory limit must error and leave every target unchanged rather than
  committing a nominal return value.
- **AC-REF-005 — Target fence:** Constants, duplicate binding identities,
  index/field targets, captured bindings, and attempts to capture a `ref`
  parameter are rejected consistently. Grouping does not alter classification:
  `ref (value)` is a plain target and `ref (items[0])` is not.
- **AC-REF-006 — Forwarding:** Forwarding a `ref` parameter is valid and
  transactional across both call frames. Inner success updates only the outer
  staged local; a later outer error still rolls back the original caller.
- **AC-REF-007 — Method receivers:** `(items).push(value)` writes back to
  `items`; `[1, 2].push(3)` returns `none` and discards its temporary mutation;
  `([1, 2]).pop()` returns `2` and discards its temporary mutation; and grouped
  or ungrouped index/field receivers produce the REF-010 diagnostic and hint.
  A user-defined `ref self` receiver updates a mutable variable transactionally
  without a call-site marker, joins explicit ref arguments in the same
  transaction, and rejects invalid or duplicate targets before argument side
  effects. Mutation through an ordinary value receiver is a parse error.
- **AC-REF-008 — Cross-engine evidence:** Differential coverage proves result,
  mutation, evaluation order, diagnostics, normal-return commit, resource-limit
  rollback, and every other rollback path agree across walker, VM, and AOT.

## Engine and representation constraints

The implementation must follow the copy-on-write container work in
[#39](https://github.com/stevepryde/nybl-lang/issues/39). CoW makes snapshotting
cheap, while #5's direct-binding mutation discipline remains necessary to avoid
creating a second `Rc` handle immediately before an in-place mutation.

The VM implementation must represent pending write-backs in call-frame state
and apply them only from the normal-return path. Every unwind path, including a
`try_call` landing, must discard them, and the normal-return path must finish
pending resource accounting before it commits. The scope and capture repairs in
#17–#22 must land first where they determine target identity, frame lifetime, or
unwind correctness. The bytecode validation work in #25 must also cover any new
ref metadata, target records, and commit/unwind instructions so a hand-built
chunk cannot bypass the fence.

AOT output may use `&mut Value` only for a staged local owned by the generated
call machinery. It must not borrow or mutate the caller binding during callee
execution. After a successful return, generated code may perform sequential
stores as the implementation of the logically atomic commit only because target
identity and every fallible validation, execution, accounting, and limit check
have already completed. Those stores target pre-resolved live bindings, are
infallible, and execute with no intervening user code. The AOT hygiene,
module-scope, and collision repairs in #26–#29 must land before this feature
relies on generated target names, module identities, and resolution.

Const-target enforcement also depends on the mutation correctness work in #7.
Closure-target and no-capture enforcement must account for #18 and #22 rather
than encoding current capture defects as language behavior.

## Promotion checklist

Implementation was considered complete only when one change set:

1. adds parser/checker representation for declaration and call-site modes;
2. carries mode metadata through every callable representation;
3. implements transactional staging, commit, and rollback in all three engines;
4. adds focused, differential, native-AOT, and diagnostic coverage for the
   acceptance criteria above;
5. promotes the accepted rules into `specs/language-runtime.md`, the grammar,
   function and method teaching pages, and any affected embedding references;
6. removes the **not implemented** status from this proposal or replaces it
   with links to the new canonical owners.

All six items are required release evidence; later changes must preserve them.
