+++
title = "Operators"
description = "All Nybl operators, grouped by category. No operator overloading — each operator works on specific types and produces a runtime error on type mismatch."
weight = 17
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Command-line interface"
path = "/docs/cli/"
[extra.next]
title = "Built-in Functions"
path = "/docs/reference/builtins/"
+++

# Operators

All Nybl operators, grouped by category. No operator overloading — each operator works on specific types and produces a runtime error on type mismatch.

## Arithmetic

| Operator | Name | Operands | Notes |
|----------|------|----------|-------|
| `+` | Add | `int`, `number`, `string`, `array` | String + anything concats after stringifying the other side. Array + array concatenates. |
| `-` | Subtract | `int`, `number` | Also unary negation: `-x` |
| `*` | Multiply | `int`, `number`, or `string * int` | `"ab" * 3` → `"ababab"` |
| `/` | Divide | `int`, `number` | Always returns `number`: `7 / 2` → `3.5`, `6 / 2` → `3` (as number) |
| `%` | Modulo | `int`, `number` | Same sign as the dividend |

```nybl
print(10 + 3)      // 13
print(10 - 3)      // 7
print(10 * 3)      // 30
print(10 / 3)      // 3.3333333333333335
print(10 % 3)      // 1

print("Hello" + " " + "world")    // "Hello world"
print([1, 2] + [3])                // [1, 2, 3]
print("ha" * 3)                    // "hahaha"
```

### Integer results

There is no dedicated integer-division operator. `/` always widens to `number`, which avoids the classic "1 / 2 == 0" footgun. When you need an integer result, coerce the quotient with `.to_int()`:

```nybl
let mid = ((low + high) / 2).to_int()
```

`.to_int()` truncates toward zero; `.abs()` preserves the numeric type.

### Overflow and divide-by-zero

- `int + int`, `int - int`, `int * int` use checked arithmetic — overflow is a runtime error, not silent wrap.
- Division / modulo by zero raises a runtime error.
- `int / number` or `number / int` widens to `number` (IEEE-754 rules; division by 0.0 is still a runtime error in Nybl).

## Comparison

| Operator | Name | Returns |
|----------|------|---------|
| `==` | Equal | `bool` |
| `!=` | Not equal | `bool` |
| `<` | Less than | `bool` |
| `>` | Greater than | `bool` |
| `<=` | Less or equal | `bool` |
| `>=` | Greater or equal | `bool` |

### Equality (`==`, `!=`)

Works on every type. Arrays and dicts compare structurally (element-wise, then entry-wise). User-defined struct and enum values compare by full type identity `(declaring module, type name)` plus their payloads — two structs with the same name declared in different modules are *not* equal even with matching field values.

Opaque host values compare by handle identity. Copies of the same host handle
are equal; separately constructed handles are unequal even when their hidden
Rust payloads would compare equal.

```nybl
print(5 == 5)                   // true
print(5 == "5")                 // false    (different types)
print(1 == 1.0)                 // true     (int ↔ number cross-type)
print([1, 2] == [1, 2])         // true     (structural)
print({"a": 1} == {"a": 1})     // true
```

### Ordering (`<`, `>`, `<=`, `>=`)

Numeric (`int` + `number`, with cross-type widening) and strings (lexicographic) only. Applying an ordering operator to anything else raises a runtime error.

```nybl
print(3 < 5)                    // true
print(1 > 0.5)                  // true     (int > number)
print("abc" < "def")            // true     (lexicographic)
// print([1, 2] < [3])          // error    — can't use `<` with array
```

## Boolean

| Operator | Name | Notes |
|----------|------|-------|
| `&&` | And | Short-circuits |
| `\|\|` | Or | Short-circuits |
| `!` | Not | Unary prefix |

There are no word-spelled aliases — `and`, `or`, `not` are not keywords.

Short-circuiting means the second operand isn't evaluated if the first determines the result:

```nybl
// && stops at the first false
if x > 0 && x < 100 {
  print("In range")
}

// || stops at the first true
if name == "" || name == none {
  print("No name provided")
}

// ! inverts a boolean
if !found {
  print("Still searching...")
}
```

### Truthiness

`false` and `none` are falsy; every other value (including `0`, `""`, `[]`, `{}`) is truthy.

## Assignment

| Operator | Equivalent to |
|----------|--------------|
| `=` | Assign |
| `+=` | `x = x + ...` |
| `-=` | `x = x - ...` |
| `*=` | `x = x * ...` |
| `/=` | `x = x / ...` |
| `%=` | `x = x % ...` |

```nybl
let score = 0
score += 10    // 10
score -= 3     // 7
score *= 2     // 14
```

Assignment targets can be:
- Bare identifiers: `x = ...`
- Index positions: `items[0] = ...`, `dict["key"] = ...`
- Struct fields: `point.x = ...`

Reassigning an all-caps identifier is a parse error ("can't reassign a constant").
The rule follows the base binding through index and field targets, so
`VALUES[0] = ...` and `CONFIG.retries += ...` are also rejected when `VALUES`
or `CONFIG` is a constant.

## Field access / method call

| Operator | What it does |
|----------|-------------|
| `.field` | Read a struct field or enum struct-variant payload field |
| `.method(...)` | Call a builtin method (on arrays / strings / dicts) or a user-declared method |
| `[idx]` | Index into an array / string / dict |

```nybl
let p = Point { x: 3, y: 4 }
print(p.x)               // 3
print(p.sum())           // calls user method `fn Point.sum`
print([1, 2, 3].len())   // 3
print("hello".upper())   // "HELLO"
```

## `try`

`try expr` is a unary prefix that unwraps `Result::Ok(v)` to `v` or propagates `Err(e)` to the enclosing function's caller. See [Error Handling](/docs/errors/).

```nybl
fn parse_and_double(s) {
  let n = try string_to_int(s)    // returns Err early on failure
  return Result::Ok(n * 2)
}
```

## Conditional expressions

Nybl has no ternary operator (`?:`). Use `if/else` as an expression:

```nybl
let label = if count > 3 { "lots" } else { "few" }
```

Both branches are required when `if/else` is used as an expression. The last expression in each branch is the value.

## Precedence

From highest (evaluated first) to lowest:

| Priority | Operators |
|----------|-----------|
| 1 | `.field`, `.method(...)`, `[idx]`, `(args)`, postfix |
| 2 | `!`, `-` (unary), `try` |
| 3 | `*`, `/`, `%` |
| 4 | `+`, `-` |
| 5 | `<`, `>`, `<=`, `>=` |
| 6 | `==`, `!=` |
| 7 | `&&` |
| 8 | `\|\|` |
| 9 | `=`, `+=`, `-=`, `*=`, `/=`, `%=` |

Parentheses override precedence:

```nybl
let result = (1 + 2) * 3    // 9, not 7
```
