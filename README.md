# nybl-lang

A small, dynamically-typed, **embeddable** programming language for Rust hosts — give your users or your agent a real scripting language at runtime, with the sandbox treated as a first-class invariant instead of a bolted-on afterthought.

[Website](https://nybl-lang.com/) · [Documentation](https://nybl-lang.com/docs/)
· [Nybl 0.4 guide](https://nybl-lang.com/docs/whats-new-0-4/)

> **Note:** Nybl is experimental and not yet battle-tested. Good for tooling, scripting, embedding experiments, and sandbox-first workloads; use with care in production.

## Why Nybl?

- **Embedded-first.** One crate (`nybl-lang`), one trait (`NyblHost`), no runtime dependencies. You wire up the functions you want Nybl to reach; Nybl can't touch anything else.
- **Opaque host resources.** `HostValue` lets scripts retain host capabilities
  and call methods through `NyblHost::call_method` without exposing Rust
  payloads to the language.
- **Sandboxed by default.** No filesystem, network, clock, or ambient I/O.
  `NyblLimits` caps steps and tracked bytes, and every sandboxed engine also
  enforces a fixed function-call depth. A runaway script halts cleanly with a
  diagnostic, not a hung process.
- **Three engines, one language.** Walker, bytecode VM, or AOT-to-Rust transpiler — same parser, same semantics, same error shapes. Switch engines with a one-line change.
- **One-shot or persistent.** Run isolated scripts, or load a `NyblInstance`
  whose `pub fn` entries, globals, modules, callbacks, types, methods, and RNG
  state remain live across host calls.
- **Explicit in-place APIs.** Second-class `ref` parameters and `ref self`
  method receivers make caller mutation visible in declarations, support deep
  field/index places, and commit transactionally on normal return. Ordinary
  `self` receivers are read-only.
  Read the [reference-parameters
  guide](https://nybl-lang.com/docs/functions/reference-parameters/).
- **`no_std` + WASM.** Core crate builds clean for `wasm32-unknown-unknown` and bare-metal targets. Enable the `no_std` feature for a `libm`-backed math facade.
- **Small, stable grammar.** Functions and variadic closures, arrays, dicts,
  structs, enums, pattern matching, string interpolation, explicit module
  surfaces, and `Result` / `Iter` built-ins. Deliberately small — easy to teach,
  easy for tooling to target.
- **Helpful errors.** Parse and runtime errors include the source snippet, a caret under the offending column, and `hint:` suggestions (`"I don't know what 'pritn' is — did you mean 'print'?"`).

## Three engines, one language

| engine | crate | persistent API | when to use it |
|---|---|---|---|
| Tree-walker | [`nybl-lang`](https://crates.io/crates/nybl-lang) | `nybl::NyblInstance` | simplest embedding, smallest binary, best diagnostics |
| Bytecode VM | [`nybl-vm`](https://crates.io/crates/nybl-vm) | `nybl_vm::NyblInstance` | **2–3× faster** than the walker, drop-in API, still zero deps |
| AOT transpile | [`nybl-compile`](https://crates.io/crates/nybl-compile) | generated `NyblInstance` in sandbox mode | compile a script to native Rust speed |

All three share the same parser, `NyblHost` trait, `Value` type, and semantics. A three-way differential test suite pins them to byte-for-byte output agreement.

## Examples

```nybl
// Variables and string interpolation
let name = "world"
print("Hello {name}!")

// Functions
fn fizzbuzz(n) {
    if n % 15 == 0 { return "FizzBuzz" }
    if n % 3 == 0 { return "Fizz" }
    if n % 5 == 0 { return "Buzz" }
    return n.to_str()
}

// Loops, arrays, method calls
let results = []
for i in range(1, 16) {
    results.push(fizzbuzz(i))
}
print(results.join(", "))

// Dicts + missing-key soft lookup
let player = {"name": "Ada", "hp": 100}
player["hp"] -= 20
if player["inventory"].is_none() { print("no inventory") }

// Result + try
fn parse_positive(s) {
    let n = s.to_int()
    if n <= 0 { return Err("must be positive") }
    return Ok(n)
}

// Explicit, transactional caller updates
fn add_score(ref score, amount) {
    score += amount
}
let score = 10
add_score(ref score, 5)

// Mutable method receivers (the call site stays natural)
struct Counter { value }
fn Counter.increment(ref self) {
    self.value += 1
}
let counter = Counter { value: 0 }
counter.increment()

// Stdlib
use std.math
print(PI)
```

## Quick start — CLI

Building or installing Nybl 0.4 requires Rust 1.88 or newer.

```
cargo install nybl-cli
```

| | |
|---|---|
| `nybl`                       | open the REPL |
| `nybl run script.nybl`        | run a script (bytecode VM by default) |
| `nybl run script.nybl --novm` | run with the tree-walker instead |
| `nybl compile script.nybl`    | AOT-compile to a native binary |
| `nybl --help`                | full usage |

## Quick start — embedding

```toml
[dependencies]
nybl-lang = "0.4"
nybl-vm   = "0.4"       # optional — drop in for 2–3× speed
nybl-sys  = "0.4"       # ready-made filesystem / stdio / env / time host
```

```rust
use nybl::NyblLimits;
use nybl_sys::StdHost;

fn main() {
    let mut host = StdHost::new();
    // Walker path:
    nybl::run("print(1 + 2)", &mut host, &NyblLimits::standard()).unwrap();
    // Or, same API, the VM for speed:
    nybl_vm::run("print(1 + 2)", &mut host, &NyblLimits::standard()).unwrap();
}
```

Stateful plugin embedding uses explicit root `pub fn` entries:

```rust
use nybl::{NyblInstance, NyblLimits, Value};

let mut instance = NyblInstance::load(
    "let count = 0\npub fn next() { count += 1; return count }",
    &mut host,
    &NyblLimits::standard(),
)?;
let value = instance.call("next", &[], &mut host)?;
assert_eq!(value.inspect(), "1");
```

See [Stateful instances](https://nybl-lang.com/docs/embedding/instances/)
for the walker, VM, and sandboxed AOT APIs.

Custom sandboxed host — Nybl can only reach the fns you expose:

```rust
use nybl::{NyblError, NyblHost, NyblLimits, Value};

struct SandboxedHost;

impl NyblHost for SandboxedHost {
    fn call(&mut self, name: &str, args: &[Value], _line: u32)
        -> Option<Result<Value, NyblError>>
    {
        match name {
            // Expose exactly the primitives your program wants to
            // let scripts reach. Everything else is invisible.
            "now" => Some(Ok(Value::from(42_i64))),
            _ => None,
        }
    }
    fn on_print(&mut self, msg: &str) {
        eprintln!("[sandbox] {msg}");
    }
}
```

For structured arguments and return values, `Value::to_rust` and `IntoValue`
convert strict Rust scalars plus `Vec<T>`, `Option<T>`, `Result<T, E>`, and
deterministic `BTreeMap<String, T>`. The JSON-like `nybl_value!` macro builds
nested arrays and dictionaries and returns a conversion `Result`, preserving
Nybl's value-depth checks and nested error paths.

For opaque resources, return `Value::new_host("type", payload)` and implement
`NyblHost::call_method`. Handles compare by identity, display as `<host type>`,
and keep host-side mutation outside Nybl's transaction and memory-accounting
semantics. See [Opaque host values and
methods](https://nybl-lang.com/docs/embedding/#opaque-host-values-and-methods).

## WASM / no_std

Nybl builds clean for `wasm32-unknown-unknown`. Walker + VM + libm + `lol_alloc` as `#[global_allocator]` ships at ~355 KB stripped.
The Rust `std` feature is enabled by default and takes precedence if Cargo
unifies it with `no_std`, so transitive feature additions cannot silently
disable stderr diagnostics or thread-local runtime state. A genuine no_std
build therefore disables defaults and explicitly selects `no_std`:

```toml
[dependencies]
nybl-lang = { version = "0.4", default-features = false, features = ["no_std"] }
nybl-vm   = { version = "0.4", default-features = false, features = ["no_std"] }
```

Memory accounting in the walker, VM, and generated sandbox runtime is carried
by an explicit per-engine context, so concurrent or nested no_std executions
cannot charge one another. The old ambient `nybl_memory_*` compatibility hooks
are available only with `std`; custom no_std engine integrations must pass an
explicit context through Nybl's context-aware internal runtime APIs. Values
created by a host through the public `Value` constructors remain untracked
until an engine mutation copies their backing storage into its own account.

## Crates in this workspace

- [`nybl-lang`](nybl/) — the language core (parser, walker, `NyblHost` trait, `Value`). The Nybl stdlib (`use std.math`, `std.json`, …) ships inside this crate as bundled Nybl source, gated behind the `nybl-std` feature (on by default).
- [`nybl-vm`](nybl-vm/) — bytecode compiler + VM, 2–3× the walker
- [`nybl-compile`](nybl-compile/) — Nybl → Rust AOT transpiler
- [`nybl-sys`](nybl-sys/) — `StdHost`, the default OS-backed host
- [`nybl-cli`](nybl-cli/) — the `nybl` command-line tool

See [CHANGELOG.md](CHANGELOG.md) for the complete `0.4.1` release notes and
the publishing order for the crates.

## Website

The Nybl website and documentation are built with
[Zola](https://www.getzola.org/) and published to Cloudflare Pages.

```sh
bun install
bun run site:serve
```

Use `bun run site:check` to validate content and internal links, and
`bun run site:build` to create the deployable site in `docs/public/`.

## AI and coding assistants

[`llms.txt`](llms.txt) is the concise, spec-compliant entry point for coding
tools that need to write Nybl programs or embed Nybl in a Rust application. The
website build also publishes clean Markdown for every curated documentation
page and a complete context bundle at
[`/llms-full.txt`](https://nybl-lang.com/llms-full.txt).

Run `bun run site:llms` to regenerate and validate these derived files locally.
The generator follows `docs/data/navigation.json`, strips Zola front matter,
rewrites internal documentation links to their Markdown forms, and fails when
`llms.txt` references a missing generated page.

## License

Dual-licensed under [Apache 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
