+++
title = "Syntax"
description = "Nybl's syntax is deliberately simple. If you've seen Python or a C-family language, most of it will look familiar — curly braces for blocks, `//` for comments, newlines (rather than semicolons) terminating statements."
weight = 2
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "What's new in 0.4"
path = "/docs/whats-new-0-4/"
[extra.next]
title = "Types"
path = "/docs/basics/types/"
+++

# Syntax

Nybl's syntax is deliberately simple. If you've seen Python or a C-family language, most of it will look familiar — curly braces for blocks, `//` for comments, newlines (rather than semicolons) terminating statements.

## Blocks

Code blocks use curly braces `{ }` and only appear after control-flow or declaration keywords (`if`, `else`, `while`, `for`, `repeat`, `fn`, `struct`, `enum`, `match`):

```nybl
if count > 3 {
  print("That's a lot!")
}
```

> **Important:** The opening `{` must be on the same line as its keyword. Nybl automatically inserts semicolons at the end of lines, so putting `{` on the next line would cause a parse error.

```nybl
// Good
if count > 3 {
  print("Nice!")
}

// Bad — will cause an error
if count > 3
{
  print("Nice!")
}
```

## Statements

Statements end with a newline. Nybl automatically inserts semicolons after lines ending in:

- An identifier or literal
- `true`, `false`, `none`
- `break`, `continue`, `return`
- `)`, `]`, `}`

You can put multiple statements on one line with an explicit semicolon:

```nybl
let x = 1; let y = 2
```

## Comments

Line comments start with `//`. Everything after `//` on that line is ignored:

```nybl
// This is a comment
let x = 5   // So is this
```

There is no block-comment syntax. `#` is **not** a comment leader — a stray `#` produces a lexer error.

## Identifiers

Variable and function names start with a letter or underscore and can contain letters, digits, and underscores:

```nybl
let my_var = 5
let _count = 0
let item3 = "hello"
```

### Case conventions are enforced

Nybl checks the *shape* of every declared name at parse time and rejects mismatches with a suggestion:

| Declaration | Required shape | Examples |
|-------------|----------------|----------|
| `let`, `fn`, parameters, fields, aliases, `for`-loop vars, match bindings | starts with a lowercase letter or `_` | `let x`, `fn double(n)`, `for i in …` |
| `const` | ALL_CAPS (+ digits / `_`) | `const PI = 3.14`, `const MAX_SIZE = 100` |
| `struct`, `enum`, variant names | starts with an uppercase letter | `struct Point`, `enum Shape { Circle, Rect }` |

Single-letter types like `enum Dir { N, E, S, W }` are fine — the rule is "starts with an uppercase letter", not "must have a lowercase character somewhere."

A leading underscore (`_foo`, `_Internal`, `_DEBUG`) marks a name as "private by convention" — glob `use` imports skip them. The wildcard `_` on its own is used in `let _ = foo()` (explicitly ignore) and in patterns (match anything).

## Whitespace

Spaces and tabs are insignificant — use whatever indentation style you like. Only newlines matter (they end statements).

## Keywords

These words are reserved and can't be used as identifiers:

```
let const pub fn return
if else while for in repeat break continue
use as struct enum match try
true false none
```

There are no word-spelled logical operators — use `&&`, `||`, `!` rather than `and`, `or`, `not`.
