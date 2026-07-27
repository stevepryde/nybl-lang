//! Bytecode compiler and stack VM for the Nybl programming language.
//!
//! `nybl-vm` implements the same language, host interface, values, limits, and
//! diagnostics as the tree-walker in [`nybl-lang`](https://docs.rs/nybl-lang),
//! while compiling source to reusable bytecode. It is the usual choice for
//! scripts that run hot loops or are executed repeatedly at runtime.
//!
//! # One-shot execution
//!
//! [`run`] parses, compiles, validates, and executes one isolated program:
//!
//! ```
//! use nybl::{NyblError, NyblHost, NyblLimits, Value};
//!
//! struct Host;
//! impl NyblHost for Host {
//!     fn call(
//!         &mut self,
//!         _: &str,
//!         _: &[Value],
//!         _: u32,
//!     ) -> Option<Result<Value, NyblError>> {
//!         None
//!     }
//!
//!     fn on_print(&mut self, message: &str) {
//!         assert_eq!(message, "42");
//!     }
//! }
//!
//! let mut host = Host;
//! nybl_vm::run(
//!     "print(6 * 7)",
//!     &mut host,
//!     &NyblLimits::standard(),
//! ).unwrap();
//! ```
//!
//! # Compile once, execute with fresh state
//!
//! Use [`compile`] and [`execute`] when the bytecode should be reused but each
//! run should start with fresh globals:
//!
//! ```
//! # use nybl::{NyblError, NyblHost, NyblLimits, Value};
//! # struct Host;
//! # impl NyblHost for Host {
//! #     fn call(&mut self, _: &str, _: &[Value], _: u32)
//! #         -> Option<Result<Value, NyblError>> { None }
//! # }
//! let statements = nybl::parse("let answer = 6 * 7").unwrap();
//! let chunk = nybl_vm::compile(&statements).unwrap();
//! nybl_vm::validate_chunk(&chunk).unwrap();
//!
//! let mut host = Host;
//! nybl_vm::execute(
//!     chunk,
//!     &mut host,
//!     &NyblLimits::standard(),
//! ).unwrap();
//! ```
//!
//! [`validate_chunk`] rejects malformed control flow, operands, pool indices,
//! and nested chunks before execution. [`disassemble`] provides a readable
//! representation for tooling and diagnostics.
//!
//! # Persistent programs
//!
//! [`NyblInstance`] loads and compiles once, then retains globals, modules,
//! functions, types, methods, callbacks, and random-number state across host
//! calls. Root-level `pub fn` declarations define the callable ABI.
//!
//! ```
//! # use nybl::{NyblError, NyblHost, NyblLimits, Value};
//! # struct Host;
//! # impl NyblHost for Host {
//! #     fn call(&mut self, _: &str, _: &[Value], _: u32)
//! #         -> Option<Result<Value, NyblError>> { None }
//! # }
//! let mut host = Host;
//! let mut instance = nybl_vm::NyblInstance::load(
//!     "let count = 0\npub fn next() { count += 1; return count }",
//!     &mut host,
//!     &NyblLimits::standard(),
//! ).unwrap();
//! assert_eq!(
//!     instance.call("next", &[], &mut host).unwrap().inspect(),
//!     "1",
//! );
//! ```
//!
//! # Reference parameters
//!
//! The VM implements Nybl's explicit `ref` parameters with the same
//! transactional copy-in/copy-out semantics as the tree-walker and AOT
//! engine. Mutable plain-variable targets commit together after a normal
//! return and roll back together on runtime or resource errors. Parameter
//! modes remain attached to first-class callable values.
//!
//! Rust [`NyblInstance::call`] and [`NyblInstance::call_value`] arguments are
//! value-only and reject ref-bearing callables before execution. See the
//! [reference-parameters
//! guide](https://nybl-lang.com/docs/functions/reference-parameters/) for the
//! complete target, forwarding, evaluation-order, and method rules.
//! User-defined `ref self` receivers use the same transactional commit and
//! rollback path as explicit reference arguments; ordinary `self` is
//! read-only.
//!
//! # Features
//!
//! - `std` (default) retains Rust standard-library host behavior and forwards
//!   to `nybl-lang/std`.
//! - `nybl-std` (default) forwards to `nybl-lang/nybl-std` so hosts can
//!   resolve the bundled `std.*` modules.
//! - `no_std` forwards to `nybl-lang/no_std`; combine it with
//!   `default-features = false` for bare-metal or
//!   `wasm32-unknown-unknown` builds.
//!   If Cargo unifies `std` and `no_std`, `std` wins.
//!
//! See the [embedding guide](https://nybl-lang.com/docs/embedding/) and
//! [stateful instance guide](https://nybl-lang.com/docs/embedding/instances/)
//! for lifecycle, limits, callback affinity, and engine-selection details.

#![cfg_attr(all(feature = "no_std", not(feature = "std")), no_std)]

#[cfg(all(feature = "no_std", not(feature = "std")))]
extern crate alloc;

// A genuine no_std unit-test build still uses Rust's standard test harness;
// make that test-only dependency explicit.
#[cfg(all(test, feature = "no_std", not(feature = "std")))]
extern crate std;

pub mod chunk;
pub mod compiler;
pub mod disasm;
pub mod validate;
pub mod vm;

pub use chunk::{
    Chunk, CodeOffset, ConstIdx, Constant, EnumConstructShape, EnumDef, EnumIdx, EnumVariantDef,
    EnumVariantShape, FnDef, FnIdx, Instr, InterpIdx, InterpPart, InterpRecipe, LoopStateKind,
    NameIdx, StructDef, StructIdx,
};
pub use compiler::compile;
pub use disasm::disassemble;
pub use validate::validate_chunk;
pub use vm::{NyblInstance, Vm, execute, run};
