//! Ready-made [`NyblHost`] building blocks for embedders.
//!
//! Most embedders only need two things beyond the default trait
//! impl: a way to plug in a module resolver, and a way to capture
//! `print` output. This module provides both as composable
//! pieces so you can mix-and-match without re-implementing the
//! whole trait from scratch.
//!
//! ```no_run
//! use nybl::host::{StringModuleHost, resolve_from_map};
//! use nybl::NyblLimits;
//!
//! // Map of module-name → source, resolved in-process (no I/O).
//! let mut host = StringModuleHost::new([
//!     ("greetings", "fn hello() { print(\"hi\") }"),
//! ]);
//! nybl::run("use greetings\nhello()", &mut host, &NyblLimits::standard())
//!     .unwrap();
//! ```
//!
//! The helpers intentionally stay minimal — they don't own the
//! terminal, they don't touch the filesystem, they don't parse
//! environment variables. Embedders that want richer behaviour
//! should implement [`NyblHost`] directly or wrap these helpers.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{borrow::ToOwned, string::String, vec::Vec};

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::collections::BTreeMap;
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::collections::BTreeMap;

use crate::NyblHost;
use crate::error::NyblError;
use crate::value::Value;

/// Build a [`NyblHost::resolve_module`] implementation from an
/// in-memory table of `(module_path, source)` pairs.
///
/// The returned closure takes the same `&str` argument the
/// `NyblHost` trait passes in and returns the matching source
/// wrapped in `Some(Ok(..))`, or `None` when the module name
/// isn't in the table. Embedders with additional lookup logic
/// (filesystem, HTTP, asset bundle) can layer this helper on top
/// of their own fallback chain — see [`StringModuleHost`] for a
/// minimal all-in-one example.
pub fn resolve_from_map<I, K, V>(entries: I) -> impl Fn(&str) -> Option<Result<String, NyblError>>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    let map: BTreeMap<String, String> = entries
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    move |name: &str| map.get(name).map(|s| Ok(s.clone()))
}

/// A minimal [`NyblHost`] that captures `print` output and serves
/// modules from an in-memory string table.
///
/// Useful for tests, playgrounds, and embedders that want
/// resolver-backed imports without writing a full `NyblHost`
/// from scratch. For embedders that only need module resolution
/// (and have their own print handling), use [`resolve_from_map`]
/// directly inside a custom trait impl instead.
pub struct StringModuleHost {
    /// Each `print(...)` invocation appends one entry. Leave
    /// public so tests can assert on output without going
    /// through an accessor.
    pub prints: Vec<String>,
    modules: BTreeMap<String, String>,
}

impl StringModuleHost {
    /// Build a host preloaded with the given module map.
    ///
    /// The iterator yields `(name, source)` pairs. Later entries
    /// with the same name overwrite earlier ones.
    pub fn new<I, K, V>(modules: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            prints: Vec::new(),
            modules: modules
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// Register (or replace) a single module after construction.
    pub fn insert_module(&mut self, name: impl Into<String>, source: impl Into<String>) {
        self.modules.insert(name.into(), source.into());
    }

    /// Join all accumulated prints with `\n` — convenient for
    /// tests that want to assert on the combined output.
    pub fn output(&self) -> String {
        self.prints.join("\n")
    }
}

impl NyblHost for StringModuleHost {
    fn call(
        &mut self,
        _name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        None
    }

    fn on_print(&mut self, message: &str) {
        self.prints.push(message.to_owned());
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        self.modules.get(name).map(|s| Ok(s.clone()))
    }
}
