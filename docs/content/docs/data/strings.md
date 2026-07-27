+++
title = "Strings"
description = "Strings are immutable sequences of characters. All string methods return new strings — the original is never modified."
weight = 10
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Arrays"
path = "/docs/data/arrays/"
[extra.next]
title = "Dictionaries"
path = "/docs/data/dictionaries/"
+++

# Strings

Strings are immutable sequences of characters. All string methods return new strings — the original is never modified.

## Creating strings

```nybl
let s = "hello world"
let empty = ""
let escaped = "Line 1\nLine 2"
```

Supported escape sequences: `\"`, `\\`, `\n`, `\t`, `\r`, `\{`, `\}`. Any other `\x` escape is a lexer error.

## Indexing

```nybl
let s = "hello"
print(s[0])      // "h"
print(s[-1])     // "o"
```

Each index returns a single-character string (there's no separate character type).

## String interpolation

Insert variable values with `{name}` inside a string:

```nybl
let name = "Alice"
let count = 5
print("Hello, {name}! You have {count} items.")
```

Only variable names are allowed inside `{}`. For expressions, use a temporary variable:

```nybl
let total = (count * 2).to_str()
print("Double: {total}")
```

Or use concatenation:

```nybl
print("Double: " + (count * 2).to_str())
```

To include a literal `{` or `}` in a string, escape it with `\{` and `\}`:

```nybl
print("Use \{name\} for interpolation")
// prints: Use {name} for interpolation
```

## Concatenation

Use `+` to join strings:

```nybl
let full = "Hello" + ", " + "world!"
print(full)    // "Hello, world!"
```

Numbers must be converted with `.to_str()` first:

```nybl
let msg = "Score: " + (42).to_str()
```

## Methods

| Method | Returns | Description |
|--------|---------|-------------|
| `s.len()` | int | Number of characters |
| `s.contains(sub)` | bool | Whether the string contains `sub` |
| `s.starts_with(prefix)` | bool | Whether it starts with `prefix` |
| `s.ends_with(suffix)` | bool | Whether it ends with `suffix` |
| `s.index_of(sub)` | int | Index of first occurrence, or `-1` |
| `s.split(sep)` | array | Split into array of strings on `sep` |
| `s.replace(old, new)` | string | Replace all occurrences |
| `s.upper()` | string | Uppercase copy |
| `s.lower()` | string | Lowercase copy |
| `s.trim()` | string | Copy with leading/trailing whitespace removed |
| `s.slice(start, end)` | string | Half-open substring by code-point index. Negative bounds count from the end; out-of-range bounds clamp |
| `s.to_int()` | int | Parse. `"3.7".to_int()` → `3` (float-then-truncate). Raises on junk. |
| `s.to_float()` | number | Parse. Raises on junk. |

Plus the universal `s.type()`, `s.to_str()`, `s.inspect()`.

## Practical examples

### Parsing CSV data

```nybl
let input = "Alice,95,A"
let parts = input.split(",")
print(parts[0])    // "Alice"
print(parts[1])    // "95"
```

### Checking prefixes

```nybl
let filename = "report.csv"
if filename.ends_with(".csv") {
  print("CSV file detected")
}
```

### Building a formatted string

```nybl
let items = ["apple", "banana", "cherry"]
let count = items.len().to_str()
let list = items.join(", ")
print("Found {count} items: {list}")
```
