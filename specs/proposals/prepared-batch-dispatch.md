# Design record: prepared and batched instance dispatch

> **Status: implemented.** This record captures the evidence
> and contract for issue [#186](https://github.com/stevepryde/nybl-lang/issues/186).
> Once shipped, the normative embedding guarantees live in
> `specs/language-runtime.md` and the public embedding guide.

## Evidence and decision

Issue #185 measured the representative 100-entity game tick at 183 us on the
walker and 242 us on the VM, with an empty public-entry call floor of 611 ns and
443 ns respectively. Eight host crossings plus the entry floor explain roughly
70% of that workload. A focused rerun on the implementation machine immediately
before this proposal measured:

| Benchmark | Walker | VM |
| --- | ---: | ---: |
| `call_trivial` | 589.17 ns | 399.99 ns |
| `game_tick_100_entities` | 179.33 us | 211.72 us |

Criterion used 1 second warmup, 2 second measurement, and 20 samples. Source
inspection attributes the entry floor to repeated linear ABI lookup and mode
validation, argument materialization, operation guarding, and moving persistent
state into and out of a temporary evaluator/VM. The batch benchmark added by
this change is the instrumentation that isolates how much of that floor can be
amortized without changing the script workload or host crossings.

The shipped design combines two backwards-compatible host APIs:

1. `prepare_entry(name)` returns an opaque, instance-bound entry handle that
   retains the already-resolved callable and its value-only ABI metadata.
2. `call_batch(prepared, calls, host)` executes many argument lists through one
   live evaluator/VM operation, while treating every item as an independent
   call for resource accounting and error semantics.

`call_prepared` is also provided so callers with an existing per-entity loop can
remove lookup/validation overhead without adopting batching immediately.

Numeric host-function IDs are not part of this change. An instance deliberately
borrows a potentially different compatible `NyblHost` for every operation, so
an ID resolved against one host has no stable meaning for a later host. Adding
a host identity/registration protocol would be a larger public capability
contract. The batch benchmark keeps all existing string-dispatched host calls,
so its delta does not hide the remaining host-boundary cost.

## Dispatch contract

- **BAT-001 — Opaque preparation.** A prepared entry is created only from a
  currently public direct-root entry. It exposes descriptive ABI metadata but
  not an engine callable or internal frame identity.
- **BAT-002 — Instance identity.** A prepared entry may be invoked only on the
  exact instance that created it. Cross-instance and cross-engine use is
  rejected before script execution, even when source and names match.
- **BAT-003 — Value-only ABI.** Preparation rejects an entry containing a
  `ref` parameter because host values do not identify Nybl bindings. Rest
  parameters remain valid and preserve their minimum arity.
- **BAT-004 — Compatibility.** Existing `call(name, args, host)` and
  `call_value(callable, args, host)` behavior and signatures do not change.
  Prepared and batch calls produce the same values, mutations, output, RNG
  progression, callable identity checks, and diagnostics as repeated `call`.
- **BAT-005 — Per-item limits.** Every batch item begins with a fresh step and
  call-depth budget exactly like an individual `call`. Tracked memory remains
  instance-persistent. A batch cannot pool, aggregate, or evade a per-call
  sandbox limit.
- **BAT-006 — Ordered failure.** Items execute in input order. The batch stops
  at the first error and returns that error. Mutations and host effects from
  completed items remain committed; the failing item follows ordinary instance
  unwind and mutation rules; later items do not run.
- **BAT-007 — Re-entry.** The whole batch is one host operation for the
  same-instance re-entry guard. A host callback may still invoke a different
  instance. Same-instance re-entry fails exactly as it does during `call`.
- **BAT-008 — Host lifetime.** A batch borrows one host for its duration and
  stores no host reference or host-specific resolved identifier afterward.

## Acceptance evidence

- Walker and VM instance tests cover prepared identity, arity, ref rejection,
  state/RNG retention, ordered failure, per-item step reset, persistent memory,
  and same-instance re-entry.
- Cross-engine instance tests compare ordinary repeated calls with prepared
  batch calls for results, persistent state, and ordered host effects.
- The embedding criterion suite reports ordinary, prepared, and batch variants
  for both the empty-call floor and representative 100-entity game tick.
- Benchmark documentation records before/after point estimates and separates
  the dispatch saving from the still-string-dispatched host-call cost.
- Workspace tests, formatting, clippy, benchmark smoke, `no_std`, and WASM
  checks protect the existing portability contract.

## Promotion checklist

The shipped contract is promoted into `RUN-027` and `AC-RUN-019`; the embedding
guide owns the public API and budget guidance, and the benchmark README records
the measured delta.
