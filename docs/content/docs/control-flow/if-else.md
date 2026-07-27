+++
title = "if / else"
description = "The `if` statement lets your program make decisions based on conditions."
weight = 5
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Variables"
path = "/docs/basics/variables/"
[extra.next]
title = "Loops"
path = "/docs/control-flow/loops/"
+++

# if / else

The `if` statement lets your program make decisions based on conditions.

## Basic if

```nybl
if count > 3 {
  print("That's a lot!")
}
```

The condition does **not** need parentheses, but they're allowed: `if (x > 3) { ... }`. Braces are always required.

## if / else

```nybl
if temperature > 30 {
  print("It's hot!")
} else {
  print("Not too bad.")
}
```

## if / else if / else

Chain multiple conditions with `else if`:

```nybl
if score > 90 {
  print("Excellent!")
} else if score > 70 {
  print("Good job!")
} else if score > 50 {
  print("Not bad!")
} else {
  print("Keep trying!")
}
```

Only the first matching branch runs. If none match and there's an `else`, that branch runs.

## if as an expression

`if/else` can produce a value when used in expression position (e.g., after `=`):

```nybl
let label = if count > 3 { "lots" } else { "few" }
print("You have {label} of items")
```

When used as an expression, both `if` and `else` branches are required. The last expression in each branch is the value.

```nybl
let message = if x > 0 {
  "positive"
} else {
  "non-positive"
}
print(message)
```

## Common patterns

### Guard clause

```nybl
fn process(value) {
  if value == none {
    return
  }
  print("Processing: " + value.to_str())
}
```

### Classify a value

```nybl
fn classify(n) {
  if n > 0 {
    return "positive"
  } else if n < 0 {
    return "negative"
  } else {
    return "zero"
  }
}
```

### Combine conditions with `&&` and `||`

```nybl
if age >= 18 && has_ticket {
  print("Welcome in!")
}

if x < 0 || x > 100 {
  print("Out of range!")
}
```
