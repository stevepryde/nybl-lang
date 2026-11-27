//! Native-stack bound: under default limits, every adversarial program in
//! the shared corpus ends in a clean `Err` on a thread of the documented
//! size, in both the tree-walker and the VM.
//!
//! The sizes are the RUN-002 contract. They are at least twice the smallest
//! stack the worst corpus shape survived (measured on arm64 macOS, Rust
//! 1.98): the walker needed 3.0 MiB in release and 11.8 MiB in debug (a
//! recursion of user-method calls, each nested in the index of the next
//! one's `ref` argument, that loads a parse-cap module from its deepest
//! frame), the VM 0.8 MiB and 4.6 MiB. A stack overflow aborts this
//! test binary, so a regression that fattens the walker's frames fails
//! loudly rather than as an assertion.
//!
//! Run the release variant with
//! `cargo test --release -p nybl-vm --test stack_bound`.

#[path = "support/stack_corpus.rs"]
mod stack_corpus;

use std::collections::BTreeMap;

use nybl::{NyblError, NyblHost, NyblLimits, Value};
use stack_corpus::Shape;

/// Thread stack sufficient for any program under default limits in a
/// release build.
const RELEASE_STACK: usize = 10 * 1024 * 1024;
/// The same bound for an unoptimised (debug) build.
const DEBUG_STACK: usize = 24 * 1024 * 1024;

fn documented_stack() -> usize {
    if cfg!(debug_assertions) {
        DEBUG_STACK
    } else {
        RELEASE_STACK
    }
}

struct ModuleHost {
    modules: BTreeMap<String, String>,
}

impl NyblHost for ModuleHost {
    fn call(&mut self, _: &str, _: &[Value], _: u32) -> Option<Result<Value, NyblError>> {
        None
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        self.modules.get(name).cloned().map(Ok)
    }
}

type Engine = fn(&str, &mut ModuleHost, &NyblLimits) -> Result<(), NyblError>;

/// Run every corpus shape on one thread of the documented size and return
/// each shape whose outcome was not one of its accepted clean errors.
fn run_corpus(engine: Engine) -> Vec<String> {
    std::thread::Builder::new()
        .stack_size(documented_stack())
        .spawn(move || {
            // Generating the corpus parses at the parse-depth cap, which
            // also belongs on the documented stack.
            let shapes: Vec<Shape> = stack_corpus::corpus();
            shapes
                .iter()
                .filter_map(|shape| {
                    let mut host = ModuleHost {
                        modules: shape.modules.iter().cloned().collect(),
                    };
                    let outcome = match engine(&shape.source, &mut host, &NyblLimits::standard()) {
                        Ok(()) => "succeeded".to_string(),
                        Err(error) if shape.accepts(&error.message) => return None,
                        Err(error) => error.message,
                    };
                    Some(format!("{}: {outcome}", shape.name))
                })
                .collect()
        })
        .expect("spawn stack-bound thread")
        .join()
        .expect("stack-bound thread panicked")
}

#[test]
fn walker_corpus_ends_cleanly_on_the_documented_stack() {
    let unexpected = run_corpus(nybl::run);
    assert!(unexpected.is_empty(), "{unexpected:#?}");
}

#[test]
fn vm_corpus_ends_cleanly_on_the_documented_stack() {
    let unexpected = run_corpus(nybl_vm::run);
    assert!(unexpected.is_empty(), "{unexpected:#?}");
}
