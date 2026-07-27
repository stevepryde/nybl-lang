# nybl-lang

The core of [Nybl](https://github.com/stevepryde/nybl-lang) — a small, dynamically-typed, **embeddable** scripting language for Rust applications.

Hand your users or your AI a real programming language at runtime, without shipping a compiler to the target machine.

> **Note:** Nybl is experimental and not yet battle-tested. Fine for tooling, scripting, and embedding experiments; use with care in production.

## What's in this crate

`nybl-lang` is the language core:

- **Lexer + parser** producing a typed AST
- **Tree-walking interpreter** (`nybl::run`) — simplest runtime, works everywhere
- **Persistent interpreter** (`nybl::NyblInstance`) — load once, then call
  explicit `pub fn` entries while globals, modules, callbacks, and RNG state
  remain live
- **`NyblHost` trait** — the only thing embedders need to implement to wire Nybl into their Rust app
- **`Value` type + builtin operators** — the shared runtime surface every Nybl engine uses
- **Transactional `ref` parameters** — explicit copy-in/copy-out updates to
  mutable caller variables, with rollback on errors
- **Resource limits** (`NyblLimits`) — step and tracked-memory budgets, plus a fixed function-call depth cap

For a faster runtime (2–3× this crate's tree-walker, same semantics), add [`nybl-vm`](https://crates.io/crates/nybl-vm). For an AOT path to native Rust, see [`nybl-compile`](https://crates.io/crates/nybl-compile).

## Selling points

- **Embeddable.** One trait (`NyblHost`) to implement; everything else is handled by the engine.
- **Zero Rust deps** Nothing to audit in your supply chain.
- **`no_std` support** via the `no_std` feature (uses the `libm` crate internally for float math, nothing else).
- **WASM-compatible.** Builds clean for `wasm32-unknown-unknown`. Use it in browsers, edge workers, or wherever you can run Rust.
- **Sandboxed by default.** `NyblLimits` caps step count and tracked memory, and the runtime caps function-call depth, so runaway user scripts halt cleanly.

## Quick start

```toml
[dependencies]
nybl-lang = "0.4"
```

```rust
use nybl::{run, NyblError, NyblHost, NyblLimits, Value};

struct MyHost;

impl NyblHost for MyHost {
    fn call(&mut self, name: &str, _args: &[Value], _line: u32)
        -> Option<Result<Value, NyblError>>
    {
        // Return Some(Ok(...)) to handle a custom function call,
        // Some(Err(...)) to raise, None to defer to builtins.
        match name {
            "greet" => Some(Ok(Value::from("hello!"))),
            _ => None,
        }
    }

    fn on_print(&mut self, msg: &str) {
        println!("{msg}");
    }
}

fn main() {
    let mut host = MyHost;
    let limits = NyblLimits::standard();
run(r#"print(greet())"#, &mut host, &limits).unwrap();
}
```

The language normally passes independent values. A user-defined function can
opt into a caller update with `ref` at both sites:

```nybl
fn increment(ref value) {
  value += 1
}

let count = 0
increment(ref count)
print(count)    // 1
```

Reference calls stage their changes and commit only after a normal return.
See the [reference-parameters
guide](https://nybl-lang.com/docs/functions/reference-parameters/) for target,
rollback, forwarding, method, and host-boundary rules.

For a stateful plugin rather than a one-shot script, declare root entry points
with `pub fn`, then load and call an instance:

```rust
use nybl::{NyblInstance, NyblLimits, Value};

let mut instance = NyblInstance::load(
    "let total = 0\npub fn add(n) { total += n; return total }",
    &mut host,
    &NyblLimits::standard(),
)?;
let value = instance.call("add", &[Value::Int(3)], &mut host)?;
```

`entry_points()` exposes each public entry's name and arity, while
`call_value()` invokes a callback returned by that same instance. See the
[stateful embedding guide](https://nybl-lang.com/docs/embedding/instances/)
for lifecycle, affinity, limits, and error-state behavior.

Host arguments support borrowed or owned typed extraction through
`Value::to_rust` (`&str`, integers, `Vec<T>`, `Option<T>`, `Result<T, E>`, and
deterministic `BTreeMap<String, T>`). Use the fallible, JSON-like `nybl_value!`
macro to construct nested values while retaining Nybl's depth checks.

## Features

| feature | default | what it does |
|---|---|---|
| `std` | yes | enables Rust standard-library integration, including stderr diagnostics and the std-only legacy ambient-memory compatibility APIs. If Cargo unifies `std` and `no_std`, this feature wins. |
| `nybl-std` | yes | bundles the Nybl stdlib (`use std.math`, `std.json`, `std.collections`, `std.iter`, `std.string`, `std.test`) as `&'static str` constants reachable via [`nybl::stdlib::resolve`] |
| `no_std` | no | opt in for bare-metal / embedded / edge wasm targets. Pulls in `libm` for float math. Enable with `default-features = false, features = ["no_std"]` (add `"nybl-std"` too if you want the bundled stdlib on those targets). |

A minimal std build — core language only, no bundled Nybl stdlib:

```toml
nybl-lang = { version = "0.4", default-features = false, features = ["std"] }
```

For compatibility, omitting both `std` and `no_std` also retains std behavior;
genuine no_std builds must explicitly select `no_std` with default features
disabled.

The walker and VM carry memory accounting through an explicit per-engine
`MemoryContext`, so nested or concurrent executions cannot charge one another.
This changes custom true-no_std runtime integrations: the legacy ambient
`nybl_memory_*` functions and `ActiveMemoryGuard` are now std-only. Code that
used those hooks must pass a `MemoryContext` through the context-aware internal
constructors and mutation helpers instead. Values created by a host through
the public `Value` constructors are untracked until an engine mutation adopts
their backing storage into its own account.

## WASM example

```toml
[dependencies]
nybl-lang = { version = "0.4", default-features = false, features = ["no_std", "nybl-std"] }
```

Build for `wasm32-unknown-unknown` as usual. See [`nybl-vm`](https://crates.io/crates/nybl-vm) for the faster runtime if you need it.

## Related crates

- [`nybl-vm`](https://crates.io/crates/nybl-vm) — bytecode compiler + VM, 2–3× faster than this crate's walker, same API
- [`nybl-compile`](https://crates.io/crates/nybl-compile) — AOT Nybl → Rust transpiler for native-speed scripts
- [`nybl-sys`](https://crates.io/crates/nybl-sys) — ready-made `StdHost` with filesystem / stdio / env / time
- [`nybl-cli`](https://crates.io/crates/nybl-cli) — the `nybl` command-line tool (`nybl run`, `nybl compile`, REPL)

## License

Dual-licensed under [MIT](https://github.com/stevepryde/nybl-lang/blob/main/LICENSE-MIT) or [Apache 2.0](https://github.com/stevepryde/nybl-lang/blob/main/LICENSE-APACHE), at your option.
