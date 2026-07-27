+++
title = "Command-line interface"
description = "Install and use the nybl CLI to run scripts with the VM or walker, compile native executables, emit Rust source, and open the persistent REPL."
weight = 16
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "REPL"
path = "/docs/repl/"
[extra.next]
title = "Operators"
path = "/docs/reference/operators/"
+++

# Command-line interface

Building or installing Nybl 0.4 requires Rust 1.88 or newer. Install the current
Nybl command-line tool from crates.io:

```sh
cargo install nybl-cli
```

## Run a script

```sh
nybl run app.nybl
```

`nybl run` uses the bytecode VM by default. It runs the warning pass before
execution, resolves `std.*` from the bundled standard library, and resolves
other module paths relative to the script.

Use the tree-walker when debugging engine behavior or minimizing the active
runtime:

```sh
nybl run --novm app.nybl
```

Both paths render the same source snippets, carets, hints, module context, and
warnings.

## Compile a native executable

```sh
nybl compile app.nybl
nybl compile app.nybl -o my-app
```

The command transpiles Nybl to Rust, creates a temporary Cargo project, builds a
release binary, and copies it to the requested output path. `cargo` and a Rust
toolchain must be available for this step. By default, `app.nybl` builds
`./app`; an extensionless source such as `app` builds `./app-bin` so the source
can never be mistaken for its output. On Windows, native outputs also receive
the `.exe` suffix.

An explicit output path that resolves to the source itself is rejected before
the build starts. Use `-o` with a distinct path instead.

Use `--emit-rs` to stop after transpilation:

```sh
nybl compile --emit-rs app.nybl -o app.rs
```

Use `--keep` when building a binary to retain the scratch Cargo project for
inspection after the command finishes.

## Open the REPL

Either spelling starts the persistent interactive session:

```sh
nybl
nybl repl
```

See [REPL](/docs/repl/) for multiline input, live bindings, history,
completion, meta-commands, and piped transcripts.

## Help and version

```sh
nybl --help
nybl --version
```

Argument and usage errors exit with status 2. Parse, runtime, module, and build
failures exit non-zero after rendering their diagnostic.
