+++
title = "Methods"
description = "Nybl dispatches methods with `.name(args...)`. Every built-in method on primitives, arrays, strings, and dicts is listed here. User-defined methods on structs use the same syntax — see [Structs & Enums](/docs/data/structs-and-enums/) for the `fn Type.method(self, ...)` form."
weight = 19
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Built-in Functions"
path = "/docs/reference/builtins/"
[extra.next]
title = "Grammar"
path = "/docs/reference/grammar/"
+++

# Methods

Nybl dispatches methods with `.name(args...)`. Every built-in method on primitives, arrays, strings, and dicts is listed here. User-defined methods on structs use the same syntax — see [Structs & Enums](/docs/data/structs-and-enums/) for the `fn Type.method(self, ...)` form.

## Mutating receivers

Array methods such as `push`, `pop`, `insert`, `remove`, `reverse`, and `sort`
mutate a mutable place rooted in a `let` binding using the same transactional
copy-in/copy-out model as a [`ref`
parameter](/docs/functions/reference-parameters/). Method
arguments run first, then Nybl snapshots the receiver, and a normal return writes
the updated value back.

A genuine temporary is allowed and its ordinary result is preserved, but its
mutation has nowhere to be stored: `([1, 2]).pop()` returns `2`, while
`[1, 2].push(3)` returns `none`. Index and field receivers are write-back
places, so `groups[0].push(x)` and `record.items.push(x)` update their root
binding atomically.

User-defined methods choose their receiver mode explicitly. An ordinary `self`
is a read-only value snapshot; assigning through it is a parse error. Declare
`ref self` when the method should update the caller's binding:

```nybl
struct Counter { amount }

fn Counter.add(ref self, amount) {
  self.amount += amount
}

let counter = Counter { amount: 3 }
counter.add(4)
print(counter.amount)    // 7
```

The receiver marker appears only in the declaration: `counter.add(4)`, not
`ref counter.add(4)`. The receiver must be a mutable field/index place rooted
in a `let` binding.
It is snapshotted after ordinary arguments, commits on a normal return, and
rolls back on an error. A method may also declare explicit `ref` parameters
after its receiver; all receiver and argument targets must be distinct and
commit as one transaction.

## Opaque host methods

An embedder can return an opaque `HostValue` and implement its methods through
`NyblHost::call_method`. These calls use the same `value.method(args...)`
syntax, but their effects belong to the host: they do not write a new receiver
back into a Nybl variable and are not rolled back by `ref` transactions or
runtime errors. Arguments are value-only.

Host values have a host-defined type name and a fixed, payload-hiding display:
`handle.type()` may return `"file"`, while `handle.to_str()` and
`handle.inspect()` return `"<host file>"`. Separate handles compare by
identity, not by their hidden payload. See the [Rust embedding
example](/docs/embedding/#opaque-host-values-and-methods).

## Methods on every value

Five methods work on any value — introspection, stringification, and optional-value checks. They're dispatched before the type-specific and host method tables, so they're always available:

| Method | Returns | Notes |
|--------|---------|-------|
| `x.type()` | string | A built-in type name, a declared struct/enum name, or an opaque host value's host-defined name |
| `x.to_str()` | string | Display repr — same as what `print(x)` would emit for a single arg |
| `x.inspect()` | string | Debug repr — strings are wrapped in `"..."`, nested strings stay quoted inside arrays / dicts |
| `x.is_none()` | bool | `true` iff `x` is the `none` value. Equivalent to `x == none`. |
| `x.is_some()` | bool | Inverse of `.is_none()` — `true` for every value except `none`. |

```nybl
print((42).type())                 // "int"
print("hi".to_str())               // "hi"
print("hi".inspect())              // "hi"   (quoted)
print([1, "two"].inspect())        // [1, "two"]

print(none.is_none())              // true
print((0).is_none())               // false — `0` is falsy but not `none`
print(first_result().is_some())    // check an optional return without `== none`
```

> `.is_none()` / `.is_some()` cover Nybl's "any variable can be `none`" story — they work on every receiver, not just `Option`-shaped ones (Nybl doesn't have `Option`). Equivalent to `x == none` / `x != none`, but reads better in method chains.

### Parens around numeric literals

Number literals need parens before a method call because `.` is otherwise a decimal point:

```nybl
// print(42.type())   // parse error — `42.t…` looks like a decimal
print((42).type())     // "int"
print((-5).abs())      // 5
```

Identifiers don't have this problem: `x.type()`, `count.to_str()`, etc.

## Numeric methods — `int` and `number`

All of these work on both `int` and `number` receivers. Return type is noted per method; most math operations always widen to `number`.

