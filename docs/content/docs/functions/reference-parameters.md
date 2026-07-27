+++
title = "Reference Parameters"
description = "Use explicit `ref` parameters for transactional copy-in/copy-out updates to caller variables, with the mutation visible at both the declaration and call site."
weight = 14
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Defining Functions"
path = "/docs/functions/defining-functions/"
[extra.next]
title = "Modules"
path = "/docs/modules/"
+++

# Reference Parameters

Nybl normally passes values independently: changing a function parameter does
not change the caller's variable. Use a `ref` parameter when the function is
specifically designed to replace a mutable variable in its caller.

`ref` is required in both the function declaration and the call:

```nybl
fn grow(ref items, count) {
  repeat count {
    items.push(0)
  }
}

let values = []
grow(ref values, 3)
print(values)    // [0, 0, 0]
```

The two markers make mutation part of the function's visible API. Omitting
`ref` for a reference parameter, or adding it to an ordinary value parameter,
is an error with a hint that identifies the argument.

## Copy-in/copy-out, not aliasing

A reference parameter is a staged local value, not an observable alias into
the caller's scope:

1. Nybl snapshots the caller variable when the call begins.
2. The function reads and changes its staged parameter.
3. A normal return writes every staged reference parameter back to its caller
   variable.
4. An error discards every staged change.

This is also called **copy-in/copy-out** or **call by value-result**. Nybl's
copy-on-write containers keep the initial snapshot cheap while preserving
ordinary value semantics.

Reaching the end of the function and an explicit `return` are both normal
returns. A language-level `Result::Err` is also an ordinary returned value, so
it commits:

```nybl
fn validate(ref attempts) {
  attempts += 1
  return Err("not accepted")
}

let attempts = 0
let result = validate(ref attempts)
print(result.is_err(), attempts)    // true 1
```

By contrast, `panic`, another runtime error, or a fatal step/memory/call-depth
limit failure rolls the call back. Rollback happens before `try_call` catches a
non-fatal error:

```nybl
fn update_then_fail(ref items) {
  items.push(2)
  panic("not committed")
}

let items = [1]
fn attempt() {
  update_then_fail(ref items)
}

let result = try_call(attempt)
print(result.is_err(), items)    // true [1]
```

## Valid reference targets

An explicit `ref` argument must name one mutable plain-variable binding:

```nybl
let value = 1
set(ref value)       // valid
set(ref (value))     // also valid; grouping is transparent
```

These are not valid targets:

```nybl
const FIXED = 1
// set(ref FIXED)          // constant
// set(ref 1)              // literal or other expression
// set(ref values[0])      // index
// set(ref record.field)   // field
```

A variable captured by a closure cannot be a reference target. Pass it through
an explicit reference parameter instead. A reference parameter itself also
cannot be captured by a nested function or lambda.

The same binding cannot fill two reference positions in one call:

```nybl
fn pair(ref left, ref right) {}

let value = 1
// pair(ref value, ref value)   // error
```

Use distinct variables. This fence prevents observable aliasing and lets all
targets commit as one transaction.

## Multiple targets and forwarding

All reference parameters in one call commit together. If the function fails
after changing any of them, none are written back:

```nybl
fn replace(ref left, ref right, should_fail) {
  left = 10
  right = 20
  if should_fail {
    panic("roll back both")
  }
}
```

A function can forward its reference parameter into another reference call:

```nybl
fn inner(ref value) {
  value += 1
}

fn outer(ref value) {
  inner(ref value)
  value *= 2
}

let score = 3
outer(ref score)
print(score)    // 8
```

The inner call commits into `outer`'s staged local. The original `score`
changes only when `outer` returns normally; a later failure in `outer` still
rolls the whole operation back.

## Evaluation and preflight order

Calls use a deterministic order:

1. evaluate the callee expression once;
2. check that it is callable and verify arity, argument modes, and reference
   target shapes that can be rejected immediately;
3. evaluate ordinary argument expressions from left to right;
4. resolve and snapshot reference targets in parameter order;
5. execute the function.

Mode or target-shape errors therefore prevent ordinary argument side effects.
Changes made by valid ordinary arguments are visible in the later reference
snapshots.

This contract also applies when a function is called through an alias, closure,
module export, or other first-class function value. Parameter modes travel
with the callable.

## Methods and mutating receivers

User-defined methods can use either a read-only value receiver or a mutable
reference receiver. An ordinary `self` is a value snapshot. Assigning to
`self`, one of its fields, or one of its indexes is a parse error instead of a
silent mutation of a discarded copy.

Declare `ref self` when a method should update its caller:

```nybl
struct Counter { amount }

fn Counter.add(ref self, amount) {
  self.amount += amount
}

let counter = Counter { amount: 3 }
counter.add(4)
print(counter.amount)    // 7
```

Method-call syntax supplies the receiver reference implicitly, so the call is
`counter.add(4)`, not `ref counter.add(4)`. The receiver must be a mutable
plain-variable binding. Constants, temporaries, indexes, fields, and captured
bindings are rejected before ordinary argument side effects.

A method may also declare explicit reference parameters after the receiver:

```nybl
fn Counter.transfer(ref self, ref total) {
  total += self.amount
  self.amount = 0
}

let counter = Counter { amount: 3 }
let total = 4
counter.transfer(ref total)
print(counter.amount, total)    // 0 7
```

The mutable receiver and explicit targets are snapshotted in parameter order
after ordinary arguments run. They must identify distinct bindings and commit
together on a normal return; any runtime or resource error rolls all of them
back.

Built-in mutating array methods use the same transaction model implicitly for
a mutable plain-variable receiver. You do not write `ref` before the receiver:

```nybl
let items = [1]
items.push(2)
```

Method arguments run before Nybl snapshots the receiver. A true temporary may
be mutated, but its mutation is discarded after the method returns:
`([1, 2]).pop()` returns `2`, while `[1, 2].push(3)` returns `none`.
Mutating through an index or field receiver is rejected with an
assign-mutate-reassign hint until those places become referenceable.

See [Methods → Mutating receivers](/docs/reference/methods/#mutating-receivers)
for the built-in behavior.

## Built-ins, host functions, and instances

Explicit reference parameters belong to user-defined Nybl functions. Built-in
functions, built-in method arguments, and functions supplied by `NyblHost`
accept value arguments only.

Rust's `NyblInstance::call` and `call_value` APIs also accept owned `Value`
arguments rather than Nybl binding locations. They reject a ref-bearing entry
or callback before its body executes. Keep host-facing `pub fn` entries
value-only and call reference-based helpers from inside Nybl:

```nybl
fn increment(ref value) {
  value += 1
}

let count = 0

pub fn next() {
  increment(ref count)
  return count
}
```

The tree-walker, bytecode VM, and AOT-generated runtime implement the same
reference semantics and diagnostics.
