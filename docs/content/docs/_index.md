+++
title = "Introduction"
description = "Nybl is a small, dynamically-typed programming language built to be **embedded inside other Rust programs** and run untrusted code safely. It ships with zero runtime dependencies, three interchangeable execution engines, first-class `no_std` and wasm support, and a resource-bounded sandbox that treats step count, memory, and call depth as first-class invariants — not features bolted on after the fact."
weight = 1
template = "docs/section.html"
page_template = "docs/page.html"
[extra.next]
title = "What's new in 0.4"
path = "/docs/whats-new-0-4/"
+++

# Welcome to Nybl

Nybl is a small, dynamically-typed programming language built to be **embedded inside other Rust programs** and run untrusted code safely. It ships with zero runtime dependencies, three interchangeable execution engines, first-class `no_std` and wasm support, and a resource-bounded sandbox that treats step count, memory, and call depth as first-class invariants — not features bolted on after the fact.

If you're here to embed Nybl in a host application, jump to [Embedding Nybl](/docs/embedding/) and start there. The rest of this site is the language guide and reference.

Nybl can be compiled to Rust as well (without the sandbox) if you're not running untrusted code and need more performance. Or, you could just use Rust ;)

## Quick example

```nybl
// Sum the numbers from 1 to 10
let total = 0
for i in range(1, 11) {
  total += i
}
print("Sum: {total}")
```

That's a complete Nybl program — no imports, no boilerplate, no semicolons. The syntax is intentionally familiar: curly braces, `let`, `fn`, pattern matching, modules. Skills transfer directly to Rust, JavaScript, Python.

## Why Nybl?

- **Embedded-first design.** The entire language core lives in one crate (`nybl-lang`) with no runtime deps and a minimal API surface. Host interaction goes through a single `NyblHost` trait — you wire up exactly the functions and state you want exposed; Nybl can't reach anything else.
- **Sandboxed by default.** No filesystem, no network, no clock, no ambient I/O of any kind. Every side-effecting operation is host-mediated. The sandbox also caps three things the language itself can't escape: steps executed (against runaway loops), bytes allocated (against memory bombs), and fn-call depth (against deep recursion). See [Error Handling → Fatal vs non-fatal](/docs/errors/#fatal-vs-non-fatal).
- **Three engines, one language.** Same parser, same semantics, pick the right executor per workload:
  - **Tree-walker** — fast to start, great diagnostics, zero build step. Ideal for short-lived scripts and REPLs.
  - **Bytecode VM** — compiles once, runs many times. Best for programs that loop or re-enter a hot path.
  - **AOT Rust transpiler** — emits plain Rust source that links against `nybl-lang`'s runtime. Closest to native speed; useful when you want compiled artifacts or you're already shipping a Rust build pipeline.

  All three are wire-compatible on `Value` and `NyblError`, and the test suite pins them to byte-for-byte output agreement via a three-way differential harness.
- **`no_std` and wasm.** The core crate compiles for `wasm32-unknown-unknown` and bare-metal targets. Disable default features and enable `no_std` for a `libm`-backed math facade. The default Rust `std` feature wins if Cargo unifies both modes, preserving host diagnostics.
- **Friendly errors.** Parse and runtime errors both include the source snippet, a caret under the offending column, and a `hint:` line when the parser or runtime can guess what you meant (`"I don't know what 'pritn' is — did you mean 'print'?"`). Imported-module failures retain their owning module and source. Designed to be read by humans *and* by automated callers that need to correct themselves.
- **Explicit mutation.** [`ref` parameters](/docs/functions/reference-parameters/)
  make caller updates visible at both the declaration and call, then commit all
  changed targets together only when the function returns normally.
- **Small, stable grammar.** Variables, loops, functions, arrays, dicts, structs, enums, pattern matching, modules, `Result` / `Iter` built-ins — that's close to the whole surface. Everything else (math, JSON, iteration helpers, string utilities, test assertions) lives in the bundled `std.*` modules or as methods on the value types. The shape is deliberately small so it stays consistent across versions.

## Where to start

- **Embedding Nybl in a host** — [Embedding Nybl](/docs/embedding/) walks through the `NyblHost` trait, resource limits, module resolution, and picking an engine.
- **Learning the language** — start with the [Language Guide](/docs/basics/syntax/) and work through `Basics` → `Control Flow` → `Data` → `Functions` → `Modules` → `Error Handling`.
- **Looking up a specific thing** — read [Reference Parameters](/docs/functions/reference-parameters/) for `ref`; the [Reference](/docs/reference/operators/) covers [Operators](/docs/reference/operators/), [Built-in Functions](/docs/reference/builtins/), [Methods](/docs/reference/methods/), and the [Grammar](/docs/reference/grammar/). The [Standard Library](/docs/stdlib/) section documents every `std.*` module.
- **Trying it interactively** — [`nybl repl`](/docs/repl/) opens a persistent REPL with multi-line input, history, and tab completion.
- **Upgrading from 0.3** — [What's new in Nybl 0.4](/docs/whats-new-0-4/) covers every new language and embedding feature plus the breaking syntax changes.
