# Nybl specifications

This directory is the canonical index for Nybl's stable product and system
contracts. It consolidates intent that was previously spread across the root
README, crate READMEs, and `plans/` without replacing their user guides or
historical delivery detail.

## Scope

- [Language and runtime](language-runtime.md) owns observable language-engine,
  sandbox, diagnostics, and embedding requirements.
- [Architecture](architecture.md) owns crate boundaries, dependency direction,
  execution data flow, and system invariants.
- [Stack](stack.md) owns committed implementation and tooling constraints.

## Design records

Design records preserve the decisions that shaped a language feature. Current
behaviour remains owned by the canonical specifications and teaching
documentation above.

- [Second-class `ref` parameters](proposals/ref-parameters.md) — implemented;
  the normative runtime requirements are RUN-020 and AC-RUN-016
- [Prepared and batched instance dispatch](proposals/prepared-batch-dispatch.md)
  — implemented; the normative runtime requirement is RUN-027

## Source map

- **Confirmed:** `README.md`, `plans/general-purpose-roadmap.md`,
  `plans/execution-modes.md`, and the crate READMEs state product intent.
- **Confirmed:** workspace manifests define the current crate graph, Rust
  edition, and minimum supported Rust version.
- **Observed:** the implementation and differential test suites describe
  current behaviour; where they conflict with a confirmed requirement, the
  requirement is the target and the discrepancy is a defect.
- **Unresolved:** GitHub issues are work records rather than normative specs.
  Their fixes must preserve the requirements indexed here.