| Method | Returns | Description |
|--------|---------|-------------|
| `x.abs()` | int / number | Absolute value. Preserves receiver type. `(-5).abs()` → `5` (int), `(-2.7).abs()` → `2.7` (number). Integer overflow on `(i64::MIN).abs()` is a runtime error. |
| `x.sqrt()` | number | Square root. |
| `x.sin()`, `x.cos()`, `x.tan()` | number | Trig. Angles in radians. |
| `x.exp()` | number | `e^x`. |
| `x.log()` | number | Natural log. |
| `x.pow(e)` | number | `x` raised to `e`. |
| `x.floor()`, `x.ceil()`, `x.round()` | int / number | Round toward `-∞`, `+∞`, or nearest (ties away from zero). Returns `int` when the rounded result fits in `i64`, `number` otherwise (so rounding a number that overflows `i64` stays a `number` instead of raising). Int receivers pass through unchanged. |
| `a.min(b)`, `a.max(b)` | int / number | Pair-wise. Preserves type when both sides match; widens to `number` on mixed int/number. |
| `x.to_int()` | int | Truncates toward zero. `(3.7).to_int()` → `3`, `(-2.7).to_int()` → `-2`. |
| `x.to_float()` | number | Widens `int` → `number`; `number` passes through. |

```nybl
print((9).sqrt())                   // 3
print((0).cos())                    // 1
print((2).pow(10))                  // 1024
print((3).min(7))                   // 3
print((1).max(2.5))                 // 2.5   (widened)
print((3.7).floor())                // 3     (int)
print((3.7).floor().type())         // "int"
```

## Boolean methods — `bool`

| Method | Returns | Description |
|--------|---------|-------------|
| `b.to_int()` | int | `true.to_int()` → `1`, `false.to_int()` → `0`. |
| `b.to_float()` | number | `true.to_float()` → `1` (as number), `false.to_float()` → `0`. |

Plus the universal `type` / `to_str` / `inspect`.

## String methods — `string`

See [Strings](/docs/data/strings/) for worked examples.

