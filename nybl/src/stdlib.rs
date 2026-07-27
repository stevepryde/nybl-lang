//! Bundled Nybl standard library modules, resolved by name.
//!
//! Each `.nybl` file under `src/modules/` is baked into the binary
//! as an `&'static str` via `include_str!`. When a Nybl script
//! does `use std.math`, the engine asks its `NyblHost` to
//! resolve the module — embedders route that call to
//! [`crate::stdlib::resolve`], which returns the bundled source text.
//!
//! Gated behind the `nybl-std` feature (on by default). Disable
//! with `default-features = false` when you want a truly minimal
//! core with no bundled modules:
//!
//! ```toml
//! nybl-lang = { version = "0.4", default-features = false }
//! ```
//!
//! Available modules:
//!
//! - `std.math` — numeric constants (`PI`, `E`, `TAU`) and
//!   helpers that don't fit on a numeric receiver (`clamp`,
//!   `factorial`, `gcd`, …)
//! - `std.iter` — functional helpers on arrays (`map`, `filter`,
//!   `reduce`, `sum`, `find`, …)
//! - `std.string` — string helpers that didn't fit the
//!   method-on-string pattern (`pad_left`, `pad_right`,
//!   `chars`, …)
//! - `std.test` — `assert`, `assert_eq`, `assert_near` plus a
//!   tiny test-runner
//! - `std.collections` — `Set`, `Queue`, `Stack` as struct
//!   types with value-semantic methods (`s = s.push(v)` etc.)
//! - `std.json` — `parse(text)` / `stringify(value)`. Pure
//!   Nybl implementation; adequate for scripting workloads.
//!
//! `Result` combinators (`is_ok`, `unwrap`, `map`, `and_then`,
//! …) used to live in `std.result` but are now methods on the
//! built-in `Result` type — always available without a `use`.
//! See `methods::result_method`.

const MATH: &str = include_str!("modules/math.nybl");
const ITER: &str = include_str!("modules/iter.nybl");
const STRING_MOD: &str = include_str!("modules/string.nybl");
const TEST_MOD: &str = include_str!("modules/test.nybl");
const COLLECTIONS: &str = include_str!("modules/collections.nybl");
const JSON_MOD: &str = include_str!("modules/json.nybl");

/// Map a `std.*` module name to its bundled Nybl source.
///
/// Returns `None` for any path outside the stdlib — chain this
/// with your own [`crate::NyblHost::resolve_module`] so user
/// modules still resolve:
///
/// ```ignore
/// use nybl::{NyblError, NyblHost};
///
/// impl NyblHost for MyHost {
///     fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
///         if let Some(src) = nybl::stdlib::resolve(name) {
///             return Some(Ok(src.to_string()));
///         }
///         // fall back to your own resolver (filesystem,
///         // embedded modules, etc.)
///         self.my_own_resolver(name)
///     }
///     # fn call(&mut self, _: &str, _: &[nybl::Value], _: u32)
///     #     -> Option<Result<nybl::Value, NyblError>> { None }
/// }
/// # struct MyHost;
/// # impl MyHost {
/// #     fn my_own_resolver(&mut self, _: &str) -> Option<Result<String, NyblError>> { None }
/// # }
/// ```
pub fn resolve(name: &str) -> Option<&'static str> {
    match name {
        "std.math" => Some(MATH),
        "std.iter" => Some(ITER),
        "std.string" => Some(STRING_MOD),
        "std.test" => Some(TEST_MOD),
        "std.collections" => Some(COLLECTIONS),
        "std.json" => Some(JSON_MOD),
        _ => None,
    }
}

/// Every module name this crate can resolve. Useful for docs,
/// diagnostics, or a "did you mean…" suggestion in error paths.
pub const MODULES: &[&str] = &[
    "std.math",
    "std.iter",
    "std.string",
    "std.test",
    "std.collections",
    "std.json",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_returns_source_for_known_modules() {
        for name in MODULES {
            assert!(
                resolve(name).is_some(),
                "stdlib module {name} should resolve"
            );
        }
    }

    #[test]
    fn resolve_returns_none_for_unknown() {
        assert!(resolve("std.nope").is_none());
        assert!(resolve("user.code").is_none());
        assert!(resolve("").is_none());
    }
}
