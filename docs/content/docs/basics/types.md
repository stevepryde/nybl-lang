+++
title = "Types"
description = "Nybl is dynamically typed — variables can hold any type, and types are checked at runtime. Every value has a `.type()` method that returns its type name:"
weight = 3
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Syntax"
path = "/docs/basics/syntax/"
[extra.next]
title = "Variables"
path = "/docs/basics/variables/"
+++

# Types

Nybl is dynamically typed — variables can hold any type, and types are checked at runtime. Every value has a `.type()` method that returns its type name:

| `.type()` returns | Description | Literal examples |
|-------------------|-------------|------------------|
| `"int"` | 64-bit signed integer | `0`, `42`, `-7` |
| `"number"` | 64-bit floating point | `3.14`, `-0.5`, `4.0` |
| `"string"` | UTF-8 text | `"hello"`, `"got {n} items"` |
| `"bool"` | Boolean | `true`, `false` |
| `"none"` | Absence of a value | `none` |
| `"array"` | Ordered, mutable collection | `[1, 2, 3]`, `[]` |
| `"dict"` | String-keyed map | `{"x": 10, "name": "Alice"}` |
| `"fn"` | First-class function / closure | `fn(x) { return x + 1 }` |
| `"struct"` | User-defined struct instance | `Point { x: 3, y: 4 }` |
| `"enum"` | User-defined enum variant | `Color::Red` |
| `"module"` | Aliased module namespace | result of `use foo as m` |
| `"iter"` | Lazy iterator | `[1, 2, 3].iter()`, `"abc".iter()` |

```nybl
let x = 42
print(x.type())      // "int"

let y = 3.14
print(y.type())      // "number"

let s = "hello"
print(s.type())      // "string"
```

## Integers and floats

Integer literals (`42`, `-7`) produce `int` values; anything with a decimal point (`3.14`, `4.0`) produces `number`. There's no exponent-shaped literal — write the full decimal or build large values by multiplication. The two numeric types coexist: arithmetic widens to `number` on mixed operands, and `==` compares numerically across the split:

```nybl
print(1 + 1)         // 2       (int + int → int)
print(1 + 1.0)       // 2       (int + number → number, prints as whole)
print(1 == 1.0)      // true    (cross-type numeric equality)
```

Ints cover the full signed 64-bit range, from `-9223372036854775808` through `9223372036854775807`. The minimum value is written with unary `-` directly before its magnitude (spaces or comments may separate the tokens); the positive magnitude `9223372036854775808` is out of range. Integer literals never silently become `number` values when they are too large.

Use `.to_int()` to truncate a number to an integer (toward zero), and `.to_float()` to widen an int to a number:

```nybl
print((3.7).to_int())      // 3
print((-2.7).to_int())     // -2
print((5).to_float())      // 5       (now a number internally)
```

Number literals need parens before a method call (otherwise `3.7.to_int()` looks like a decimal followed by a field). Variables don't: `let x = 3.7; print(x.to_int())`.

### Division

`/` always produces a `number`, even for `int / int`:

```nybl
print(7 / 2)              // 3.5
print(6 / 2)              // 3        (whole value — still a number)
print((6 / 2).type())     // "number"
```

This sidesteps the classic "1 / 2 == 0" footgun that trips beginners in C / Rust / Java.

When you *do* want an integer result (index math, bucketing, etc.), coerce the quotient back with `.to_int()`:

```nybl
print((7 / 2).to_int())           // 3
print((-7 / 2).to_int())          // -3
print((7 / 2).to_int().type())    // "int"
```

`.to_int()` truncates toward zero. `/` raises a runtime error on division by zero.

## Strings

Strings use double quotes only. Supported escape sequences: `\"`, `\\`, `\n`, `\t`, `\r`, `\{`, `\}`.

```nybl
let greeting = "Hello, world!"
let with_newline = "Line 1\nLine 2"
```

Strings are indexable and iterable, but immutable — you can read characters but not change them in place:

```nybl
let s = "hello"
print(s[0])          // "h"
print(s[-1])         // "o"

for ch in s {
  print(ch)          // "h", "e", "l", "l", "o"
}
```

### String interpolation

Use `{variable}` inside a string to insert a variable's value. Only variable names are allowed inside `{}` — not expressions:

```nybl
let name = "Alice"
let count = 5
print("Hello, {name}! You have {count} items.")
```

For computed values, store the result in a variable first:

```nybl
let doubled = count * 2
print("Double: {doubled}")

// Or use concatenation:
print("Double: " + (count * 2).to_str())
```

To include a literal `{` or `}` in a string, escape it with a backslash:

```nybl
print("Use \{name\} for interpolation")
// prints: Use {name} for interpolation
```

### String concatenation

`+` joins two strings, or a string and a number (the number is converted first):

```nybl
print("Score: " + (42).to_str())    // "Score: 42"
print("n=" + 7)                      // "n=7"   (int auto-stringified)
```

## Booleans

`true` and `false`. Used in conditions and comparisons:

```nybl
let found = true

if found {
  print("Got it!")
}
```

## None

`none` represents the absence of a value. Functions that don't explicitly return a value return `none`. It's also what you get from any operation designed to signal "no value here" (e.g. `first_or_none` helpers, optional return values):

```nybl
fn first_or_none(arr) {
  if arr.len() == 0 { return none }
  return arr[0]
}

let r = first_or_none([])
print(r)           // none
```

### Checking for `none`

Two equivalent ways:

```nybl
if r.is_none() { print("empty") }
if r == none    { print("empty") }      // same thing

if r.is_some() { print("got one") }     // inverse — every non-none value is "some"
```

`.is_none()` / `.is_some()` are [universal methods](/docs/reference/methods/#methods-on-every-value) — they work on any value, not just optional-shaped ones. In a dynamically-typed language every variable can hold `none`, so the check is always available.

Don't confuse falsy-ness with none-ness: `false`, `0`, `""`, `[]`, and `{}` are all falsy in `if` conditions but they're **not** `none`. `none.is_none()` is `true`; `false.is_none()` is `false`.

## User-defined types

Nybl also lets you declare your own struct and enum types with methods — see [Structs & Enums](/docs/data/structs-and-enums/).
