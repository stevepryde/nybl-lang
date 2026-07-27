//! Ahead-of-time Nybl-to-Rust transpiler.
//!
//! [`transpile`] parses a Nybl program and emits readable Rust source that
//! links against `nybl-lang` for runtime values, operators, built-ins, host
//! calls, and diagnostics. The generated source can be compiled as a native
//! binary or included as a module in a larger Rust application.
//!
//! The AOT engine implements the same language semantics as the tree-walker
//! and bytecode VM, including methods, closures, string interpolation,
//! indexed and field mutation, transactional `ref` parameters, structs and
//! enums, pattern matching, `Result`, lazy iteration, and all forms of `use`.
//! A three-engine differential suite exercises their observable behavior
//! together.
//!
//! # Basic transpilation
//!
//! ```
//! use nybl_compile::{Options, transpile};
//!
//! let rust_source = transpile(
//!     r#"let answer = 6 * 7
//! print("answer = {answer}")"#,
//!     &Options::default(),
//! ).unwrap();
//! assert!(rust_source.contains("fn main()"));
//! ```
//!
//! `use` statements need a compile-time [`ModuleResolver`]. For bundled
//! stdlib modules, callers normally delegate that resolver to
//! `nybl::stdlib::resolve`; for in-memory programs, [`modules_from_map`] is
//! convenient:
//!
//! ```
//! use nybl_compile::{Options, modules_from_map, transpile};
//!
//! let rust_source = transpile(
//!     "use config.{answer}\nprint(answer)",
//!     &Options {
//!         module_resolver: Some(modules_from_map([
//!             ("config", "let answer = 42"),
//!         ])),
//!         ..Options::default()
//!     },
//! ).unwrap();
//! assert!(rust_source.contains("42"));
//! ```
//!
//! With the default options, the output contains `fn main()` and constructs
//! `nybl_sys::StandardHost`. Set [`Options::emit_main`] and
//! [`Options::use_nybl_sys`] to `false` for a library surface driven by a
//! custom [`nybl::NyblHost`].
//!
//! # Sandboxed output
//!
//! [`Options::sandbox`] adds [`nybl::NyblLimits`] accounting and host tick
//! callbacks to generated code. With `sandbox: true` and `emit_main: false`,
//! direct root-level `pub fn` declarations also generate a persistent
//! `NyblInstance` API with `load`, `entry_points`, `call`, and `call_value`.
//! Globals, modules, functions, callbacks, types, methods, and random-number
//! state remain live between calls.
//!
//! Rust calls into the generated `NyblInstance` remain value-only and reject
//! ref-bearing entries or callbacks before execution. A value-only public
//! entry can use reference parameters internally. Generated ref calls preserve
//! the language's target validation, copy-in/copy-out staging, atomic commit,
//! error rollback, forwarding, and mutating receiver behavior. See the
//! [reference-parameters
//! guide](https://nybl-lang.com/docs/functions/reference-parameters/).
//! Generated user methods also preserve `ref self` receiver write-back and
//! rollback; ordinary `self` receivers are read-only.
//!
//! Unsandboxed library output exposes the one-shot `run` API and deliberately
//! omits accounting overhead. Do not run untrusted programs through that
//! mode.
//!
//! # Output shaping
//!
//! [`Options::module_name`] wraps the generated items in a public Rust module,
//! which lets one crate include several transpiled programs without name
//! collisions. [`transpile_ast`] accepts an already parsed Nybl AST for build
//! tools that perform their own parse and diagnostic pass.
//!
//! See the [embedding guide](https://nybl-lang.com/docs/embedding/) and
//! [stateful instance guide](https://nybl-lang.com/docs/embedding/instances/)
//! for generated signatures, dependency setup, limits, and lifecycle rules.

use std::cell::RefCell;
use std::rc::Rc;

use nybl::error::NyblError;
use nybl::parser::Stmt;

mod emit;

