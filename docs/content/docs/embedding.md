+++
title = "Embedding Nybl"
description = "Nybl is designed to be embedded in Rust applications. Three ways to run a program are available; every one uses the same `NyblHost` trait as the integration seam for print output, custom functions, module resolution, timeouts, etc."
weight = 28
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "std.test"
path = "/docs/stdlib/test/"
[extra.next]
title = "Stateful instances"
path = "/docs/embedding/instances/"
+++

# Embedding Nybl

Nybl is designed to be embedded in Rust applications. Three ways to run a program are available; every one uses the same `NyblHost` trait as the integration seam for print output, custom functions, module resolution, timeouts, etc.

## Quick start

Add the crates you need to `Cargo.toml`:

```toml
[dependencies]
nybl = { package = "nybl-lang", version = "0.4" }
nybl-sys = "0.4"          # optional — OS-backed standard host
nybl-vm = "0.4"           # optional — bytecode VM (faster per-fn cost)
```

Run a program with the standard host:

```rust
use nybl::NyblLimits;
use nybl_sys::StandardHost;

fn main() {
    let source = r#"
        let name = "world"
        print("Hello, {name}!")
    "#;

    let mut host = StandardHost::new();

    if let Err(e) = nybl::run(source, &mut host, &NyblLimits::standard()) {
        eprintln!("{}", e.render(source));
    }
}
```

`StandardHost` (aliased as `StdHost`) lives in `nybl-sys` and is the OS-backed reference host. It supports `print()` output plus these host functions:

| Function | Description |
|----------|-------------|
| `readline()` / `readline(prompt)` | Read one line from stdin; `none` at EOF |
| `read_file(path)` | Read a UTF-8 file into a string |
| `write_file(path, contents)` | Replace a file with string contents |
| `append_file(path, contents)` | Append string contents to a file |
| `file_exists(path)` | `true` / `false` |
| `env(name)` | Environment variable, or `none` if missing |
| `unix_time()` | Seconds since the epoch |
| `unix_time_ms()` | Milliseconds since the epoch |

`StandardHost::new()` resolves filesystem modules from the current working
directory. Call `.with_module_root(<path>)` to choose a different guarded root:
`use foo.bar.baz` then maps to `<root>/foo/bar/baz.nybl`.

## Three engines, same host

Nybl ships three execution engines — all of them share the `NyblHost` trait, `NyblError`, `NyblLimits`, and `Value` types, so the same program and host work with any of them. Pick whichever fits your workload:

| Engine | One-shot entry point | Persistent entry point | When it's best |
|--------|----------------------|------------------------|----------------|
| Tree-walker | `nybl::run(src, host, limits)` | `nybl::NyblInstance` | Lowest start-up cost. Best for one-off scripts, REPL, small inputs, no_std / wasm. |
| Bytecode VM | `nybl_vm::run(src, host, limits)` | `nybl_vm::NyblInstance` | 2–3× faster than the walker on hot loops. Same program, no compilation to disk. |
| AOT transpiler | `nybl_compile::transpile(src, opts)` → `cargo build` | Generated `NyblInstance` in sandbox mode | Nybl → Rust source, compiled to a native binary. Maximum throughput, at the cost of a `cargo build` step. |

The walker and VM always obey `NyblLimits`. Generated AOT code obeys the same
limits only when `Options::sandbox` is enabled; default AOT output and
`nybl compile` are unsandboxed and must not run untrusted source. All three
surface failures as `NyblError` and use the same host boundary.

Use the one-shot functions when a program should start from scratch on every
run. For plugin-style programs whose globals, imports, callbacks, types, and
random-number state must survive host calls, see [Stateful
instances](/docs/embedding/instances/).

## The `NyblHost` trait

The `NyblHost` trait is the integration point between your application and Nybl:

```rust
pub trait NyblHost {
    /// Called for unknown function names. `None` = not handled.
    fn call(
        &mut self,
        name: &str,
        args: &[Value],
        line: u32,
    ) -> Option<Result<Value, NyblError>>;

    /// Called by `print()`. Default: drops the message.
    fn on_print(&mut self, message: &str) { let _ = message; }

    /// Report a failure retained by the most recent `on_print`.
    /// Engines call this immediately after `on_print`.
    fn print_error(&self, line: u32) -> Option<NyblError>
    { let _ = line; None }

    /// Appended to "function not found" errors as a
    /// friendly hint (e.g. "Available functions: ...").
    fn function_hint(&self) -> &str { "" }

    /// Called at each interpreter tick (statement, loop
    /// iteration, fn entry). Return `Err` to halt.
    fn on_tick(&mut self) -> Result<(), NyblError> { Ok(()) }

    /// Resolve a `use` target to Nybl source.
    /// `None` = "not mine" (the runtime raises "module not
    /// found"); `Some(Err(e))` = resolver failed (propagated).
    fn resolve_module(&mut self, name: &str)
        -> Option<Result<String, NyblError>>
    { let _ = name; None }
}
```

