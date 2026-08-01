+++
title = "Grammar"
description = "An informal grammar for the Nybl language, plus the complete list of reserved words."
weight = 20
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Methods"
path = "/docs/reference/methods/"
[extra.next]
title = "Overview"
path = "/docs/stdlib/"
+++

# Grammar

An informal grammar for the Nybl language, plus the complete list of reserved words.

## Reserved words

```
let const pub ref fn return
if else while for in repeat break continue
use as struct enum match try
true false none
```

These can't be used as variable or function names. There are no word-spelled logical operators — use `&&`, `||`, `!` rather than `and`, `or`, `not`.

## Built-in functions

Always in scope; can't be shadowed by user fns. See [Built-in Functions](/docs/reference/builtins/) for details.

| Function | Returns | Description |
|----------|---------|-------------|
| `print(args...)` | none | Host-captured output |
| `range(n)` / `range(s, e)` / `range(s, e, step)` | array | Integer range |
| `rand(n)` | int | Pseudo-random `0..n` |
| `try_call(f)` | Result | Run `f`, return `Ok(v)` or `Err(RuntimeError)` |
| `panic(message)` | never returns | Raise a non-fatal runtime error carrying `message` |

All math and conversion operations are [methods on values](/docs/reference/methods/): `x.type()`, `x.to_str()`, `x.to_int()`, `x.to_float()`, `x.abs()`, `a.min(b)`, `x.sqrt()`, `x.len()`, etc.

## Grammar

Informal EBNF. Whitespace is insignificant *except* for newlines, which auto-insert semicolons (see below).

```
program     = statement*
statement   = letDecl | constDecl | assign | ifStmt | whileStmt | repeatStmt
            | forStmt | fnDecl | returnStmt | breakStmt | continueStmt
            | useStmt | publicSurface | structDecl | enumDecl | methodDecl
            | exprStmt

letDecl     = "let" IDENT "=" expr
constDecl   = "const" IDENT "=" expr
assign      = target ("=" | "+=" | "-=" | "*=" | "/=" | "%=") expr
target      = IDENT | postfix "[" expr "]" | postfix "." IDENT

ifStmt      = "if" expr block ("else" "if" expr block)* ("else" block)?
whileStmt   = "while" expr block
repeatStmt  = "repeat" expr block
forStmt     = "for" IDENT "in" expr block
fnDecl      = "pub"? "fn" IDENT "(" params? ")" block
returnStmt  = "return" expr?
breakStmt   = "break"
continueStmt = "continue"

useStmt     = "use" path
            | "use" path "." "{" IDENT ("," IDENT)* "}"
            | "use" path "as" IDENT
            | "use" path "." "{" IDENT ("," IDENT)* "}" "as" IDENT
path        = IDENT ("." IDENT)*
publicSurface = "pub" "{" (IDENT ("," IDENT)* ","?)? "}"

structDecl  = "struct" IDENT "{" fields? "}"
fields      = IDENT ("," IDENT)*
enumDecl    = "enum" IDENT "{" variants? "}"
variants    = variant ("," variant)*
variant     = IDENT                                             // unit
            | IDENT "(" IDENT ("," IDENT)* ")"                  // tuple
            | IDENT "{" IDENT ("," IDENT)* "}"                  // struct
methodDecl  = "fn" IDENT "." IDENT "(" params ")" block

exprStmt    = expr
block       = "{" statement* "}"

expr        = or
or          = and ("||" and)*
and         = equality ("&&" equality)*
equality    = comparison (("==" | "!=") comparison)*
comparison  = addition (("<" | ">" | "<=" | ">=") addition)*
addition    = multiply (("+" | "-") multiply)*
multiply    = unary (("*" | "/" | "%") unary)*
unary       = ("!" | "-" | "try") unary | postfix
postfix     = primary (call | index | field | method | structLit | variantCtor)*
call        = "(" args? ")"
index       = "[" expr "]"
field       = "." IDENT
method      = "." IDENT "(" args? ")"
structLit   = "{" (IDENT ":" expr ("," IDENT ":" expr)*)? "}"   // only at expr position
variantCtor = "::" IDENT payload?
payload     = "(" expr ("," expr)* ")"                          // tuple variant
            | "{" IDENT ":" expr ("," IDENT ":" expr)* "}"      // struct variant

primary     = INT | NUMBER | STRING | "true" | "false" | "none"
            | IDENT | resultShorthandExpr | "(" expr ")" | arrayLit | dictLit
            | ifExpr | matchExpr | fnExpr

resultShorthandExpr = ("Ok" | "Err") "(" expr ("," expr)* ")"  // sugar for Result::Ok/Err

arrayLit    = "[" (expr ("," expr)* ","?)? "]"
dictLit     = "{" (STRING ":" expr ("," STRING ":" expr)* ","?)? "}"
ifExpr      = "if" expr "{" expr "}" "else" "{" expr "}"
matchExpr   = "match" expr "{" arm ("," arm)* ","? "}"
arm         = pattern ("if" expr)? "=>" expr
fnExpr      = "fn" "(" params? ")" block

pattern     = orPattern
orPattern   = singlePattern ("|" singlePattern)*
singlePattern = "_" | IDENT | literal | variantPattern | structPattern | arrayPattern
              | resultShorthand
variantPattern = (IDENT ".")? IDENT "::" IDENT
              | (IDENT ".")? IDENT "::" IDENT "(" pattern ("," pattern)* ")"
              | (IDENT ".")? IDENT "::" IDENT "{" IDENT ":" pattern ("," IDENT ":" pattern)* "}"
resultShorthand = ("Ok" | "Err") "(" pattern ("," pattern)* ")"  // sugar for Result::Ok/Err
structPattern  = (IDENT ".")? IDENT "{" IDENT ":" pattern ("," IDENT ":" pattern)* "}"
arrayPattern   = "[" patternList? arrayRest? "]"
patternList    = pattern ("," pattern)*
arrayRest      = ".." | ".." IDENT

params      = fixedParam ("," fixedParam)* ("," restParam)? | restParam
fixedParam  = "ref"? IDENT
restParam   = ".." IDENT
args        = arg ("," arg)*
arg         = "ref"? expr
```

