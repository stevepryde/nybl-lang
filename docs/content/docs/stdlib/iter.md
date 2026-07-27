+++
title = "std.iter"
description = "Eager functional helpers over arrays: map, filter, reduce, slicing, combining, and reductions. Nybl's separate `.iter()` protocol remains lazy."
weight = 23
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "std.math"
path = "/docs/stdlib/math/"
[extra.next]
title = "std.collections"
path = "/docs/stdlib/collections/"
+++

# std.iter

`std.iter` provides eager functional helpers over arrays. Array-in,
array-out helpers such as `map` and `filter` allocate their result; a
hand-written `for` loop can avoid that work when allocation matters.

This module is separate from Nybl's built-in lazy
[`.iter()` / `.next()` protocol](/docs/reference/methods/#iter-methods-iter).
You do not need `use std.iter` to write `for value in collection`, and the
built-in protocol does not make these array helpers lazy.

Every helper handles empty arrays gracefully and preserves relative order.

## Import

```nybl
use std.iter                                // glob
use std.iter.{map, filter, reduce}          // selective
use std.iter as i                           // aliased
```

## Higher-order

### `map(arr, f)`

Apply `f` to each element; return a new array of results.

```nybl
use std.iter.{map}
print(map([1, 2, 3], fn(x) { return x * 2 }))     // [2, 4, 6]
```

### `filter(arr, pred)`

Keep only the elements for which `pred(x)` is truthy.

```nybl
use std.iter.{filter}
let evens = filter([1, 2, 3, 4, 5], fn(n) { return n % 2 == 0 })
print(evens)    // [2, 4]
```

### `reduce(arr, initial, combine)`

Fold over the array left-to-right with a two-arg combiner.

```nybl
use std.iter.{reduce}
let sum = reduce([1, 2, 3, 4], 0, fn(acc, x) { return acc + x })
print(sum)    // 10
```

### `all(arr, pred)`, `any(arr, pred)`

`all` returns `true` when every element passes (vacuously true for empty). `any` returns `true` when at least one element passes.

```nybl
use std.iter.{all, any}
print(all([2, 4, 6], fn(n) { return n % 2 == 0 }))    // true
print(any([1, 3, 4], fn(n) { return n % 2 == 0 }))    // true
```

### `count(arr, pred)`

Count elements for which `pred(x)` is truthy.

```nybl
use std.iter.{count}
print(count([1, 2, 3, 4, 5], fn(n) { return n > 2 }))   // 3
```

### `find(arr, pred)` / `find_index(arr, pred)`

`find` returns the first matching element, or `none` if no match. `find_index` returns the 0-based index, or `-1`.

```nybl
use std.iter.{find, find_index}
print(find([1, 2, 3, 4], fn(n) { return n > 2 }))          // 3
print(find_index([1, 2, 3, 4], fn(n) { return n > 2 }))    // 2
```

## Slicing

### `take(arr, n)`

First `n` elements (or the whole array if it's shorter). Negative `n` yields an empty array.

```nybl
use std.iter.{take}
print(take([1, 2, 3, 4, 5], 3))    // [1, 2, 3]
print(take([1, 2], 10))            // [1, 2]
```

### `drop(arr, n)`

Drop the first `n` elements. Negative `n` yields a full copy; `n >= arr.len()` yields `[]`.

```nybl
use std.iter.{drop}
print(drop([1, 2, 3, 4, 5], 2))    // [3, 4, 5]
```

## Combining

### `zip(a, b)`

Pair elements from two arrays. Stops at the shorter array's length.

```nybl
use std.iter.{zip}
print(zip([1, 2, 3], ["a", "b", "c"]))    // [[1, "a"], [2, "b"], [3, "c"]]
```

### `enumerate(arr)`

Pair each element with its 0-based index.

```nybl
use std.iter.{enumerate}
print(enumerate(["a", "b"]))    // [[0, "a"], [1, "b"]]
```

### `flatten(arr)`

Flatten an array of arrays one level down.

```nybl
use std.iter.{flatten}
print(flatten([[1, 2], [3, 4], [5]]))    // [1, 2, 3, 4, 5]
```

## Reductions

### `sum(arr)`

Sum of a numeric array. Empty → `0`.

```nybl
use std.iter.{sum}
print(sum([1, 2, 3, 4]))     // 10
print(sum([]))               // 0
```

### `product(arr)`

Product of a numeric array. Empty → `1` (multiplicative identity, matching NumPy / Python's `math.prod`).

```nybl
use std.iter.{product}
print(product([2, 3, 4]))    // 24
print(product([]))           // 1
```

### `min_array(arr)` / `max_array(arr)`

Minimum / maximum of a numeric array. Raises on empty input so callers notice.

```nybl
use std.iter.{min_array, max_array}
print(min_array([3, 1, 4, 1, 5]))    // 1
print(max_array([3, 1, 4, 1, 5]))    // 5
```

For pairwise min / max use the `.min()` / `.max()` [numeric methods](/docs/reference/methods/#numeric-methods-int-and-number) instead.
