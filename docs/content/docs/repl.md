+++
title = "REPL"
description = "The `nybl` CLI ships an interactive REPL that carries state across submissions, echoes bare-expression results, supports multi-line input, and persists history across sessions."
weight = 16
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "Error Handling"
path = "/docs/errors/"
[extra.next]
title = "Command-line interface"
path = "/docs/cli/"
+++

# REPL

The `nybl` CLI ships an interactive REPL that carries state across submissions, echoes bare-expression results, supports multi-line input, and persists history across sessions.

```
$ nybl repl
> let x = 5
> let f = fn(n) { return n * x }
> f(7)
35
> :quit
```

## Starting the REPL

```
nybl            # default subcommand is `repl`
nybl repl
```

Ctrl-D on an empty prompt exits. Ctrl-C clears the current line without touching the session.

## What persists across submissions

Everything that shows up in the program's scope:

- `let` / `const` bindings.
- `fn` declarations (named and anonymous-then-let-bound).
- `struct` / `enum` / method declarations.
- `use` imports and module aliases.
- The `rand()` seed — same starting seed, same sequence (helpful for reproducing bugs).

Resource limits (`NyblLimits::standard()` by default) reset per submission, so the step budget doesn't accumulate across lines.

## Bare expressions echo

When the last statement in a submission is a bare expression, the REPL prints its value:

```
> 1 + 2
3
> let x = 5      // `let` has no value — no echo
> x
5
```

`print(...)` returns `none`; the REPL suppresses `none` echoes so `print(42)` only shows `42` once (from the host), not "42" followed by "none".

## Multi-line input

When a line parses with "end of code" — unclosed brace, trailing `+`, unfinished `match` — the REPL keeps the buffer open and prompts for more. Paste a multi-line block and it runs as one submission:

```
> fn greet(name) {
...   return "hi " + name
... }
> greet("Nybl")
hi Nybl
```

A different parse error (typo, unexpected token) submits immediately so you can see the error rather than hunting through a stale buffer.

## Tab completion

Hit Tab on an identifier prefix to see matches from:

- Nybl keywords (`let`, `fn`, `match`, `use`, …)
- Built-in functions (`print`, `range`, `rand`, `try_call`, `panic`)
- Names currently in the session (`let my_var = …` shows up after declaration)
- Identifiers the REPL has seen you type in previous submissions (covers fn parameters, struct field names)

## Meta-commands

Lines starting with `:` are REPL commands, not Nybl code:

| Command | Action |
|---------|--------|
| `:help` | Print the meta-command list |
| `:vars` | List all currently-bound names (sorted) |
| `:reset` / `:clear` | Drop every binding and start fresh |
| `:quit` / `:q` / `:exit` | Exit the REPL |

Unknown commands surface a friendly "try `:help`" hint rather than being silently ignored.

## History

Arrow keys browse history. `~/.nybl_history` (`$USERPROFILE\.nybl_history` on Windows) persists history across sessions. History save on exit is best-effort — the REPL doesn't error if it can't write the file.

## Error handling

Runtime and parse errors render with the same source-snippet + caret as errors from `nybl run`:

```
> let f = fn(n) { return missing(n) }
> f(5)
error: I don't know what 'missing' is
  --> line 1:23
  |
1 | let f = fn(n) { return missing(n) }
  |                        ^
hint: Did you forget to create it with `let`?
```

The error doesn't reset the session — subsequent submissions still see the prior bindings.

## Piped / non-TTY input

When stdin isn't a terminal (piped, heredoc, test harness), the REPL accumulates stdin line by line using the same incomplete-input heuristic, so a script like:

```bash
nybl repl <<EOF
fn double(n) {
  return n + n
}
print(double(21))
EOF
```

prints `42` and exits 0. Errors during a piped session exit 1 but don't abort the remaining input — if you pipe five submissions and #2 fails, #3 through #5 still run. That matches how a user would experience the same sequence interactively.

## Using the session from Rust

`nybl::ReplSession` is the same session type the CLI uses, exposed for embedders that want to drive Nybl as a scripting layer from their own app. See [Embedding → Stateful REPL sessions](/docs/embedding/#stateful-repl-sessions).
