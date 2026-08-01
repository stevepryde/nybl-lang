+++
title = "Structs & Enums"
description = "Nybl supports user-defined **struct** and **enum** types, with methods attached via `fn Type.method(self, ...)`. They give you type names in error messages, structural pattern matching, and an identity-aware equality."
weight = 12
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Dictionaries"
path = "/docs/data/dictionaries/"
[extra.next]
title = "Defining Functions"
path = "/docs/functions/defining-functions/"
+++

# Structs & Enums

Nybl supports user-defined **struct** and **enum** types, with methods attached via `fn Type.method(self, ...)`. They give you type names in error messages, structural pattern matching, and an identity-aware equality.

## Structs

A `struct` is a named record — a set of fields in a declared order. Field names are the part that matters; types aren't declared.

```nybl
struct Point { x, y }
struct Player { name, hp, inventory }
```

Create a value with `TypeName { field: value, ... }`:

```nybl
let p = Point { x: 3, y: 4 }
print(p)                  // Point { x: 3, y: 4 }
print(p.x)                // 3
print(p.type())           // "struct"
```

Construction is **strict**: fields you provide must match the declaration exactly (no unknown fields, no duplicates, no missing ones). Extra fields or typos become parse or runtime errors with a "did you mean?" suggestion.

### Field access and assignment

Read with `.field`, write with `.field = value` or any compound assignment:

```nybl
let c = Counter { n: 10 }
c.n += 5                 // works
c.n *= 2                 // works
print(c.n)               // 30
```

The field has to already exist; assigning to an undeclared field is an error.

### Passing structs

Structs follow Nybl's copy-by-value rule: passing one to a function, returning it, or assigning it to another variable makes an independent copy. Mutating the copy leaves the original alone.

```nybl
fn grow(p) { p.x += 10; return p }

let a = Point { x: 1, y: 2 }
let b = grow(a)
print(a)                 // Point { x: 1, y: 2 }
print(b)                 // Point { x: 11, y: 2 }
```

## Enums

An `enum` is a tagged union — one of several named variants, each with an optional payload:

```nybl
enum Shape {
  Circle(r),
  Rectangle { w, h },
  Empty,
}
```

Variants come in three shapes:

| Shape | Declaration | Construction |
|-------|-------------|--------------|
| Unit | `Empty` | `Shape::Empty` |
| Tuple | `Circle(r)` | `Shape::Circle(5)` |
| Struct | `Rectangle { w, h }` | `Shape::Rectangle { w: 4, h: 3 }` |

```nybl
let a = Shape::Circle(5)
let b = Shape::Rectangle { w: 4, h: 3 }
let c = Shape::Empty
print(a.type())          // "enum"
```

Variants with a struct payload expose their fields via `.field` just like structs:

```nybl
let r = Shape::Rectangle { w: 4, h: 3 }
print(r.w * r.h)         // 12
```

Short-name variants like `enum Dir { N, E, S, W }` are accepted — the case rule is "starts with an uppercase letter", not "must contain a lowercase".

## Methods

Attach a method to a type with `fn Type.method(self, ...) { ... }`. The receiver arrives as the first parameter (called `self` by convention; any name works).

```nybl
struct Point { x, y }

fn Point.sum(self)      { return self.x + self.y }
fn Point.moved(self, dx, dy) {
  return Point { x: self.x + dx, y: self.y + dy }
}

let p = Point { x: 3, y: 4 }
print(p.sum())                   // 7
print(p.moved(1, 1))             // Point { x: 4, y: 5 }
```

For enums, methods dispatch on the enum type — not per-variant:

```nybl
enum Shape { Circle(r), Rectangle { w, h } }

fn Shape.area(self) {
  return match self {
    Shape::Circle(r)          => 3.14159 * r * r,
    Shape::Rectangle { w, h } => w * h,
  }
}

print(Shape::Circle(3).area())                  // 28.27431
print(Shape::Rectangle { w: 4, h: 3 }.area())   // 12
```

A user-declared method with the same name as a builtin (`len`, `keys`, etc.) **wins** over the builtin for receivers of that type — the precedence matches the walker, VM, and AOT.