| Method | Returns | Description |
|--------|---------|-------------|
| `s.len()` | int | Number of Unicode code points. |
| `s.contains(sub)` | bool | Whether `sub` appears anywhere. |
| `s.starts_with(prefix)` | bool | |
| `s.ends_with(suffix)` | bool | |
| `s.index_of(sub)` | int | Byte index of first occurrence, or `-1` if not found. |
| `s.split(sep)` | array | Split into an array of strings on `sep`. |
| `s.replace(old, new)` | string | Replace every occurrence. |
| `s.upper()`, `s.lower()` | string | Case conversion. |
| `s.trim()` | string | Strip leading / trailing whitespace. |
| `s.slice(start, end)` | string | Half-open substring by code-point index. Negative bounds count from the end; out-of-range bounds clamp. |
| `s.to_int()` | int | Parse. `"3.7".to_int()` parses as float then truncates → `3`. Raises on junk. |
| `s.to_float()` | number | Parse. Raises on junk. |
| `s.iter()` | iter | Lazy iterator over Unicode code points. See [Iter methods](#iter-methods-iter). |

## Array methods — `array`

See [Arrays](/docs/data/arrays/) for worked examples.

Mutating methods write back when the receiver is a mutable place rooted in a
`let` binding (`items.push(x)`, `groups[0].push(x)`, or
`holder.items.pop()`).
Calling one on a genuine temporary, such as `[1].push(2)` or
`make_items().pop()`, is legal; the temporary is mutated and then discarded.
Nested receiver projections are evaluated once and the updated leaf is written
back atomically through the root. If the method errors, the whole root remains
unchanged.

| Method | Returns | Description |
|--------|---------|-------------|
| `arr.len()` | int | Number of elements. |
| `arr.push(v)` | none | Append. |
| `arr.pop()` | value | Remove and return the last element. |
| `arr.has(v)` | bool | Structural equality check. |
| `arr.index_of(v)` | int | Index of first match, or `-1`. |
| `arr.insert(i, v)` | none | Insert at a signed index, shifting right. Negative indices count from the end; `len` appends. |
| `arr.remove(i)` | value | Remove at a signed index, returning the removed value. Negative indices count from the end. |
| `arr.truncate(n)` | none | Shorten to at most `n` elements, dropping the tail. Negative lengths count from the end like a `slice` bound; no-op when already short enough. |
| `arr.clear()` | none | Remove every element. |
| `arr.slice(start, end)` | array | Half-open sub-array. Negative bounds count from the end; out-of-range bounds clamp. |
| `arr.reverse()` | none | In-place. |
| `arr.sort()` | none | In-place, numeric or lexicographic depending on element types. |
| `arr.join(sep)` | string | Join after stringifying each element. |
| `arr.iter()` | iter | Lazy iterator over the elements. See [Iter methods](#iter-methods-iter). |

## Dict methods — `dict`

See [Dictionaries](/docs/data/dictionaries/) for worked examples.

`d.remove(key)` and `d.clear()` mutate their receiver with the same write-back
rules as the mutating array methods above: mutable places rooted in a `let`
binding write back atomically, constants are rejected, and a genuine temporary
is mutated and then discarded. Because they are mutating methods, they also
work through `ref` parameters and `ref self`, where reassigning the callee's
binding would not.

| Method | Returns | Description |
|--------|---------|-------------|
| `d.len()` | int | Number of entries. |
| `d.keys()` | array | All keys as strings. |
| `d.values()` | array | All values. |
| `d.has(key)` | bool | Whether `key` exists. |
| `d.remove(key)` | value | Remove `key`, returning its value, or `none` when absent. The key must be a string. |
| `d.clear()` | none | Remove every entry. |
| `d.iter()` | iter | Lazy iterator over keys, in declaration order. See [Iter methods](#iter-methods-iter). |

## Result methods — `Result`

`Result` is an [engine built-in](/docs/errors/). All combinators are methods on the built-in type — no import required. `Ok(x)` and `Err(e)` are [parser-level shorthand](/docs/errors/#ok-err-shorthand) for `Result::Ok(x)` / `Result::Err(e)` in both expression and pattern position.

| Method | Returns | Description |
|--------|---------|-------------|
| `r.is_ok()` | bool | `true` when `r` is `Result::Ok(_)`. |
| `r.is_err()` | bool | `true` when `r` is `Result::Err(_)`. |
| `r.unwrap()` | value | Payload on `Ok`; raises a runtime error on `Err` (message includes the `.inspect()` of the payload). |
| `r.expect(msg)` | value | Payload on `Ok`; raises with `msg` on `Err`. |
| `r.unwrap_or(default)` | value | Payload on `Ok`; `default` on `Err`. |
| `r.map(f)` | Result | `Ok(v)` → `Ok(f(v))`; `Err(e)` passes through. |
| `r.map_err(f)` | Result | `Err(e)` → `Err(f(e))`; `Ok(v)` passes through. |
| `r.and_then(f)` | Result | `Ok(v)` → `f(v)` (expected to return a Result); `Err(e)` passes through. |

```nybl
print(Ok(5).is_ok())                           // true
print(Err("bad").unwrap_or(0))                 // 0
print(Ok(5).map(fn(v) { return v * 2 }))       // Result::Ok(10)
print(Err("x").map(fn(v) { return v * 2 }))    // Result::Err("x")

fn halve(x) {
  if x % 2 == 0 { return Ok((x / 2).to_int()) }
  return Err("odd")
}
print(Ok(8).and_then(halve).and_then(halve))   // Result::Ok(2)
```

## Iter methods — `iter`

An `iter` is Nybl's lazy iterator. Values you can iterate over — arrays, strings, dicts, built-in iterators, and user-defined containers — all participate in the same protocol:

1. `v.iter()` returns an iterator.
2. `it.next()` advances it, returning `Iter::Next(value)` or `Iter::Done`.

`for x in v` uses this protocol, so anything with a working `.iter()` method works with `for`.

| Method | Returns | Description |
|--------|---------|-------------|
| `it.next()` | `Iter::Next(v)` / `Iter::Done` | Advance by one. Cloning an iterator shares its cursor (like Python / Rust / JS) — two names pointing at the same iterator advance together. |
| `it.iter()` | iter | Returns the same iterator. Makes `for x in it` work whether `it` is already an iterator or a fresh iterable. |

```nybl
let it = [10, 20, 30].iter()
print(it.type())                // "iter"
print(it.next())                // Iter::Next(10)
print(it.next())                // Iter::Next(20)

for x in it { print(x) }        // 30  (picks up from the current cursor)
```

### User-defined iterables

A struct can participate in the iterator protocol by implementing `.iter()` (and, if it's its own iterator, `.next()`):

```nybl
struct Bag { items }
fn bag_of(arr) { return Bag { items: arr } }
fn Bag.iter(self) { return self.items.iter() }   // delegate to the backing array

let b = bag_of(["x", "y", "z"])
for v in b { print(v) }                           // x  y  z
```

That's the minimal shape. A container that wraps an array and delegates `.iter()` is the 80% case. User types with genuine internal state (like a lazy counter) work the same way — define `fn Counter.iter(self)` to return an iterator (either the backing data's iterator, or `self` if `self` also has `.next()`).

### The `Iter` enum

`.next()` returns one of two variants of the built-in `Iter` enum — always in scope, no `use` required:

```
enum Iter {
  Next(value),
  Done,
}
```

Pattern-match directly:

```nybl
let it = [1, 2].iter()
let r = it.next()
print(match r {
  Iter::Next(v) => "got: " + v.to_str(),
  Iter::Done    => "exhausted",
})
// got: 1
```

## Struct / enum methods

User-declared. See [Structs & Enums](/docs/data/structs-and-enums/). Method dispatch on a struct tries the universal common methods first, then looks up `fn TypeName.method` declared in the same module.

> **Same-module rule**: user-declared methods must live in the module that declares the type. You can't extend a type imported from another module, and you can't add methods to the built-ins (`int`, `string`, `array`, `Result`, `Iter`, …). See [Methods must live in the type's own module](/docs/data/structs-and-enums/#methods-must-live-in-the-types-own-module) for the rationale and the "use a free function instead" workaround.

## Module methods

If you `use path as m`, `m` is a `Value::Module`. `m.type()` → `"module"`, `m.inspect()` → `"<module path>"`. Otherwise `.` on a module accesses its exports:

```nybl
use std.math as m
print(m.PI)             // exported constant
print(m.type())         // "module"   (universal method, not an export)
```
