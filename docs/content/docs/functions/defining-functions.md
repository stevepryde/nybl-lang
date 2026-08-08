+++
title = "Defining Functions"
description = "Functions let you name a sequence of actions and reuse it. Nybl has first-class functions — you can store them in variables, pass them to other functions, return them from other functions, and stash them in arrays."
weight = 13
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Structs & Enums"
path = "/docs/data/structs-and-enums/"
[extra.next]
title = "Reference Parameters"
path = "/docs/functions/reference-parameters/"
+++

# Defining Functions

Functions let you name a sequence of actions and reuse it. Nybl has first-class functions — you can store them in variables, pass them to other functions, return them from other functions, and stash them in arrays.

## Declaring a function

```nybl
fn greet() {
  print("Hello!")
}

greet()    // "Hello!"
greet()    // "Hello!"
```

### Public host entry points

At the direct root of a program, `pub fn` marks a function as callable through
a stateful embedder's `NyblInstance` ABI:

```nybl
let visits = 0

pub fn visit(name) {
  visits += 1
  return "hello {name}; visit {visits}"
}
```

`pub` does not change how Nybl code resolves or calls the function. It is
metadata for the host API, and is rejected on nested functions, methods, and
function expressions. Only declarations that execute during instance loading
are exposed; see [Stateful instances](/docs/embedding/instances/#declaring-host-entry-points)
for redeclaration and entry-order rules.

The host API passes owned values, not Nybl variable bindings. A public function
with a `ref` parameter can still be called normally from Nybl source, but
`NyblInstance::call` rejects it because the host cannot supply a referenceable
target. The same value-only rule applies when the host invokes a returned
function through `call_value`.

### Shadowing engine builtins

An executed lexical function declaration shadows an engine builtin with the
same name, just like a `let`-bound callable does:

```nybl
fn rand(max) {
  return 0
}

print(rand(10)) // 0, from the user function
```

This applies to `range`, `rand`, `print`, `try_call`, and `panic`. Declarations
take effect when execution reaches them, so an earlier call still resolves to
the builtin. A host deny list therefore allows a shadowing user function but
still rejects any source-ordered call that actually reaches the disabled
builtin.

## Parameters

Functions can take parameters — values you pass in when calling:

```nybl
fn repeat_string(text, times) {
  let result = ""
  repeat times {
    result += text
  }
  return result
}

print(repeat_string("ha", 3))    // "hahaha"
```

Parameters are positional. There are no default values or type annotations.

### Rest parameters

A final `..name` parameter collects any remaining positional arguments into an
array. It accepts zero or more values and works on named functions, function
expressions, and methods:

```nybl
fn collect(first, ..rest) {
  return [first, rest]
}

print(collect(1))          // [1, []]
print(collect(1, 2, 3))    // [1, [2, 3]]
```

The rest parameter must be last and cannot be `ref`. Fixed `ref` parameters
may precede it, but every collected argument is value-only. Public instance
entry points with a rest parameter expose their fixed parameter count as the
minimum accepted arity.

### Reference parameters

Use a `ref` parameter when a function should replace one of the caller's
variables. The marker is required in both the declaration and the call, so the
mutation is visible at each site:

```nybl
fn grow(ref items, count) {
  repeat count {
    items.push(0)
  }
}

let values = []
grow(ref values, 3)
print(values)    // [0, 0, 0]
```

Refs use transactional copy-in/copy-out rather than observable aliases. Read
[Reference Parameters](/docs/functions/reference-parameters/) for valid
targets, atomic commit and rollback, forwarding, evaluation order, closure and
method rules, diagnostics, and the value-only Rust host boundary.

## Return values

Use `return` to send a value back from the function:

```nybl
fn double(x) {
  return x * 2
}

let result = double(5)
print(result)    // 10
```

`return` with no value (or reaching the end of the function) returns `none`:

```nybl
fn do_something() {
  print("Working...")
  // no return — returns none
}

let result = do_something()
print(result)    // none
```

## Early return

`return` exits the function immediately, even from inside loops or conditionals:

```nybl
fn find_first_big(numbers, threshold) {
  for n in numbers {
    if n > threshold {
      return n
    }
  }
  return none
}

let result = find_first_big([3, 7, 1, 15, 4], 10)
print(result)    // 15
```

## Calling functions

Parentheses are always required, even with no arguments:

```nybl
greet()          // correct
// greet         // error — 'greet' is a function, call it with greet()
```

## Practical example: sum of squares

```nybl
fn sum_of_squares(n) {
  let total = 0
  for i in range(1, n + 1) {
    total += i * i
  }
  return total
}

let result = sum_of_squares(5)
print("Sum of squares: {result}")    // Sum of squares: 55
```

## Recursion

Functions can call themselves. Nybl caps recursion depth to prevent runaway stacks (also bounded by the step limit):

```nybl
fn factorial(n) {
  if n <= 1 {
    return 1
  }
  return n * factorial(n - 1)
}

print(factorial(5))    // 120
```

## First-class functions

A named `fn` is a value just like anything else — you can assign it to a variable, pass it as an argument, return it from another function, or store it in a collection:

```nybl
fn double(x) { return x * 2 }
let f = double
print(f(7))          // 14

fn apply(f, x) { return f(x) }
print(apply(double, 21))    // 42
```

### Function expressions (lambdas)

`fn(...) { ... }` — without a name — is an expression that produces a function value. Use it when you want a one-off function inline:

```nybl
let square = fn(x) { return x * x }
print(square(6))     // 36

let mul = fn(a, b) { return a * b }
print([mul(2, 3), mul(4, 5)])    // [6, 20]
```

### Closures

Function expressions capture lexical locals from the enclosing scope. Those
local captures are a **snapshot** taken when the closure is built — mutating a
captured local afterwards doesn't change what the closure sees:

```nybl
let n = 5
let add_n = fn(x) { return x + n }
n = 100
print(add_n(3))      // 8, not 103
```

The classic "factory returning a specialised function" pattern works:

```nybl
fn make_adder(n) {
  return fn(x) { return x + n }
}

let add5 = make_adder(5)
let add10 = make_adder(10)
print(add5(3))       // 8
print(add10(3))      // 13
```

A function expression created directly at the program root also snapshots the
root bindings it uses. There is one important stateful-embedding distinction:
a callback created while a named function is running keeps snapshot-captured
locals, but names resolved from that function's defining module remain live.
That lets a callback returned from `NyblInstance::call` observe later updates to
the same instance's module globals:

```nybl
let count = 0

pub fn make_counter(step) {
  return fn() {
    count += step
    return count
  }
}
```

Here `step` is the callback's captured local and `count` is live instance
state. The callback must be invoked through the same instance that created it.

### Recursion in lambdas

An anonymous `fn(...)` can't see itself by name. If a lambda needs to recurse, assign it to a named `fn` instead — named fns are visible inside their own body:

```nybl
fn fib(n) {
  if n < 2 { return n }
  return fib(n - 1) + fib(n - 2)
}
print(fib(10))       // 55
```