### Methods must live in the type's own module

Methods can only be declared on structs and enums you own — that is, types declared in the *same module* as the method. Concretely:

- You **cannot** extend a type imported from another module with new methods. If `paint` declares `struct Color { ... }`, then `fn Color.brighten(self)` must also live in `paint`, not in a consumer.
- You **cannot** add methods to the built-in types (`int`, `number`, `string`, `bool`, `array`, `dict`, `fn`, `module`, `iter`) or to the engine-registered types `Result`, `RuntimeError`, `Iter`.

Declarations that violate this rule parse fine but never dispatch — the method registers against `(your_module, TypeName)`, while values of that type carry their original home module in their identity, so lookups miss. If you catch yourself wanting to "just add a helper to `Array`" or "extend `Result` with a domain combinator," write a free function that takes the value as an argument instead:

```nybl
// Not this (declared in some consumer module):
//   fn Result.tag(self, label) { ... }   // ghost method — never fires
// Do this:
fn tag(r, label) {
  return match r {
    Ok(v)  => "{label}: ok {v}",
    Err(e) => "{label}: err {e}",
  }
}
```

This is the same discipline Go enforces: a type's behaviour lives where the
type is declared. It keeps dispatch ownership coherent at the cost of the
extensions you'd get in Swift / Kotlin / Ruby. Within the owning module,
execution order still determines when a method becomes available.

### Value and mutable receivers

An ordinary `self` receiver is a read-only value snapshot. Use it for queries
and functional transformations that return a new value:

```nybl
fn Point.shift(self, dx, dy) {
  return Point { x: self.x + dx, y: self.y + dy }
}

let p = Point { x: 0, y: 0 }
p = p.shift(3, 4)        // p is now Point { x: 3, y: 4 }
```

This plays well with fluent chains:

```nybl
let final = Point { x: 0, y: 0 }
  .shift(1, 0)
  .shift(0, 2)
  .shift(5, 5)
```

To update the caller's binding in place, declare the first parameter as
`ref self`:

```nybl
fn Point.shift_in_place(ref self, dx, dy) {
  self.x += dx
  self.y += dy
}

let p = Point { x: 0, y: 0 }
p.shift_in_place(3, 4)
print(p)    // Point { x: 3, y: 4 }
```

The call site does not write another `ref`: method syntax supplies the receiver
implicitly. A mutable receiver may be a field/index place rooted in a `let`
binding, and it uses the same transactional copy-in/copy-out rules as an explicit [`ref`
parameter](/docs/functions/reference-parameters/). A normal return commits it;
an error rolls it back.

Assigning to `self`, `self.field`, or `self[index]` in a value-receiver method
is a parse error with a hint to use `ref self`. This prevents a method from
silently changing a discarded copy.

## Equality

Two struct or enum values are equal when their **full type identity** — the module they were declared in plus the type name — *and* every payload matches structurally:

```nybl
let p = Point { x: 1, y: 2 }
let q = Point { x: 1, y: 2 }
print(p == q)            // true (structural)

let r = Point { x: 1, y: 3 }
print(p == r)            // false (field differs)
```

Two types with the same name declared in different modules are **distinct** — see [Modules](/docs/modules/#type-identity).

## Redeclaring the same shape is fine

Declaring the exact same struct or enum twice inside one module is a no-op — matches the "idempotent re-import" rule `use` already follows. Declaring two different shapes with the same name in the same module is a hard error.

## Declarations execute in source order

Struct, enum, and method declarations take effect when execution reaches their
statement; they are not hoisted. A declaration in a branch that is not taken
does not run, and a declaration inside a function does not run until that
function is called:

```nybl
struct Box { value }
let box = Box { value: 4 }

if true {
  fn Box.read(self) { return self.value }
}

print(box.read())
```

Nested type names follow ordinary lexical scope. Runtime validation also
happens when the declaration executes, so an invalid declaration in dead code
does not fail a program that never reaches it.

Methods are installed in their type's declaring module when their declaration
executes. A method declared by a called function remains installed after that
function returns, and the last executed declaration for the same method name
determines its body and arity. This is intentionally dynamic; use top-level
declarations for the least surprising API.
