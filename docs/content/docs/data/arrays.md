+++
title = "Arrays"
description = "Arrays are ordered, mutable collections that can hold any mix of types."
weight = 9
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Pattern Matching"
path = "/docs/control-flow/match/"
[extra.next]
title = "Strings"
path = "/docs/data/strings/"
+++

# Arrays

Arrays are ordered, mutable collections that can hold any mix of types.

## Creating arrays

```nybl
let items = [1, 2, 3]
let empty = []
let mixed = [1, "two", true, none]
```

## Accessing elements

Arrays are 0-indexed. Negative indices count from the end:

```nybl
let items = [10, 20, 30]
print(items[0])     // 10
print(items[2])     // 30
print(items[-1])    // 30 (last element)
print(items[-2])    // 20
```

Out-of-bounds access produces an error.

## Modifying elements

```nybl
let items = [10, 20, 30]
items[0] = 99
print(items)    // [99, 20, 30]
```

## Methods

Mutating methods such as `push`, `pop`, `insert`, `remove`, `truncate`,
`clear`, `reverse`, and `sort` write their updated array back to a mutable variable
receiver using the same transactional copy-in/copy-out mechanism as a [`ref`
parameter](/docs/functions/reference-parameters/). Method
arguments run before the receiver snapshot. Nested index and field receivers
are write-back places too: `dict["items"].push(value)` and
`holder.items.sort()` evaluate the receiver projections once and commit the
updated leaf atomically through the root — if the method errors, the whole
root remains unchanged.

True temporary receivers remain valid. `[1, 2].push(3)` mutates the temporary,
discards it, and returns `none`.

| Method | Returns | Description |
|--------|---------|-------------|
| `arr.len()` | int | Number of elements |
| `arr.push(val)` | none | Append to end |
| `arr.pop()` | value | Remove and return last element |
| `arr.has(val)` | bool | Whether the array contains the value |
| `arr.index_of(val)` | int | Index of first occurrence, or `-1` |
| `arr.insert(i, val)` | none | Insert at a signed index, shifting right. Negative indices count from the end; `len` appends |
| `arr.remove(i)` | value | Remove at a signed index. Negative indices count from the end |
| `arr.truncate(n)` | none | Shorten to at most `n` elements, dropping the tail. Negative lengths count from the end like a `slice` bound |
| `arr.clear()` | none | Remove every element |
| `arr.slice(start, end)` | array | Half-open sub-array. Negative bounds count from the end; out-of-range bounds clamp |
| `arr.reverse()` | none | Reverse in place |
| `arr.sort()` | none | Sort in place |
| `arr.join(sep)` | string | Join elements into a string |

Plus the universal `arr.type()`, `arr.to_str()`, `arr.inspect()`.

## Practical examples

### Building a list

```nybl
let squares = []
for i in range(1, 6) {
  squares.push(i * i)
}
print(squares)    // [1, 4, 9, 16, 25]
```

### Filtering values

```nybl
let numbers = [3, 7, 1, 9, 4, 6, 2, 8]
let big = []
for n in numbers {
  if n > 5 {
    big.push(n)
  }
}
print(big)    // [7, 9, 6, 8]
```

### Checking membership

```nybl
let allowed = ["admin", "editor", "viewer"]
let role = "editor"
if allowed.has(role) {
  print("Access granted")
} else {
  print("Access denied")
}
```

### Sorting and joining

```nybl
let scores = [42, 17, 85, 3]
scores.sort()
print(scores)    // [3, 17, 42, 85]

let names = ["Charlie", "Alice", "Bob"]
names.sort()
print(names.join(", "))    // "Alice, Bob, Charlie"
```
