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
  `nybl-rust-embedding-examples` integration fixture; manifests and
  `Cargo.lock` own exact versions.
- **Testing:** Cargo unit/integration tests plus VM differential and AOT
  three-way suites. `cargo clippy --workspace --all-targets` is the code-health
  target, and the explicit Rust 1.88 command above is the release MSRV gate.
- **AOT runtime mode:** `nybl-compile` supports opt-in sandbox emission, while
  `nybl-cli compile` currently emits unsandboxed binaries.
- **Website and documentation:** Zola templates, Tailwind CSS v4, and Markdown
  content live under `docs/`. The root `llms.txt` is the canonical LLM index;
  `scripts/llm-docs.ts` derives clean per-page Markdown and `llms-full.txt`
  from the curated navigation and documentation sources. Generated
  `docs/static/docs/`, LLM site artifacts, and `docs/public/` output are
  derived rather than normative and are published through Cloudflare Pages.

## Constraints

- Preserve the existing Rust-first architecture and shared runtime instead of
  reimplementing semantics per engine.
- Do not add OS access or general-purpose dependencies to `nybl-lang`.
- New language behaviour must include focused regression coverage and, when it
  can diverge by engine, differential coverage.
- Performance work must retain sandbox checks and parity before speed claims
  are accepted.