### `call` — Custom functions

The primary extension point. Lexical callable values dispatch directly. For a
direct value-only function name, fallback resolution is built-in, host, then
user-defined function, so a host can deliberately override that declaration.
Ref-aware Nybl functions also dispatch directly so their reference metadata is
preserved. Return `None` when you do not handle a fallback name; Nybl then tries
the user-defined function and finally surfaces "function not found" with the
optional `function_hint`. Return `Some(Ok(v))` on success or `Some(Err(e))` to
raise a runtime error.

```rust
use nybl::{NyblError, NyblHost, Value};

struct MyHost;

impl NyblHost for MyHost {
    fn call(
        &mut self,
        name: &str,
        args: &[Value],
        line: u32,
    ) -> Option<Result<Value, NyblError>> {
        match name {
            "square" => match args {
                [Value::Int(n)] => Some(
                    n.checked_mul(*n)
                        .map(Value::Int)
                        .ok_or_else(|| NyblError::runtime(
                            "square(n) overflowed", line
                        ))
                ),
                [Value::Number(n)] => Some(Ok(Value::Number(n * n))),
                _ => Some(Err(NyblError::runtime(
                    "square(n) expects one number", line
                ))),
            },
            _ => None,
        }
    }

    fn function_hint(&self) -> &str {
        "Custom functions: square(n)"
    }
}
```

Nybl scripts now call `square(5)` as if it were built-in.

### Typed `Value` conversions

Use `IntoValue` and `Value::to_rust` at the host boundary instead of manually
matching every nested array or dictionary. Extraction borrows the input, so
targets such as `&str` do not copy; owned targets such as `String`, `Vec<T>`,
`Option<T>`, `Result<T, E>`, and `BTreeMap<String, T>` are supported too.

```rust
use nybl::{NyblError, NyblHost, IntoValue, Value};

struct MathHost;

impl NyblHost for MathHost {
    fn call(
        &mut self,
        name: &str,
        args: &[Value],
        line: u32,
    ) -> Option<Result<Value, NyblError>> {
        if name != "sum_values" {
            return None;
        }
        Some((|| {
            let values: Vec<i64> = args
                .first()
                .ok_or_else(|| NyblError::runtime("missing array", line))?
                .to_rust()
                .map_err(|error| NyblError::runtime(error.to_string(), line))?;
            values
                .into_iter()
                .sum::<i64>()
                .into_value()
                .map_err(|error| NyblError::runtime(error.to_string(), line))
        })())
    }
}
```

Infallible scalars also implement standard `From`. Recursive values use the
fallible trait because Nybl enforces a maximum safe value depth. Integer
conversion is strict (`Int` is not silently accepted as `Number`, or vice
versa), and nested errors include paths such as `$[0]["stats"]["hp"]`.

For literals, `nybl_value!` provides JSON-like array/dict syntax and returns a
`Result` for the same reason:

```rust
use nybl::nybl_value;

let request = nybl_value!({
    "name": "Ada",
    "scores": [10, 20, 30],
    "nickname": none,
})?;
```

`Result<T, E>` maps to the engine's canonical built-in `Result::Ok(value)` or
`Result::Err(error)`. Deterministic dictionary conversion deliberately uses
`BTreeMap`; no `std::collections::HashMap` implementation is provided, keeping
the API available under `no_std` and its output order stable.

### `on_print` — Capturing output

Override `on_print` to redirect `print()` output to a buffer, log, UI widget — anywhere that isn't stdout:

```rust
struct Buffered { output: Vec<String> }

impl NyblHost for Buffered {
    fn call(&mut self, _: &str, _: &[Value], _: u32)
        -> Option<Result<Value, NyblError>> { None }
    fn on_print(&mut self, msg: &str) { self.output.push(msg.into()); }
}
```

### `resolve_module` — Custom `use` resolution

Supply module source for `use path.to.module` statements. Return:
- `Some(Ok(source))` — the module's source text (Nybl parses and executes it).
- `Some(Err(err))` — resolver error; propagated to the user.
- `None` — "not my module"; Nybl raises "module `foo` not found".

