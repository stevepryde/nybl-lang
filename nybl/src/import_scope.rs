//! Per-scope import bookkeeping shared by the walker and VM.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::String,
};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::collections::{BTreeMap, BTreeSet};

/// The `(module path, name)` declaration a binding resolves to.
pub type BindingOrigin = (String, String);

/// A name bound by a flat (non-aliased) `use` in one lexical scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedName {
    /// Declaration the imported binding resolves to.
    pub origin: BindingOrigin,
    /// Module path written in the `use` statement that bound the name.
    pub path: String,
}

/// Imports applied to one lexical scope.
#[derive(Debug, Clone, Default)]
pub struct ImportScope {
    /// Paths a plain glob already applied here; repeating one is a no-op.
    pub paths: BTreeSet<String>,
    /// Names whose current binding in this scope came from a flat `use`.
    /// A later local declaration (`let`) of the name removes its entry.
    pub names: BTreeMap<String, ImportedName>,
    /// A named function body's binding of the function's own name (unless a
    /// parameter reuses it), which resolves to that declaration.
    pub self_binding: Option<BindingOrigin>,
    /// `fn` declarations executed in this scope; a later flat import of the
    /// same name is a clash.
    pub functions: BTreeSet<String>,
}

impl ImportScope {
    /// Forget that `name` came from an import: a local declaration now
    /// owns the binding.
    pub fn forget(&mut self, name: &str) {
        if !self.names.is_empty() {
            self.names.remove(name);
        }
        if self
            .self_binding
            .as_ref()
            .is_some_and(|(_, self_name)| self_name == name)
        {
            self.self_binding = None;
        }
    }

    /// Origin recorded for `name` in this scope: a flat import or the
    /// function's own name. `None` means any binding is an ordinary local.
    pub fn known_origin(&self, name: &str) -> Option<&BindingOrigin> {
        self.names
            .get(name)
            .map(|imported| &imported.origin)
            .or_else(|| {
                self.self_binding
                    .as_ref()
                    .filter(|(_, self_name)| self_name == name)
            })
    }
}
