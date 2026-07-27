# nybl-cli

The `nybl` command-line tool for the [Nybl](https://github.com/stevepryde/nybl-lang) programming language.

Building or installing Nybl 0.4 requires Rust 1.88 or newer.

```sh
cargo install nybl-cli
```

Then:

| command                       | what it does                                          |
|-------------------------------|-------------------------------------------------------|
| `nybl`                         | open the REPL                                         |
| `nybl run script.nybl`          | run with the bytecode VM (default, 2–3× the walker)   |
| `nybl run script.nybl --novm`   | run with the tree-walker                              |
| `nybl compile script.nybl`      | AOT-compile to a native binary                        |
| `nybl compile --emit-rs ...`   | emit the transpiled Rust source only                  |
| `nybl --help`                  | full usage                                            |

## REPL

`nybl` and `nybl repl` open the same stateful session. Declarations survive
between submissions, bare expressions echo their value, incomplete blocks
continue on a secondary prompt, and tab completion includes keywords,
built-ins, and current bindings.

History is persisted at `$HOME/.nybl_history`. Meta-commands are:

- `:vars` — list live top-level bindings
- `:reset` or `:clear` — clear the session
- `:help` — show REPL help
- `:quit`, `:q`, or `:exit` — leave

Piped input uses the same multiline submission rules. A parse or runtime error
sets a failing exit status but does not discard later transcript input.

The REPL, `nybl run`, and `nybl compile` all support transactional `ref`
parameters. See the [reference-parameters
guide](https://nybl-lang.com/docs/functions/reference-parameters/) for syntax,
target restrictions, rollback, and host-boundary rules.

## `nybl compile`

Transpiles the script via [`nybl-compile`](https://crates.io/crates/nybl-compile), drops the result into a scratch cargo project, builds it, and copies the binary next to the script (or wherever `-o` points).

```sh
nybl compile fib.nybl
# builds ./fib  — a standalone native binary
./fib
```

An extensionless source such as `fib` builds `./fib-bin` by default
(`fib-bin.exe` on Windows). Explicit output paths that resolve to the source
file are rejected before Cargo runs, preventing accidental source overwrite.

Flags:

- `-o PATH` / `--output PATH` — where to put the output
- `--emit-rs` — emit the transpiled `.rs` only, don't invoke cargo
- `--keep` — keep the scratch cargo project around (for inspection)

If `cargo` isn't on the PATH, `nybl compile` prints a pointer to https://rustup.rs and suggests `--emit-rs` as an escape hatch. `nybl run` never needs a toolchain — it only depends on the CLI itself.

## Why the VM by default

Running `nybl script.nybl` goes through the bytecode VM because it's **2–3× faster than the tree-walker on realistic workloads** with identical semantics. `--novm` is kept as an escape hatch for debugging, or for targets where binary size matters more than execution speed.

## Related crates

- [`nybl-lang`](https://crates.io/crates/nybl-lang) — the language core
- [`nybl-vm`](https://crates.io/crates/nybl-vm) — the bytecode runtime `nybl run` uses by default
- [`nybl-compile`](https://crates.io/crates/nybl-compile) — the AOT transpiler `nybl compile` drives
- [`nybl-sys`](https://crates.io/crates/nybl-sys) — the standard host `nybl` uses (filesystem imports, stdio, env, time)
- The Nybl stdlib (`use std.math`, `std.json`, …) is bundled inside `nybl-lang` behind the `nybl-std` feature — on by default, so `nybl run` / `nybl compile` Just Work.

## License

Dual-licensed under [MIT](https://github.com/stevepryde/nybl-lang/blob/main/LICENSE-MIT) or [Apache 2.0](https://github.com/stevepryde/nybl-lang/blob/main/LICENSE-APACHE), at your option.
