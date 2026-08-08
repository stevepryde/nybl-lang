# nybl-sys

The standard host for the [Nybl](https://github.com/stevepryde/nybl-lang) programming language — a `NyblHost` implementation that wires Nybl up to the normal OS-backed conveniences (filesystem, stdio, environment, time) plus the bundled Nybl stdlib.

If you're writing a command-line tool or a desktop / server app that runs Nybl scripts, `StdHost` is the default you want. Custom embeddings (sandboxed, wasm, no_std) should write their own `NyblHost` impl.

## What `StdHost` provides

### Import resolution

- `use std.math` / `std.json` / `std.collections` / … → resolved via `nybl-lang`'s bundled stdlib (the `nybl-std` feature, forwarded by default)
- `use my_module` / `my.nested.module` → resolved from the filesystem beneath
  the host's module root (the current working directory by default; the CLI
  sets it to the script's parent)

### Host functions (available to Nybl code as `fn_name(...)`)

- `readline()` — read a line from stdin
- `read_file(path)` / `write_file(path, contents)` / `append_file(path, contents)` / `file_exists(path)` — filesystem basics
- `env(var_name)` — read an environment variable
- `unix_time()` / `unix_time_ms()` — current time, seconds / milliseconds since epoch
- `print` is provided by `nybl-lang` itself; `StdHost` routes output to stdout

Host functions always receive value arguments. Explicit `ref` arguments are
supported only by user-defined Nybl functions; write a Nybl wrapper when a
script needs to update one of its own variables transactionally.

## Quick start

```toml
[dependencies]
nybl-lang = "0.4"
nybl-sys  = "0.4"
```

```rust
use nybl::{run, NyblLimits};
use nybl_sys::StdHost;

fn main() {
    let mut host = StdHost::new();
    run(r#"
        use std.math
        print("π ≈ {PI}")
        let now = unix_time()
        print("running at {now}")
    "#, &mut host, &NyblLimits::standard()).unwrap();
}
```

Prefer the faster bytecode runtime? Drop in [`nybl-vm`](https://crates.io/crates/nybl-vm) — `StdHost` works unchanged:

```rust
nybl_vm::run(source, &mut host, &NyblLimits::standard())?;
```

## When *not* to use `nybl-sys`

- **Sandboxed embeddings** that need to block filesystem / env access — write a bare `NyblHost` impl and skip this crate entirely.
- **WASM / no_std builds** — `nybl-sys` depends on `std` and is an OS-oriented
  host. On WASI its clock functions use the platform clock. On
  `wasm32-unknown-unknown`, `unix_time()` and `unix_time_ms()` return a Nybl
  runtime error instead of panicking because that target has no system clock.
  Browser and other freestanding wasm embeddings should use `nybl-lang`
  (optionally with `nybl-vm`) directly and provide time through a custom
  `NyblHost`.
- **Anywhere you want a tighter custom host surface** — `NyblHost::call` is the only thing you need to implement; your host can expose exactly the functions your app wants Nybl to reach.

## Related crates

- [`nybl-lang`](https://crates.io/crates/nybl-lang) — the language core + `NyblHost` trait
- The Nybl stdlib this crate routes `use std.*` to lives inside `nybl-lang` behind the `nybl-std` feature (forwarded by default from `nybl-sys`).
- [`nybl-vm`](https://crates.io/crates/nybl-vm) — faster bytecode runtime, drop-in with the same `StdHost`
- [`nybl-cli`](https://crates.io/crates/nybl-cli) — the `nybl` binary built on top of `nybl-sys`

## License

Dual-licensed under [MIT](https://github.com/stevepryde/nybl-lang/blob/main/LICENSE-MIT) or [Apache 2.0](https://github.com/stevepryde/nybl-lang/blob/main/LICENSE-APACHE), at your option.
