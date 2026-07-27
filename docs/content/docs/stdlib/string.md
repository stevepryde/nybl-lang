+++
title = "std.string"
description = "Formatting and character-level helpers that don't fit the method-on-string pattern."
weight = 25
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "std.collections"
path = "/docs/stdlib/collections/"
[extra.next]
title = "std.json"
path = "/docs/stdlib/json/"
+++

# std.string

Formatting and character-level helpers that don't fit the method-on-string pattern.

> The core string operations — `.len()`, `.trim()`, `.upper()`, `.split()`, `.contains()`, `.slice()`, `.replace()`, `.to_int()`, `.to_float()`, etc. — are [methods on strings](/docs/reference/methods/#string-methods-string). This module adds things that are awkward as methods.

## Import

```nybl
use std.string
use std.string.{pad_left, center}
use std.string as str
```

## Padding

### `pad_left(s, width, ch)` / `pad_right(s, width, ch)`

Pad `s` on the left (or right) with `ch` until it reaches `width` total characters. Already-long strings are returned unchanged. `ch` should be a single-character string.

```nybl
use std.string.{pad_left, pad_right}
print(pad_left("42", 5, "0"))      // "00042"
print(pad_right("hi", 6, "."))     // "hi...."
```

### `center(s, width, ch)`

Centre `s` inside `width` chars using `ch` as filler. Odd leftover space goes on the right (matches Python's `str.center`).

```nybl
use std.string.{center}
print(center("OK", 6, "-"))     // "--OK--"
print(center("OK", 7, "-"))     // "--OK---"
```

## Character-level

### `chars(s)`

Split `s` into an array of single-character strings, preserving order.

```nybl
use std.string.{chars}
print(chars("abc"))    // ["a", "b", "c"]
```

### `reverse(s)`

Reverse the character sequence. Works on ASCII and UTF-8 (iterates by code point).

```nybl
use std.string.{reverse}
print(reverse("hello"))    // "olleh"
```

### `is_palindrome(s)`

`true` when `s` reads the same forwards and backwards. Case-sensitive — lowercase the input first if you need case-insensitive comparison.

```nybl
use std.string.{is_palindrome}
print(is_palindrome("racecar"))    // true
print(is_palindrome("Racecar"))    // false
```

## Other helpers

### `count(s, needle)`

Count non-overlapping occurrences of `needle` in `s`. An empty `needle` returns `0` (avoids the "infinite matches at every position" ambiguity).

```nybl
use std.string.{count}
print(count("banana", "a"))      // 3
print(count("aaaa", "aa"))       // 2  (non-overlapping)
```

### `join(arr, sep)`

Join an array of strings with `sep`. This is a thin wrapper around the built-in `arr.join(sep)` [array method](/docs/reference/methods/#array-methods-array) — it's here so users starting from `std.string` don't have to look elsewhere.

```nybl
use std.string.{join}
print(join(["a", "b", "c"], "-"))    // "a-b-c"
```
