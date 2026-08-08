+++
title = "Stateful instances"
description = "`NyblInstance` loads a program once and lets the host call its public entry"
weight = 29
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Embedding Nybl"
path = "/docs/embedding/"
[extra.next]
title = "WebAssembly"
path = "/docs/embedding/wasm/"
+++

# Stateful instances

`NyblInstance` loads a program once and lets the host call its public entry
points repeatedly. It is the plugin-style counterpart to the one-shot
`nybl::run` and `nybl_vm::run` functions.

The instance retains the state produced while loading and by later calls:

- root and imported-module bindings;
- functions and returned callbacks;
- type and method declarations;
- module aliases and the import cache;
- the random-number generator state.

The tree-walker and VM expose the same API. Sandboxed AOT output generates an
equivalent API with the source already compiled into it.

## Declaring host entry points

Mark a direct root function with `pub` to include it in the instance ABI:

```nybl
let count = 0

pub fn increment(by) {
  count += by
  return count
}

pub fn make_reader() {
  return fn() { return count }
}

fn private_helper() {
  return count
}
```

`pub fn` is only valid at the direct program root. It does not make a function
globally visible to ordinary Nybl code, and `pub` declarations inside imported
modules are not root instance entries. It only opts the final executed root
declaration into the host-callable ABI.

Loading executes top-level code before the entry list is collected. Therefore:

- a declaration after a top-level `return` is not an entry;
- redeclaring a public name replaces its earlier ABI position and arity;
- a later private `fn` with the same name removes it from the ABI;
- `entry_points()` reports the final surviving entries in declaration order.

Ordinary Nybl calls continue to use normal lexical name lookup. Host
`NyblInstance::call` uses the dedicated public-entry table, so assigning another
value to an ordinary name cannot redirect the host ABI.

Instance calls accept owned `Value` arguments and therefore cannot identify a
mutable Nybl binding for a [`ref`
parameter](/docs/functions/reference-parameters/).
`call` and `call_value` reject ref-bearing functions before execution. Keep
host-facing entries value-only and put ref-based mutation behind an ordinary
Nybl wrapper when needed.

A public entry may end in a value-only `..rest` parameter. For those entries,
`EntryPoint::arity()` is the minimum fixed argument count,
`is_variadic()` is true, `max_arity()` is `None`, and `accepts_arity(count)`
performs the complete check.

## Tree-walker instance

```rust
use nybl::{NyblError, NyblHost, NyblInstance, NyblLimits, Value};

struct Host;

impl NyblHost for Host {
    fn call(&mut self, _: &str, _: &[Value], _: u32)
        -> Option<Result<Value, NyblError>>
    {
        None
    }
}

fn main() -> Result<(), NyblError> {
    let source = r#"
        let count = 0
        pub fn increment(by) {
            count += by
            return count
        }
        pub fn make_reader() {
            return fn() { return count }
        }
    "#;

    let mut host = Host;
    let limits = NyblLimits::standard();
    let mut instance = NyblInstance::load(source, &mut host, &limits)?;

    for entry in instance.entry_points() {
        println!("{}/{}", entry.name(), entry.arity());
    }

    let first = instance.call("increment", &[Value::Int(2)], &mut host)?;
    assert_eq!(first.inspect(), "2");

    let reader = instance.call("make_reader", &[], &mut host)?;
    instance.call("increment", &[Value::Int(3)], &mut host)?;
    let current = instance.call_value(&reader, &[], &mut host)?;
    assert_eq!(current.inspect(), "5");
    Ok(())
}
```

`call` validates the public name and arity. `call_value` accepts a function
value created by that exact instance, including a callback returned by another
call.

## Bytecode VM instance

The VM is a drop-in replacement at this API boundary:

```rust
use nybl::{NyblError, NyblHost, NyblLimits, Value};
use nybl_vm::NyblInstance;

struct Host;

impl NyblHost for Host {
    fn call(&mut self, _: &str, _: &[Value], _: u32)
        -> Option<Result<Value, NyblError>>
    {
        None
    }
}

fn main() -> Result<(), NyblError> {
    let mut host = Host;
    let mut instance = NyblInstance::load(
        "let total = 0\npub fn add(n) { total += n; return total }",
        &mut host,
        &NyblLimits::standard(),
    )?;

    assert_eq!(
        instance.call("add", &[Value::Int(4)], &mut host)?.inspect(),
        "4",
    );
    assert_eq!(
        instance.call("add", &[Value::Int(5)], &mut host)?.inspect(),
        "9",
    );
    Ok(())
}
```

Use `compile` plus `execute` when you want to reuse bytecode but intentionally
start with fresh program state on every execution. Use `NyblInstance` when the
state itself must persist.

## Sandboxed AOT instances

The AOT transpiler emits a persistent `NyblInstance` only when
`Options::sandbox` is enabled. Generate library-shaped Rust and compile it into
the host application:

```rust
use nybl_compile::{Options, transpile};

let generated = transpile(
    "let count = 0\npub fn next() { count += 1; return count }",
    &Options {
        emit_main: false,
        use_nybl_sys: false,
        sandbox: true,
        ..Options::default()
    },
)?;
```

The generated module provides:

```rust,ignore
let mut instance = NyblInstance::load(&mut host, &limits)?;
let entries = instance.entry_points();
let value = instance.call("next", &[], &mut host)?;
let value = instance.call_value(&callback, &[], &mut host)?;
```

Because the Nybl source is already compiled into the generated Rust,
`NyblInstance::load` takes only `host` and `limits`, not a source string.
Unsandboxed output remains a one-shot `run` API and does not emit the
persistent instance surface.

Generated code also contains hygienically named convenience wrappers for
potential direct-root public declarations. They delegate to `call`, so the
runtime entry table remains authoritative when top-level control flow skips or
replaces a declaration.

## Hosts and re-entry

An instance borrows a `NyblHost` only for `load` or one call; it never stores the
host. Later operations may use a different compatible host. This also keeps
host-owned allocations outside the instance's memory account.

Opaque `HostValue` handles may be stored in globals and survive across calls.
The compatible host supplied for the current operation dispatches their
methods; the instance does not retain the host that originally created them.
Their payload allocation is host-owned and untracked, and any external
mutation performed by a host method remains visible even if the enclosing
Nybl call later fails.

The same instance cannot be re-entered while one of its operations is active.
For example, a host function called by instance A must not recursively call A.
It may call a different instance B, and B keeps independent state, limits, and
memory accounting.

Function values have instance affinity. Pass a callback back only to the
instance that created it; `call_value`, public entries, and callback-taking
builtins reject functions from another walker, VM, or generated AOT instance.

## Limits and failed calls

`load` and every later operation enforce the limits captured at load time:

- the step counter and fixed call-depth guard start fresh for each operation;
- tracked memory belongs to the instance and remains accounted across calls;
- returned values continue to charge the originating instance while they keep
  instance-owned storage alive.

A step or call-depth failure unwinds transient call frames and leaves the
instance callable again. Calls are not transactions: mutations completed
before an ordinary or fatal error remain visible to later calls.

Memory exhaustion is different because the retained state may itself still be
over budget. The instance continues returning a fatal memory error until
enough charged values are released. If an over-budget value was stored in a
persistent global, the instance can remain unusable.

## Instances versus REPL sessions

Use [`ReplSession`](/docs/embedding/#stateful-repl-sessions) when each interaction
introduces more source, as in a REPL or notebook. Use `NyblInstance` when the
program is loaded once and exposes a deliberate host ABI through `pub fn`.
