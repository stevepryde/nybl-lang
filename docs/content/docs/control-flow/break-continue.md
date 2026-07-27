+++
title = "break & continue"
description = "`break` and `continue` give you finer control over loops."
weight = 7
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Loops"
path = "/docs/control-flow/loops/"
[extra.next]
title = "Pattern Matching"
path = "/docs/control-flow/match/"
+++

# break & continue

`break` and `continue` give you finer control over loops.

## break

Exits the loop immediately. Execution continues after the loop:

```nybl
let i = 0
while true {
  if i >= 10 {
    break
  }
  i += 1
}
print("Stopped at " + i.to_str())
```

### Searching for something

```nybl
let numbers = [4, 8, 15, 16, 23, 42]
let found = false
for n in numbers {
  if n > 20 {
    found = true
    break
  }
}

if found {
  print("Found a number greater than 20!")
} else {
  print("No number greater than 20.")
}
```

## continue

Skips the rest of the current iteration and jumps to the next one:

```nybl
for i in range(10) {
  if i % 2 == 0 {
    continue
  }
  print(i.to_str())    // 1, 3, 5, 7, 9
}
```

### Filter and process

```nybl
let words = ["hello", "", "world", "", "nybl"]
for word in words {
  if word == "" {
    continue
  }
  print(word.upper())
}
```

## Which loops support break and continue?

All three loop types — `while`, `for...in`, and `repeat` — support both `break` and `continue`.

```nybl
repeat 10 {
  let n = rand(100)
  if n < 10 {
    break
  }
  print(n.to_str())
}
```

> **Note:** `break` and `continue` only affect the innermost loop. If you have nested loops and want to exit the outer one, use a variable flag or a function with `return`.