/// A compile-time module resolver. The AOT runs this eagerly for
/// every `use` it encounters, threading the entire module graph
/// into the generated Rust. Same contract as
/// [`nybl::NyblHost::resolve_module`]: `None` = not handled,
/// `Some(Ok(source))` = module source text,
/// `Some(Err(_))` = resolver failure.
///
/// Wrapped as `Rc<RefCell<..>>` so `Options` stays `Clone` while
/// the callback keeps `FnMut` freedom.
pub type ModuleResolver =
    Rc<RefCell<dyn FnMut(&str) -> Option<Result<String, NyblError>> + 'static>>;

/// Build a [`ModuleResolver`] from an in-memory name→source map.
/// Convenience for tests and simple embedders.
pub fn modules_from_map<S: Into<String>>(
    modules: impl IntoIterator<Item = (S, S)>,
) -> ModuleResolver {
    let map: std::collections::HashMap<String, String> = modules
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    Rc::new(RefCell::new(move |name: &str| {
        map.get(name).cloned().map(Ok)
    }))
}

/// Options that control the shape of the emitted Rust.
///
/// `Debug` is skipped — `module_resolver` is a trait object that
/// has no useful debug representation. If you need to diff options
/// in tests, compare specific fields directly.
#[derive(Clone)]
pub struct Options {
    /// If true, emit a `fn main()` that drives the program with
    /// `nybl_sys::StandardHost`. If false, emit only the library
    /// surface (the caller is expected to provide their own
    /// entry point and host).
    pub emit_main: bool,
    /// If true, pull `nybl-sys::StandardHost` into the generated code
    /// so `main()` can construct it directly. Implied by
    /// [`Self::emit_main`].
    pub use_nybl_sys: bool,
    /// If true, the emitted code enforces [`nybl::NyblLimits`]: step
    /// counts are checked at every loop iteration and function
    /// entry, an explicit memory context enforces `max_memory`, and
    /// [`nybl::NyblHost::on_tick`] fires at the
    /// same checkpoints. The generated `run` takes a `&NyblLimits`
    /// parameter in this mode.
    ///
    /// When false (the default), the emitted code uses an untracked
    /// memory context and has no limit-checking overhead: `run` takes
    /// only a host, and runaway programs are the caller's problem.
    pub sandbox: bool,
    /// If `Some(name)`, wrap the entire emitted output in
    /// `pub mod <name> { ... }`. Use this when you want to embed
    /// several transpiled programs in one Rust source file without
    /// colliding on top-level items (`Ctx`, `run_program`,
    /// `run`, `__nybl_tick`, user-fn names, etc.). `emit_main` is
    /// ignored in this mode — you provide your own driver.
    pub module_name: Option<String>,
    /// Resolver used to inline imported modules into the emitted
    /// Rust at transpile time. Required when the program contains
    /// any `use` statement; missing (`None`) + an import in
    /// source raises a clear "set `module_resolver`" error.
    ///
    /// The resolver is called eagerly for each transitive import
    /// before any Rust is emitted, so cycle detection and missing
    /// modules both surface at build time rather than at run time.
    pub module_resolver: Option<ModuleResolver>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            emit_main: true,
            use_nybl_sys: true,
            sandbox: false,
            module_name: None,
            module_resolver: None,
        }
    }
}

/// Parse Nybl source and emit the equivalent Rust source.
///
/// The caller is responsible for writing the returned string to a
/// crate (or `src/main.rs`) and invoking `cargo` or `rustc` on it.
/// The result depends on:
///
/// - `nybl-lang` (`nybl` crate) — for `Value`, `ops`, `builtins`, …
/// - `nybl-sys` — only if [`Options::use_nybl_sys`] is true.
pub fn transpile(source: &str, opts: &Options) -> Result<String, NyblError> {
    let stmts = nybl::parse(source)?;
    transpile_ast(&stmts, opts)
}

/// Lower-level entry point: emit Rust from an already-parsed AST.
pub fn transpile_ast(stmts: &[Stmt], opts: &Options) -> Result<String, NyblError> {
    emit::emit(stmts, opts)
}