```rust
impl NyblHost for MyHost {
    fn resolve_module(&mut self, name: &str)
        -> Option<Result<String, NyblError>>
    {
        match name {
            "greetings" => Some(Ok(r#"
                fn hello(who) { return "hi " + who }
            "#.into())),
            _ => None,
        }
    }
    // ... call, etc.
}
```

`nybl::host::resolve_from_map` and `nybl::host::StringModuleHost` (below) are ready-made helpers that cover the common "in-memory module table" pattern.

### `on_tick` — Execution control

Called on every tick — fn entry, loop iteration, most statements. Use it for:

- **Timeouts** — check elapsed time and halt.
- **Cancellation** — read a `&AtomicBool` set by another thread.
- **Progress tracking** — increment a counter or refresh a progress bar.

```rust
use std::time::{Duration, Instant};

struct Timed { start: Instant, budget: Duration }

impl NyblHost for Timed {
    fn call(&mut self, _: &str, _: &[Value], _: u32)
        -> Option<Result<Value, NyblError>> { None }
    fn on_tick(&mut self) -> Result<(), NyblError> {
        if self.start.elapsed() > self.budget {
            Err(NyblError::runtime("execution timed out", 0))
        } else {
            Ok(())
        }
    }
}
```

`on_tick` errors count as runtime errors — they can be caught by `try_call` inside Nybl. Use `NyblError::fatal` instead if you need the halt to be uncatchable:

```rust
fn on_tick(&mut self) -> Result<(), NyblError> {
    if cancel_flag.load(Ordering::Relaxed) {
        Err(NyblError::fatal("cancelled", 0))   // `try_call` won't swallow this
    } else {
        Ok(())
    }
}
```

## Resource limits

`NyblLimits` controls how much work a program can do before it's killed with a fatal error:

```rust
pub struct NyblLimits {
    pub max_steps: u64,      // tick budget
    pub max_memory: usize,   // bytes for strings + arrays + structs
}
```

Two presets:

| Preset | `max_steps` | `max_memory` |
|--------|-------------|--------------|
| `NyblLimits::standard()` | 10,000 | 10 MB |
| `NyblLimits::demo()` | 1,000 | 1 MB |

Custom:

```rust
let limits = NyblLimits {
    max_steps: 50_000,
    max_memory: 32 * 1024 * 1024,
};
```

Limit violations are **fatal** — `try_call` in user code can't swallow them.

## Ready-made host helpers

`nybl::host` bundles the two most common host shapes so embedders don't have to hand-roll them.

### `nybl::host::resolve_from_map(entries)`

Build a `resolve_module`-compatible closure from any iterable of `(name, source)` pairs. Drop it inside your own `NyblHost` impl:

```rust
use nybl::host::resolve_from_map;

struct MyHost { resolve: Box<dyn Fn(&str) -> Option<Result<String, NyblError>>> }

impl MyHost {
    fn new() -> Self {
        let resolve = resolve_from_map([
            ("greetings", "fn hello() { return \"hi\" }"),
            ("math_ext",  "fn sq(n) { return n * n }"),
        ]);
        Self { resolve: Box::new(resolve) }
    }
}

impl NyblHost for MyHost {
    // ...
    fn resolve_module(&mut self, name: &str)
        -> Option<Result<String, NyblError>>
    { (self.resolve)(name) }
}
```

### `nybl::host::StringModuleHost`

A minimal full `NyblHost` implementation — captures prints to an in-memory vec and resolves modules from a string map. Useful for tests and playgrounds:

```rust
use nybl::host::StringModuleHost;
use nybl::NyblLimits;

let mut host = StringModuleHost::new([
    ("greetings", "fn hello(who) { return \"hi \" + who }"),
]);

nybl::run(
    r#"use greetings
print(hello("Nybl"))"#,
    &mut host,
    &NyblLimits::standard(),
).unwrap();

assert_eq!(host.output(), "hi Nybl");
```

## Stateful REPL sessions

`nybl::ReplSession` carries `let` bindings, `fn` declarations, user types, methods, module aliases, and the import cache across `eval` calls. Use it when you want "one Nybl interpreter, many user inputs" — interactive REPLs, notebook cells, per-request scripting.

```rust
use nybl::{NyblLimits, ReplSession};
use nybl_sys::StandardHost;

let mut session = ReplSession::new();
let mut host = StandardHost::new();

session.eval("let x = 5", &mut host, &NyblLimits::standard()).unwrap();
session.eval("let y = x + 3", &mut host, &NyblLimits::standard()).unwrap();

// `eval` returns `Ok(Some(v))` when the last statement is a bare
// expression; `Ok(None)` for `let` / `fn` / `use` / etc.
let r = session.eval("y * 2", &mut host, &NyblLimits::standard()).unwrap();
assert!(matches!(r, Some(nybl::Value::Int(16))));

// Introspection.
assert!(session.get("x").is_some());
assert_eq!(session.binding_names(), vec!["x".to_string(), "y".to_string()]);
```