`INT` is an exact signed 64-bit integer after unary parsing. Decimal magnitudes through `9223372036854775807` are ordinary primary expressions; the boundary spelling `-9223372036854775808` is accepted when unary `-` directly owns that magnitude, including in literal patterns. A bare `9223372036854775808`, `0 - 9223372036854775808`, or any larger magnitude is out of range rather than being converted to a floating-point `number`.

`pub fn` is accepted only on a named function at the direct program root and
marks an entry for the stateful embedding ABI. Separately, a direct module-root
`pub { name, Type }` statement declares that module's importable allow-list.
It is inert in the directly executed root.

`ref` marks copy-in/copy-out parameters and normally appears at the same
positional argument at the call site. A method's first parameter is the
exception: `fn Point.move(ref self, dx) { ... }` is called as `point.move(dx)`
because method syntax supplies the receiver reference implicitly. Although the
grammar accepts an expression after an argument marker so parsing stays
independent of the dynamic callee, semantic validation requires a mutable
field/index place rooted in an uncaptured `let` binding. Ordinary method receivers are read-only, and
assigning through one is a parse error. See
[Reference Parameters](/docs/functions/reference-parameters/).

A final `..rest` parameter collects zero or more extra value arguments into an
array. It is declaration-only, value-only, and must be the last parameter.

`methodDecl`, enum variant `IDENT`s, and `struct` names must start with an
uppercase letter. `IDENT` bound by `let`, `fn`, parameters, `for`, etc. must
start with lowercase or `_`. `const` names must be all-caps. Mis-shaped
declarations parse-error with a "did you mean?" suggestion — see
[Variables](/docs/basics/variables/#name-shapes-are-checked).

## Automatic semicolons

Nybl automatically inserts a semicolon at the end of a line if the last token is one of:

- An identifier or literal (`int`, `number`, `string`)
- `true`, `false`, `none`
- `break`, `continue`, `return`
- `)`, `]`, `}`

Newlines do not insert semicolons while the innermost open delimiter is `(` or `[`. This makes calls, conditions, array literals, and index expressions safe to lay out over multiple lines. A newline immediately before a closing `)`, `]`, or `}` is also ignored, so the final item in a multiline literal does not require a trailing comma.

Braces remain statement-capable even when their block is nested inside parentheses or brackets. Newlines between statements in a function or control-flow block still insert semicolons:

```nybl
let callbacks = [
  fn() {
    let x = 1
    return x
  },
]
```

A line starting with `.` continues a preceding value, including across blank lines or comments:

```nybl
let size = values
  // Continue the same expression.
  .len()
  .to_str()
```

`return` itself remains a semicolon trigger. A newline immediately after it therefore means a bare `return` whose value is `none`; put the value on the same line, or open a parenthesized expression on that line when it needs multiline layout:

```nybl
return (
  left +
  right
)
```

This means the opening `{` of a block must be on the same line as its keyword:

```nybl
// Correct
if x > 3 {
  print("yes")
}

// Wrong — semicolon inserted after "3"
if x > 3
{
  print("yes")
}
```

You can also separate statements on the same line with an explicit `;`:

```nybl
let x = 1; let y = 2
```

## Comments

`//` starts a line comment — everything to the end of line is ignored:

```nybl
// Whole-line comment
let x = 5   // Inline trailing comment
```

There's no block-comment syntax.

## String interpolation

Inside double-quoted strings, `{identifier}` inserts the value of a variable. Only plain variable names are allowed — no expressions, operators, or function calls:

```nybl
let name = "Alice"
print("Hello, {name}!")     // works
// print("Hello, {1 + 2}!")  // error — expressions not allowed
```

Use `\{` and `\}` for literal braces in strings. Other supported escapes: `\"`, `\\`, `\n`, `\t`, `\r`.
