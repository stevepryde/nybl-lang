# nybl-vm

Bytecode compiler + stack VM for the [Nybl](https://github.com/stevepryde/nybl-lang) programming language.

**2–3× faster** than the tree-walker in [`nybl-lang`](https://crates.io/crates/nybl-lang), with identical semantics. Same `NyblHost` trait, same `NyblLimits`, same error shapes — swap `nybl::run` for `nybl_vm::run` and you have the fast engine.

## Why a VM at all

A tree-walker is simple but does a lot of redundant work per instruction — scope lookups, AST shuffling, allocation. `nybl-vm` compiles Nybl source to a compact bytecode where:

- Locals live in a flat `Vec<Value>` (numbered slots, no `String` hashing)
- Common patterns (`i = i + 1`, `n - 1`, `n < 2`, `total + i`) collapse into fused superinstructions with typed Int fast paths
- Function calls pop args directly into the new frame's slot array, no intermediate `Vec<Value>`
- Slot vecs are pooled across calls, so a recursive program doesn't hit the allocator 500k times

Net result (release, Apple silicon):

| workload | walker | VM | speedup |
|---|---|---|---|
| `fib(28)` — call-heavy | 180 ms | 72 ms | **2.5×** |
| 500k-iter tight loop | 72 ms | 23 ms | **3.1×** |
| combined | 70 ms | 27 ms | **2.6×** |

## The embedding niche

`nybl-vm` earns its place in the embedding use case: a Rust application that hands Nybl source to its users (or to an AI) at runtime. You can't bring `rustc` to the user's machine for AOT — the VM fills the gap:

- **No dependencies**
- **no_std-capable** via the `no_std` feature (pulls in `libm` internally for float math)
- **WASM-compatible** (builds clean for `wasm32-unknown-unknown`; ~90 KB added over the walker-only bundle)
- **Same trait surface as the walker** — any `NyblHost` impl works unchanged

## Quick start

```toml
[dependencies]
nybl-lang = "0.4"
nybl-vm = "0.4"
```

```rust
use nybl::{NyblError, NyblHost, NyblLimits, Value};

struct MyHost;
impl NyblHost for MyHost {
    fn call(&mut self, _: &str, _: &[Value], _: u32) -> Option<Result<Value, NyblError>> { None }
    fn on_print(&mut self, msg: &str) { println!("{msg}"); }
}

fn main() {
    let mut host = MyHost;
    nybl_vm::run("print(1 + 2)", &mut host, &NyblLimits::standard()).unwrap();
}
```

For scripts you'll run repeatedly, compile once and execute many times:

```rust
use nybl_vm::{compile, execute};
let stmts = nybl::parse(source)?;
let chunk = compile(&stmts)?;
for _ in 0..1000 {
    execute(chunk.clone(), &mut host, &NyblLimits::standard())?;
}
```

That deliberately starts with fresh runtime state on every `execute`. When a
plugin's globals, modules, callbacks, types, methods, or RNG state must survive
between host calls, use `nybl_vm::NyblInstance` instead:

```rust
use nybl::{NyblLimits, Value};
use nybl_vm::NyblInstance;

let mut instance = NyblInstance::load(
    "let count = 0\npub fn next() { count += 1; return count }",
    &mut host,
    &NyblLimits::standard(),
)?;

let first = instance.call("next", &[], &mut host)?;
let second = instance.call("next", &[], &mut host)?;
assert_eq!(first.inspect(), "1");
assert_eq!(second.inspect(), "2");
```

The walker and VM instance APIs are equivalent: `load`, `entry_points`,
`call`, and `call_value`.

The VM implements Nybl's transactional `ref` parameters with the same
copy-in/copy-out, target validation, commit, rollback, and diagnostic behavior
as the walker and AOT engine. Rust `NyblInstance::call` and `call_value`
arguments remain value-only; expose a value-only `pub fn` that performs any
ref call inside Nybl. See the [reference-parameters
guide](https://nybl-lang.com/docs/functions/reference-parameters/).
User-defined methods may declare `ref self` for a transactional mutable
receiver; ordinary `self` receivers are read-only.

## Bytecode tooling

For tooling and caches, `compile` returns a public `Chunk`,
`validate_chunk(&chunk)` rejects malformed operands, pool indices, control
flow, and nested function chunks, and `disassemble(&chunk)` renders a readable
instruction listing. `execute` validates every chunk before running it,
including hand-built or deserialized bytecode.

## Features

| feature | default | what it does |
|---|---|---|
| `std` | yes | retains Rust standard-library host behavior and forwards to `nybl-lang/std`. If Cargo unifies `std` and `no_std`, this feature wins. |
| `nybl-std` | yes | forwards to `nybl-lang`'s `nybl-std` feature (bundles the Nybl stdlib so `use std.math` etc. resolve through any host that calls `nybl::stdlib::resolve`) |
| `no_std` | no | forwards to `nybl-lang`'s `no_std` feature, which pulls in `libm`. Enable with `default-features = false, features = ["no_std"]` (add `"nybl-std"` too if you want the bundled stdlib on bare-metal targets). |

## WASM example

```toml
[dependencies]
nybl-lang = { version = "0.4", default-features = false, features = ["no_std", "nybl-std"] }
nybl-vm   = { version = "0.4", default-features = false, features = ["no_std", "nybl-std"] }
```

Tested and working end-to-end on `wasm32-unknown-unknown` with `lol_alloc` as the global allocator.

## Related crates

- [`nybl-lang`](https://crates.io/crates/nybl-lang) — core language + `NyblHost` trait + walker
- [`nybl-compile`](https://crates.io/crates/nybl-compile) — AOT Nybl → Rust transpiler
- [`nybl-cli`](https://crates.io/crates/nybl-cli) — the `nybl` binary (`nybl run` uses this VM by default)

## License

Dual-licensed under [MIT](https://github.com/stevepryde/nybl-lang/blob/main/LICENSE-MIT) or [Apache 2.0](https://github.com/stevepryde/nybl-lang/blob/main/LICENSE-APACHE), at your option.
