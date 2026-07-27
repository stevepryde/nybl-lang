+++
title = "std.collections"
description = "`Stack`, `Queue`, and `Set` as value-semantics structs."
weight = 24
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "std.iter"
path = "/docs/stdlib/iter/"
[extra.next]
title = "std.string"
path = "/docs/stdlib/string/"
+++

# std.collections

`Stack`, `Queue`, and `Set` as value-semantics structs.

## Value semantics

Nybl's user methods pass `self` by value, so these collections all return a **fresh instance** on mutation and you rebind:

```nybl
let s = stack()
s = s.push(1)
s = s.push(2)
```

The pattern trades a little boilerplate for predictable semantics: `let a = b` doesn't alias, and method calls never surprise you by mutating out of band.

## Import

```nybl
use std.collections                          // glob
use std.collections.{stack, queue, set}      // selective
use std.collections as c                     // aliased
```

## `Stack` (LIFO)

```nybl
use std.collections.{stack}

let s = stack()
s = s.push(1)
s = s.push(2)
s = s.push(3)
print(s.top())       // 3
print(s.size())      // 3

s = s.pop()
print(s.top())       // 2
```

| Method | Returns | Notes |
|--------|---------|-------|
| `stack()` | `Stack` | Empty constructor |
| `s.is_empty()` | bool | |
| `s.size()` | int | |
| `s.push(v)` | `Stack` | New stack with `v` on top |
| `s.top()` | value | Top element, or `none` if empty |
| `s.pop()` | `Stack` | New stack without the top; popping empty is a no-op |

## `Queue` (FIFO)

```nybl
use std.collections.{queue}

let q = queue()
q = q.enqueue("a")
q = q.enqueue("b")
q = q.enqueue("c")
print(q.front())    // "a"

q = q.dequeue()
print(q.front())    // "b"
```

| Method | Returns | Notes |
|--------|---------|-------|
| `queue()` | `Queue` | Empty constructor |
| `q.is_empty()` | bool | |
| `q.size()` | int | |
| `q.enqueue(v)` | `Queue` | New queue with `v` at the back |
| `q.front()` | value | Front element, or `none` if empty |
| `q.dequeue()` | `Queue` | New queue without the front; dequeuing empty is a no-op |

Dequeue is O(n) in this naive implementation — good enough for scripting workloads. If you need tighter bounds, write an array-backed ring buffer.

## `Set` (unique, insertion-ordered)

```nybl
use std.collections.{set, set_of}

let a = set_of([1, 2, 3])
let b = set_of([2, 3, 4])

print(a.union(b).values())         // [1, 2, 3, 4]
print(a.intersect(b).values())     // [2, 3]
print(a.difference(b).values())    // [1]
```

| Method | Returns | Notes |
|--------|---------|-------|
| `set()` | `Set` | Empty constructor |
| `set_of(arr)` | `Set` | From an array (duplicates collapse, first-seen wins) |
| `s.is_empty()` | bool | |
| `s.size()` | int | |
| `s.has(v)` | bool | |
| `s.add(v)` | `Set` | New set with `v`. Idempotent. |
| `s.remove(v)` | `Set` | New set without `v`. Removing absent is a no-op. |
| `s.values()` | array | Elements in insertion order |
| `s.union(other)` | `Set` | |
| `s.intersect(other)` | `Set` | |
| `s.difference(other)` | `Set` | `self` minus `other` |