Each `eval` still respects the `NyblLimits` you pass — useful if you want to allow a higher step budget per cell than for a batch-run program. The built-in `nybl` CLI's `repl` subcommand is the canonical consumer: it adds rustyline, multi-line input, `:help` / `:vars` / `:reset` / `:quit` meta-commands, tab completion, and a persistent history file. See [REPL](/docs/repl/) for the user-facing view.

`ReplSession` accepts and evaluates new source over time. If the source is
loaded once and the host should call an explicit, stable set of functions,
prefer [`NyblInstance`](/docs/embedding/instances/).

## Error rendering

`NyblError::render(source)` produces a terminal-friendly error with a source snippet and a `^` caret under the offending column (when the error carries column info). Parse errors always have columns; runtime errors do when the failing expression was parsed from source.

Errors raised while loading an imported module carry a `source_context`.
`render` automatically uses that module's source and labels the location as
`in module \`path\``, so callers should continue passing the root source exactly
as shown below. If an embedder attaches only a module identity with
`NyblError::with_module`, rendering deliberately omits the snippet rather than
showing an unrelated root line. `NyblError::with_module_source` attaches both
identity and source; nested loaders preserve the deepest existing context.

```rust
match nybl::run(src, &mut host, &NyblLimits::standard()) {
    Ok(()) => {}
    Err(e) => eprintln!("{}", e.render(src)),
}
```

Typical output:

```
error: Variable `undefined` not found
  --> line 2:7
  |
2 | print(undefined)
  |       ^
hint: Did you forget to create it with `let`?
```

## Putting it all together

A complete host that provides domain-specific functions, captures output, resolves modules from memory, and enforces a timeout:

```rust
use nybl::{NyblError, NyblHost, NyblLimits, Value};
use nybl::host::resolve_from_map;
use std::time::{Duration, Instant};

struct AppHost {
    output: Vec<String>,
    start: Instant,
    data: Vec<f64>,
    resolve: Box<dyn Fn(&str) -> Option<Result<String, NyblError>>>,
}

impl AppHost {
    fn new() -> Self {
        let resolve = resolve_from_map([
            ("stats_helpers", r#"
                fn median(xs) {
                    let sorted = xs
                    sorted.sort()
                    let mid = (sorted.len() / 2).to_int()
                    return sorted[mid]
                }
            "#),
        ]);
        Self {
            output: vec![],
            start: Instant::now(),
            data: vec![],
            resolve: Box::new(resolve),
        }
    }
}

impl NyblHost for AppHost {
    fn call(
        &mut self,
        name: &str,
        args: &[Value],
        line: u32,
    ) -> Option<Result<Value, NyblError>> {
        match name {
            "add_data" => match args {
                [Value::Int(n)]    => { self.data.push(*n as f64); Some(Ok(Value::None)) }
                [Value::Number(n)] => { self.data.push(*n);       Some(Ok(Value::None)) }
                _ => Some(Err(NyblError::runtime("add_data(n) expects a number", line))),
            },
            "average" => {
                if self.data.is_empty() {
                    Some(Ok(Value::Number(0.0)))
                } else {
                    let sum: f64 = self.data.iter().sum();
                    Some(Ok(Value::Number(sum / self.data.len() as f64)))
                }
            }
            _ => None,
        }
    }

    fn on_print(&mut self, message: &str) {
        self.output.push(message.to_string());
    }

    fn on_tick(&mut self) -> Result<(), NyblError> {
        if self.start.elapsed() > Duration::from_secs(5) {
            Err(NyblError::fatal("timed out", 0))
        } else {
            Ok(())
        }
    }

    fn function_hint(&self) -> &str {
        "Custom host: add_data(n), average()"
    }

    fn resolve_module(&mut self, name: &str)
        -> Option<Result<String, NyblError>>
    { (self.resolve)(name) }
}

fn main() {
    let source = r#"
        use stats_helpers
        for n in [10, 20, 30, 40, 50] {
            add_data(n)
        }
        let avg = average()
        let mid = median([10, 20, 30, 40, 50])
        print("Average: " + avg.to_str() + ", median: " + mid.to_str())
    "#;

    let mut host = AppHost::new();
    match nybl::run(source, &mut host, &NyblLimits::standard()) {
        Ok(()) => {
            for line in &host.output { println!("{}", line); }
        }
        Err(e) => eprintln!("{}", e.render(source)),
    }
}
```
