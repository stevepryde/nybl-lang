//! Nybl language core and tree-walking runtime.
//!
//! Nybl is a small, dynamically typed scripting language designed to be
//! embedded in Rust applications. The core runtime has no ambient access to
//! the filesystem, network, environment, clock, or standard I/O: a program
//! can only reach capabilities exposed through [`NyblHost`].
//!
//! # Choose an execution API
//!
//! - [`run`] parses and executes one isolated program with the tree-walker.
//! - [`NyblInstance`] loads a program once and lets a host repeatedly call
//!   root-level `pub fn` entry points while program state remains live.
//! - [`ReplSession`] retains declarations across source submissions and is
//!   intended for interactive tools.
//! - [`parse`], [`parse_with_warnings`], and
//!   [`parse_with_warnings_and_resolver`] expose the parser and static
//!   diagnostics without executing the program.
//!
//! The [`nybl-vm`](https://docs.rs/nybl-vm) crate provides a faster bytecode
//! implementation with equivalent one-shot and persistent APIs.
//! [`nybl-compile`](https://docs.rs/nybl-compile) transpiles the same language
//! to Rust.
//!
//! # One-shot embedding
//!
//! ```
//! use nybl::{NyblError, NyblHost, NyblLimits, Value};
//!
//! struct Host {
//!     output: Vec<String>,
//! }
//!
//! impl NyblHost for Host {
//!     fn call(
//!         &mut self,
//!         name: &str,
//!         args: &[Value],
//!         line: u32,
//!     ) -> Option<Result<Value, NyblError>> {
//!         match (name, args) {
//!             ("double", [Value::Int(value)]) => {
//!                 Some(Ok(Value::Int(value * 2)))
//!             }
//!             ("double", _) => Some(Err(NyblError::runtime(
//!                 "double(value) expects one int",
//!                 line,
//!             ))),
//!             _ => None,
//!         }
//!     }
//!
//!     fn on_print(&mut self, message: &str) {
//!         self.output.push(message.to_owned());
//!     }
//! }
//!
//! let mut host = Host { output: Vec::new() };
//! nybl::run(
//!     "print(double(21))",
//!     &mut host,
//!     &NyblLimits::standard(),
//! ).unwrap();
//! assert_eq!(host.output, ["42"]);
//! ```
//!
//! # Reference parameters
//!
//! Nybl normally passes independent values. User-defined functions may opt
//! into explicit, second-class reference parameters by writing `ref` at both
//! the declaration and call:
//!
//! ```nybl
//! fn increment(ref value) {
//!   value += 1
//! }
//!
//! let count = 0
//! increment(ref count)
//! print(count)    // 1
//! ```
//!
//! A reference argument must be a distinct mutable field/index place rooted in
//! a `let` binding.
//! The callee receives a staged copy; every reference target commits together
//! only after a normal return and rolls back together on runtime or resource
//! errors. Reference parameters may be forwarded but not captured. Built-in
//! and host functions are value-only.
//!
//! User-defined methods may declare `ref self` to update a mutable
//! place receiver. Method-call syntax supplies that reference
//! implicitly (`counter.increment()`); the receiver joins explicit reference
//! arguments in the same transaction. Ordinary `self` receivers are read-only,
//! and assigning through one is a parse error with a `ref self` hint.
//!
//! [`NyblInstance::call`] and [`NyblInstance::call_value`] also accept values,
//! not Nybl bindings, and reject ref-bearing callables before execution. Keep
//! host-facing `pub fn` entries value-only and perform ref calls inside Nybl.
//! The complete language contract is in the [reference-parameters
//! guide](https://nybl-lang.com/docs/functions/reference-parameters/).
//!
//! # Persistent programs
//!
//! A persistent program explicitly publishes its host-callable ABI with
//! direct root-level `pub fn` declarations:
//!
//! ```
//! # use nybl::{NyblError, NyblHost, NyblInstance, NyblLimits, Value};
//! # struct Host;
//! # impl NyblHost for Host {
//! #     fn call(&mut self, _: &str, _: &[Value], _: u32)
//! #         -> Option<Result<Value, NyblError>> { None }
//! # }
//! let mut host = Host;
//! let mut instance = NyblInstance::load(
//!     "let count = 0\npub fn next() { count += 1; return count }",
//!     &mut host,
//!     &NyblLimits::standard(),
//! ).unwrap();
//!
//! assert_eq!(
//!     instance.call("next", &[], &mut host).unwrap().inspect(),
//!     "1",
//! );
//! assert_eq!(
//!     instance.call("next", &[], &mut host).unwrap().inspect(),
//!     "2",
//! );
//! ```
//!
//! # Rust values
//!
//! [`Value::to_rust`], [`IntoValue`], [`FromValue`], and the
//! [`nybl_value!`] macro provide checked conversions at host boundaries.
//! Conversions include strict numeric scalars, borrowed and owned strings,
//! `Vec<T>`, `Option<T>`, `Result<T, E>`, and deterministic
//! `BTreeMap<String, T>` dictionaries. Recursive construction remains
//! fallible so Nybl's maximum value-depth invariant cannot be bypassed.
//!
//! # Limits and features
//!
//! [`NyblLimits`] bounds executed steps and tracked allocations. All engines
//! also enforce a fixed function-call depth. Limit failures are fatal and
//! cannot be swallowed by Nybl's `try_call`.
//!
//! The default `std` feature enables Rust standard-library integration. The
//! separate default `nybl-std` feature bundles the `std.math`, `std.iter`,
//! `std.collections`, `std.string`, `std.json`, and `std.test` modules as Nybl
//! source; resolve them through [`stdlib::resolve`]. Disable default features
//! to omit the bundled modules while keeping Rust `std` by explicitly enabling
//! `std`. A genuine no_std build uses `default-features = false` plus the
//! opt-in `no_std` feature, which replaces standard-library floating-point
//! operations with `libm`. If Cargo unifies `std` and `no_std`, `std` wins.
//!
//! The complete language and embedding guides live at
//! <https://nybl-lang.com/docs/>.

#![cfg_attr(all(feature = "no_std", not(feature = "std")), no_std)]

#[cfg(all(feature = "no_std", not(feature = "std")))]
extern crate alloc;

// A genuine no_std unit-test build still uses Rust's standard test harness;
// make that test-only dependency explicit.
#[cfg(all(test, feature = "no_std", not(feature = "std")))]
#[macro_use]
extern crate std;

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{collections::BTreeSet, string::String, vec::Vec};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::collections::BTreeSet;

#[doc(hidden)]
pub mod ast_visit;
pub mod builtins;
pub mod check;
pub mod error;
pub mod error_messages;
#[doc(hidden)]
pub mod formatting;
pub mod host;
mod instance;
pub mod lexer;
pub mod math;
pub mod memory;
pub mod methods;
pub mod naming;
pub mod ops;
pub mod parser;
pub mod precheck;
pub mod ref_params;
/// Bundled Nybl standard library (`use std.math`, `std.json`,
/// `std.collections`, `std.iter`, `std.string`, `std.test`).
/// Feature-gated behind `nybl-std` (on by default).
///
/// The module is named `stdlib` rather than `std` specifically
/// to avoid shadowing Rust's own `::std` when a consumer does
/// `use nybl::*` (or when anything in the crate's own tests
/// does `use super::*`). `nybl::stdlib::resolve(...)` is the
/// public entry point.
#[cfg(feature = "nybl-std")]
pub mod stdlib;
pub mod suggest;
pub mod value;
pub mod value_conversion;

mod evaluator;

pub use error::NyblError;
pub use error::NyblWarning;
pub use instance::{EntryPoint, NyblInstance};
pub use parser::{ParamMode, Stmt, count_instructions};
pub use ref_params::{validate_call_modes, validate_value_only_call_modes};
pub use value::{HostValue, Value};
pub use value_conversion::{FromValue, IntoValue, ValueConversionError, ValuePathSegment};

/// The core pattern matcher. Re-exported so engines beyond the
/// tree-walker (the bytecode VM, the AOT runtime) can apply the
/// same structural rules without re-implementing them.
pub use evaluator::{pattern_matches, pattern_matches_in};

/// Shared scope walker for resolving source-level type
/// references to the declaring module. Re-exported because VM
/// and AOT both need to build per-frame type resolvers when
/// calling [`pattern_matches`] — the identity comparison lives
/// on the matcher side, but the scope lookup is identical
/// across engines.
pub use evaluator::resolve_type_in;
#[doc(hidden)]
pub use evaluator::resolve_type_in_scoped;

/// Type alias for the resolver closure `pattern_matches` expects.
pub use evaluator::TypeResolveFn;

/// Persistent state for an interactive REPL session. Unlike the one-shot
/// [`run`] API, a
/// `ReplSession` carries its scopes, fns, user types, method
/// tables, and import cache across calls, so
/// `session.eval("let x = 5")` followed by
/// `session.eval("print(x)")` sees the same `x`.
pub use evaluator::ReplSession;

// ─── NyblLimits ─────────────────────────────────────────────────────────────

/// Resource limits enforced during execution.
#[derive(Debug, Clone)]
pub struct NyblLimits {
    /// Max interpreter ticks (loop iterations, statements, etc.)
    pub max_steps: u64,
    /// Max total tracked memory (bytes) for strings + arrays
    pub max_memory: usize,
    /// Engine builtins the host forbids for this program (e.g.
    /// `rand` in a deterministic simulation whose randomness must
    /// flow through a host-provided seeded RNG). A definite
    /// reference to a disabled builtin is a fatal load-time error;
    /// references that static analysis cannot prove (a shadowing
    /// binding might apply) fail with the same fatal error at the
    /// moment they would invoke the builtin, and `try_call` cannot
    /// catch them. Names that are not engine builtins are allowed
    /// and never match. Empty (the default) disables nothing.
    pub disabled_builtins: BTreeSet<String>,
}

impl NyblLimits {
    /// The general-purpose preset: 10,000 steps and 10 MiB of tracked memory.
    pub fn standard() -> Self {
        Self {
            max_steps: 10_000,
            max_memory: 10 * 1024 * 1024, // 10 MB
            disabled_builtins: BTreeSet::new(),
        }
    }

    /// A smaller preset for demos: 1,000 steps and 1 MiB of tracked memory.
    pub fn demo() -> Self {
        Self {
            max_steps: 1_000,
            max_memory: 1024 * 1024, // 1 MB
            disabled_builtins: BTreeSet::new(),
        }
    }

    /// Builder-style helper that disables the given builtins on top of
    /// the receiver's other settings:
    /// `NyblLimits::standard().with_disabled_builtins(["rand"])`.
    pub fn with_disabled_builtins(
        mut self,
        names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.disabled_builtins
            .extend(names.into_iter().map(Into::into));
        self
    }
}

impl Default for NyblLimits {
    fn default() -> Self {
        Self::standard()
    }
}

// ─── NyblHost trait ─────────────────────────────────────────────────────────

/// Extension point for embedders to add custom built-in functions.
pub trait NyblHost {
    /// Called for unknown function names. Return `None` = not handled.
    fn call(&mut self, name: &str, args: &[Value], line: u32) -> Option<Result<Value, NyblError>>;

    /// Called for a non-common method on an opaque [`HostValue`]. Return
    /// `None` when this host does not implement the method.
    ///
    /// Host methods are value-only calls. Any mutation they perform is an
    /// external host side effect and is not part of Nybl's `ref` transaction
    /// or rollback semantics.
    fn call_method(
        &mut self,
        receiver: &HostValue,
        method: &str,
        args: &[Value],
        line: u32,
    ) -> Option<Result<Value, NyblError>> {
        let _ = (receiver, method, args, line);
        None
    }

    /// Called by `print()`.
    fn on_print(&mut self, message: &str) {
        let _ = message;
    }

    /// Report a failure from the most recent [`Self::on_print`] call.
    ///
    /// `on_print` predates fallible host output, so changing its return type
    /// would break every existing host implementation. Hosts that write to a
    /// fallible destination can instead retain the error in `on_print` and
    /// return it here. Engines call this hook immediately after `on_print`.
    ///
    /// The default keeps existing in-memory and infallible hosts unchanged.
    fn print_error(&self, line: u32) -> Option<NyblError> {
        let _ = line;
        None
    }

    /// Hint text for "function not found" errors.
    fn function_hint(&self) -> &str {
        ""
    }

    /// Called each tick. Return `Err` to halt execution.
    fn on_tick(&mut self) -> Result<(), NyblError> {
        Ok(())
    }

    /// Resolve a `use` target to Nybl source.
    ///
    /// The core language doesn't know where modules live — a
    /// filesystem embedder reads `.nybl` files, a browser embedder
    /// might fetch a URL, an embedded host might look up bundled
    /// string assets. Returning:
    ///
    /// - `None` — "I don't handle this module path": the runtime
    ///   raises a *module not found* error.
    /// - `Some(Ok(source))` — the module's source text, to be
    ///   parsed and executed by the engine.
    /// - `Some(Err(e))` — the resolver itself failed (I/O error,
    ///   bad path, …); the engine propagates the error as-is.
    ///
    /// The default impl returns `None`, so by default a program
    /// that uses another module halts with *module not found*.
    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        let _ = name;
        None
    }
}

// ─── Public API ────────────────────────────────────────────────────────────

/// Run a Nybl program with the given host and limits.
pub fn run<H: NyblHost>(source: &str, host: &mut H, limits: &NyblLimits) -> Result<(), NyblError> {
    let tokens = lexer::lex(source)?;
    let stmts = parser::parse(tokens)?;
    check::enforce_disabled_builtins(&stmts, &limits.disabled_builtins)?;
    let eval = evaluator::Evaluator::new(host, limits.clone());
    eval.run(&stmts)
}

/// Parse Nybl source into an AST (useful for instruction counting).
pub fn parse(source: &str) -> Result<Vec<Stmt>, NyblError> {
    let tokens = lexer::lex(source)?;
    parser::parse(tokens)
}

/// Parse Nybl source and run the static check pass, returning
/// both the AST and any non-fatal warnings (currently:
/// match-exhaustiveness). Prefer this over [`parse`] in tools
/// that surface diagnostics to users — the CLI uses it to
/// print warnings before running the program.
///
/// Imported enums are opaque at this layer — see
/// [`parse_with_warnings_and_resolver`] for a variant that
/// walks `use` statements to pick up imported enum decls so
/// exhaustiveness warnings fire on them too.
pub fn parse_with_warnings(
    source: &str,
) -> Result<(Vec<Stmt>, Vec<error::NyblWarning>), NyblError> {
    let stmts = parse(source)?;
    let warnings = check::check_program(&stmts);
    Ok((stmts, warnings))
}

