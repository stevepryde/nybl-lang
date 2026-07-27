+++
title = "std.json"
description = "RFC-8259 JSON `parse` and `stringify`, implemented in pure Nybl."
weight = 26
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "std.string"
path = "/docs/stdlib/string/"
[extra.next]
title = "std.test"
path = "/docs/stdlib/test/"
+++

# std.json

RFC-8259 JSON `parse` and `stringify`, implemented in pure Nybl.

Performance is reasonable for scripting workloads (config files, API payloads up to a few MB). A C-backed parser would be faster but would pull `nybl-std` out of its zero-Rust-dep contract.

## Import

```nybl
use std.json                       // glob
use std.json.{parse, stringify}    // selective
use std.json as j                  // aliased
```

## `stringify(value)`

Emit `value` as JSON text. Nybl values that have no JSON analogue (`fn`, `struct`, `enum`) raise a runtime error — strip them out before calling.

```nybl
use std.json.{stringify}

print(stringify(42))                           // "42"
print(stringify("hello"))                      // "\"hello\""
print(stringify([1, 2, 3]))                    // "[1,2,3]"
print(stringify({"name": "Alice", "age": 30})) // '{"name":"Alice","age":30}'
print(stringify(true))                         // "true"
print(stringify(none))                         // "null"
```

Strings are escaped for the five characters JSON requires (`"`, `\`, `\n`, `\r`, `\t`). Other control characters under 0x20 are not currently escaped — fine for the common cases, but a known gap if you're stringifying raw binary.

## `parse(text)`

Parse JSON text into a Nybl value. Parse errors raise a runtime error with a position marker.

```nybl
use std.json.{parse}

print(parse("42"))                       // 42
print(parse("\"hello\""))                // "hello"
print(parse("[1, 2, 3]"))                // [1, 2, 3]
print(parse("{\"name\": \"Alice\"}"))    // {"name": "Alice"}
print(parse("null"))                     // none
```

### Mapping

JSON type | Nybl type
--- | ---
number (integer) | `int`
number (decimal / exponent) | `number`
string | `string`
boolean | `bool`
null | `none`
array | `array`
object | `dict` (keys must be strings, which JSON already requires)

### Catching parse errors

`parse` raises on malformed input. Wrap in `try_call` if you want a `Result`-shaped outcome:

```nybl
use std.json.{parse}

let r = try_call(fn() { return parse("\{broken") })
match r {
  Result::Ok(v)  => print("parsed: " + v.to_str()),
  Result::Err(e) => print("parse failed: " + e.message),
}
// parse failed: ...
```

(The leading `\{` escapes the `{` so Nybl doesn't mistake it for a string-interpolation marker.)

### Known gaps

- `\b` / `\f` escapes are rejected. Rare in real payloads.
- `\uXXXX` escapes are rejected — they'd need 4-hex parsing plus code-point-to-UTF-8 conversion, which is nontrivial in pure Nybl.

Both raise a clear "unsupported escape" error so you know what happened.
