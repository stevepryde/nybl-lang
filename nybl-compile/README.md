# nybl-compile

Ahead-of-time [Nybl](https://github.com/stevepryde/nybl-lang) → Rust transpiler.

Given Nybl source, `nybl-compile::transpile` produces human-readable Rust source
that links against [`nybl-lang`](https://crates.io/crates/nybl-lang) and compiles
via `cargo` to native code. Standalone generated binaries also use
[`nybl-sys`](https://crates.io/crates/nybl-sys); library-shaped output can use
your own host without that dependency. This is the fastest of Nybl's three
engines.

## When to reach for the AOT

- **Scripts you'll run repeatedly** — builds once, runs at native speed forever after.
- **Performance-sensitive workloads** — where even the bytecode VM's 2–3× speedup isn't enough.
- **Deploying a script as a self-contained binary** — `nybl compile script.nybl` and ship the resulting executable.

For scripts you compile *at the host's runtime*, the bytecode VM in [`nybl-vm`](https://crates.io/crates/nybl-vm) is the right choice instead — AOT needs `rustc` on the target machine, which embedded hosts typically can't rely on.

## CLI usage (the common path)

Most users never call `nybl-compile` directly; they use [`nybl-cli`](https://crates.io/crates/nybl-cli):

```sh
nybl compile script.nybl          # → ./script (native binary)
nybl compile script.nybl -o app   # → ./app
nybl compile --emit-rs script.nybl -o script.rs   # transpile only
```

## Library usage

When you want to wire the transpiler into your own build pipeline (a `build.rs`, a custom tool, a CI job):

```toml
[dependencies]
nybl-compile = "0.4"
```

```rust
use nybl_compile::{transpile, Options};

let rust_source = transpile(
    r#"print("hello from nybl")"#,
    &Options::default(),
)?;
// write rust_source to src/main.rs and run `cargo build`…
```

`Options` controls the output shape: standalone program vs. library, module name wrapping, sandbox mode for step/memory enforcement, and the module resolver callback for `use` statements.

### Persistent sandboxed output

Sandbox mode can generate a stateful library surface for plugin-style AOT
embedding. Mark direct root entries with `pub fn`, disable `main`, and compile
the generated Rust into the host application:

```rust
let rust_source = transpile(
    "let count = 0\npub fn next() { count += 1; return count }",
    &Options {
        emit_main: false,
        use_nybl_sys: false,
        sandbox: true,
        ..Options::default()
    },
)?;
```

The generated module exposes `NyblInstance::load(host, limits)`,
`entry_points()`, `call(name, args, host)`, and
`call_value(callback, args, host)`. It retains program and module state across
calls and enforces instance affinity, re-entry, and resource limits.

The persistent API is sandbox-only. Unsandboxed generated output retains its
one-shot `run` API. See the [stateful embedding
guide](https://nybl-lang.com/docs/embedding/instances/) for the
full lifecycle contract.

Generated Rust preserves Nybl's transactional `ref` semantics in both sandboxed
and unsandboxed output: explicit parameter modes, valid-target checks,
copy-in/copy-out staging, multi-target commit, error rollback, forwarding, and
mutating receiver behavior match the walker and VM. Generated persistent
`NyblInstance` calls remain value-only. See the [reference-parameters
guide](https://nybl-lang.com/docs/functions/reference-parameters/).
User-defined `ref self` receivers share that transaction with any explicit ref
arguments, while ordinary `self` receivers are read-only.

## Selling points

- **Native-speed scripts.** The transpiled output is ordinary Rust — rustc optimises it the same way it optimises hand-written code.
- **Human-readable output.** User-defined Nybl functions become top-level Rust fns with reasonable names, so the generated code is debuggable.
- **Same semantics as walker + VM.** The three-engine differential suite exercises hundreds of programs to catch any behavioural drift.
- **Same `NyblHost` surface.** The generated binary uses `nybl-sys::StdHost` by default, so your custom hosts work without changes.

## Features

| feature | default | what it does |
|---|---|---|
| `nybl-std` | yes | forwards to `nybl-lang`'s `nybl-std` feature (bundles the Nybl stdlib). Turn off with `default-features = false` when building a truly minimal AOT pipeline. |

## Related crates

- [`nybl-lang`](https://crates.io/crates/nybl-lang) — the language core the generated code depends on
- [`nybl-sys`](https://crates.io/crates/nybl-sys) — the standard host the generated `main()` wires up
- [`nybl-cli`](https://crates.io/crates/nybl-cli) — the `nybl compile` command-line driver
- [`nybl-vm`](https://crates.io/crates/nybl-vm) — bytecode VM, for when you need speed at the host's runtime rather than AOT

## License

Dual-licensed under [MIT](https://github.com/stevepryde/nybl-lang/blob/main/LICENSE-MIT) or [Apache 2.0](https://github.com/stevepryde/nybl-lang/blob/main/LICENSE-APACHE), at your option.
