+++
title = "WebAssembly"
description = "Nybl on wasm32: supported crates and features, allocator, math backend, timing caveats, and how CI keeps native and wasm execution identical"
weight = 30
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Stateful instances"
path = "/docs/embedding/instances/"
+++

# WebAssembly

Nybl runs on wasm32 in production embeddings (browser clients and edge
runtimes). This page collects the supported configuration in one place:
which crates build for wasm, which features to pick, and the handful of
host-side caveats.

## What builds for wasm

`nybl-lang` (the tree-walker) and `nybl-vm` (the bytecode VM) both build
clean for `wasm32-unknown-unknown`, in **both** feature configurations:

- **Default features** (`std` + `nybl-std`) — works because Rust's `std`
  compiles for wasm32. Simplest option when you are using a bundler-style
  toolchain (wasm-bindgen, wasm-pack) that expects `std`.
- **`no_std`** — disable default features and opt in explicitly. This is
  the configuration for bare-metal-style wasm modules and for embeddings
  that want a deterministic pure-Rust math backend (see below):

```toml
[dependencies]
nybl-lang = { version = "0.4", default-features = false, features = ["no_std", "nybl-std"] }
nybl-vm   = { version = "0.4", default-features = false, features = ["no_std", "nybl-std"] }
```

Keep `nybl-std` if you want the bundled Nybl stdlib (`use std.math`,
`std.json`, …) to resolve. If Cargo ever unifies `std` and `no_std` in
one build graph, `std` wins — a genuine no_std build must disable default
features.

`nybl-compile`'s generated sandbox runtime follows the same feature split.
`nybl-cli` is a native application. `nybl-sys` compiles for wasm, but remains
an OS-oriented host rather than the recommended browser/freestanding host;
its unsupported clock behavior is described below.

## Allocator

On `wasm32-unknown-unknown` without `std`'s default machinery you provide
the global allocator yourself. The pattern used by the shipped embeddings
is [`lol_alloc`](https://crates.io/crates/lol_alloc), a tiny
single-threaded wasm allocator:

```rust
#[cfg(target_arch = "wasm32")]
#[global_allocator]
static ALLOCATOR: lol_alloc::AssumeSingleThreaded<lol_alloc::FreeListAllocator> =
    unsafe { lol_alloc::AssumeSingleThreaded::new(lol_alloc::FreeListAllocator::new()) };
```

Walker + VM + libm + `lol_alloc` ships at roughly **355 KB stripped**.
With default features (`std`), Rust's ordinary wasm allocator is used and
no setup is needed.

## Math backend and float determinism

Nybl's `f64` builtins (`sqrt`, `sin`, `cos`, `tan`, `exp`, `log`, `pow`,
…) go through a single math facade:

- With `std` (default), they call the platform's native `f64` methods.
- With `no_std`, they call the pure-Rust
  [`libm`](https://crates.io/crates/libm) crate.

Arithmetic (`+ - * / %`), `sqrt`, and the rounding builtins are exact
IEEE 754 operations and bit-identical everywhere. The transcendentals
(`sin`, `cos`, `tan`, `exp`, `log`, `pow`) are **not** guaranteed
bit-identical across platform math libraries — e.g. macOS's system libm
returns `1.tan()` one ULP away from wasi-libc's result. If your embedding
needs bit-identical script output across native and wasm builds (lockstep
simulation, replay verification), build **both** sides with `no_std` so
every platform uses the same pure-Rust `libm` code.

CI executes the same curated parity corpus twice: once with the default
configuration and once with both engines linked using
`default-features = false, features = ["no_std", "nybl-std"]`. The second
comparison is the lockstep configuration described above — see
[CI enforcement](#ci-enforcement) below.

## Timing: `Instant`, `SystemTime`, and `web-time`

`std::time::Instant::now()` and `SystemTime::now()` compile on
`wasm32-unknown-unknown` but **panic at runtime** — the target has no
clock. Two places this bites:

- **Host timeout patterns.** The `Instant`-based timeout host from
  [Embedding Nybl](/docs/embedding/#the-nyblhost-trait) needs a
  wasm-aware clock. The standard fix is the
  [`web-time`](https://crates.io/crates/web-time) crate — a drop-in
  `Instant`/`SystemTime` replacement that uses `performance.now()` on
  wasm32 with browser bindings and re-exports `std::time` everywhere
  else. Swap the import and the rest of the host code is unchanged:

  ```rust
  use web_time::Instant; // instead of std::time::Instant
  ```

  On WASI targets (`wasm32-wasip1`), `std::time` works natively and no
  replacement is needed.

- **`nybl-sys`.** `StdHost` uses `SystemTime::now()` on native and WASI
  targets. On `wasm32-unknown-unknown`, `unix_time()` and `unix_time_ms()`
  instead return a Nybl runtime error explaining that the system clock is
  unavailable and recommending a custom `NyblHost`; they do not panic or
  trap. Browser and other freestanding wasm embeddings should provide time
  through a host function backed by `web-time`, JavaScript, or a
  caller-supplied deterministic tick. `nybl-vm` does not depend on
  `nybl-sys`, so this does not constrain the VM.

## CI enforcement

Two CI jobs keep the wasm surface from regressing:

- **Compile:** `cargo check` runs for `nybl-lang` and `nybl-vm` on
  `wasm32-unknown-unknown` in both the default and `no_std` configurations.
  `nybl-sys` is also compile-checked there with its default features.
- **Execution:** a small `wasm32-unknown-unknown` module is run under wasmtime
  to prove `nybl-sys` reports its unsupported clock as a normal Nybl error.
  The `wasm_parity` runner (`tests/wasm-parity`) then runs a curated corpus of
  Nybl programs — float arithmetic and formatting, transcendental builtins,
  rounding at the i64 boundary, negative division/modulo, string
  interpolation, dict/array ordering, the deterministic `rand` sequence,
  error messages, and composite programs — through **both** engines, natively
  and under [wasmtime](https://wasmtime.dev/) on
  `wasm32-wasip1`, and byte-compares the transcripts in **both** feature
  configurations: default `std` (platform math) and defaults-off `no_std`
  (pure-Rust `libm`), with `nybl-std` enabled in both so the corpus can use
  bundled modules. Any native/wasm difference fails CI. An opt-in negative
  control perturbs one transcendental Nybl input on wasm: CI also verifies
  that the same comparison rejects the resulting math-output divergence.
  Parity executes on
  `wasm32-wasip1` because the std runner needs stdout; the engine libraries
  themselves are separately compile-checked for `wasm32-unknown-unknown`.
