+++
title = "Dictionaries"
description = "Dictionaries (dicts) are key-value stores. Keys are always strings; values can be any type."
weight = 11
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Strings"
path = "/docs/data/strings/"
[extra.next]
title = "Structs & Enums"
path = "/docs/data/structs-and-enums/"
+++

# Dictionaries

Dictionaries (dicts) are key-value stores. Keys are always strings; values can be any type.

## Creating dictionaries

```nybl
let person = {"name": "Alice", "age": 30, "active": true}
let empty = {}
```

## Accessing values

Use bracket notation with a string key:

```nybl
let name = person["name"]     // "Alice"
let age = person["age"]       // 30
```

Accessing a missing key returns `none` (no error):

```nybl
let email = person["email"]
print(email)    // none

if email.is_none() { print("no email on file") }
```

Two caveats:

- A key whose value is explicitly `none` is **present** — `d.has(k)` returns `true` for it, even though `d[k]` and `d["absent_key"]` are both `none`. Use `d.has(k)` when you need to distinguish "unset" from "set to none".
- If you want a read to *fail* on a missing key, check `d.has(key)` first and raise explicitly — `d[key]` itself always succeeds.

## Modifying values

```nybl
person["age"] = 31             // update existing key
person["email"] = "a@b.com"   // add new entry
let removed = person.remove("email")   // delete a key, returning its value
```

`remove` returns `none` when the key is absent. Together with `clear`, it
mutates its receiver with the same write-back rules as the [mutating array
methods](/docs/data/arrays/#methods): mutable places rooted in a `let`
binding write back atomically (including nested places like
`state["session"].remove("token")`), constants are rejected, and a genuine
temporary is mutated and then discarded. Because they are mutating methods,
they also work through `ref` parameters and `ref self` — `fn wipe(ref d) {
d.clear() }` empties the caller's dict, where `d = {}` would only rebind the
callee's local.

## Methods

| Method | Returns | Description |
|--------|---------|-------------|
| `d.len()` | int | Number of entries |
| `d.keys()` | array | Array of all keys |
| `d.values()` | array | Array of all values |
| `d.has(key)` | bool | Whether the key exists |
| `d.remove(key)` | value | Remove `key`, returning its value, or `none` when absent. The key must be a string |
| `d.clear()` | none | Remove every entry |

Plus the universal `d.type()`, `d.to_str()`, `d.inspect()`.

## Practical examples

### Counting occurrences

```nybl
let words = ["apple", "banana", "apple", "cherry", "banana", "apple"]
let counts = {}
for word in words {
  if counts.has(word) {
    counts[word] += 1
  } else {
    counts[word] = 1
  }
}

for key in counts {
  print(key + ": " + counts[key].to_str())
}
```

### Storing structured data

```nybl
let point = {"x": 10, "y": 20}
let x = point["x"].to_str()
let y = point["y"].to_str()
print("Position: ({x}, {y})")
```

### Iterating over entries

```nybl
let config = {"width": 800, "height": 600, "title": "My App"}
for key in config {
  let val = config[key].to_str()
  print(key + ": " + val)
}
```

### Checking for a key before using it

```nybl
let settings = {"volume": 80}

if settings.has("volume") {
  let v = settings["volume"].to_str()
  print("Volume is {v}")
} else {
  print("Using default volume")
}
```
