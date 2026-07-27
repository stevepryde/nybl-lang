+++
title = "Variables"
description = "Variables store values that you can use and change throughout your program."
weight = 4
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Types"
path = "/docs/basics/types/"
[extra.next]
title = "if / else"
path = "/docs/control-flow/if-else/"
+++

# Variables

Variables store values that you can use and change throughout your program.

## Declaring variables

Use `let` to create a new variable:

```nybl
let x = 5
let name = "Alice"
let found = true
let items = [1, 2, 3]
let config = {"width": 10, "height": 5}
```

`let` is required the first time — using an undeclared variable is an error. This catches typos early:

```nybl
let count = 5
conut = 10    // Error: I don't know what 'conut' is — did you mean 'count'?
```

## Constants

Use `const` for values that won't change:

```nybl
const PI = 3.14
const MAX_SIZE = 100
```

Reassigning a constant is rejected at parse time — the compiler sees that the left-hand side is an all-caps identifier and refuses:

```nybl
const MAX_SIZE = 100
MAX_SIZE = 200      // Error: can't reassign a constant
```

Index and field assignments respect the same rule. An assignment cannot reach
through a constant array, dict, or struct binding to change part of its value:

```nybl
const ORIGIN = [0, 0]
ORIGIN[0] = 10      // Error: can't reassign `ORIGIN` — it's a constant

const OPTIONS = {"retries": 3}
OPTIONS["retries"] += 1   // Same error
```

Built-in mutating array methods are assignments through their receiver, so
they cannot change a constant either:

```nybl
const SCORES = [3, 1, 2]
SCORES.sort()       // Error: can't reassign `SCORES` — it's a constant
```

Read-only methods remain valid. User-defined methods are value calls, even
when their names happen to match an array mutator.

Constants must be **all-caps** (with digits / underscores allowed). `const Pi = 3.14` is rejected: the parser will suggest `const PI = 3.14` instead.

## Reassignment

After declaration, reassign with just `=`:

```nybl
let score = 0
score = 10
score += 5     // score is now 15
```

Compound assignment operators: `+=`, `-=`, `*=`, `/=`, `%=`.

```nybl
let x = 10
x += 3    // x = x + 3 → 13
x -= 1    // x = x - 1 → 12
x *= 2    // x = x * 2 → 24
```

## Name shapes are checked

Nybl enforces case conventions at declaration sites so intent is visible at a glance:

| Declaration | Required shape |
|-------------|----------------|
| `let x`, `fn foo(param)`, struct fields, match bindings, `for` variables, aliases | starts with lowercase or `_` |
| `const FOO` | all caps (+ digits / `_`) |
| `struct Point`, `enum Shape`, enum variants | starts with uppercase |

Mis-shaped declarations parse-error with a suggestion:

```nybl
let Count = 5        // Error: names bound by `let` start with a lowercase letter. Try `count`?
const pi = 3.14      // Error: `const` names are SCREAMING_SNAKE_CASE. Try `PI`?
struct point {}      // Error: type names start with an uppercase letter. Try `Point`?
```

A leading underscore marks a name as "private by convention." It doesn't change the shape check — `_count` is still a lowercase-starting name — but glob `use` imports skip names that start with `_` (see [Modules](/docs/modules/)).

## Block scoping

Variables are block-scoped — a variable declared inside `{ }` is not visible outside:

```nybl
let x = 1
if true {
  let y = 2       // y only exists inside this block
  print(y)        // 2
}
// print(y)       // Error: I don't know what 'y' is
```

## Shadowing

You can re-declare a variable with `let` in an inner block. The inner variable "shadows" the outer one:

```nybl
let x = 1
if true {
  let x = 2     // shadows outer x
  x = 3         // reassigns inner x
  print(x)      // 3
}
print(x)         // 1 — outer x is unchanged
```

## Copying and passing values

Arrays, dicts, structs, and enum variants have **value semantics**. When you assign one, pass it to a function, or return it, the destination behaves like an independent copy. Changing either value never affects the other.

Nybl implements these copies with copy-on-write storage: the operation itself is constant-time and shares the existing container safely. The backing storage is copied only if one of the values is later mutated. This is an implementation detail — programs observe the same independent values without paying for an eager deep copy.

### Assignment copies

```nybl
let a = [1, 2, 3]
let b = a           // b is a separate copy
b.push(4)
print(a)            // [1, 2, 3] — unchanged
print(b)            // [1, 2, 3, 4]
```

### Function arguments are copies

When you pass a value to a function, the function gets its own copy. Modifying it inside the function has no effect on the caller's variable:

```nybl
fn try_to_modify(items) {
  items.push(99)
  print(items)       // [1, 2, 3, 99]
}

let original = [1, 2, 3]
try_to_modify(original)
print(original)      // [1, 2, 3] — unchanged
```

To get a modified value out of a function, `return` it:

```nybl
fn add_item(items, val) {
  items.push(val)
  return items
}

let original = [1, 2, 3]
original = add_item(original, 99)
print(original)      // [1, 2, 3, 99]
```

The same ordinary value behavior applies to numbers, strings, and bools. Closures and modules are shared handles, while iterators intentionally share their cursor: advancing one iterator handle advances its aliases too.

When a function is intentionally designed to update a caller variable, declare
and call an explicit [`ref` parameter](/docs/functions/reference-parameters/).
References are second-class transactional parameters, not general values or
aliases: ordinary assignment and ordinary function arguments keep the value
semantics described above.

## Dynamic typing

Variables can hold any type. You can even change the type of a variable by reassigning it:

```nybl
let val = 42
print(val.type())   // "int"

val = "hello"
print(val.type())   // "string"
```

This flexibility is useful but can be surprising — the error only surfaces when some later operation expects the original type. Case conventions help: a `count`-like variable holding a string usually means the wrong thing landed in it upstream.
