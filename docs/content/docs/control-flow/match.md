+++
title = "Pattern Matching"
description = "`match` is an expression: it evaluates a value and runs the first arm whose pattern matches, binding any captured names along the way."
weight = 8
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "break & continue"
path = "/docs/control-flow/break-continue/"
[extra.next]
title = "Arrays"
path = "/docs/data/arrays/"
+++

# Pattern Matching

`match` is an expression: it evaluates a value and runs the first arm whose pattern matches, binding any captured names along the way.

```nybl
let shape = Shape::Circle(3)

let label = match shape {
  Shape::Circle(r)          => "circle with radius {r}",
  Shape::Rectangle { w, h } => "rectangle {w}x{h}",
  Shape::Empty              => "empty",
}
print(label)
```

Arms are tried top to bottom; the first matching arm wins. A match with no matching arm at runtime raises an error — the checker warns at parse time when an enum match misses variants (see [Exhaustiveness](#exhaustiveness)).

## Patterns

### Literal

Matches a specific value:

```nybl
match n {
  0    => "zero",
  1    => "one",
  42   => "the answer",
  _    => "something else",
}
```

Literal patterns use the same cross-type numeric rule as `==` — `1` matches both `Int(1)` and `Number(1.0)`.

### Wildcard `_`

Matches anything, binds nothing. Typical "default" arm.

### Bindings

A lowercase identifier captures the scrutinee value under that name for the arm's guard and body:

```nybl
match request {
  request => handle(request),   // `request` is bound here
}
```

Inside nested patterns, bindings capture the piece they sit at:

```nybl
match pair {
  [first, second] => first + second,
}
```

### Struct patterns

Match a user struct with an exact type identity. Each field pattern runs against the corresponding field's value:

```nybl
struct Point { x, y }
let p = Point { x: 3, y: 4 }

match p {
  Point { x: 0, y: 0 } => "origin",
  Point { x, y }       => "at ({x}, {y})",
}
```

Field patterns can be bindings (`x`), literals (`x: 0`), or any nested pattern.

### Enum variant patterns

```nybl
match shape {
  Shape::Circle(r)              => r * r,
  Shape::Rectangle { w, h }     => w * h,
  Shape::Empty                  => 0,
}
```

Unit, tuple, and struct variants all work. Like struct patterns, field / tuple entries can themselves be patterns.

### Namespaced patterns

Types imported through an aliased `use` must be matched through the same namespace:

```nybl
use paint as p

match c {
  p.Color::Red   => "stop",
  p.Color::Green => "go",
  _              => "?",
}
```

The matcher compares the value's full `(module_path, type_name)` identity against what the alias resolves to — `p.Color::Red` only matches values that came from the `paint` module.

### Array patterns

```nybl
match items {
  []           => "empty",
  [only]       => "one: {only}",
  [a, b]       => "two: {a} and {b}",
  [head, ..]   => "starts with {head}",
  [head, ..tail] => "{head} then {tail}",
}
```

- `[..]` — matches any array, captures nothing.
- `[..name]` — captures the trailing elements as an array.
- Patterns before the rest must all match; the rest is optional.

### Or-patterns

`p1 | p2 | p3` — matches if *any* of the alternatives matches. Each alternative must bind the same set of names so the arm body has a consistent view:

```nybl
match day {
  "Sat" | "Sun" => "weekend",
  _             => "weekday",
}
```

## Guards

An arm can add a boolean guard after `if`. The arm only fires when the pattern matches **and** the guard is true:

```nybl
match n {
  x if x < 0  => "negative",
  0           => "zero",
  x if x < 10 => "small positive",
  _           => "big positive",
}
```

Guards can see any names the pattern bound.

## As an expression

`match` is an expression — every arm's body is an expression, and the whole thing evaluates to the winning arm's body:

```nybl
let grade = match score {
  s if s >= 90 => "A",
  s if s >= 80 => "B",
  s if s >= 70 => "C",
  _            => "F",
}
```

All arms should produce compatible types if you rely on the result — Nybl is dynamically typed, so heterogeneous arms aren't a parse error, but they usually signal a bug.

## Exhaustiveness

Nybl's static checker warns when a `match` over an enum misses variants:

```nybl
enum Color { Red, Green, Blue }

let _ = match Color::Red {
  Color::Red   => "r",
  Color::Green => "g",
  // warning: missing variant `Color::Blue`
}
```

A wildcard or a bare-name catch-all (`_` or `other`) marks the match as exhaustive. Guards don't count toward coverage — a guarded arm covers only the guarded subset, so a partially-guarded match still needs a catch-all.

The checker follows `use` statements when the embedder supplies a module resolver, so imported enums aren't opaque — missing-variant warnings fire on them too.

Exhaustiveness analysis follows the same source-ordered lexical declaration
model as execution. A type is not visible before its declaration, declarations
inside one branch or callable do not leak into siblings, and shadowed or
ambiguous type/module bindings suppress the advisory instead of risking a
false warning. This check is a warning only; a `match` with no matching arm
still raises a runtime error.
