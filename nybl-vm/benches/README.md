# Embedding hot-path benchmarks

Criterion suite for the costs a host engine pays when it drives Nybl per
entity per tick: `instance.call()` dispatch, `NyblHost::call` round-trips, a
representative game-tick workload, Rust <-> `Value` conversion, and one-shot
`load`. Every engine-sensitive benchmark runs on both the tree-walker
(`nybl::NyblInstance`) and the bytecode VM (`nybl_vm::NyblInstance`).

## Running

```sh
# Full run (minutes; produces target/criterion reports)
cargo bench -p nybl-vm --bench embedding

# Smoke mode — each benchmark once, no measurement (what CI runs)
cargo bench -p nybl-vm --bench embedding -- --test

# One group
cargo bench -p nybl-vm --bench embedding -- game_tick
```

Limits are deliberately generous (`max_steps` 100M, 100 MiB memory) so the
numbers measure engine overhead, not budget enforcement. Both engines reset
the step budget on every `call`.

## Baseline numbers

Machine: Apple M5 (4P + 6E cores), macOS 26.5.2, rustc 1.96.0, default
release bench profile. Criterion point estimates from a quiet machine,
2026-08-08. Expect run-to-run noise of a few percent.

| Benchmark | Walker | VM |
| --- | ---: | ---: |
| `call_trivial` (empty `pub fn tick(a, b)`) | 611 ns | 443 ns |
| `host_call_roundtrip` (one `NyblHost::call`) | 716 ns | 609 ns |
| — implied host round-trip (minus trivial floor) | ~105 ns | ~166 ns |
| `game_tick_100_entities` (state machine, ~8 host calls/entity) | 183 µs | 242 µs |
| — per entity | 1.83 µs | 2.42 µs |
| `load_game_tick` (one-shot `NyblInstance::load`) | 14.2 µs | 24.8 µs |

| Value conversion | Time |
| --- | ---: |
| `i64` `into_value` | ~1.0 ns |
| `i64` `from_value` | ~0.9 ns |
| dict (5 keys) `into_value` | 149 ns |
| dict (5 keys) `from_value` (`BTreeMap<String, i64>`) | 226 ns |
| array (10 ints) `into_value` | 177 ns |
| array (10 ints) `from_value` (`Vec<i64>`) | 119 ns |

Context for the roadmap numbers: at 100 entities x 60 Hz the game-tick
workload costs ~11 ms/s (walker) to ~15 ms/s (VM) of a frame budget, and the
`call_trivial` floor alone is 2.7-3.7 ms/s. Notably, **the walker beats the
VM on this host-call-heavy workload** — the VM's ad-hoc 2.6x advantage
applies to script-compute-heavy code, not to embedding traffic dominated by
per-call and host-boundary overhead.

## Top 3 overhead sources

Method: this analysis is derived from criterion deltas between the
benchmarks above plus targeted reading of the call paths — not from a
sampling profiler. Treat the attribution as directional; the arithmetic is
exact, the code-level blame is by inspection.

### 1. Host-call round-trips dominate real workloads

Subtracting the trivial floor, one host round-trip costs ~105 ns on the
walker and ~166 ns on the VM. The game-tick script makes ~8 crossings per
entity, which predicts 1.45 µs (walker) / 1.77 µs (VM) of its measured
1.83 / 2.42 µs per entity — the host boundary plus the call floor is roughly
70% of a representative entity tick. Each crossing materializes a fresh
`Vec<Value>` of cloned arguments and walks a string-compare ladder over the
built-in names (`range`/`rand`/`print`/`try_call`/`panic`) before reaching
`self.host.call` (nybl-vm/src/vm.rs:3803-3832 `invoke_named_fallback`, also
vm.rs:4014-4015; walker: nybl/src/evaluator.rs:4260), and the host then does
its own name-string match. Nothing caches "this call site is host fn X", so
every crossing re-resolves from scratch — this is the case for callable
pre-resolution and/or a batch ABI.

The VM's per-crossing cost is ~1.6x the walker's, which is why the VM loses
the game-tick benchmark despite winning `call_trivial`.

### 2. Per-call instance rehydration and teardown

An empty-body `call()` costs 443 ns (VM) / 611 ns (walker) — pure entry
overhead. The VM rebuilds a `Vm` from the stashed `VmState` on every call
via `Vm::from_state` (nybl-vm/src/vm.rs:911) and tears it back down with
`restore_instance_baseline` (vm.rs:949) plus `into_state` (vm.rs:883),
moving ~27 struct fields each way per call. The walker's analog is the
`take_evaluator`/`put_evaluator` shuffle (nybl/src/evaluator.rs:5290,
5341), which `mem::take`s a comparable pile of session fields into a fresh
`Evaluator` per call. At engine scale this floor alone is milliseconds per
second; a persistent-Vm call mode (issue #184's compile-once direction)
attacks exactly this.

### 3. Per-call entry lookup and validation allocations

Entry resolution is a linear name-string scan of `abi_declarations` on
every call (nybl-vm/src/vm.rs:6650-6655; nybl/src/instance.rs:132-136), the
VM allocates a `vec![ParamMode::Value; args.len()]` per call just to
validate call modes (vm.rs:6658-6663), and arguments are cloned onto the
stack. These are small individually but sit on the unavoidable per-call
path, and they are the cheapest of the three to fix (pre-resolved entry
handles, a cached value-mode slice).

Value conversion is *not* a top overhead source: scalar conversions are
~1 ns and even small dict/array conversions are 120-230 ns — an entity's
worth of scalar field traffic through `IntoValue`/`FromValue` is noise next
to the call and host-boundary costs above.

## CI

CI runs `cargo bench -p nybl-vm --bench embedding -- --test` (each
benchmark body once, no measurement) so the suite cannot rot.
