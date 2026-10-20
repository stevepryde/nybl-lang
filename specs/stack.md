# Stack

## Committed choices

- **Language:** Rust 2024 edition for all workspace crates.
- **MSRV:** Rust 1.88, as declared by the workspace manifest. Verify the pinned
  release dependency graph with
  `cargo +1.88.0 check --workspace --all-targets --locked`.
- **Formatting:** The rustfmt bundled with Rust 1.88 is canonical. Install it
  with `rustup component add rustfmt --toolchain 1.88`, then verify the tree
  with `cargo +1.88 fmt --all -- --check`.
- **Core dependency policy:** `nybl-lang` remains zero third-party Rust
  dependencies in its standard configuration; `alloc`/`core` support the
  portable core and the existing `libm` feature supports `no_std` math.
  Rust `std` is an explicit default feature and takes precedence when Cargo
  unifies it with `no_std`; genuine no_std builds use
  `--no-default-features --features no_std`.
- **Workspace:** Cargo workspace containing `nybl`, `nybl-vm`, `nybl-compile`,
  `nybl-sys`, and `nybl-cli`, plus the non-published
  `nybl-rust-embedding-examples`, `nybl-wasm-clock-smoke`, and
  `nybl-wasm-parity` integration fixtures; manifests and `Cargo.lock` own
  exact versions. The wasm fixtures runtime-check the freestanding clock
  error and native-to-WASI execution parity respectively.
- **Testing:** Cargo unit/integration tests plus VM differential and AOT
  three-way suites. `cargo clippy --workspace --all-targets` is the code-health
  target, and the explicit Rust 1.88 command above is the release MSRV gate.
- **Release checks:** CI's test jobs build the debug profile and skip
  `#[ignore]`d suites, so before publishing run:
  - `cargo test --release -p nybl-vm --test stack_bound --test module_accounting`
    (the RUN-002 release stack bound for the walker and VM);
  - `cargo test -p nybl-compile --test e2e -- --ignored` (native and sandboxed
    AOT parity, including the sandboxed stack corpus on the release thread
    size and module accounting);
  - `cargo test -p nybl-compile --test three_way -- --ignored --test-threads=1`
    (the cases share one scratch driver directory).
- **AOT runtime mode:** `nybl-compile` supports opt-in sandbox emission, while
  `nybl-cli compile` currently emits unsandboxed binaries.
- **Website and documentation:** The Zola site, language guide, and LLM exports
  live in
  [`stevepryde/nybl-app/site`](https://github.com/stevepryde/nybl-app/tree/main/site)
  and publish independently through Cloudflare Pages. This repository owns the
  language crates, crate READMEs, Rustdoc, and release metadata.

## Constraints

- Preserve the existing Rust-first architecture and shared runtime instead of
  reimplementing semantics per engine.
- Do not add OS access or general-purpose dependencies to `nybl-lang`.
- New language behaviour must include focused regression coverage and, when it
  can diverge by engine, differential coverage.
- Performance work must retain sandbox checks and parity before speed claims
  are accepted.