/// Like [`parse_with_warnings`] but follows every top-level
/// `use` statement via the supplied resolver so the
/// exhaustiveness check can see imported enums. `resolver`
/// has the same shape as [`NyblHost::resolve_module`].
///
/// Returning `Some(Err(_))` or `None` from `resolver` is
/// *not* propagated — the checker silently falls back to
/// treating that module's enums as opaque, same as
/// [`parse_with_warnings`]. Only a parse error of the root
/// `source` is surfaced.
pub fn parse_with_warnings_and_resolver<R>(
    source: &str,
    mut resolver: R,
) -> Result<(Vec<Stmt>, Vec<error::NyblWarning>), NyblError>
where
    R: FnMut(&str) -> Option<Result<String, NyblError>>,
{
    let stmts = parse(source)?;
    let warnings = check::check_program_with_resolver(&stmts, &mut resolver);
    Ok((stmts, warnings))
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    use alloc::string::ToString;
    use std::cell::RefCell;

    // ─── Test host ─────────────────────────────────────────────────

    struct TestHost {
        prints: RefCell<Vec<String>>,
    }

    impl TestHost {
        fn new() -> Self {
            Self {
                prints: RefCell::new(Vec::new()),
            }
        }

        fn last_print(&self) -> String {
            self.prints
                .borrow()
                .last()
                .cloned()
                .expect("no print output")
        }
    }

    impl NyblHost for TestHost {
        fn call(
            &mut self,
            _name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, NyblError>> {
            None
        }

        fn on_print(&mut self, message: &str) {
            self.prints.borrow_mut().push(message.to_string());
        }
    }

    // ─── Test helpers ──────────────────────────────────────────────

    fn test_limits() -> NyblLimits {
        NyblLimits::standard()
    }

    /// Run code, return last print output
    fn say(code: &str) -> String {
        // Change say() -> print() in test code
        let mut host = TestHost::new();
        run(code, &mut host, &test_limits()).unwrap();
        host.last_print()
    }

    /// Run code, expect runtime error, return message
    fn run_err(code: &str) -> String {
        let mut host = TestHost::new();
        run(code, &mut host, &test_limits()).unwrap_err().message
    }

    /// Expect a lex or parse error, return message
    fn parse_err(code: &str) -> String {
        parse(code).unwrap_err().message
    }

    /// Run code with custom limits, expect a runtime error, return message
    fn run_err_with_limits(code: &str, limits: NyblLimits) -> String {
        let mut host = TestHost::new();
        run(code, &mut host, &limits).unwrap_err().message
    }

    /// Tight limits for safety tests
    fn tight_limits() -> NyblLimits {
        NyblLimits {
            max_steps: 500,
            max_memory: 64 * 1024,
            ..NyblLimits::standard()
        }
    }

    #[test]
    fn multiline_delimiters_parse_and_run() {
        let source = r#"fn add(a, b) { return a + b }
let values = [
    1,
    add(
        2,
        3
    ),
    [
        6,
    ][
        0
    ],
]
let config = {
    "target": values[
        1
    ],
    "label": "nybl"
}
if (
    config[
        "target"
    ] == 5 && values.len() == 3
) {
    print(
        values[0] +
        values[1] +
        values[2]
    )
}
let length = values
    // Leading-dot continuation may cross comments and blank lines.

    .len()
    .to_str()
print(length)"#;

        parse(source).unwrap();
        let mut host = TestHost::new();
        run(source, &mut host, &test_limits()).unwrap();
        assert_eq!(host.prints.borrow().as_slice(), ["12", "3"]);
    }

    #[test]
    fn nested_lambda_blocks_keep_newline_statement_boundaries() {
        assert_eq!(
            say(r#"let functions = [
    fn() {
        let x = 1
        let y = 2
        return x + y
    },
]
let wrapped = (fn() {
    let x = 4
    let y = 5
    return x + y
})
print(functions[0]() + wrapped())"#),
            "12"
        );
    }

    #[test]
    fn return_newline_is_bare_but_parenthesized_return_can_span_lines() {
        let mut host = TestHost::new();
        run(
            r#"fn bare() {
    return
    42
}
fn grouped() {
    return (
        42
    )
}
print(bare())
print(grouped())"#,
            &mut host,
            &test_limits(),
        )
        .unwrap();
        assert_eq!(host.prints.borrow().as_slice(), ["none", "42"]);
    }

    // ─── Arithmetic ────────────────────────────────────────────────

    #[test]
    fn add_numbers() {
        assert_eq!(say("print(1 + 2)"), "3");
    }

    #[test]
    fn subtract() {
        assert_eq!(say("print(10 - 3)"), "7");
    }

    #[test]
    fn multiply() {
        assert_eq!(say("print(4 * 5)"), "20");
    }

    #[test]
    fn divide_float() {
        assert_eq!(say("print(7 / 2)"), "3.5");
    }

    #[test]
    fn divide_whole() {
        assert_eq!(say("print(6 / 2)"), "3");
    }

    #[test]
    fn modulo() {
        assert_eq!(say("print(10 % 3)"), "1");
    }

    #[test]
    fn precedence() {
        assert_eq!(say("print(2 + 3 * 4)"), "14");
    }

    #[test]
    fn parentheses() {
        assert_eq!(say("print((2 + 3) * 4)"), "20");
    }

    #[test]
    fn unary_neg() {
        assert_eq!(say("print(-5)"), "-5");
    }

    #[test]
    fn unary_not() {
        assert_eq!(say("print(!true)"), "false");
    }

    // ─── Strings ───────────────────────────────────────────────────

    #[test]
    fn string_concat() {
        assert_eq!(say(r#"print("hello" + " " + "world")"#), "hello world");
    }

    #[test]
    fn string_repeat() {
        assert_eq!(say(r#"print("ab" * 3)"#), "ababab");
    }

    #[test]
    fn string_interpolation() {
        assert_eq!(
            say(r#"let name = "nybl"
print("hi {name}!")"#),
            "hi nybl!"
        );
    }

    #[test]
    fn string_auto_coerce_in_add() {
        assert_eq!(say(r#"print("val=" + 42)"#), "val=42");
    }

    // ─── Comparisons & Logic ───────────────────────────────────────

    #[test]
    fn equality() {
        assert_eq!(say("print(1 == 1)"), "true");
        assert_eq!(say("print(1 == 2)"), "false");
        assert_eq!(say("print(1 != 2)"), "true");
    }

    #[test]
    fn ordering() {
        assert_eq!(say("print(3 < 5)"), "true");
        assert_eq!(say("print(5 <= 5)"), "true");
        assert_eq!(say("print(6 > 5)"), "true");
        assert_eq!(say("print(5 >= 6)"), "false");
    }

    #[test]
    fn logical_and_or() {
        assert_eq!(say("print(true && false)"), "false");
        assert_eq!(say("print(true || false)"), "true");
    }

    #[test]
    fn short_circuit_and() {
        assert_eq!(say("print(false && x)"), "false");
    }

    #[test]
    fn short_circuit_or() {
        assert_eq!(say("print(true || x)"), "true");
    }

    // ─── Variables ─────────────────────────────────────────────────

    #[test]
    fn let_and_use() {
        assert_eq!(say("let x = 10\nprint(x)"), "10");
    }

    #[test]
    fn assign() {
        assert_eq!(say("let x = 1\nx = 5\nprint(x)"), "5");
    }

    #[test]
    fn compound_assign() {
        assert_eq!(say("let x = 10\nx += 5\nprint(x)"), "15");
        assert_eq!(say("let x = 10\nx -= 3\nprint(x)"), "7");
        assert_eq!(say("let x = 4\nx *= 3\nprint(x)"), "12");
        assert_eq!(say("let x = 10\nx /= 4\nprint(x)"), "2.5");
        assert_eq!(say("let x = 10\nx %= 3\nprint(x)"), "1");
    }

    #[test]
    fn undefined_variable_error() {
        assert!(run_err("print(nope)").contains("not found"));
    }

    #[test]
    fn assign_undeclared_error() {
        assert!(run_err("x = 5").contains("doesn't exist"));
    }

    // ─── If / Else ─────────────────────────────────────────────────

    #[test]
    fn if_true_branch() {
        assert_eq!(
            say("if true { print(\"yes\") } else { print(\"no\") }"),
            "yes"
        );
    }

    #[test]
    fn if_false_branch() {
        assert_eq!(
            say("if false { print(\"yes\") } else { print(\"no\") }"),
            "no"
        );
    }

    #[test]
    fn if_else_if() {
        assert_eq!(
            say(r#"let x = 2
if x == 1 { print("one") } else if x == 2 { print("two") } else { print("other") }"#),
            "two"
        );
    }

    #[test]
    fn if_expression() {
        assert_eq!(say("let x = if true { 1 } else { 2 }\nprint(x)"), "1");
    }

    #[test]
    fn if_expression_accepts_multiline_single_expression_branches() {
        let source = r#"let first = if true {
    // Leading comments and blank lines are layout, not statements.

    1 +
        2;
}
else {
    99
}
print(first)

let second = if false {
    0
} else {
    if true {
        4
    }
    else {
        5
    }
}
print(second)

let third = if true {
    (
        5
        + 6
    )
} else {
    0
}
print(third)"#;
        let mut host = TestHost::new();
        run(source, &mut host, &test_limits()).expect("multiline if-expression should run");
        assert_eq!(host.prints.borrow().clone(), ["3", "4", "11"]);
    }

    #[test]
    fn if_expression_rejects_multiple_branch_expressions_at_second_token() {
        let source = r#"let value = if true {
    1
    2
} else {
    3
}"#;
        let error = parse_err_full(source);
        assert_eq!(error.line, Some(3));
        assert_eq!(error.column, Some(5));
        assert_eq!(error.message, "Expected `}` but found `an integer`");

        let statement_source = r#"let value = if true {
    1
    let inner = 2
} else {
    3
}"#;
        let statement_error = parse_err_full(statement_source);
        assert_eq!(statement_error.line, Some(3));
        assert_eq!(statement_error.column, Some(5));
        assert_eq!(statement_error.message, "Expected `}` but found `let`");
    }

    #[test]
    fn if_expression_rejects_statement_branches() {
        let error = parse_err_full("let value = if true { let inner = 1 } else { 2 }");
        assert_eq!(error.line, Some(1));
        assert_eq!(error.column, Some(23));
        assert!(error.message.contains("I didn't expect `let` here"));

        let leading_semicolon = parse_err_full("let value = if true { ; 1 } else { 2 }");
        assert_eq!(leading_semicolon.column, Some(23));
        assert_eq!(leading_semicolon.message, "I didn't expect `;` here");

        let boundary_semicolon = parse_err_full("let value = if true { 1 };\nelse { 2 }");
        assert_eq!(boundary_semicolon.line, Some(1));
        assert_eq!(boundary_semicolon.column, Some(26));
        assert_eq!(boundary_semicolon.message, "Expected `else` but found `;`");
    }

    // ─── While ─────────────────────────────────────────────────────

    #[test]
    fn while_loop() {
        assert_eq!(say("let i = 0\nwhile i < 5 { i += 1 }\nprint(i)"), "5");
    }

    #[test]
    fn while_break() {
        assert_eq!(
            say("let i = 0\nwhile true { i += 1\nif i == 3 { break } }\nprint(i)"),
            "3"
        );
    }

    #[test]
    fn while_continue() {
        assert_eq!(
            say(r#"let sum = 0
let i = 0
while i < 10 {
    i += 1
    if i % 2 == 0 { continue }
    sum += i
}
print(sum)"#),
            "25"
        );
    }

    // ─── For ───────────────────────────────────────────────────────

    #[test]
    fn for_over_array() {
        assert_eq!(
            say(r#"let sum = 0
for x in [10, 20, 30] { sum += x }
print(sum)"#),
            "60"
        );
    }

    #[test]
    fn for_over_range() {
        assert_eq!(
            say("let sum = 0\nfor i in range(5) { sum += i }\nprint(sum)"),
            "10"
        );
    }

    #[test]
    fn for_over_string() {
        assert_eq!(
            say(r#"let out = ""
for ch in "abc" { out += ch + "-" }
print(out)"#),
            "a-b-c-"
        );
    }

    #[test]
    fn for_with_break() {
        assert_eq!(
            say("let last = 0\nfor i in range(100) { if i == 3 { break }\nlast = i }\nprint(last)"),
            "2"
        );
    }

    // ─── Repeat ────────────────────────────────────────────────────

    #[test]
    fn repeat_loop() {
        assert_eq!(say("let n = 0\nrepeat 4 { n += 1 }\nprint(n)"), "4");
    }

    #[test]
    fn repeat_zero() {
        assert_eq!(say("let n = 99\nrepeat 0 { n = 0 }\nprint(n)"), "99");
    }

    // ─── Functions ─────────────────────────────────────────────────

    #[test]
    fn fn_basic() {
        assert_eq!(say("fn double(x) { return x * 2 }\nprint(double(5))"), "10");
    }

    #[test]
    fn fn_implicit_return_none() {
        assert_eq!(
            say(r#"fn noop() { let x = 1 }
print(noop().type())"#),
            "none"
        );
    }

    #[test]
    fn fn_multiple_params() {
        assert_eq!(say("fn add(a, b) { return a + b }\nprint(add(3, 7))"), "10");
    }

    #[test]
    fn fn_uses_defining_root_but_not_caller_locals() {
        assert_eq!(
            say("let secret = 42\nfn peek() { return secret }\nprint(peek())"),
            "42"
        );
        assert!(
            run_err(
                r#"fn peek() { return caller_local }
fn caller() { let caller_local = 42; return peek() }
caller()"#
            )
            .contains("not found")
        );
    }

    #[test]
    fn fn_wrong_arg_count() {
        assert!(run_err("fn f(a, b) { return a }\nf(1)").contains("expects 2"));
    }

    #[test]
    fn ref_parameters_commit_on_normal_returns_and_survive_aliases() {
        assert_eq!(
            say(r#"fn swap(ref left, ref right) {
    let old = left
    left = right
    right = old
}
let alias = swap
let a = 1
let b = 2
alias(ref a, ref b)
print([a, b])"#,),
            "[2, 1]"
        );
        assert_eq!(
            say(r#"fn set_and_return_err(ref value) {
    value = 7
    return Err("ordinary value")
}
let value = 1
let result = set_and_return_err(ref value)
print([value, result.is_err()])"#,),
            "[7, true]"
        );
        assert_eq!(
            say(r#"fn set_and_try(ref value) {
    value = 8
    try Err("ordinary early return")
}
let value = 1
let result = set_and_try(ref value)
print([value, result.is_err()])"#,),
            "[8, true]"
        );
    }

    #[test]
    fn ref_preflight_blocks_argument_side_effects_and_reports_modes() {
        assert_eq!(
            say(r#"let hits = []
fn side() { hits.push(1); return 1 }
fn needs_ref(ref value) { value = 2 }
fn missing_marker() { needs_ref(side()) }
let result = try_call(missing_marker)
print([hits, result.is_err()])"#,),
            "[[], true]"
        );

        assert_eq!(
            say(r#"let hits = []
fn target(ref value) { }
fn choose() { hits.push("callee"); return target }
fn side() { hits.push("argument"); return 1 }
fn invoke() { choose()(side()) }
let result = try_call(invoke)
print([hits, result.is_err()])"#,),
            "[[\"callee\"], true]"
        );

        let invalid = run_err_full("fn target(ref value) { }\ntarget(ref [1])");
        assert_eq!(
            invalid.message,
            "`ref` argument 1 must name a mutable variable"
        );
        assert!(
            invalid
                .friendly_hint
                .as_deref()
                .unwrap()
                .contains("`let` variable")
        );
        let missing = run_err_full("fn needs_ref(ref value) { }\nlet value = 1\nneeds_ref(value)");
        assert_eq!(
            missing.message,
            "argument 1 to `needs_ref` must be passed with `ref`"
        );
        assert!(
            missing
                .friendly_hint
                .as_deref()
                .unwrap()
                .contains("Write `ref`")
        );

        assert_eq!(
            say(r#"let hits = []
fn side() { hits.push(1); return 1 }
fn invalid_target(ref value, ordinary) { }
fn invoke() { invalid_target(ref [1], side()) }
let result = try_call(invoke)
print([hits, result.is_err()])"#,),
            "[[], true]"
        );
    }

    #[test]
    fn ref_snapshots_after_ordinary_arguments_and_commits_all_targets() {
        assert_eq!(
            say(r#"let value = 1
fn ordinary() { value += 1; return 10 }
fn update(ref target, amount) { target += amount }
update(ref value, ordinary())
print(value)"#,),
            "12"
        );
        assert_eq!(
            say(r#"fn assign_pair(ref left, ref right) {
    left = 10
    right = 20
}
let left = 1
let right = 2
assign_pair(ref left, ref right)
print([left, right])"#,),
            "[10, 20]"
        );
    }

    #[test]
    fn ref_errors_roll_back_every_target_including_forwarded_stages() {
        assert_eq!(
            say(r#"let left = 1
let right = 2
fn fail(ref a, ref b) {
    a = 10
    b = 20
    panic("stop")
}
fn invoke() { fail(ref left, ref right) }
let result = try_call(invoke)
print([left, right, result.is_err()])"#,),
            "[1, 2, true]"
        );
        assert_eq!(
            say(r#"let value = 1
fn inner(ref staged) { staged = 9 }
fn outer(ref staged) {
    inner(ref staged)
    panic("after inner commit")
}
fn invoke() { outer(ref value) }
let result = try_call(invoke)
print([value, result.is_err()])"#,),
            "[1, true]"
        );
    }

    #[test]
    fn ref_target_fences_reject_duplicates_captures_and_ref_capture() {
        let duplicate =
            run_err("fn pair(ref a, ref b) { }\nlet value = 1\npair(ref value, ref value)");
        assert!(duplicate.contains("same variable"));

        assert_eq!(
            say(r#"fn touch(ref value) { value = 2 }
fn make() {
    let captured = 1
    return fn() { touch(ref captured) }
}
let closure = make()
let result = try_call(closure)
print(result.is_err())"#,),
            "true"
        );

        assert_eq!(
            say(r#"let value = 1
fn outer(ref staged) {
    let closure = fn() { return staged }
}
fn invoke() { outer(ref value) }
let result = try_call(invoke)
print([value, result.is_err()])"#,),
            "[1, true]"
        );
    }

    #[test]
    fn ref_method_receivers_stage_after_args_and_user_methods_allow_ref_args() {
        assert_eq!(
            say(r#"let items = [1]
fn argument() { items.push(2); return 3 }
(items).push(argument())
print(items)"#,),
            "[1, 2, 3]"
        );
        assert_eq!(say("print(([1, 2]).pop())"), "2");
        assert_eq!(say("print(([1, 2]).push(3))"), "none");
        assert_eq!(
            say(r#"struct Amount { n }
fn Amount.add_to(self, ref target) { target += self.n }
let amount = Amount { n: 4 }
let value = 3
amount.add_to(ref value)
print(value)"#,),
            "7"
        );

        assert_eq!(
            say(r#"let items = []
let hits = []
fn side() { hits.push(1); return 1 }
fn invoke() { items.push(ref side()) }
let result = try_call(invoke)
print([items, hits, result.is_err()])"#,),
            "[[], [], true]"
        );
        assert_eq!(
            say(r#"struct Box { value }
fn Box.set(ref self, value) { self.value = value }
let item = Box { value: 1 }
item.set(4)
print(item.value)"#),
            "4"
        );
    }

    #[test]
    fn nested_places_support_assignment_refs_and_mutating_receivers() {
        assert_eq!(
            say(r#"
struct Bucket { items }
struct Counter { value }

fn add_many(ref value, ..extra) {
    value += extra.len()
    return extra
}

fn Counter.add(ref self, amount) {
    self.value += amount
}

let buckets = [Bucket { items: [10, 20] }]
buckets[0].items[1] += 2
let captured = add_many(ref buckets[0].items[0], 7, 8, 9)
buckets[0].items.push(30)

let rows = [[Counter { value: 1 }]]
rows[0][0].add(4)

print([buckets[0].items, captured, rows[0][0].value])
"#),
            "[[13, 22, 30], [7, 8, 9], 5]"
        );
    }

    #[test]
    fn nested_ref_places_are_atomic_and_unique_by_root() {
        let error = run_err(
            r#"
fn fail(ref value) {
    value = 99
    return 1 / 0
}
let rows = [[1]]
fail(ref rows[0][0])
"#,
        );
        assert!(error.contains("Division by zero"), "got: {error}");

        let duplicate = run_err(
            r#"
fn swap(ref left, ref right) {}
let values = [1, 2]
swap(ref values[0], ref values[1])
"#,
        );
        assert!(duplicate.contains("same variable"), "got: {duplicate}");
    }

    #[test]
    fn place_indexes_run_once_before_root_snapshot() {
        let assignment = run_err("let values = [0, 1]\nvalues[values.pop()] = 9");
        assert!(assignment.contains("out of bounds"), "got: {assignment}");

        assert_eq!(
            say(r#"
let indexes = [0]
let groups = [[1]]
groups[indexes.pop()].push(indexes.len())
print([groups, indexes])
"#),
            "[[[1, 0]], []]"
        );
    }

    #[test]
    fn rest_parameters_are_value_only_and_accept_zero_or_many_extras() {
        assert_eq!(
            say(r#"
fn collect(head, ..tail) { return [head, tail] }
let closure = fn(..items) { return items.len() }
print([collect(1), collect(1, 2, 3), closure(), closure(4, 5)])
"#),
            "[[1, []], [1, [2, 3]], 0, 2]"
        );

        let error = run_err(
            r#"
fn collect(..items) {}
let value = 1
collect(ref value)
"#,
        );
        assert!(error.contains("value parameter"), "got: {error}");
    }

    #[test]
    fn rest_and_public_surface_syntax_is_strict() {
        let not_final = parse_err("fn collect(..items, value) {}");
        assert!(not_final.contains("final parameter"), "got: {not_final}");

        let nested_surface = parse_err("if true { pub { value } }");
        assert!(
            nested_surface.contains("module root"),
            "got: {nested_surface}"
        );

        let duplicate = parse_err("pub { value }\npub { value }");
        assert!(
            duplicate.contains("duplicate public name"),
            "got: {duplicate}"
        );

        let ast = parse("fn collect(ref first, ..items) {}\npub { collect }").unwrap();
        let parser::StmtKind::FnDecl { params, .. } = &ast[0].kind else {
            panic!("expected function declaration");
        };
        assert_eq!(params[0].mode, ParamMode::Ref);
        assert_eq!(params[1].mode, ParamMode::Rest);
        assert!(matches!(
            ast[1].kind,
            parser::StmtKind::PublicSurface { .. }
        ));
    }

    #[test]
    fn walker_instances_expose_variadic_entry_metadata() {
        let mut host = TestHost::new();
        let mut instance = NyblInstance::load(
            "pub fn collect(first, ..items) { return [first, items] }",
            &mut host,
            &test_limits(),
        )
        .unwrap();
        let entry = &instance.entry_points()[0];
        assert_eq!(entry.name(), "collect");
        assert_eq!(entry.arity(), 1);
        assert_eq!(entry.max_arity(), None);
        assert!(entry.is_variadic());
        assert!(entry.accepts_arity(3));
        assert_eq!(
            instance
                .call(
                    "collect",
                    &[Value::Int(1), Value::Int(2), Value::Int(3)],
                    &mut host,
                )
                .unwrap()
                .inspect(),
            "[1, [2, 3]]"
        );
    }

    #[test]
    fn zero_parameter_method_reports_total_arity_before_evaluating_arguments() {
        let error =
            run_err_full("struct Empty { }\nfn Empty.zero() { return none }\nEmpty { }.zero()");
        assert_eq!(
            error.message,
            "`Empty.zero` expects 0 arguments (including `self`), but got 1"
        );
        assert_eq!(error.line, Some(3));
        assert!(!error.is_fatal);

        assert_eq!(
            say(r#"struct Empty { }
let events = []
fn Empty.zero() { return none }
fn make() { events.push("receiver"); return Empty { } }
fn side() { events.push("argument"); return 1 }
fn invoke() { make().zero(side()) }
let result = try_call(invoke)
print([events, result.is_err()])"#,),
            r#"[["receiver"], true]"#
        );
    }

    #[test]
    fn fatal_ref_failure_rolls_back_staged_values() {
        let mut session = ReplSession::new();
        let mut host = TestHost::new();
        session
            .eval(
                r#"let value = 1
fn exhaust(ref staged) {
    staged = 99
    while true { staged += 1 }
}
fn invoke() { exhaust(ref value) }"#,
                &mut host,
                &NyblLimits::standard(),
            )
            .unwrap();
        let error = session
            .eval(
                "invoke()",
                &mut host,
                &NyblLimits {
                    max_steps: 8,
                    max_memory: 1024 * 1024,
                    ..NyblLimits::standard()
                },
            )
            .unwrap_err();
        assert!(error.is_fatal);
        assert_eq!(session.get("value").unwrap().inspect(), "1");
    }

    #[test]
    fn final_memory_limit_failure_rolls_back_all_ref_targets() {
        let mut session = ReplSession::new();
        let mut host = TestHost::new();
        session
            .eval(
                r#"let first = 1
let second = "safe"
let payload = "this payload is retained outside the call"
fn grow(ref staged_first, ref staged_second, value) {
    staged_first = 99
    staged_second = value + value
}
fn invoke() { grow(ref first, ref second, payload) }"#,
                &mut host,
                &NyblLimits::standard(),
            )
            .unwrap();

        let error = session
            .eval(
                "invoke()",
                &mut host,
                &NyblLimits {
                    max_steps: 1_000,
                    max_memory: 1,
                    ..NyblLimits::standard()
                },
            )
            .unwrap_err();
        assert!(error.is_fatal);
        assert!(error.message.contains("Memory limit exceeded"));
        assert_eq!(session.get("first").unwrap().inspect(), "1");
        assert_eq!(session.get("second").unwrap().inspect(), "\"safe\"");
    }

    #[test]
    fn fn_recursion() {
        assert_eq!(
            say(r#"fn fib(n) {
    if n <= 1 { return n }
    return fib(n - 1) + fib(n - 2)
}
print(fib(10))"#),
            "55"
        );
    }

    // ─── Closures / first-class functions ──────────────────────────

    #[test]
    fn lambda_basic() {
        assert_eq!(
            say(r#"let double = fn(x) { return x * 2 }
print(double(5))"#),
            "10"
        );
    }

    #[test]
    fn lambda_captures_value() {
        assert_eq!(
            say(r#"let n = 5
let add_n = fn(x) { return x + n }
print(add_n(3))"#),
            "8"
        );
    }

    #[test]
    fn lambda_captures_are_snapshot() {
        // Mutating `n` after the lambda is built should not
        // affect the captured value — the snapshot semantics are
        // deliberate.
        assert_eq!(
            say(r#"let n = 5
let add_n = fn(x) { return x + n }
n = 100
print(add_n(3))"#),
            "8"
        );
    }

    #[test]
    fn lambda_returned_from_fn() {
        // Classic closure pattern: factory function returns a
        // specialised closure. The captured `n` outlives the
        // enclosing frame because it was cloned into the closure.
        assert_eq!(
            say(r#"fn make_adder(n) { return fn(x) { return x + n } }
let add5 = make_adder(5)
let add10 = make_adder(10)
print(add5(3))
print(add10(3))"#),
            "13"
        );
    }

    #[test]
    fn named_fn_is_first_class_value() {
        assert_eq!(
            say(r#"fn double(x) { return x * 2 }
let f = double
print(f(7))"#),
            "14"
        );
    }

    #[test]
    fn fn_stored_in_array_and_called_via_index() {
        assert_eq!(
            say(r#"fn add(x, y) { return x + y }
fn mul(x, y) { return x * y }
let ops = [add, mul]
print(ops[0](2, 3))
print(ops[1](2, 3))"#),
            "6"
        );
    }

    #[test]
    fn higher_order_apply() {
        // `apply` takes any callable, proving we call through a
        // parameter, not a statically known name.
        assert_eq!(
            say(r#"fn apply(f, x) { return f(x) }
fn square(n) { return n * n }
print(apply(square, 4))
print(apply(fn(n) { return n + 1 }, 4))"#),
            "5"
        );
    }

    #[test]
    fn lambda_self_reference_via_named_fn() {
        // Named-fn decls see themselves through `self.functions`,
        // so recursion works. Let-bound lambdas are documented as
        // not supporting self-reference — this test pins the
        // working case.
        assert_eq!(
            say(r#"fn countdown(n) {
    if n <= 0 { return "done" }
    return countdown(n - 1)
}
print(countdown(3))"#),
            "done"
        );
    }

    #[test]
    fn type_of_fn_is_fn() {
        assert_eq!(say("fn f() { }\nprint(f.type())"), "fn");
        assert_eq!(say("let g = fn() { }\nprint(g.type())"), "fn");
    }

    #[test]
    fn calling_non_callable_value_errors() {
        assert!(run_err("let x = 5\nx(1)").contains("not a function"));
    }

    #[test]
    fn lambda_captures_nested_scope() {
        // Closures snapshot the full lexical scope stack, so
        // bindings from outer blocks are visible inside nested
        // lambdas.
        assert_eq!(
            say(r#"let a = 1
if true {
    let b = 2
    let f = fn() { return a + b }
    print(f())
}"#),
            "3"
        );
    }

    #[test]
    fn iife() {
        // Immediately-invoked lambda: `(fn() { ... })()`. Falls
        // out of the parser for free once lambdas are expressions.
        assert_eq!(say("print((fn(x) { return x * 3 })(4))"), "12");
    }

    // ─── Arrays ────────────────────────────────────────────────────

    #[test]
    fn array_literal_and_index() {
        assert_eq!(say("let a = [10, 20, 30]\nprint(a[1])"), "20");
    }

    #[test]
    fn array_negative_index() {
        assert_eq!(say("let a = [10, 20, 30]\nprint(a[-1])"), "30");
    }

    #[test]
    fn array_assign_index() {
        assert_eq!(say("let a = [1, 2, 3]\na[1] = 99\nprint(a[1])"), "99");
    }

    #[test]
    fn array_push_pop() {
        assert_eq!(
            say(r#"let a = [1, 2]
a.push(3)
print(a.len())"#),
            "3"
        );
        assert_eq!(
            say(r#"let a = [1, 2, 3]
let last = a.pop()
print(last)"#),
            "3"
        );
    }

    #[test]
    fn array_has() {
        assert_eq!(say("print([1, 2, 3].has(2))"), "true");
        assert_eq!(say("print([1, 2, 3].has(9))"), "false");
    }

    #[test]
    fn array_index_of() {
        assert_eq!(say("print([10, 20, 30].index_of(20))"), "1");
        assert_eq!(say("print([10, 20, 30].index_of(99))"), "-1");
    }

    #[test]
    fn array_slice() {
        assert_eq!(say("print([1, 2, 3, 4, 5].slice(1, 4))"), "[2, 3, 4]");
    }

    #[test]
    fn array_join() {
        assert_eq!(say(r#"print([1, 2, 3].join("-"))"#), "1-2-3");
    }

    #[test]
    fn array_sort() {
        assert_eq!(say("let a = [3, 1, 2]\na.sort()\nprint(a)"), "[1, 2, 3]");
    }

    #[test]
    fn array_reverse() {
        assert_eq!(say("let a = [1, 2, 3]\na.reverse()\nprint(a)"), "[3, 2, 1]");
    }

    #[test]
    fn array_insert_remove() {
        assert_eq!(
            say(r#"let a = [1, 3]
a.insert(1, 2)
print(a)"#),
            "[1, 2, 3]"
        );
        assert_eq!(
            say(r#"let a = [1, 2, 3]
let removed = a.remove(1)
print(removed)"#),
            "2"
        );
    }

    #[test]
    fn array_concat() {
        assert_eq!(say("print([1, 2] + [3, 4])"), "[1, 2, 3, 4]");
    }

    #[test]
    fn array_out_of_bounds() {
        assert!(run_err("let a = [1]\nprint(a[5])").contains("out of bounds"));
    }

    // ─── Strings (methods) ─────────────────────────────────────────

    #[test]
    fn string_len() {
        assert_eq!(say(r#"print("hello".len())"#), "5");
    }

    #[test]
    fn string_contains() {
        assert_eq!(say(r#"print("abcdef".contains("cd"))"#), "true");
        assert_eq!(say(r#"print("abcdef".contains("zz"))"#), "false");
    }

    #[test]
    fn string_starts_ends_with() {
        assert_eq!(say(r#"print("hello".starts_with("he"))"#), "true");
        assert_eq!(say(r#"print("hello".ends_with("lo"))"#), "true");
    }

    #[test]
    fn string_split() {
        assert_eq!(say(r#"print("a,b,c".split(","))"#), r#"["a", "b", "c"]"#);
    }

    #[test]
    fn string_replace() {
        assert_eq!(
            say(r#"print("hello world".replace("world", "nybl"))"#),
            "hello nybl"
        );
    }

    #[test]
    fn string_upper_lower_trim() {
        assert_eq!(say(r#"print("Hello".upper())"#), "HELLO");
        assert_eq!(say(r#"print("Hello".lower())"#), "hello");
        assert_eq!(say(r#"print("  hi  ".trim())"#), "hi");
    }

    #[test]
    fn string_slice() {
        assert_eq!(say(r#"print("hello".slice(1, 4))"#), "ell");
    }

    #[test]
    fn string_index_of() {
        assert_eq!(say(r#"print("hello".index_of("ll"))"#), "2");
        assert_eq!(say(r#"print("hello".index_of("zz"))"#), "-1");
    }

    #[test]
    fn string_index_char() {
        assert_eq!(say(r#"print("abc"[1])"#), "b");
    }

    // ─── Dicts ─────────────────────────────────────────────────────

    #[test]
    fn dict_literal_and_access() {
        assert_eq!(
            say(r#"let d = {"name": "nybl", "hp": 100}
print(d["name"])"#),
            "nybl"
        );
    }

    #[test]
    fn dict_assign_key() {
        assert_eq!(
            say(r#"let d = {"a": 1}
d["b"] = 2
print(d["b"])"#),
            "2"
        );
    }

    #[test]
    fn dict_methods() {
        assert_eq!(
            say(r#"let d = {"x": 1, "y": 2}
print(d.len())"#),
            "2"
        );
        assert_eq!(say(r#"print({"a": 1, "b": 2}.has("a"))"#), "true");
        assert_eq!(say(r#"print({"a": 1, "b": 2}.has("z"))"#), "false");
    }

    #[test]
    fn dict_keys_values() {
        assert_eq!(say(r#"print({"a": 1, "b": 2}.keys())"#), r#"["a", "b"]"#);
        assert_eq!(say(r#"print({"a": 1, "b": 2}.values())"#), "[1, 2]");
    }

    #[test]
    fn dict_remove_mutates_named_binding() {
        assert_eq!(
            say(r#"let d = {"a": 1, "b": 2, "c": 3}
let gone = d.remove("b")
print([gone, d.len(), d.has("b"), d.keys()])"#),
            r#"[2, 2, false, ["a", "c"]]"#
        );
        // Missing keys return `none`, matching missing-key reads.
        assert_eq!(
            say(r#"let d = {"a": 1}
print([d.remove("zzz"), d.len()])"#),
            "[none, 1]"
        );
        assert_eq!(
            run_err(
                r#"let d = {"a": 1}
d.remove(0)"#
            ),
            ".remove() needs a string key"
        );
    }

    #[test]
    fn dict_remove_commits_through_nested_places() {
        assert_eq!(
            say(r#"struct Holder { config }
let holder = Holder { config: {"x": 1, "y": 2} }
let d = {"inner": {"a": 1, "b": 2}}
print([holder.config.remove("x"), d["inner"].remove("b"), holder.config.keys(), d["inner"].keys()])"#),
            r#"[1, 2, ["y"], ["a"]]"#
        );
    }

    #[test]
    fn dict_remove_on_shared_backing_preserves_value_semantics() {
        assert_eq!(
            say(r#"let d = {"a": 1, "b": 2}
let snapshot = d
d.remove("a")
print([d.len(), snapshot.len(), snapshot.has("a")])"#),
            "[1, 2, true]"
        );
    }

    #[test]
    fn collection_clear_semantics() {
        assert_eq!(
            say(r#"let a = [1, 2, 3]
a.clear()
let d = {"x": 1, "y": 2}
d.clear()
d["z"] = 9
print([a, a.len(), d.len(), d.has("x"), d["z"]])"#),
            "[[], 0, 1, false, 9]"
        );
        assert_eq!(
            run_err("let d = {\"a\": 1}\nd.clear(1)"),
            "`.clear()` needs 0 arguments"
        );
    }

    #[test]
    fn collection_mutators_commit_through_ref_params_and_ref_self() {
        // Reassignment inside a callee only rebinds its local; `ref`
        // receivers must observe the mutating built-ins themselves.
        assert_eq!(
            say(r#"fn wipe(ref d) { d.clear() }
fn prune(ref d, key) { return d.remove(key) }
fn shorten(ref a, n) { a.truncate(n) }
let state = {"a": 1, "b": 2}
wipe(ref state)
let scores = {"x": 1, "y": 2}
let pruned = prune(ref scores, "x")
let items = [1, 2, 3, 4]
shorten(ref items, 2)
struct Cache { entries }
fn Cache.reset(ref self) { self.entries.clear() }
let cache = Cache { entries: {"k": 1} }
cache.reset()
print([state.len(), pruned, scores.keys(), items, cache.entries.len()])"#),
            r#"[0, 1, ["y"], [1, 2], 0]"#
        );
    }

    #[test]
    fn array_truncate_semantics() {
        for (call, expected) in [
            ("truncate(2)", "[1, 2]"),
            ("truncate(0)", "[]"),
            ("truncate(99)", "[1, 2, 3, 4]"),
            // Negative lengths count from the end, matching `.slice()` bounds.
            ("truncate(-1)", "[1, 2, 3]"),
            ("truncate(-99)", "[]"),
        ] {
            let source = format!("let a = [1, 2, 3, 4]\na.{call}\nprint(a)");
            assert_eq!(say(&source), expected, "call: {call}");
        }
        assert_eq!(
            run_err("let a = [1, 2]\na.truncate(\"x\")"),
            "`truncate` expects a number, but got string"
        );
    }

    // ─── Built-in functions ────────────────────────────────────────

    #[test]
    fn builtin_range_1arg() {
        assert_eq!(say("print(range(5))"), "[0, 1, 2, 3, 4]");
    }

    #[test]
    fn builtin_range_2args() {
        assert_eq!(say("print(range(2, 5))"), "[2, 3, 4]");
    }

    #[test]
    fn builtin_range_3args() {
        assert_eq!(say("print(range(0, 10, 3))"), "[0, 3, 6, 9]");
    }

    #[test]
    fn builtin_range_reverse() {
        assert_eq!(say("print(range(5, 0))"), "[5, 4, 3, 2, 1]");
    }

    #[test]
    fn builtin_str() {
        assert_eq!(say(r#"print(42.to_str())"#), "42");
        assert_eq!(say(r#"print(true.to_str())"#), "true");
    }

    #[test]
    fn builtin_int() {
        assert_eq!(say("print(3.7.to_int())"), "3");
        assert_eq!(say("print((-2.9).to_int())"), "-2");
    }

    #[test]
    fn builtin_type() {
        // Phase 6 split numeric types: `42` is an int, `42.0`
        // is a number.
        assert_eq!(say("print(42.type())"), "int");
        assert_eq!(say("print(42.0.type())"), "number");
        assert_eq!(say(r#"print("hi".type())"#), "string");
        assert_eq!(say("print(true.type())"), "bool");
        assert_eq!(say("print(none.type())"), "none");
        assert_eq!(say("print([].type())"), "array");
    }

    #[test]
    fn builtin_abs_min_max() {
        assert_eq!(say("print((-5).abs())"), "5");
        assert_eq!(say("print(3.min(7))"), "3");
        assert_eq!(say("print(3.max(7))"), "7");
    }

    #[test]
    fn builtin_len() {
        assert_eq!(say(r#"print("hello".len())"#), "5");
        assert_eq!(say("print([1, 2, 3].len())"), "3");
    }

    #[test]
    fn builtin_inspect() {
        assert_eq!(say(r#"print("hi".inspect())"#), r#""hi""#);
        assert_eq!(say("print(42.inspect())"), "42");
    }

    #[test]
    fn builtin_print_multi_args() {
        let mut host = TestHost::new();
        run(r#"print("a", "b", "c")"#, &mut host, &test_limits()).unwrap();
        assert_eq!(host.prints.borrow().as_slice(), &["a b c"]);
    }

    #[test]
    fn builtin_rand_deterministic() {
        let a = say("print(rand(100))");
        let b = say("print(rand(100))");
        assert_eq!(a, b);
    }

    // ─── Error cases ───────────────────────────────────────────────

    #[test]
    fn error_division_by_zero() {
        assert!(run_err("print(1 / 0)").contains("Division by zero"));
    }

    #[test]
    fn error_type_mismatch_subtract() {
        let msg = run_err(r#"print("a" - 1)"#);
        assert!(msg.contains("Can't use `-`"));
    }

    #[test]
    fn error_unknown_function() {
        assert!(run_err("nope()").contains("not found"));
    }

    #[test]
    fn error_infinite_loop_protection() {
        let msg = run_err("while true { }");
        assert!(msg.contains("too many steps"));
    }

    #[test]
    fn error_break_outside_loop() {
        assert!(run_err("break").contains("outside of a loop"));
    }

    #[test]
    fn error_continue_outside_loop() {
        assert!(run_err("continue").contains("outside of a loop"));
    }

    // ─── Parse errors ──────────────────────────────────────────────

    #[test]
    fn parse_error_missing_rparen() {
        assert!(parse_err("print(1").contains("Expected `)`"));
    }

    #[test]
    fn parse_error_missing_rbrace() {
        assert!(parse_err("if true {").contains("Expected `}`"));
    }

    // ─── Edge cases ────────────────────────────────────────────────

    #[test]
    fn empty_program() {
        let mut host = TestHost::new();
        run("", &mut host, &test_limits()).unwrap();
        assert!(host.prints.borrow().is_empty());
    }

    #[test]
    fn trailing_comma_in_array() {
        assert_eq!(say("print([1, 2, 3,])"), "[1, 2, 3]");
    }

    #[test]
    fn trailing_comma_in_dict() {
        assert_eq!(say(r#"print({"a": 1,}.len())"#), "1");
    }

    #[test]
    fn none_value() {
        assert_eq!(say("print(none)"), "none");
        assert_eq!(say("print(none == none)"), "true");
    }

    #[test]
    fn equality_across_types() {
        assert_eq!(say("print(1 == true)"), "false");
        assert_eq!(say(r#"print(0 == "")"#), "false");
        assert_eq!(say("print(none == false)"), "false");
    }

    #[test]
    fn dict_equality() {
        assert_eq!(
            say(r#"print({"a": 1, "b": 2} == {"b": 2, "a": 1})"#),
            "true"
        );
        assert_eq!(say(r#"print({"a": 1} == {"a": 2})"#), "false");
        assert_eq!(say(r#"print({"a": 1} == {"b": 1})"#), "false");
        assert_eq!(say(r#"print({"a": 1} == {"a": 1, "b": 2})"#), "false");
        assert_eq!(say(r#"print({"a": {"x": 1}} == {"a": {"x": 1}})"#), "true");
    }

    #[test]
    fn nested_array_access() {
        assert_eq!(say("let m = [[1, 2], [3, 4]]\nprint(m[1][0])"), "3");
    }

    #[test]
    fn method_chain() {
        assert_eq!(say(r#"print("  HELLO  ".trim().lower())"#), "hello");
    }

    // ─── Error diagnostics (phase 8 polish) ──────────────────────

    #[test]
    fn parse_error_carries_column_info() {
        // "let 42" is a parse error — the int `42` shows up
        // where a name was expected. Column should point at the
        // start of the `42` token (column 5).
        let err = parse_err_full("let 42");
        assert_eq!(err.line, Some(1), "err: {err:?}");
        assert_eq!(err.column, Some(5), "err: {err:?}");
        assert!(err.message.contains("Expected a name"));
    }

    #[test]
    fn parser_reports_real_reserved_word_bindings_at_the_keyword() {
        for (source, keyword, line, column) in [
            ("let if = 1", "if", 1, 5),
            ("\nfn const() { none }", "const", 2, 4),
            ("for while in [] {}", "while", 1, 5),
            ("print(match 1 { if => 1, _ => 2 })", "if", 1, 17),
            ("print(match [1] { [..if] => 1, _ => 2 })", "if", 1, 22),
        ] {
            let error = parse_err_full(source);
            assert_eq!(error.line, Some(line), "source: {source}");
            assert_eq!(error.column, Some(column), "source: {source}");
            assert_eq!(
                error.message,
                format!("`{keyword}` is a reserved word in Nybl"),
                "source: {source}"
            );
            assert!(error.friendly_hint.is_some(), "source: {source}");
        }
    }

    #[test]
    fn parse_error_renders_with_snippet_and_caret() {
        let src = "let 42";
        let err = parse_err_full(src);
        let rendered = err.render(src);
        assert!(rendered.contains("--> line 1:5"), "rendered:\n{rendered}");
        assert!(rendered.contains("let 42"));
        // Four spaces for `let ` before the caret at col 5.
        assert!(rendered.contains("    ^"), "rendered:\n{rendered}");
    }

    #[test]
    fn parse_error_on_line_2_points_at_line_2() {
        let src = "let x = 1\nlet = 2";
        let err = parse_err_full(src);
        assert_eq!(err.line, Some(2), "err: {err:?}");
        let rendered = err.render(src);
        assert!(rendered.contains("let = 2"), "rendered:\n{rendered}");
    }

    #[test]
    fn runtime_error_renders_without_column() {
        // Runtime errors don't currently carry column; the
        // renderer should still produce a readable snippet.
        let src = "let x = 1 / 0";
        let err = run_err_full(src);
        assert_eq!(err.line, Some(1));
        assert!(err.column.is_none());
        let rendered = err.render(src);
        assert!(rendered.contains("--> line 1"));
        assert!(rendered.contains("let x = 1 / 0"));
        // No caret line (column unknown).
        assert!(!rendered.contains("^"), "rendered:\n{rendered}");
    }

    /// Like `parse_err` but returns the full `NyblError` so
    /// tests can inspect line / column fields directly.
    fn parse_err_full(code: &str) -> NyblError {
        parse(code).unwrap_err()
    }

    /// Like `run_err` but returns the full `NyblError`.
    fn run_err_full(code: &str) -> NyblError {
        let mut host = TestHost::new();
        run(code, &mut host, &test_limits()).unwrap_err()
    }

    // ─── "Did you mean?" suggestions ─────────────────────────────

    #[test]
    fn typo_variable_suggests_closest_local() {
        let err = run_err_full(
            r#"let length = 5
print(lenght)"#,
        );
        assert!(err.message.contains("not found"), "err: {err:?}");
        assert_eq!(err.friendly_hint.as_deref(), Some("Did you mean `length`?"));
    }

    #[test]
    fn typo_variable_falls_back_when_nothing_close() {
        // No similar name in scope → keeps the generic hint
        // rather than suggesting something wild.
        let err = run_err_full("print(xylophone_constant)");
        assert_eq!(
            err.friendly_hint.as_deref(),
            Some("Did you forget to create it with `let`?")
        );
    }

    #[test]
    fn typo_function_suggests_user_fn() {
        let err = run_err_full(
            r#"fn greet(name) { print("hi " + name) }
gret("world")"#,
        );
        assert!(err.message.contains("not found"));
        assert_eq!(err.friendly_hint.as_deref(), Some("Did you mean `greet`?"));
    }

    #[test]
    fn typo_builtin_suggests_core_name() {
        // `rang(5)` → core builtin `range`.
        let err = run_err_full("rang(5)");
        assert_eq!(err.friendly_hint.as_deref(), Some("Did you mean `range`?"));
    }

    #[test]
    fn typo_struct_field_at_access_suggests_declared() {
        let err = run_err_full(
            r#"struct Point { x, y }
let p = Point { x: 1, y: 2 }
print(p.z)"#,
        );
        assert!(err.message.contains("has no field `z`"));
        // Both `x` and `y` are within 1 edit of `z`; the
        // candidate order in `s.fields()` is declaration order,
        // so `x` wins the tie.
        assert_eq!(err.friendly_hint.as_deref(), Some("Did you mean `x`?"));
    }

    #[test]
    fn typo_struct_field_at_construction_suggests_declared() {
        let err = run_err_full(
            r#"struct Point { x, y }
let p = Point { x: 1, ya: 2 }"#,
        );
        assert!(err.message.contains("has no field `ya`"));
        assert_eq!(err.friendly_hint.as_deref(), Some("Did you mean `y`?"));
    }

    #[test]
    fn typo_enum_variant_suggests_declared() {
        let err = run_err_full(
            r#"enum Shape { Circle(r), Rectangle { w, h } }
let s = Shape::Circel(5)"#,
        );
        assert!(err.message.contains("has no variant `Circel`"));
        assert_eq!(err.friendly_hint.as_deref(), Some("Did you mean `Circle`?"));
    }

    #[test]
    fn typo_hint_renders_in_source_snippet() {
        let src = r#"let length = 5
print(lenght)"#;
        let err = run_err_full(src);
        let rendered = err.render(src);
        assert!(
            rendered.contains("hint: Did you mean `length`?"),
            "rendered:\n{rendered}"
        );
    }

    #[test]
    fn comments_in_code() {
        // `//` is the line-comment marker. There's no
        // integer-division operator — `/` always returns a
        // `Number`, and users who want an integer cast through
        // `(a / b).to_int()` — which frees `//` for comments.
        assert_eq!(
            say(r#"// this is a comment
let x = 42 // inline comment
print(x)"#),
            "42"
        );
    }

    // ─── Instruction counting ─────────────────────────────────────

    fn count(code: &str) -> u32 {
        let stmts = parse(code).unwrap();
        count_instructions(&stmts)
    }

    #[test]
    fn count_simple_calls() {
        assert_eq!(count("print(1)"), 1);
        assert_eq!(count("print(1); print(2); print(3)"), 3);
    }

    #[test]
    fn count_repeat() {
        assert_eq!(count("repeat 7 { print(1) }"), 2);
    }

    #[test]
    fn count_if() {
        assert_eq!(count("if true { print(1) }"), 2);
        assert_eq!(count("if true { print(1) } else { print(2) }"), 3);
    }

    #[test]
    fn count_while() {
        assert_eq!(count("while true { print(1) }"), 2);
    }

    #[test]
    fn count_fn_skips_body() {
        assert_eq!(count("fn go() { print(1); print(2); print(3) }\ngo()"), 2);
    }

    #[test]
    fn count_format_independent() {
        let one_line = count("repeat 7 { print(1) }");
        let multi_line = count("repeat 7 {\n    print(1)\n}");
        assert_eq!(one_line, multi_line);
        assert_eq!(one_line, 2);
    }

    #[test]
    fn count_nested() {
        assert_eq!(count("repeat 7 { if true { print(1) } }"), 3);
    }

    #[test]
    fn count_empty_program() {
        assert_eq!(count(""), 0);
    }

    // ─── Scope / block isolation ───────────────────────────────────

    #[test]
    fn if_block_scope() {
        assert!(
            run_err(
                r#"if true { let inner = 1 }
print(inner)"#
            )
            .contains("not found")
        );
    }

    #[test]
    fn for_loop_var_scoped() {
        assert!(
            run_err(
                r#"for item in [1, 2] { let x = item }
print(item)"#
            )
            .contains("not found")
        );
    }

    // ─── Complex programs ──────────────────────────────────────────

    #[test]
    fn fizzbuzz() {
        assert_eq!(
            say(r#"let result = []
for i in range(1, 16) {
    if i % 15 == 0 {
        result.push("FizzBuzz")
    } else if i % 3 == 0 {
        result.push("Fizz")
    } else if i % 5 == 0 {
        result.push("Buzz")
    } else {
        result.push(i.to_str())
    }
}
print(result.join(", "))"#),
            "1, 2, Fizz, 4, Buzz, Fizz, 7, 8, Fizz, Buzz, 11, Fizz, 13, 14, FizzBuzz"
        );
    }

    #[test]
    fn nested_function_calls() {
        assert_eq!(
            say(r#"fn square(n) { return n * n }
fn sum_squares(a, b) { return square(a) + square(b) }
print(sum_squares(3, 4))"#),
            "25"
        );
    }

    #[test]
    fn array_manipulation_program() {
        assert_eq!(
            say(r#"let data = [5, 2, 8, 1, 9, 3]
data.sort()
let top3 = data.slice(3, 6)
print(top3.join(", "))"#),
            "5, 8, 9"
        );
    }

    // ─── Truthiness ────────────────────────────────────────────────

    #[test]
    fn truthy_values() {
        assert_eq!(say("print(if 1 { \"yes\" } else { \"no\" })"), "yes");
        assert_eq!(say(r#"print(if "x" { "yes" } else { "no" })"#), "yes");
        assert_eq!(say("print(if [1] { \"yes\" } else { \"no\" })"), "yes");
    }

    #[test]
    fn falsy_values() {
        assert_eq!(say("print(if 0 { \"yes\" } else { \"no\" })"), "no");
        assert_eq!(say("print(if false { \"yes\" } else { \"no\" })"), "no");
        assert_eq!(say("print(if none { \"yes\" } else { \"no\" })"), "no");
        assert_eq!(say(r#"print(if "" { "yes" } else { "no" })"#), "no");
    }

    // ─── Number display ────────────────────────────────────────────

    #[test]
    fn display_whole_number_as_int() {
        assert_eq!(say("print(5.0)"), "5");
    }

    #[test]
    fn display_float_with_decimals() {
        assert_eq!(say("print(3.14)"), "3.14");
    }

    // ─── Safety / resource-limit tests ──────────────────────────────

    #[test]
    fn safety_infinite_loop_halts() {
        let msg = run_err_with_limits("while true { }", tight_limits());
        assert!(msg.contains("too many steps"), "got: {msg}");
    }

    #[test]
    fn safety_memory_bomb_string_doubling() {
        let msg = run_err_with_limits(
            r#"let s = "aaaaaaaaaa"
repeat 100 { s = s + s }"#,
            tight_limits(),
        );
        assert!(msg.contains("Memory limit"), "got: {msg}");
    }

    #[test]
    fn safety_memory_bomb_array_growth() {
        let msg = run_err_with_limits(
            r#"let arr = []
repeat 1000 {
    arr.push("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
}"#,
            tight_limits(),
        );
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "got: {msg}"
        );
    }

    #[test]
    fn safety_deep_recursion_halts() {
        // Run the recursing program on a worker thread with a
        // generous stack budget. Debug builds grow each walker
        // frame enough that the default ~2 MiB stack can abort
        // (SIGABRT) before the engine's 64-frame call-depth cap
        // fires — which would look like a crash rather than the
        // clean "too many nested function calls" the sandbox
        // promises. 8 MiB comfortably fits `MAX_CALL_DEPTH`
        // walker frames with debug frame bloat, and release
        // builds never needed the headroom.
        let handle = std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| run_err_with_limits("fn f() { f() }\nf()", tight_limits()))
            .expect("spawn recursion test thread");
        let msg = handle.join().expect("recursion test thread panicked");
        assert!(
            msg.contains("nested function calls") || msg.contains("recursion"),
            "got: {msg}"
        );
    }

    #[test]
    fn safety_deep_parse_nesting() {
        let code = "(".repeat(200) + "1" + &")".repeat(200);
        let msg = parse(&code).unwrap_err().message;
        assert!(msg.contains("nested too deeply"), "got: {msg}");
    }

    #[test]
    fn safety_deep_parse_nesting_fits_a_2_mib_stack() {
        // Unlike the walker test above, the *parser's* depth cap must
        // hold within the default ~2 MiB stack of a spawned thread —
        // embedders commonly parse on worker threads, and a parse of
        // hostile input that aborts the process instead of returning
        // "nested too deeply" breaks the sandbox promise. This pins
        // the frame-size budget deliberately: growing `NyblError` (or
        // parser frames) past what MAX_PARSE_DEPTH levels fit in
        // 2 MiB must fail here, not in embedders' processes.
        let handle = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                let code = "(".repeat(200) + "1" + &")".repeat(200);
                parse(&code).unwrap_err().message
            })
            .expect("spawn parse test thread");
        let msg = handle
            .join()
            .expect("deep parse must not overflow a 2 MiB stack");
        assert!(msg.contains("nested too deeply"), "got: {msg}");
    }

    #[test]
    fn safety_string_repeat_bomb() {
        let msg = run_err_with_limits(r#"let s = "x" * 999999"#, tight_limits());
        assert!(msg.contains("Memory limit"), "got: {msg}");
    }

    #[test]
    fn safety_string_concat_bomb() {
        let msg = run_err_with_limits(
            r#"let s = "x" * 1000
repeat 100 { s = s + s }"#,
            tight_limits(),
        );
        assert!(msg.contains("Memory limit"), "got: {msg}");
    }

    #[test]
    fn safety_array_concat_bomb() {
        let msg = run_err_with_limits(
            r#"let a = range(100)
repeat 50 { a = a + a }"#,
            tight_limits(),
        );
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "got: {msg}"
        );
    }

    #[test]
    fn safety_for_in_large_string() {
        let msg = run_err_with_limits(
            r#"let s = "x" * 10000
for c in s { }"#,
            tight_limits(),
        );
        assert!(
            msg.contains("too many steps") || msg.contains("Memory limit"),
            "got: {msg}"
        );
    }

    #[test]
    fn safety_demo_limits_step_bound() {
        let msg = run_err_with_limits("let i = 0\nwhile true { i = i + 1 }", NyblLimits::demo());
        assert!(msg.contains("too many steps"), "got: {msg}");
    }

    #[test]
    fn safety_demo_limits_memory_bound() {
        let msg = run_err_with_limits(
            r#"let s = "x" * 1100000
print(s)"#,
            NyblLimits::demo(),
        );
        assert!(msg.contains("Memory limit"), "got: {msg}");
    }

    #[test]
    fn safety_nested_loop_step_bound() {
        let msg = run_err_with_limits("repeat 100 { repeat 100 { let x = 1 } }", tight_limits());
        assert!(msg.contains("too many steps"), "got: {msg}");
    }

    #[test]
    fn safety_string_split_bomb() {
        let msg = run_err_with_limits(
            r#"let s = "abababababab" * 2000
let parts = s.split("a")
let x = 1"#,
            tight_limits(),
        );
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "got: {msg}"
        );
    }

    #[test]
    fn safety_join_bomb() {
        let msg = run_err_with_limits(
            r#"let a = []
repeat 400 { a.push("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa") }
let s = a.join("")
let x = 1"#,
            tight_limits(),
        );
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "got: {msg}"
        );
    }

    #[test]
    fn safety_range_hard_cap() {
        let msg = run_err_with_limits(
            r#"let a = range(100000)
let x = 1"#,
            tight_limits(),
        );
        assert!(
            msg.contains(builtins::RANGE_LIMIT_ERROR_MESSAGE),
            "got: {msg}"
        );
    }

    #[test]
    fn safety_array_method_doubling() {
        let msg = run_err_with_limits(
            r#"let a = []
repeat 400 { a.push("aaaaaaaaaaaaaaaaaaaaaa") }
a.reverse()
let x = 1"#,
            tight_limits(),
        );
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "got: {msg}"
        );
    }

    #[test]
    fn safety_preflight_catches() {
        let limits = NyblLimits {
            max_steps: 500,
            max_memory: 32 * 1024,
            ..NyblLimits::standard()
        };
        let msg = run_err_with_limits(r#"let s = "x" * 40000"#, limits);
        assert!(msg.contains("Memory limit"), "got: {msg}");
    }

    #[test]
    fn safety_final_retained_allocation_is_checked() {
        let limits = NyblLimits {
            max_steps: 500,
            max_memory: 64 * 1024,
            ..NyblLimits::standard()
        };
        let mut host = TestHost::new();
        let error = run(
            r#"let s = "abababab" * 1000
let parts = s.split("a")"#,
            &mut host,
            &limits,
        )
        .expect_err("retained allocations above the limit must fail at the return boundary");
        assert!(error.is_fatal);
        assert!(error.message.contains("Memory limit exceeded"));
    }

    #[test]
    fn safety_dict_growth_tracked() {
        let msg = run_err_with_limits(
            r#"let d = {}
repeat 400 {
    d[d.len().to_str()] = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
}
let x = 1"#,
            tight_limits(),
        );
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "got: {msg}"
        );
    }

    // ─── NyblHost extension ─────────────────────────────────────────

    struct CustomHost {
        prints: Vec<String>,
    }

    impl NyblHost for CustomHost {
        fn call(
            &mut self,
            name: &str,
            args: &[Value],
            line: u32,
        ) -> Option<Result<Value, NyblError>> {
            match name {
                "greet" => {
                    if args.len() != 1 {
                        return Some(Err(NyblError {
                            line: Some(line),
                            column: None,
                            message: "greet() needs 1 argument".into(),
                            friendly_hint: None,
                            source_context: None,
                            is_fatal: false,
                            is_try_return: false,
                        }));
                    }
                    Some(Ok(Value::new_str(format!("Hello, {}!", args[0]))))
                }
                _ => None,
            }
        }

        fn on_print(&mut self, message: &str) {
            self.prints.push(message.to_string());
        }

        fn function_hint(&self) -> &str {
            "Available: greet(name)"
        }
    }

    #[test]
    fn host_custom_builtin() {
        let mut host = CustomHost { prints: vec![] };
        run(
            r#"print(greet("world"))"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.prints, vec!["Hello, world!"]);
    }

    #[test]
    fn host_function_hint() {
        let mut host = CustomHost { prints: vec![] };
        let err = run("unknown()", &mut host, &NyblLimits::standard()).unwrap_err();
        assert!(err.message.contains("not found"));
    }

    // ─── Pattern matching ─────────────────────────────────────────

    #[test]
    fn match_literal_arms() {
        assert_eq!(
            say(r#"let x = 2
let out = match x {
    1 => "one",
    2 => "two",
    3 => "three",
    _ => "other",
}
print(out)"#),
            "two"
        );
    }

    #[test]
    fn match_falls_through_to_wildcard() {
        assert_eq!(
            say(r#"let x = 42
print(match x {
    1 => "one",
    _ => "other",
})"#),
            "other"
        );
    }

    #[test]
    fn match_no_arm_errors() {
        let err = run_err(
            r#"let x = 5
match x { 1 => "a", 2 => "b" }"#,
        );
        assert!(err.contains("No match arm matched"), "got: {err}");
    }

    #[test]
    fn match_binding_captures_scrutinee() {
        assert_eq!(
            say(r#"print(match 42 {
    x => x + 1,
})"#),
            "43"
        );
    }

    #[test]
    fn match_guard_accepts() {
        assert_eq!(
            say(r#"print(match 7 {
    n if n > 10 => "big",
    n if n > 0 => "small",
    _ => "zero or less",
})"#),
            "small"
        );
    }

    #[test]
    fn match_guard_rejects_continues() {
        assert_eq!(
            say(r#"print(match 5 {
    n if n < 0 => "neg",
    n if n > 100 => "huge",
    _ => "mid",
})"#),
            "mid"
        );
    }

    #[test]
    fn match_or_pattern() {
        assert_eq!(
            say(r#"let x = 3
print(match x {
    1 | 2 | 3 => "small",
    _ => "other",
})"#),
            "small"
        );
    }

    #[test]
    fn match_enum_unit_variant() {
        assert_eq!(
            say(r#"enum E { A, B, C }
print(match E::B {
    E::A => "a",
    E::B => "b",
    E::C => "c",
})"#),
            "b"
        );
    }

    #[test]
    fn match_enum_tuple_binds() {
        assert_eq!(
            say(r#"enum Shape { Circle(r), Square(s), Empty }
let s = Shape::Circle(5)
print(match s {
    Shape::Circle(r) => r * 2,
    Shape::Square(s) => s * s,
    Shape::Empty => 0,
})"#),
            "10"
        );
    }

    #[test]
    fn match_enum_struct_variant_binds() {
        assert_eq!(
            say(r#"enum Shape { Rect { w, h }, Empty }
let r = Shape::Rect { w: 4, h: 3 }
print(match r {
    Shape::Rect { w, h } => w * h,
    Shape::Empty => 0,
})"#),
            "12"
        );
    }

    #[test]
    fn match_struct_destructure() {
        assert_eq!(
            say(r#"struct Point { x, y }
let p = Point { x: 7, y: 3 }
print(match p {
    Point { x, y } => x + y,
})"#),
            "10"
        );
    }

    #[test]
    fn match_struct_partial_with_rest() {
        // `Point { x, .. }` matches regardless of the other
        // fields. The walker's match_struct_fields looks up by
        // name; `rest` relaxes the "mention every field" rule.
        assert_eq!(
            say(r#"struct Triple { a, b, c }
let t = Triple { a: 1, b: 2, c: 3 }
print(match t {
    Triple { b, .. } => b,
})"#),
            "2"
        );
    }

    #[test]
    fn match_nested_pattern() {
        // Classic Rust-style: Err(FileError::NotFound(path)).
        assert_eq!(
            say(
                r#"enum FileError { NotFound(path), Permission(path), Other }
enum Result { Ok(value), Err(error) }
let r = Result::Err(FileError::NotFound("/etc/passwd"))
print(match r {
    Result::Ok(v) => v,
    Result::Err(FileError::NotFound(p)) => p,
    Result::Err(FileError::Permission(p)) => p,
    Result::Err(FileError::Other) => "other",
})"#
            ),
            "/etc/passwd"
        );
    }

    #[test]
    fn match_array_exact() {
        assert_eq!(
            say(r#"let a = [1, 2, 3]
print(match a {
    [] => "empty",
    [x] => "one",
    [x, y] => "two",
    [x, y, z] => x + y + z,
    _ => "long",
})"#),
            "6"
        );
    }

    #[test]
    fn match_array_with_rest() {
        assert_eq!(
            say(r#"let a = [10, 20, 30, 40, 50]
print(match a {
    [head, ..rest] => rest,
    _ => [],
})"#),
            "[20, 30, 40, 50]"
        );
    }

    #[test]
    fn match_array_with_ignored_rest() {
        assert_eq!(
            say(r#"let a = [10, 20, 30]
print(match a {
    [first, ..] => first,
    _ => 0,
})"#),
            "10"
        );
    }

    #[test]
    fn match_binding_scope_limited_to_arm() {
        assert!(
            run_err(
                r#"let v = 5
match v { x => print(x) }
print(x)"#
            )
            .contains("not found")
        );
    }

    #[test]
    fn match_negative_literal() {
        assert_eq!(
            say(r#"print(match -3 {
    -3 => "neg three",
    _ => "other",
})"#),
            "neg three"
        );
    }

    #[test]
    fn match_string_literal() {
        assert_eq!(
            say(r#"let s = "hello"
print(match s {
    "hi" => 1,
    "hello" => 2,
    _ => 0,
})"#),
            "2"
        );
    }

    #[test]
    fn match_bool_none() {
        assert_eq!(
            say(r#"print(match true {
    true => "t",
    false => "f",
})"#),
            "t"
        );
        assert_eq!(
            say(r#"print(match none {
    none => "n",
    _ => "other",
})"#),
            "n"
        );
    }

    // ─── `try` operator ────────────────────────────────────────────

    #[test]
    fn try_unwraps_ok_variant() {
        assert_eq!(
            say(r#"enum Result { Ok(v), Err(e) }
fn doit() {
    let v = try Result::Ok(42)
    return v
}
print(doit())"#),
            "42"
        );
    }

    #[test]
    fn try_propagates_err_variant() {
        // `try` on Err inside a fn causes the fn to return the
        // same Err variant unchanged. The caller matches it out.
        assert_eq!(
            say(r#"enum Result { Ok(v), Err(e) }
fn doit() {
    let v = try Result::Err("boom")
    return Result::Ok(v)
}
let r = doit()
print(match r {
    Result::Ok(v) => v,
    Result::Err(e) => e,
})"#),
            "boom"
        );
    }

    #[test]
    fn try_sentinel_uses_flag_not_message_string() {
        // Regression for the tech-debt-2 refactor: the walker
        // used to check whether a `NyblError`'s `.message`
        // equalled the string `"__nybl_try_return_signal__"` to
        // know whether an error was a `try`-unwinding sentinel.
        // Now it checks `is_try_return` — a dedicated flag that
        // user-authored errors can't accidentally set.
        //
        // A user program whose code happens to produce that
        // exact message should NOT trigger try-return unwind;
        // it should propagate as a normal runtime error.
        // We can't construct that specific message from Nybl
        // source directly (none of our error sites spell it),
        // but we can verify the flag-based design by pinning
        // the NyblError the walker produces: a real runtime
        // failure has `is_try_return: false`, while a `try`-
        // triggered unwind internally carries `is_try_return:
        // true` but is caught at the fn boundary and never
        // reaches the caller.
        let mut host = TestHost::new();
        let err = run("print(1 / 0)", &mut host, &test_limits()).unwrap_err();
        assert_eq!(err.message, "Division by zero");
        // A real error must not be classified as a try-return
        // — otherwise it'd silently get swallowed by any
        // enclosing `try_call`.
        assert!(!err.is_try_return, "got: {err:?}");
        // And the fatal flag is independent of the try flag.
        assert!(!err.is_fatal, "got: {err:?}");
    }

    #[test]
    fn try_chains_through_nested_calls() {
        // An Err at the deepest fn short-circuits back up through
        // the whole chain, skipping each caller's remaining work.
        assert_eq!(
            say(r#"enum Result { Ok(v), Err(e) }
fn leaf() { return Result::Err("leaf-err") }
fn middle() {
    let v = try leaf()
    return Result::Ok(v + 1)
}
fn top() {
    let v = try middle()
    return Result::Ok(v * 2)
}
print(match top() {
    Result::Ok(v) => v,
    Result::Err(e) => e,
})"#),
            "leaf-err"
        );
    }

    #[test]
    fn user_result_with_unit_ok_coexists_with_builtin() {
        // Module-scoped types: the program's own `enum Result
        // { Ok, Err(e) }` lives under `<root>.Result`, distinct
        // from the `<builtin>.Result` that `try_call` returns.
        // `try Result::Ok` resolves to the user's root-level
        // Result and unwraps the Unit-Ok variant to `none`.
        assert_eq!(
            say(r#"enum Result { Ok, Err(e) }
fn doit() {
    let v = try Result::Ok
    return v.type()
}
print(doit())"#),
            "none"
        );
    }

    #[test]
    fn try_inside_lambda_returns_from_lambda_only() {
        // The lambda's own fn-boundary catches the `try` unwind;
        // the caller keeps running.
        assert_eq!(
            say(r#"enum Result { Ok(v), Err(e) }
let f = fn() {
    let v = try Result::Err("inner")
    return Result::Ok(v)
}
let r = f()
print("after lambda")
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e,
})"#),
            "inner"
        );
    }

    #[test]
    fn try_at_top_level_on_err_value_errors() {
        let error = run_err_full(
            r#"enum Result { Ok(v), Err(e) }
let r = try Result::Err("boom")"#,
        );
        assert_eq!(
            error.message,
            crate::error_messages::TOP_LEVEL_TRY_ERROR_MESSAGE
        );
        assert_eq!(
            error.friendly_hint.as_deref(),
            Some(crate::error_messages::TOP_LEVEL_TRY_HINT)
        );
        assert_eq!(error.line, Some(2));
        assert_eq!(error.column, None);
    }

    #[test]
    fn try_on_non_result_errors() {
        let msg = run_err(
            r#"fn doit() {
    let v = try 42
    return v
}
doit()"#,
        );
        assert!(msg.contains("Result-shaped"), "got: {msg}");
    }

    #[test]
    fn try_ok_tuple_wrong_arity_errors() {
        // Module-scoped types: the program's own two-field
        // `Ok(a, b)` is a valid enum declaration (lives under
        // `<root>.Result`), but `try` still enforces the
        // "Ok must carry exactly one value" rule it uses to
        // unwrap successful results. The check fires at the
        // `try` site, not at the type declaration.
        let msg = run_err(
            r#"enum Result { Ok(a, b), Err(e) }
fn doit() {
    let v = try Result::Ok(1, 2)
    return v
}
doit()"#,
        );
        assert!(
            msg.contains("Ok variant must carry exactly one"),
            "got: {msg}"
        );
    }

    #[test]
    fn try_in_for_loop_short_circuits() {
        // `try` on the first Err ends the loop and the fn.
        assert_eq!(
            say(r#"enum Result { Ok(v), Err(e) }
fn lookup(i) {
    if i == 2 { return Result::Err("stop") }
    return Result::Ok(i * 10)
}
fn sum_until_err() {
    let total = 0
    for i in range(5) {
        let v = try lookup(i)
        total = total + v
    }
    return Result::Ok(total)
}
print(match sum_until_err() {
    Result::Ok(v) => v,
    Result::Err(e) => e,
})"#),
            "stop"
        );
    }

    #[test]
    fn try_threaded_through_nested_fn_composition() {
        // Mirrors the "try lowers to match" equivalence: a fn
        // using `try` delivers the same outcome as a hand-
        // written match+return using the same Err short-circuit.
        assert_eq!(
            say(r#"enum Result { Ok(v), Err(e) }
fn compute(input) {
    if input < 0 { return Result::Err("negative") }
    return Result::Ok(input * 2)
}
fn with_try(x) {
    let doubled = try compute(x)
    return Result::Ok(doubled + 1)
}
print(match with_try(5) { Result::Ok(v) => v, Result::Err(_) => -1 })
print(match with_try(-1) { Result::Ok(_) => "ok", Result::Err(e) => e })"#),
            "negative"
        );
    }

    // ─── `try_call` builtin ────────────────────────────────────────

    #[test]
    fn try_call_wraps_successful_return_in_ok() {
        // Plain successful call: `try_call(f)` yields
        // `Result::Ok(return_value)`. The program doesn't need
        // to declare `Result` — the value comes out pre-shaped
        // because `try_call` constructs it directly.
        assert_eq!(
            say(r#"let r = try_call(fn() { return 42 })
print(match r {
    Result::Ok(v) => v,
    Result::Err(_) => -1,
})"#),
            "42"
        );
    }

    #[test]
    fn try_call_wraps_non_fatal_error_in_err() {
        // Division by zero is a non-fatal runtime error, so
        // `try_call` catches it and yields
        // `Result::Err(RuntimeError { message, line })`.
        assert_eq!(
            say(r#"let r = try_call(fn() { return 1 / 0 })
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e.message,
})"#),
            "Division by zero"
        );
    }

    #[test]
    fn try_call_runtime_error_carries_line_number() {
        // The RuntimeError struct exposes `line` so callers can
        // report where the failure happened.
        assert_eq!(
            say(r#"let r = try_call(fn() {
    let x = 1
    return x / 0
})
print(match r {
    Result::Ok(_) => -1,
    Result::Err(e) => e.line,
})"#),
            "3"
        );
    }

    #[test]
    fn try_call_step_limit_error_is_fatal_and_bypasses_wrap() {
        // The step-limit error is fatal — `try_call` must NOT
        // swallow it or the sandbox invariant breaks. The
        // outer `run()` sees the fatal error unchanged.
        let tight = NyblLimits {
            max_steps: 200,
            max_memory: 1 << 20,
            ..NyblLimits::standard()
        };
        let mut host = TestHost::new();
        let err = run(
            r#"let r = try_call(fn() {
    while true { }
})
print("should never run")"#,
            &mut host,
            &tight,
        )
        .unwrap_err();
        assert!(err.is_fatal, "expected fatal: {}", err.message);
        assert!(
            err.message.contains("too many steps"),
            "got: {}",
            err.message
        );
        // The post-try_call `print` never ran because the
        // error short-circuited the program.
        assert!(host.prints.borrow().is_empty());
    }

    #[test]
    fn try_call_plays_with_try_operator_to_chain_errors() {
        // Classic "convert caught runtime error into a Result".
        // The fn uses `try` to short-circuit on Err; the outer
        // wraps the whole thing in try_call to catch anything
        // it didn't anticipate.
        assert_eq!(
            say(r#"fn risky(x) {
    let arr = [1, 2]
    return arr[x]  // out-of-bounds when x > 1
}
let r = try_call(fn() { return risky(5) })
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e.message,
})"#),
            "Index 5 is out of bounds (array has 2 items)"
        );
    }

    #[test]
    fn try_call_errors_on_wrong_arg_count() {
        let msg = run_err("try_call()");
        assert!(msg.contains("try_call` expects 1"), "got: {msg}");
    }

    #[test]
    fn try_call_errors_on_non_function_arg() {
        let msg = run_err("try_call(42)");
        assert!(msg.contains("try_call` expects a function"), "got: {msg}");
    }

    #[test]
    fn try_call_result_ok_is_matchable_even_without_declared_type() {
        // The returned `Result::Ok(...)` carries a type_name
        // of `"Result"` and variant_name of `"Ok"` — the
        // pattern matcher uses string comparison, so the user's
        // pattern matches regardless of whether they declared
        // their own `Result` enum. Same for `RuntimeError`.
        assert_eq!(
            say(r#"let r = try_call(fn() { return "yay" })
print(match r {
    Result::Ok(v) => v + "!",
    Result::Err(_) => "bad",
})"#),
            "yay!"
        );
    }

    #[test]
    fn try_call_nested_outer_sees_ok_of_inner_err() {
        // Inner try_call catches its own error and returns
        // `Result::Err(...)`. Outer try_call sees a clean
        // return and wraps THAT in `Result::Ok(...)`.
        assert_eq!(
            say(r#"let r = try_call(fn() {
    let inner = try_call(fn() { return 1 / 0 })
    return inner
})
print(match r {
    Result::Ok(Result::Err(e)) => e.message,
    Result::Ok(Result::Ok(_)) => "inner ok?",
    Result::Err(_) => "outer caught",
})"#),
            "Division by zero"
        );
    }

    // ─── Integer type (phase 6) ────────────────────────────────────

    #[test]
    fn int_literal_produces_int_value() {
        assert_eq!(say("print(42.type())"), "int");
        assert_eq!(say("print((-3).type())"), "int");
        assert_eq!(say("print(0.type())"), "int");
    }

    #[test]
    fn float_literal_produces_number_value() {
        assert_eq!(say("print(42.0.type())"), "number");
        assert_eq!(say("print(3.14.type())"), "number");
        assert_eq!(say("print((-0.5).type())"), "number");
    }

    #[test]
    fn int_int_arithmetic_stays_int() {
        assert_eq!(say("print((1 + 2).type())"), "int");
        assert_eq!(say("print(1 + 2)"), "3");
        assert_eq!(say("print(10 - 4)"), "6");
        assert_eq!(say("print(3 * 4)"), "12");
        assert_eq!(say("print(10 % 3)"), "1");
    }

    #[test]
    fn division_slash_always_returns_number() {
        // `/` always produces a `Number`, even for `Int / Int`.
        // Sidesteps the "1 / 2 == 0" surprise every C-family
        // language inflicts on beginners.
        assert_eq!(say("print((10 / 3).type())"), "number");
        assert_eq!(say("print(10 / 4)"), "2.5");
        assert_eq!(say("print((10 / 5).type())"), "number");
    }

    #[test]
    fn int_division_via_int_of_quotient() {
        // There's no dedicated `//` operator — users who want
        // integer division coerce the float result back with
        // `(...).to_int()`. Truncates toward zero, matching the
        // behaviour of the removed `//` operator.
        assert_eq!(say("print((10 / 3).to_int().type())"), "int");
        assert_eq!(say("print((10 / 3).to_int())"), "3");
        assert_eq!(say("print((-7 / 2).to_int())"), "-3");
        assert_eq!(say("print((10 / -3).to_int())"), "-3");
    }

    #[test]
    fn int_number_mixed_widens_to_number() {
        assert_eq!(say("print((1 + 2.0).type())"), "number");
        assert_eq!(say("print(1 + 2.0)"), "3");
        assert_eq!(say("print(3 * 0.5)"), "1.5");
        assert_eq!(say("print((2.0 - 1).type())"), "number");
    }

    #[test]
    fn int_comparison_uses_exact_integer_ordering() {
        assert_eq!(say("print(10 < 20)"), "true");
        assert_eq!(say("print(10 == 10)"), "true");
        // Cross-type numeric equality: int == number when
        // numerically equal.
        assert_eq!(say("print(1 == 1.0)"), "true");
        assert_eq!(say("print(2 > 1.5)"), "true");
    }

    #[test]
    fn division_by_zero_errors() {
        let msg = run_err("print(10 / 0)");
        assert!(msg.contains("Division by zero"), "got: {msg}");
    }

    #[test]
    fn int_overflow_on_add_errors() {
        // i64::MAX + 1 overflows. The message should mention
        // "overflow".
        let msg = run_err("print(9223372036854775807 + 1)");
        assert!(msg.contains("Integer overflow"), "got: {msg}");
    }

    #[test]
    fn i64_min_literal_is_an_exact_int_in_expressions_and_patterns() {
        assert_eq!(say("print(-9223372036854775808)"), "-9223372036854775808");
        assert_eq!(say("print((-9223372036854775808).type())"), "int");
        assert_eq!(
            say("print(-9223372036854775808 == (-9223372036854775807 - 1))"),
            "true"
        );
        assert_eq!(
            say("print(-9223372036854775808 < -9223372036854775807)"),
            "true"
        );
        assert_eq!(
            say("print(-9223372036854775808 + 1)"),
            "-9223372036854775807"
        );
        assert_eq!(
            say(r#"let min = -9223372036854775808
print(match min {
    -9223372036854775808 => "minimum",
    _ => "other",
})"#,),
            "minimum"
        );
    }

    #[test]
    fn int_overflow_on_neg_of_i64_min_errors() {
        for source in [
            "print(--9223372036854775808)",
            "print(-9223372036854775808 - 1)",
        ] {
            let msg = run_err(source);
            assert_eq!(msg, "Integer overflow in `-`", "source: {source}");
        }
    }

    #[test]
    fn int_builtin_converts_to_int() {
        assert_eq!(say("print(3.7.to_int())"), "3");
        assert_eq!(say("print(3.7.to_int().type())"), "int");
        assert_eq!(say(r#"print("42".to_int())"#), "42");
        assert_eq!(say(r#"print("42".to_int().type())"#), "int");
        // Truncating a string that looks float-y still works.
        assert_eq!(say(r#"print("3.7".to_int())"#), "3");
    }

    #[test]
    fn float_builtin_converts_to_number() {
        assert_eq!(say("print(42.to_float())"), "42");
        assert_eq!(say("print(42.to_float().type())"), "number");
        assert_eq!(say(r#"print("3.14".to_float())"#), "3.14");
    }

    #[test]
    fn len_returns_int() {
        assert_eq!(say(r#"print("hi".len().type())"#), "int");
        assert_eq!(say("print([1, 2, 3].len().type())"), "int");
    }

    #[test]
    fn range_produces_int_elements() {
        assert_eq!(say("print((range(3)[0]).type())"), "int");
    }

    #[test]
    fn array_index_accepts_int_and_float() {
        // Both `arr[0]` (Int) and `arr[0.0]` (Number-via-cast)
        // should work — keeps legacy code with `0.0` running.
        assert_eq!(say("let a = [10, 20]\nprint(a[0])"), "10");
        assert_eq!(say("let a = [10, 20]\nprint(a[0.0])"), "10");
    }

    #[test]
    fn int_match_literal_pattern() {
        assert_eq!(
            say(r#"let x = 2
print(match x {
    1 => "one",
    2 => "two",
    _ => "other",
})"#),
            "two"
        );
    }

    #[test]
    fn repeat_accepts_int() {
        assert_eq!(
            say(r#"let n = 0
repeat 5 { n = n + 1 }
print(n)"#),
            "5"
        );
    }

    #[test]
    fn int_overflow_literal_parse_errors() {
        // Integer literal that doesn't fit in i64 is a
        // lex-time error, not a silent downgrade to float.
        let msg = parse_err("let x = 99999999999999999999");
        assert!(msg.contains("out of range"), "got: {msg}");
    }

    #[test]
    fn signed_integer_literal_boundaries_report_the_owning_column() {
        let cases = [
            ("let x = 9223372036854775808", 9),
            ("let x = 9223372036854775809", 9),
            ("let x = -9223372036854775809", 10),
        ];
        for (source, column) in cases {
            let error = parse_err_full(source);
            assert_eq!(error.line, Some(1), "source: {source}");
            assert_eq!(error.column, Some(column), "source: {source}");
            assert!(error.message.contains("out of range"), "error: {error:?}");
        }
    }

    // ─── Modules / use ──────────────────────────────────────────

    /// Host that resolves modules from an in-memory map keyed by
    /// the dot-joined use path. Captures prints and tracks how
    /// many times each module was resolved so we can pin the
    /// caching behaviour.
    struct ModuleHost {
        prints: RefCell<Vec<String>>,
        modules: std::collections::HashMap<String, String>,
        resolve_counts: RefCell<std::collections::HashMap<String, u32>>,
    }

    impl ModuleHost {
        fn new(modules: &[(&str, &str)]) -> Self {
            let mut map = std::collections::HashMap::new();
            for (name, source) in modules {
                map.insert((*name).to_string(), (*source).to_string());
            }
            Self {
                prints: RefCell::new(Vec::new()),
                modules: map,
                resolve_counts: RefCell::new(std::collections::HashMap::new()),
            }
        }

        fn prints(&self) -> Vec<String> {
            self.prints.borrow().clone()
        }

        fn resolve_count(&self, name: &str) -> u32 {
            *self.resolve_counts.borrow().get(name).unwrap_or(&0)
        }
    }

    impl NyblHost for ModuleHost {
        fn call(
            &mut self,
            _name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, NyblError>> {
            None
        }

        fn on_print(&mut self, message: &str) {
            self.prints.borrow_mut().push(message.to_string());
        }

        fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
            *self
                .resolve_counts
                .borrow_mut()
                .entry(name.to_string())
                .or_insert(0) += 1;
            self.modules.get(name).cloned().map(Ok)
        }
    }

    #[test]
    fn import_brings_let_binding_into_scope() {
        let mut host = ModuleHost::new(&[("math", "let pi = 3")]);
        run(
            r#"use math
print(pi)"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.prints(), vec!["3"]);
    }

    #[test]
    fn import_brings_fn_into_scope() {
        let mut host = ModuleHost::new(&[(
            "math",
            r#"fn square(n) { return n * n }
let pi = 3"#,
        )]);
        run(
            r#"use math
print(square(5))
print(pi)"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.prints(), vec!["25", "3"]);
    }

    #[test]
    fn import_dotted_path_passes_through_to_host() {
        let mut host = ModuleHost::new(&[("std.math", "let e = 2")]);
        run(
            r#"use std.math
print(e)"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.prints(), vec!["2"]);
        // Exactly one resolve — `std.math` is the full key.
        assert_eq!(host.resolve_count("std.math"), 1);
    }

    #[test]
    fn import_module_not_found_errors() {
        let mut host = ModuleHost::new(&[]);
        let err = run("use nope", &mut host, &NyblLimits::standard()).unwrap_err();
        assert!(
            err.message.contains("Module `nope` not found"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn import_cache_resolves_once() {
        // Two imports of the same module in the same run should
        // only hit the resolver once.
        let mut host = ModuleHost::new(&[("m", "let x = 1")]);
        run(
            r#"use m
use m
print(x)"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.prints(), vec!["1"]);
        assert_eq!(host.resolve_count("m"), 1);
    }

    #[test]
    fn import_module_can_import_other_modules() {
        let mut host = ModuleHost::new(&[
            ("a", "use b\nlet doubled_pi = pi + pi"),
            ("b", "let pi = 3"),
        ]);
        run(
            r#"use a
print(doubled_pi)"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.prints(), vec!["6"]);
    }

    #[test]
    fn imported_module_parse_error_renders_against_module_source() {
        let module_source = "let okay = 1\nlet broken =";
        let root_source = "use bad";
        let mut host = ModuleHost::new(&[("bad", module_source)]);

        let error = run(root_source, &mut host, &NyblLimits::standard()).unwrap_err();
        let rendered = error.render(root_source);

        assert_eq!(
            error
                .source_context
                .as_ref()
                .map(|context| context.module_path.as_str()),
            Some("bad")
        );
        assert!(rendered.contains("in module `bad` at line 2"));
        assert!(rendered.contains("let broken ="));
        assert!(!rendered.contains("1 | use bad"));
    }

    #[test]
    fn transitive_module_parse_error_preserves_leaf_identity() {
        let root_source = "use outer";
        let inner_source = "let okay = 1\nlet broken =";
        let mut host = ModuleHost::new(&[
            ("outer", "use inner\nlet outer = 1"),
            ("inner", inner_source),
        ]);

        let error = run(root_source, &mut host, &NyblLimits::standard()).unwrap_err();
        let context = error.source_context.as_ref().expect("module context");

        assert_eq!(context.module_path, "inner");
        assert_eq!(context.source.as_deref(), Some(inner_source));
        assert!(
            error
                .render(root_source)
                .contains("in module `inner` at line 2")
        );
    }

    #[test]
    fn import_circular_detected() {
        let mut host = ModuleHost::new(&[("a", "use b\nlet x = 1"), ("b", "use a\nlet y = 2")]);
        let err = run("use a", &mut host, &NyblLimits::standard()).unwrap_err();
        assert!(
            err.message.contains("Circular import"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn glob_use_shadowing_is_a_warning_first_wins() {
        // Under the new semantics, glob imports emit a warning
        // (rather than an error) when a name they'd bring in is
        // already bound. The first definition wins and the
        // program continues. A user who genuinely wants the
        // imported value can use the selective or aliased form
        // to opt in explicitly.
        let mut host = ModuleHost::new(&[("m", "let x = 99")]);
        run(
            r#"let x = 1
use m
print(x)"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .expect("run ok");
        assert_eq!(host.prints(), vec!["1".to_string()]);
    }

    #[test]
    fn use_selective_form_pulls_only_listed_names() {
        let mut host = ModuleHost::new(&[("m", "let a = 1\nlet b = 2\nlet c = 3")]);
        run(
            r#"use m.{a, c}
print(a)
print(c)"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .expect("run ok");
        assert_eq!(host.prints(), vec!["1".to_string(), "3".to_string()]);
    }

    #[test]
    fn use_selective_unknown_name_errors() {
        let mut host = ModuleHost::new(&[("m", "let a = 1")]);
        let err = run(r#"use m.{b}"#, &mut host, &NyblLimits::standard()).unwrap_err();
        assert!(
            err.message.contains("isn't exported"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn use_alias_binds_module_value() {
        let mut host = ModuleHost::new(&[("m", "let pi = 3\nfn double(n) { return n + n }")]);
        run(
            r#"use m as m
print(m.pi)
print(m.double(7))"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .expect("run ok");
        assert_eq!(host.prints(), vec!["3".to_string(), "14".to_string()]);
    }

    #[test]
    fn use_alias_selective_form() {
        let mut host = ModuleHost::new(&[("m", "let a = 1\nlet b = 2\nlet c = 3")]);
        run(
            r#"use m.{a, c} as m
print(m.a)
print(m.c)"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .expect("run ok");
        assert_eq!(host.prints(), vec!["1".to_string(), "3".to_string()]);
    }

    #[test]
    fn use_alias_rejects_missing_module_field() {
        let mut host = ModuleHost::new(&[("m", "let a = 1")]);
        let err = run(
            r#"use m as m
print(m.b)"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap_err();
        assert!(
            err.message.contains("isn't exported"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn glob_skips_underscore_prefixed_exports() {
        // Privacy convention: glob import skips `_foo`, so the
        // importer can't see it unless they selectively opt in.
        let mut host = ModuleHost::new(&[("m", "let public = 1\nlet _private = 2")]);
        let err = run(
            r#"use m
print(_private)"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap_err();
        // The private binding didn't land in scope — we get a
        // normal "variable not found" error.
        assert!(
            err.message.contains("_private"),
            "expected `_private not found`, got: {}",
            err.message
        );
    }

    #[test]
    fn selective_form_can_reach_underscore_prefixed_names() {
        // Explicit listing overrides the privacy skip — the user
        // said they want `_private` and got it.
        let mut host = ModuleHost::new(&[("m", "let _private = 42")]);
        run(
            r#"use m.{_private}
print(_private)"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .expect("run ok");
        assert_eq!(host.prints(), vec!["42".to_string()]);
    }

    #[test]
    fn alias_exposes_underscore_prefixed_names() {
        // Alias form keeps everything — privacy was about not
        // polluting the caller's scope with glob. When the user
        // says `as m`, they accept the whole module surface.
        let mut host = ModuleHost::new(&[("m", "let _private = 7")]);
        run(
            r#"use m as m
print(m._private)"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .expect("run ok");
        assert_eq!(host.prints(), vec!["7".to_string()]);
    }

    #[test]
    fn alias_namespaced_struct_literal() {
        let mut host = ModuleHost::new(&[(
            "g",
            "struct Entity { id, hp }\nfn spawn(id) { return Entity { id: id, hp: 100 } }",
        )]);
        run(
            r#"use g as g
let e = g.Entity { id: 1, hp: 50 }
print(e.id)
print(e.hp)"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .expect("run ok");
        assert_eq!(host.prints(), vec!["1".to_string(), "50".to_string()]);
    }

    #[test]
    fn alias_namespaced_variant_ctor() {
        let mut host = ModuleHost::new(&[("r", "enum Result { Ok(v), Err(e) }")]);
        run(
            r#"use r as r
let v = r.Result::Ok(42)
match v {
    r.Result::Ok(n) => print(n),
    r.Result::Err(_) => print("err"),
}"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .expect("run ok");
        assert_eq!(host.prints(), vec!["42".to_string()]);
    }

    #[test]
    fn alias_module_value_is_a_module_type() {
        let mut host = ModuleHost::new(&[("m", "let x = 1")]);
        run(
            r#"use m as mm
print(mm.type())"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .expect("run ok");
        assert_eq!(host.prints(), vec!["module".to_string()]);
    }

    // ─── Structs ──────────────────────────────────────────────────

    #[test]
    fn struct_decl_and_construct() {
        assert_eq!(
            say(r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
print(p.x)
print(p.y)"#),
            "4"
        );
    }

    #[test]
    fn struct_display_shows_type_name_and_fields() {
        assert_eq!(
            say(r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
print(p)"#),
            "Point { x: 3, y: 4 }"
        );
    }

    #[test]
    fn struct_fields_respect_declaration_order() {
        // Fields specified out of declaration order should still
        // appear in declaration order in the value — stable
        // ordering matters for `print` / `inspect` / equality.
        assert_eq!(
            say(r#"struct Point { x, y }
let p = Point { y: 4, x: 3 }
print(p)"#),
            "Point { x: 3, y: 4 }"
        );
    }

    #[test]
    fn struct_equality_is_structural() {
        assert_eq!(
            say(r#"struct Point { x, y }
let a = Point { x: 1, y: 2 }
let b = Point { x: 1, y: 2 }
print(a == b)"#),
            "true"
        );
        assert_eq!(
            say(r#"struct Point { x, y }
let a = Point { x: 1, y: 2 }
let b = Point { x: 1, y: 3 }
print(a == b)"#),
            "false"
        );
    }

    #[test]
    fn struct_different_types_never_equal() {
        assert_eq!(
            say(r#"struct A { x }
struct B { x }
let a = A { x: 1 }
let b = B { x: 1 }
print(a == b)"#),
            "false"
        );
    }

    #[test]
    fn struct_type_name_is_struct() {
        // `type()` returns a generic bucket; a per-type name
        // would require `display_type_name()` which isn't wired
        // to the builtin yet.
        assert_eq!(
            say(r#"struct Foo { a }
print(Foo { a: 1 }.type())"#),
            "struct"
        );
    }

    #[test]
    fn struct_missing_field_errors() {
        let err = run_err(
            r#"struct Point { x, y }
let p = Point { x: 1 }"#,
        );
        assert!(err.contains("Missing field"), "got: {err}");
    }

    #[test]
    fn struct_extra_field_errors() {
        let err = run_err(
            r#"struct Point { x, y }
let p = Point { x: 1, y: 2, z: 3 }"#,
        );
        assert!(err.contains("has no field"), "got: {err}");
    }

    #[test]
    fn struct_duplicate_field_errors() {
        let err = run_err(
            r#"struct Point { x, y }
let p = Point { x: 1, x: 2, y: 3 }"#,
        );
        assert!(err.contains("specified twice"), "got: {err}");
    }

    #[test]
    fn struct_undeclared_type_errors() {
        let err = run_err(r#"let p = Nope { x: 1 }"#);
        assert!(err.contains("not declared"), "got: {err}");
    }

    #[test]
    fn struct_field_access_missing_errors() {
        let err = run_err(
            r#"struct Point { x, y }
let p = Point { x: 1, y: 2 }
print(p.z)"#,
        );
        assert!(err.contains("no field"), "got: {err}");
    }

    #[test]
    fn struct_field_access_on_non_struct_errors() {
        let err = run_err("let x = 42\nprint(x.value)");
        assert!(err.contains("Can't read field"), "got: {err}");
    }

    #[test]
    fn struct_duplicate_decl_errors() {
        let err = run_err(
            r#"struct Foo { x }
struct Foo { y }"#,
        );
        assert!(err.contains("already declared"), "got: {err}");
    }

    #[test]
    fn struct_nested() {
        assert_eq!(
            say(r#"struct Inner { v }
struct Outer { name, inner }
let o = Outer { name: "nest", inner: Inner { v: 42 } }
print(o.inner.v)"#),
            "42"
        );
    }

    #[test]
    fn struct_in_array_and_iteration() {
        assert_eq!(
            say(r#"struct Item { name, qty }
let cart = [Item { name: "apple", qty: 3 }, Item { name: "banana", qty: 2 }]
let total = 0
for i in cart { total += i.qty }
print(total)"#),
            "5"
        );
    }

    #[test]
    fn struct_literal_disallowed_in_if_condition_parses() {
        // `if Foo { body }` should parse as `if Foo` with body
        // `{ body }`, not as `if (Foo { body })`. Reading a
        // bare `Foo` ident that isn't bound fails at runtime
        // with "not found" — confirming the struct-literal
        // restriction held at parse.
        let err = run_err("if Foo { print(\"hi\") }");
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn struct_literal_disallowed_in_for_iterable() {
        // `for x in arr { body }` — without the struct-literal
        // restriction, `arr { body }` would try to parse as a
        // struct literal where `arr` is the type name and `body`
        // is a field. The restriction ensures the `{` belongs to
        // the for body.
        assert_eq!(
            say("let arr = [1, 2, 3]\nlet sum = 0\nfor x in arr { sum += x }\nprint(sum)"),
            "6"
        );
    }

    #[test]
    fn struct_literals_parse_inside_condition_delimiters() {
        let mut host = TestHost::new();
        run(
            r#"struct Point { x, y }
fn get_x(point) { return point.x }
let choices = [false, true]

if get_x(Point { x: 1, y: 2 }) == 1 { print("call") }
if (Point { x: 2, y: 3 }).x == 2 { print("paren") }
if choices[Point { x: 1, y: 0 }.x] { print("index") }
if [Point { x: 3, y: 0 }][0].x == 3 { print("array") }
if {"point": Point { x: 4, y: 0 }}["point"].x == 4 { print("dict") }
if Ok(Point { x: 5, y: 0 }).is_ok() { print("result") }
if match 1 {
    value if Point { x: value, y: 0 }.x == 1 => Point { x: 6, y: 0 }.x,
    _ => 0,
} == 6 { print("match") }
if (if true { Point { x: 7, y: 0 }.x } else { 0 }) == 7 { print("if-expr") }
if fn() { return Point { x: 8, y: 0 }.x }() == 8 { print("lambda") }"#,
            &mut host,
            &test_limits(),
        )
        .expect("delimiter-scoped struct literals should run");
        assert_eq!(
            host.prints.borrow().clone(),
            [
                "call", "paren", "index", "array", "dict", "result", "match", "if-expr", "lambda",
            ]
        );
    }

    #[test]
    fn control_flow_head_braces_remain_disambiguated() {
        assert_eq!(
            say(r#"const CONDITION = false
const ITEMS = [1, 2]
const COUNT = 0
const VALUE = 1
if CONDITION { print("bad-if") }
while CONDITION { print("bad-while") }
for item in ITEMS { print(item) }
repeat COUNT { print("bad-repeat") }
print(match VALUE { _ => "ok" })"#,),
            "ok"
        );
    }

    #[test]
    fn struct_literal_ok_in_let_rhs() {
        // In a let rhs, struct literals are allowed. This
        // confirms the context flag is re-enabled outside
        // control-flow conditions.
        assert_eq!(
            say(r#"struct P { x }
let p = P { x: 7 }
print(p.x)"#),
            "7"
        );
    }

    #[test]
    fn struct_field_assign_basic() {
        assert_eq!(
            say(r#"struct Point { x, y }
let p = Point { x: 1, y: 2 }
p.x = 99
print(p.x)
print(p.y)"#),
            "2"
        );
    }

    #[test]
    fn struct_field_compound_assign() {
        assert_eq!(
            say(r#"struct Counter { n }
let c = Counter { n: 10 }
c.n += 5
c.n *= 2
print(c.n)"#),
            "30"
        );
    }

    #[test]
    fn struct_field_assign_unknown_field_errors() {
        let err = run_err(
            r#"struct P { x }
let p = P { x: 1 }
p.y = 99"#,
        );
        assert!(err.contains("no field"), "got: {err}");
    }

    #[test]
    fn struct_field_assign_on_non_struct_errors() {
        let err = run_err(
            r#"let x = 5
x.field = 1"#,
        );
        assert!(err.contains("Can't assign to field"), "got: {err}");
    }

    #[test]
    fn struct_field_assign_chain_via_intermediate_var() {
        // `outer.inner.v = 99` isn't supported yet (needs nested
        // writeback). Users can re-build through intermediate
        // vars instead.
        assert_eq!(
            say(r#"struct Inner { v }
struct Outer { inner }
let o = Outer { inner: Inner { v: 1 } }
let i = o.inner
i.v = 99
o.inner = i
print(o.inner.v)"#),
            "99"
        );
    }

    // ─── Enums ────────────────────────────────────────────────────

    #[test]
    fn enum_unit_variant_basic() {
        assert_eq!(
            say(r#"enum Shape { Empty, Circle(r), Square(s) }
let s = Shape::Empty
print(s)"#),
            "Shape::Empty"
        );
    }

    #[test]
    fn enum_tuple_variant() {
        assert_eq!(
            say(r#"enum Shape { Empty, Circle(r), Pair(x, y) }
let p = Shape::Pair(3, 4)
print(p)"#),
            "Shape::Pair(3, 4)"
        );
    }

    #[test]
    fn enum_struct_variant() {
        assert_eq!(
            say(r#"enum Shape { Rectangle { width, height }, Empty }
let r = Shape::Rectangle { width: 4, height: 3 }
print(r)
print(r.width)
print(r.height)"#),
            "3"
        );
    }

    #[test]
    fn enum_equality_same_variant() {
        assert_eq!(
            say(r#"enum E { A, B(x) }
print(E::A == E::A)
print(E::B(1) == E::B(1))
print(E::B(1) == E::B(2))
print(E::A == E::B(1))"#),
            "false"
        );
    }

    #[test]
    fn enum_different_types_not_equal() {
        assert_eq!(
            say(r#"enum A { X }
enum B { X }
print(A::X == B::X)"#),
            "false"
        );
    }

    #[test]
    fn enum_variant_mismatch_unit_given_args() {
        let err = run_err(
            r#"enum E { A }
let x = E::A(1)"#,
        );
        assert!(err.contains("no payload"), "got: {err}");
    }

    #[test]
    fn enum_variant_mismatch_tuple_arity() {
        let err = run_err(
            r#"enum E { P(x, y) }
let p = E::P(1)"#,
        );
        assert!(err.contains("expects 2 argument"), "got: {err}");
    }

    #[test]
    fn enum_variant_mismatch_struct_missing_field() {
        let err = run_err(
            r#"enum E { R { w, h } }
let r = E::R { w: 1 }"#,
        );
        assert!(err.contains("Missing field"), "got: {err}");
    }

    #[test]
    fn enum_variant_mismatch_struct_extra_field() {
        let err = run_err(
            r#"enum E { R { w, h } }
let r = E::R { w: 1, h: 2, extra: 3 }"#,
        );
        assert!(err.contains("no field"), "got: {err}");
    }

    #[test]
    fn enum_undeclared_variant_errors() {
        let err = run_err(
            r#"enum E { A }
let x = E::Z"#,
        );
        assert!(err.contains("no variant"), "got: {err}");
    }

    #[test]
    fn enum_undeclared_type_errors() {
        let err = run_err("let x = Nope::V");
        assert!(err.contains("not declared"), "got: {err}");
    }

    #[test]
    fn enum_struct_variant_field_access() {
        assert_eq!(
            say(r#"enum Shape { Rect { w, h }, Empty }
let r = Shape::Rect { w: 10, h: 3 }
print(r.w * r.h)"#),
            "30"
        );
    }

    #[test]
    fn enum_used_in_if_condition() {
        // The struct-literal disambiguation flag also covers
        // enum struct-variants — `if Foo::V { body }` must
        // parse `V` as a unit variant and `{ body }` as the
        // if's block.
        assert_eq!(
            say(r#"enum E { V }
if E::V == E::V {
    print("yes")
} else {
    print("no")
}"#),
            "yes"
        );
    }

    #[test]
    fn enum_type_name_is_enum() {
        assert_eq!(
            say(r#"enum E { V }
print((E::V).type())"#),
            "enum"
        );
    }

    #[test]
    fn enum_in_array_of_values() {
        assert_eq!(
            say(r#"enum Color { Red, Green, Blue }
let palette = [Color::Red, Color::Green, Color::Blue]
print(palette)"#),
            "[Color::Red, Color::Green, Color::Blue]"
        );
    }

    #[test]
    fn enum_duplicate_decl_errors() {
        let err = run_err(
            r#"enum E { A }
enum E { B }"#,
        );
        assert!(err.contains("already declared"), "got: {err}");
    }

    // ─── User-defined methods on structs + enums ──────────────────

    #[test]
    fn method_on_struct_basic() {
        assert_eq!(
            say(r#"struct Point { x, y }
fn Point.sum(self) { return self.x + self.y }
let p = Point { x: 3, y: 4 }
print(p.sum())"#),
            "7"
        );
    }

    #[test]
    fn method_with_extra_args() {
        assert_eq!(
            say(r#"struct Counter { n }
fn Counter.add(self, delta) { return Counter { n: self.n + delta } }
let c = Counter { n: 10 }
let c2 = c.add(5)
print(c2.n)
print(c.n)"#),
            "10"
        );
    }

    #[test]
    fn value_method_receiver_cannot_be_mutated() {
        let error = parse_err(
            r#"struct Counter { n }
fn Counter.bump(self) { self.n = self.n + 1 }"#,
        );
        assert!(error.contains("can't mutate value receiver `self`"));
    }

    #[test]
    fn method_on_enum_dispatches_on_type() {
        assert_eq!(
            say(r#"enum Shape { Circle(r), Rect { w, h }, Empty }
fn Shape.name(self) { return "shape" }
print(Shape::Circle(3).name())
print(Shape::Rect { w: 4, h: 3 }.name())
print(Shape::Empty.name())"#),
            "shape"
        );
    }

    #[test]
    fn method_overrides_builtin() {
        // A user-defined method of the same name as a built-in
        // wins — matches the walker-level dispatch rule.
        assert_eq!(
            say(r#"struct Wrapper { data }
fn Wrapper.len(self) { return 99 }
let w = Wrapper { data: [1, 2, 3] }
print(w.len())"#),
            "99"
        );
    }

    #[test]
    fn method_unknown_on_struct_errors() {
        let err = run_err(
            r#"struct P { x }
let p = P { x: 1 }
p.nope()"#,
        );
        assert!(err.contains(".nope()"), "got: {err}");
    }

    #[test]
    fn method_wrong_arg_count_errors() {
        let err = run_err(
            r#"struct P { x }
fn P.set(self, v) { return P { x: v } }
let p = P { x: 1 }
p.set(1, 2)"#,
        );
        assert!(err.contains("expects"), "got: {err}");
    }

    #[test]
    fn method_chain_user_defined() {
        assert_eq!(
            say(r#"struct Adder { n }
fn Adder.then(self, m) { return Adder { n: self.n + m } }
let result = Adder { n: 1 }.then(2).then(3).then(4)
print(result.n)"#),
            "10"
        );
    }

    #[test]
    fn method_self_is_clone() {
        // `self` in the method is independent from the caller's
        // binding, even if the method returns self: structural
        // equality still holds on the returned clone.
        assert_eq!(
            say(r#"struct P { x }
fn P.identity(self) { return self }
let a = P { x: 7 }
let b = a.identity()
print(a == b)
print(b.x)"#),
            "7"
        );
    }

    #[test]
    fn method_on_enum_reads_payload_field() {
        assert_eq!(
            say(r#"enum Shape { Circle(r), Rect { w, h } }
fn Shape.label(self, prefix) {
    return prefix + "-shape"
}
let c = Shape::Circle(5)
print(c.label("small"))"#),
            "small-shape"
        );
    }

    #[test]
    fn enum_duplicate_variant_errors() {
        let err = run_err(r#"enum E { A, A }"#);
        assert!(err.contains("duplicate variant"), "got: {err}");
    }

    #[test]
    fn struct_empty() {
        assert_eq!(
            say(r#"struct Unit { }
let u = Unit { }
print(u)"#),
            "Unit {}"
        );
    }

    #[test]
    fn import_module_does_not_see_importer_scope() {
        // `outer` is defined in the importer's scope; the module
        // must not be able to reach it.
        let mut host = ModuleHost::new(&[("m", "fn leak() { return outer }")]);
        let err = run(
            r#"let outer = 42
use m
print(leak())"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap_err();
        assert!(
            err.message.contains("outer"),
            "expected 'outer' not-found error, got: {}",
            err.message
        );
    }

    // ─── Const declarations + case conventions ─────────────────────
    //
    // The parser enforces three naming buckets — values are
    // lowercase-first, types start uppercase, constants are
    // all-uppercase. These tests cover the new `const` keyword
    // and each of the rule-violation error paths.

    #[test]
    fn const_declares_an_immutable_binding() {
        assert_eq!(say("const PI = 3\nprint(PI)"), "3");
    }

    #[test]
    fn const_can_reference_another_const() {
        assert_eq!(
            say("const PI = 3\nconst DIAMETER = PI * 2\nprint(DIAMETER)"),
            "6"
        );
    }

    #[test]
    fn const_reassignment_is_refused_at_parse_time() {
        let err = run_err("const PI = 3\nPI = 4");
        assert!(
            err.contains("can't reassign") && err.contains("constant"),
            "expected const-reassignment error, got: {err}"
        );
    }

    #[test]
    fn const_container_assignment_is_refused_at_parse_time() {
        let cases = [
            ("const VALUES = [1, 2]\nVALUES[0] = 9", "VALUES", 2),
            ("const VALUES = [1, 2]\nVALUES[0] += 9", "VALUES", 2),
            ("const LOOKUP = {\"n\": 1}\nLOOKUP[\"n\"] = 9", "LOOKUP", 2),
            ("const LOOKUP = {\"n\": 1}\nLOOKUP[\"n\"] += 9", "LOOKUP", 2),
            (
                "struct Counter { n }\nconst COUNTER = Counter { n: 1 }\nCOUNTER.n = 9",
                "COUNTER",
                3,
            ),
            (
                "struct Counter { n }\nconst COUNTER = Counter { n: 1 }\nCOUNTER.n += 9",
                "COUNTER",
                3,
            ),
        ];

        for (source, name, line) in cases {
            let err = parse(source).unwrap_err();
            assert_eq!(
                err.message,
                format!("can't reassign `{name}` — it's a constant"),
                "source: {source}"
            );
            assert_eq!(err.line, Some(line), "source: {source}");
            assert_eq!(
                err.friendly_hint.as_deref(),
                Some("constants are immutable. Use `let` if you want a mutable binding."),
                "source: {source}"
            );
        }
    }

    #[test]
    fn const_assignment_finds_grouped_and_nested_place_roots() {
        let cases = [
            "const VALUES = [1, 2]\n(VALUES)[0] = 9",
            "const VALUES = [1, 2]\n((VALUES))[0] += 9",
            "const GRID = [[1]]\nGRID[0][0] = 9",
            "struct Counter { n }\nconst COUNTER = Counter { n: 1 }\n(COUNTER).n = 9",
        ];

        for source in cases {
            let err = parse(source).unwrap_err();
            assert!(
                err.message.contains("can't reassign") && err.message.contains("constant"),
                "source: {source}\nerror: {err}"
            );
        }
    }

    #[test]
    fn const_array_mutating_methods_use_the_canonical_constant_error() {
        let cases = [
            "VALUES.push(4)",
            "VALUES.pop()",
            "VALUES.insert(0, 4)",
            "VALUES.remove(0)",
            "VALUES.truncate(1)",
            "VALUES.clear()",
            "VALUES.reverse()",
            "(VALUES).sort()",
        ];

        for call in cases {
            let source = format!("const VALUES = [3, 1, 2]\n{call}");
            let err = run_err_full(&source);
            assert_eq!(
                err.message, "can't reassign `VALUES` — it's a constant",
                "source: {source}"
            );
            assert_eq!(err.line, Some(2), "source: {source}");
            assert_eq!(
                err.friendly_hint.as_deref(),
                Some(error_messages::CONSTANT_MUTATION_HINT),
                "source: {source}"
            );
        }

        let err = run_err_full(
            r#"fn mutate() {
    const VALUES = [3, 1, 2]
    VALUES.sort()
}
mutate()"#,
        );
        assert_eq!(err.message, "can't reassign `VALUES` — it's a constant");
        assert_eq!(err.line, Some(3));
    }

    #[test]
    fn const_non_array_receivers_keep_ordinary_method_dispatch() {
        assert_eq!(
            say(r#"struct Accumulator { total }
fn Accumulator.push(self, value) { return self.total + value }
fn Accumulator.pop(self) { return self.total }
const ACCUMULATOR = Accumulator { total: 7 }
const LOOKUP = {"n": 1}
print([ACCUMULATOR.push(5), ACCUMULATOR.pop(), LOOKUP.keys()])"#),
            r#"[12, 7, ["n"]]"#
        );

        // `.remove()` is now a mutating dict built-in, so a constant dict
        // receiver gets the canonical constant error rather than dispatch.
        assert_eq!(
            run_err(
                r#"const LOOKUP = {"n": 1}
LOOKUP.remove("n")"#
            ),
            "can't reassign `LOOKUP` — it's a constant"
        );
    }

    #[test]
    fn lowercase_array_mutating_methods_remain_valid() {
        assert_eq!(
            say(r#"let values = [3, 1, 2]
values.sort()
values.reverse()
values.insert(1, 4)
values.remove(0)
values.push(5)
values.pop()
print(values)"#),
            "[4, 2, 1]"
        );
    }

    #[test]
    fn const_declarations_remain_declarations_not_assignments() {
        // Issue #7 distinguishes a declaration that shadows/replaces an
        // existing same-shaped binding from an assignment to that binding.
        assert_eq!(
            say("const VALUE = [1]\nconst VALUE = [2]\nprint(VALUE[0])"),
            "2"
        );
    }

    #[test]
    fn const_names_remain_readable_inside_mutable_assignment_targets() {
        assert_eq!(
            say("const INDEX = 0\nlet values = [1]\nvalues[INDEX] += 2\nprint(values)"),
            "[3]"
        );
    }

    #[test]
    fn let_name_must_start_lowercase() {
        let err = run_err("let Foo = 1");
        assert!(err.to_lowercase().contains("value"), "got: {err}");
    }

    #[test]
    fn let_with_all_caps_suggests_const() {
        let err = run_err("let MAX = 1");
        assert!(
            err.contains("const"),
            "expected hint to suggest `const`, got: {err}"
        );
    }

    #[test]
    fn struct_name_must_start_uppercase() {
        let err = run_err("struct entity { id }");
        assert!(err.to_lowercase().contains("type"), "got: {err}");
    }

    #[test]
    fn enum_variants_start_uppercase() {
        let err = run_err("enum Event { spawn, damage }");
        assert!(err.to_lowercase().contains("type"), "got: {err}");
    }

    #[test]
    fn enum_with_all_caps_variants_is_allowed() {
        // `enum Dir { N, E, S, W }` — short-acronym variants are
        // type-shape and pass the type-name check.
        assert_eq!(
            say("enum Dir { N, E, S, W }\nlet d = Dir::E\nprint(\"ok\")"),
            "ok"
        );
    }

    #[test]
    fn fn_name_must_start_lowercase() {
        let err = run_err("fn Greet() { return 1 }");
        assert!(err.to_lowercase().contains("value"), "got: {err}");
    }

    #[test]
    fn fn_params_must_start_lowercase() {
        let err = run_err("fn greet(Name) { return Name }");
        assert!(err.to_lowercase().contains("value"), "got: {err}");
    }

    #[test]
    fn all_function_forms_use_canonical_parameter_name_diagnostics() {
        for (source, site, name, line, column) in [
            ("fn run(BAD) { none }", "function parameter", "BAD", 1, 8),
            (
                "fn Thing.run(BAD) { none }",
                "method parameter",
                "BAD",
                1,
                14,
            ),
            (
                "let f = fn(BAD) { none }",
                "function parameter",
                "BAD",
                1,
                12,
            ),
            (
                "let f = fn(\n  good,\n  AlsoBad\n) { return good }",
                "function parameter",
                "AlsoBad",
                3,
                3,
            ),
        ] {
            let error = parse_err_full(source);
            assert_eq!(error.line, Some(line), "source: {source}");
            assert_eq!(error.column, Some(column), "source: {source}");
            assert_eq!(
                error.message,
                format!(
                    "{site} `{name}` looks like a {}, but a value name is required here",
                    if name == "BAD" { "constant" } else { "type" }
                ),
                "source: {source}"
            );
            assert!(error.friendly_hint.is_some(), "source: {source}");
        }

        let reserved = parse_err_full("let f = fn(if) { none }");
        assert_eq!(reserved.line, Some(1));
        assert_eq!(reserved.column, Some(12));
        assert_eq!(reserved.message, "`if` is a reserved word in Nybl");
        assert!(reserved.friendly_hint.is_some());
    }

    #[test]
    fn valid_lambda_params_keep_function_binding_semantics() {
        let source = r#"fn make(value) { return fn(value) { return value } }
let outer = 40
let pick = fn(_ignored, value, value) { return outer + value }
print(pick(1, 2, 3))
print(make(4)(5))"#;
        let mut host = TestHost::new();
        run(source, &mut host, &test_limits()).unwrap();
        assert_eq!(host.prints.borrow().as_slice(), ["43", "5"]);
    }

    #[test]
    fn for_loop_var_must_start_lowercase() {
        let err = run_err("for I in range(3) { print(I) }");
        assert!(err.to_lowercase().contains("value"), "got: {err}");
    }

    #[test]
    fn const_name_must_be_all_caps() {
        let err = run_err("const Pi = 3");
        assert!(err.to_lowercase().contains("constant"), "got: {err}");
    }

    #[test]
    fn underscore_prefix_is_allowed_for_all_buckets() {
        // Private-by-convention — classification is unchanged.
        assert_eq!(
            say(r#"
                let _hidden = 1
                const _DEBUG = true
                struct _Internal { _counter }
                let s = _Internal { _counter: _hidden }
                print(s._counter)
            "#),
            "1"
        );
    }

    // ─── host helpers ────────────────────────────────────────────────

    // ─── module-qualified type identity ──────────────────────────────

    #[test]
    fn two_modules_can_declare_same_type_name_with_different_shapes() {
        // Phase 2b — module-qualified types. Two modules each
        // declare `enum Color { ... }` with *different* variant
        // sets; both are usable through aliases, and values
        // constructed from one never compare equal to values
        // from the other even when the variant names match.
        let mut host = crate::host::StringModuleHost::new([
            ("paint", "enum Color { Red, Blue }"),
            ("other", "enum Color { Red, Green, Yellow }"),
        ]);
        run(
            r#"use paint as p
use other as o
let a = p.Color::Red
let b = o.Color::Red
print(a == b)
print(a == a)
print(a)
print(b)"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        // a and b are both `Color::Red` but from *different*
        // modules, so `==` is false. `a == a` is true (same
        // identity). Display keeps the bare type name in both
        // cases so surface rendering stays readable even when
        // the underlying identities differ.
        assert_eq!(host.output(), "false\ntrue\nColor::Red\nColor::Red");
    }

    #[test]
    fn namespaced_pattern_matches_only_same_module_value() {
        // Patterns resolve their type reference through the
        // alias, so `p.Color::Red` *only* matches a value
        // declared in the `paint` module — not the lookalike
        // value from `other`.
        let mut host = crate::host::StringModuleHost::new([
            ("paint", "enum Color { Red, Blue }"),
            ("other", "enum Color { Red, Green, Yellow }"),
        ]);
        run(
            r#"use paint as p
use other as o
fn label(c) {
    return match c {
        p.Color::Red => "paint red",
        o.Color::Red => "other red",
        _ => "other",
    }
}
print(label(p.Color::Red))
print(label(o.Color::Red))
print(label(o.Color::Green))"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.output(), "paint red\nother red\nother");
    }

    #[test]
    fn string_module_host_runs_use_end_to_end() {
        // End-to-end sanity on the embedder helper: the host
        // exposes modules through its in-memory map, captures
        // prints, and the runtime threads them together with no
        // extra wiring on the embedder side.
        let mut host = crate::host::StringModuleHost::new([(
            "greetings",
            "fn hello(name) { print(\"hi \" + name) }",
        )]);
        run(
            "use greetings\nhello(\"nybl\")",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.output(), "hi nybl");
    }

    #[test]
    fn explicit_public_surfaces_filter_values_functions_and_types() {
        let module = r#"
let visible = 1
let hidden = 2
let _shown = 3
struct Visible { value }
struct Hidden { value }
fn read_hidden() { return hidden }
pub { visible, _shown, read_hidden, Visible }
"#;
        let mut host = crate::host::StringModuleHost::new([("surface", module)]);
        run(
            r#"
use surface as module
use surface
print([module.visible, visible, _shown, module.read_hidden(), Visible { value: 4 }])
"#,
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.output(), "[1, 1, 3, 2, Visible { value: 4 }]");

        let mut hidden_host = crate::host::StringModuleHost::new([("surface", module)]);
        let hidden = run(
            "use surface as module\nprint(module.hidden)",
            &mut hidden_host,
            &NyblLimits::standard(),
        )
        .unwrap_err();
        assert!(hidden.message.contains("isn't exported"), "got: {hidden:?}");

        let mut type_host = crate::host::StringModuleHost::new([("surface", module)]);
        let hidden_type = run(
            "use surface.{Hidden}",
            &mut type_host,
            &NyblLimits::standard(),
        )
        .unwrap_err();
        assert!(
            hidden_type.message.contains("isn't exported"),
            "got: {hidden_type:?}"
        );
    }

    #[test]
    fn modules_without_public_surfaces_keep_legacy_private_access() {
        let mut host = crate::host::StringModuleHost::new([("legacy", "let _private = 9")]);
        run(
            "use legacy.{_private}\nprint(_private)",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.output(), "9");
    }

    // ─── column info on runtime errors ───────────────────────────────

    #[test]
    fn runtime_error_carries_column_for_undefined_ident() {
        // Parse-time errors already carry column; runtime
        // errors historically did not. With column now
        // threaded through every `Expr` / `Stmt` at parse
        // time and surfaced via `error_at`, a runtime error
        // on a nested ident carries both line and column.
        let err = run_err_full("let x = 1\nprint(undefined)");
        assert_eq!(err.line, Some(2), "line");
        assert!(
            err.column.is_some(),
            "expected runtime error to carry column info, got None"
        );
    }

    #[test]
    fn runtime_error_column_renders_with_caret() {
        // Smoke-test for the end-to-end UX: a rendered
        // runtime error shows `--> line:col` and a caret at
        // the offending column.
        let src = "let x = 1\nprint(undefined)";
        let err = run_err_full(src);
        let rendered = err.render(src);
        assert!(
            rendered.contains("--> line 2:"),
            "rendered should include line+col header, got:\n{rendered}"
        );
        assert!(
            rendered.contains("^"),
            "rendered should draw a caret, got:\n{rendered}"
        );
    }

    #[test]
    fn resolve_from_map_returns_none_for_unknown_modules() {
        // The closure helper is a pure resolver — no print
        // handling, no default module. Unknown names return
        // `None` so the runtime falls through to its normal
        // "module not found" error.
        let resolver = crate::host::resolve_from_map([("m", "let x = 1")]);
        assert!(resolver("m").is_some());
        assert!(resolver("other").is_none());
    }

    // ─── ReplSession ─────────────────────────────────────────────────

    fn repl_eval(
        session: &mut ReplSession,
        src: &str,
        host: &mut TestHost,
    ) -> Result<Option<Value>, NyblError> {
        session.eval(src, host, &test_limits())
    }

    #[test]
    fn session_let_binding_survives_between_evals() {
        let mut session = ReplSession::new();
        let mut host = TestHost::new();
        repl_eval(&mut session, "let x = 5", &mut host).unwrap();
        repl_eval(&mut session, "print(x)", &mut host).unwrap();
        assert_eq!(host.last_print(), "5");
    }

    #[test]
    fn session_mutated_let_reflects_in_next_eval() {
        let mut session = ReplSession::new();
        let mut host = TestHost::new();
        repl_eval(&mut session, "let counter = 0", &mut host).unwrap();
        repl_eval(&mut session, "counter = counter + 1", &mut host).unwrap();
        repl_eval(&mut session, "counter = counter + 1", &mut host).unwrap();
        repl_eval(&mut session, "print(counter)", &mut host).unwrap();
        assert_eq!(host.last_print(), "2");
    }

    #[test]
    fn session_fn_declared_on_one_eval_callable_next() {
        let mut session = ReplSession::new();
        let mut host = TestHost::new();
        repl_eval(&mut session, "fn double(x) { return x + x }", &mut host).unwrap();
        repl_eval(&mut session, "print(double(21))", &mut host).unwrap();
        assert_eq!(host.last_print(), "42");
    }

    #[test]
    fn session_struct_and_method_survive() {
        // The stickier path: user types live in
        // (module_path, type_name) registries and their
        // methods in a separate table. Both need to carry
        // across inputs.
        let mut session = ReplSession::new();
        let mut host = TestHost::new();
        repl_eval(
            &mut session,
            "struct Point { x, y }\nfn Point.sum(self) { return self.x + self.y }",
            &mut host,
        )
        .unwrap();
        repl_eval(
            &mut session,
            "let p = Point { x: 3, y: 4 }\nprint(p.sum())",
            &mut host,
        )
        .unwrap();
        assert_eq!(host.last_print(), "7");
    }

    #[test]
    fn session_bare_expression_returns_value() {
        // The REPL's "echo the last expression" affordance:
        // `let` returns None, a bare expression returns
        // Some(Value). Drives the REPL echo behaviour.
        let mut session = ReplSession::new();
        let mut host = TestHost::new();
        assert!(
            repl_eval(&mut session, "let x = 5", &mut host)
                .unwrap()
                .is_none()
        );
        let v = repl_eval(&mut session, "x + 1", &mut host).unwrap();
        match v {
            Some(Value::Int(n)) => assert_eq!(n, 6),
            other => panic!("expected Int(6), got: {other:?}"),
        }
    }

    #[test]
    fn session_errors_preserve_earlier_effects() {
        // Partial-execution semantics: if a later stmt
        // errors, earlier bindings still stick. Matches what
        // a user expects from an interactive prompt.
        let mut session = ReplSession::new();
        let mut host = TestHost::new();
        let err = repl_eval(
            &mut session,
            "let kept = 1\nlet bad = undefined\nlet skipped = 3",
            &mut host,
        );
        assert!(err.is_err(), "expected runtime error");
        // `kept` made it; `skipped` did not.
        assert!(session.get("kept").is_some());
        assert!(session.get("skipped").is_none());
    }

    #[test]
    fn session_normalises_scope_depth_after_block_error() {
        // A runtime error inside a nested block
        // (`if true { <error> }`) used to leave extra
        // scopes pushed. The session's writeback truncates
        // back to the root so the next `eval` starts clean.
        let mut session = ReplSession::new();
        let mut host = TestHost::new();
        let _ = repl_eval(
            &mut session,
            "if true {\n    let y = undefined\n}",
            &mut host,
        );
        // Next eval must not error on "scopes mysteriously
        // pre-pushed" — a plain let should work.
        repl_eval(&mut session, "let after = 7", &mut host).unwrap();
        let v = repl_eval(&mut session, "after", &mut host).unwrap();
        match v {
            Some(Value::Int(n)) => assert_eq!(n, 7),
            other => panic!("expected Int(7), got: {other:?}"),
        }
    }

    #[test]
    fn session_binding_names_surfaces_lets_and_fns() {
        // `binding_names()` gives a REPL `:vars`-style
        // introspection hook. Covers both `let`-scope
        // bindings and `fn` declarations.
        let mut session = ReplSession::new();
        let mut host = TestHost::new();
        repl_eval(&mut session, "let alpha = 1", &mut host).unwrap();
        repl_eval(&mut session, "fn beta() { return 2 }", &mut host).unwrap();
        let names = session.binding_names();
        assert!(names.contains(&"alpha".to_string()));
        assert!(names.contains(&"beta".to_string()));
    }

    #[test]
    fn session_use_carries_imports_across_evals() {
        // Custom host for this test because we need to
        // resolve a module. TestHost doesn't implement
        // `resolve_module` at the stdlib level, so we wire
        // a tiny one up inline.
        struct ModHost {
            prints: std::cell::RefCell<Vec<String>>,
        }
        impl NyblHost for ModHost {
            fn call(&mut self, _: &str, _: &[Value], _: u32) -> Option<Result<Value, NyblError>> {
                None
            }
            fn on_print(&mut self, message: &str) {
                self.prints.borrow_mut().push(message.to_string());
            }
            fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
                match name {
                    "m" => Some(Ok("fn greet() { return \"hi\" }".to_string())),
                    _ => None,
                }
            }
        }
        let mut host = ModHost {
            prints: std::cell::RefCell::new(Vec::new()),
        };
        let mut session = ReplSession::new();
        session.eval("use m", &mut host, &test_limits()).unwrap();
        session
            .eval("print(greet())", &mut host, &test_limits())
            .unwrap();
        let prints = host.prints.borrow();
        assert_eq!(prints.last().map(|s| s.as_str()), Some("hi"));
    }
}
