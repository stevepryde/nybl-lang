//! Function-local slot allocation and lexical visibility tracking.
//!
//! Slot indices grow monotonically for the whole function, while each lexical
//! scope records a restoration journal. The journal owns declaration order;
//! the visibility map is only a compiler-time lookup accelerator. Default
//! builds use randomized hashing for expected constant-time lookup on
//! untrusted identifiers. True `no_std` builds use a dependency-free ordered
//! map with logarithmic lookup.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    collections::{BTreeMap as VisibilityMap, BTreeSet as RefParameterSlots},
    rc::Rc,
    string::String,
    vec,
    vec::Vec,
};
#[cfg(test)]
use core::cell::Cell;
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::{
    collections::{HashMap as VisibilityMap, HashSet as RefParameterSlots},
    rc::Rc,
    string::String,
    vec,
    vec::Vec,
};

use nybl::parser::{ParamMode, Parameter};

use crate::chunk::{
    LocalScopeIdx, LocalScopeName, LocalScopeNames, LocalScopeSnapshot, NameIdx, SlotIdx,
};

#[derive(Default)]
struct VisibleSlots {
    slots: VisibilityMap<Rc<str>, SlotIdx>,
    #[cfg(test)]
    lookup_count: Cell<usize>,
}

impl VisibleSlots {
    fn get(&self, name: &str) -> Option<SlotIdx> {
        #[cfg(test)]
        self.lookup_count.set(self.lookup_count.get() + 1);
        self.slots.get(name).copied()
    }

    fn insert(&mut self, name: Rc<str>, slot: SlotIdx) -> Option<SlotIdx> {
        self.slots.insert(name, slot)
    }

    fn remove(&mut self, name: &str) -> Option<SlotIdx> {
        self.slots.remove(name)
    }

    #[cfg(test)]
    fn reset_lookup_count(&self) {
        self.lookup_count.set(0);
    }

    #[cfg(test)]
    fn lookup_count(&self) -> usize {
        self.lookup_count.get()
    }
}

struct ScopeBinding {
    name: Rc<str>,
    slot: SlotIdx,
    previous: Option<SlotIdx>,
}

struct LocalScope {
    /// Replaying these entries in reverse restores outer bindings, including
    /// repeated declarations at one lexical depth.
    bindings: Vec<ScopeBinding>,
    metadata: Option<LocalScopeIdx>,
    /// Name -> (first declaration position, interned chunk name).
    first_bindings: VisibilityMap<Rc<str>, (u32, NameIdx)>,
}

pub(super) struct LocalResolver {
    /// The journals own source order; `visible` must never influence emission.
    scopes: Vec<LocalScope>,
    visible: VisibleSlots,
    /// Slot numbers never roll back when a lexical scope exits.
    next_slot: u32,
    /// High-water mark used to size the VM frame once at call time.
    pub(super) max_slot: u32,
    /// Positional ABI parameter -> canonical language binding slot.
    pub(super) parameter_slots: Vec<SlotIdx>,
    ref_param_slots: RefParameterSlots<u32>,
    local_scopes: Vec<LocalScopeNames>,
}

impl LocalResolver {
    pub(super) fn new(params: &[Parameter], parameter_names: &[NameIdx]) -> Self {
        assert_eq!(params.len(), parameter_names.len());
        let mut resolver = Self {
            scopes: vec![LocalScope {
                bindings: Vec::with_capacity(params.len()),
                metadata: None,
                first_bindings: VisibilityMap::default(),
            }],
            visible: VisibleSlots::default(),
            next_slot: 0,
            max_slot: 0,
            parameter_slots: Vec::with_capacity(params.len()),
            ref_param_slots: RefParameterSlots::new(),
            local_scopes: Vec::new(),
        };
        for (parameter, name) in params.iter().zip(parameter_names.iter().copied()) {
            let slot = match resolver.visible.get(&parameter.name) {
                Some(slot) => slot,
                None => {
                    let slot = SlotIdx(resolver.next_slot);
                    resolver.next_slot += 1;
                    resolver.bind_slot(&parameter.name, name, slot);
                    slot
                }
            };
            resolver.parameter_slots.push(slot);
            if parameter.mode == ParamMode::Ref {
                resolver.ref_param_slots.insert(slot.0);
            }
        }
        resolver.max_slot = resolver.next_slot;
        resolver
    }

    pub(super) fn is_ref_parameter_binding(&self, slot: SlotIdx) -> bool {
        self.ref_param_slots.contains(&slot.0)
    }

    fn bind_slot(&mut self, name: &str, name_idx: NameIdx, slot: SlotIdx) {
        let name: Rc<str> = Rc::from(name);
        let previous = self.visible.insert(Rc::clone(&name), slot);
        let scope = self.scopes.last_mut().expect("resolver always has a scope");
        let binding = scope.bindings.len() as u32;
        if !scope.first_bindings.contains_key(name.as_ref()) {
            scope
                .first_bindings
                .insert(Rc::clone(&name), (binding, name_idx));
            if let Some(metadata) = scope.metadata {
                self.local_scopes[metadata.0 as usize]
                    .entries
                    .push(LocalScopeName {
                        name: name_idx,
                        first_binding: binding,
                    });
            }
        }
        scope.bindings.push(ScopeBinding {
            name,
            slot,
            previous,
        });
        if let Some(metadata) = scope.metadata {
            self.local_scopes[metadata.0 as usize].binding_count = binding + 1;
        }
    }

    /// Allocate a fresh slot in the innermost scope. A repeated declaration
    /// receives a fresh slot and becomes the visible same-scope winner.
    pub(super) fn declare(&mut self, name: &str, name_idx: NameIdx) -> SlotIdx {
        let slot = SlotIdx(self.next_slot);
        self.next_slot += 1;
        self.max_slot = self.max_slot.max(self.next_slot);
        self.bind_slot(name, name_idx, slot);
        slot
    }

    /// Returns `None` for captures, imports, named functions, and builtins so
    /// the compiler can retain its existing name-based fallback.
    pub(super) fn resolve(&self, name: &str) -> Option<SlotIdx> {
        self.visible.get(name)
    }

    /// Capture the shared scope and source-order frontier visible at one use.
    pub(super) fn innermost_scope_snapshot(&mut self) -> LocalScopeSnapshot {
        let scope = self.scopes.last_mut().expect("resolver always has a scope");
        let metadata = *scope.metadata.get_or_insert_with(|| {
            let metadata = LocalScopeIdx(self.local_scopes.len() as u32);
            self.local_scopes.push(LocalScopeNames {
                binding_count: scope.bindings.len() as u32,
                entries: scope
                    .first_bindings
                    .values()
                    .map(|(first_binding, name)| LocalScopeName {
                        name: *name,
                        first_binding: *first_binding,
                    })
                    .collect(),
            });
            metadata
        });
        LocalScopeSnapshot {
            scope: metadata,
            binding_count: scope.bindings.len() as u32,
        }
    }

    pub(super) fn push_scope(&mut self) {
        self.scopes.push(LocalScope {
            bindings: Vec::new(),
            metadata: None,
            first_bindings: VisibilityMap::default(),
        });
    }

    pub(super) fn pop_scope(&mut self) {
        assert!(
            self.scopes.len() > 1,
            "resolver root scope must remain active"
        );
        let scope = self.scopes.pop().expect("scope count checked above");
        for binding in scope.bindings.into_iter().rev() {
            debug_assert_eq!(self.visible.get(&binding.name), Some(binding.slot));
            if let Some(previous) = binding.previous {
                self.visible.insert(binding.name, previous);
            } else {
                self.visible.remove(&binding.name);
            }
        }
        // Reusing a popped slot would require liveness tracking across control
        // flow. Retaining it keeps every local access a direct frame Vec read.
    }

    pub(super) fn finish(mut self, names: &[String]) -> LocalResolverOutput {
        for scope in &mut self.local_scopes {
            scope.entries.sort_by(|left, right| {
                names[left.name.0 as usize].cmp(&names[right.name.0 as usize])
            });
        }
        LocalResolverOutput {
            max_slot: self.max_slot,
            parameter_slots: self.parameter_slots,
            local_scopes: self.local_scopes,
        }
    }
}

pub(super) struct LocalResolverOutput {
    pub(super) max_slot: u32,
    pub(super) parameter_slots: Vec<SlotIdx>,
    pub(super) local_scopes: Vec<LocalScopeNames>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    use alloc::format;
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    use std::format;

    fn parameter(name: &str, mode: ParamMode) -> Parameter {
        Parameter {
            name: String::from(name),
            mode,
        }
    }

    #[test]
    fn visibility_index_preserves_redeclaration_shadowing_and_scope_restoration() {
        let params = [
            parameter("outer", ParamMode::Value),
            parameter("by_ref", ParamMode::Ref),
        ];
        let mut resolver = LocalResolver::new(&params, &[NameIdx(0), NameIdx(1)]);

        assert_eq!(resolver.resolve("outer"), Some(SlotIdx(0)));
        assert_eq!(resolver.resolve("by_ref"), Some(SlotIdx(1)));
        assert_eq!(resolver.parameter_slots, [SlotIdx(0), SlotIdx(1)]);
        assert!(resolver.is_ref_parameter_binding(SlotIdx(1)));

        let first = resolver.declare("same_scope", NameIdx(2));
        let second = resolver.declare("same_scope", NameIdx(2));
        assert_eq!((first, second), (SlotIdx(2), SlotIdx(3)));
        assert_eq!(resolver.resolve("same_scope"), Some(second));

        resolver.push_scope();
        let inner_outer = resolver.declare("outer", NameIdx(0));
        resolver.declare("zebra", NameIdx(3));
        resolver.declare("alpha", NameIdx(4));
        resolver.declare("zebra", NameIdx(3));
        assert_eq!(resolver.resolve("outer"), Some(inner_outer));
        assert_eq!(
            resolver.innermost_scope_snapshot(),
            LocalScopeSnapshot {
                scope: LocalScopeIdx(0),
                binding_count: 4,
            }
        );

        resolver.pop_scope();
        assert_eq!(resolver.resolve("outer"), Some(SlotIdx(0)));
        assert_eq!(resolver.resolve("same_scope"), Some(second));
        assert_eq!(resolver.resolve("missing"), None);
        assert_eq!(resolver.max_slot, 8);
    }

    #[test]
    fn duplicate_parameters_share_one_binding_and_preserve_positional_metadata() {
        let params = [
            parameter("same", ParamMode::Value),
            parameter("same", ParamMode::Ref),
            parameter("other", ParamMode::Ref),
            parameter("same", ParamMode::Value),
        ];
        let resolver =
            LocalResolver::new(&params, &[NameIdx(0), NameIdx(0), NameIdx(1), NameIdx(0)]);

        assert_eq!(
            resolver.parameter_slots,
            [SlotIdx(0), SlotIdx(0), SlotIdx(1), SlotIdx(0)]
        );
        assert_eq!(resolver.resolve("same"), Some(SlotIdx(0)));
        assert_eq!(resolver.resolve("other"), Some(SlotIdx(1)));
        assert!(resolver.is_ref_parameter_binding(SlotIdx(0)));
        assert!(resolver.is_ref_parameter_binding(SlotIdx(1)));
        assert_eq!(resolver.max_slot, 2);
    }

    fn indexed_lookup_count(binding_count: usize) -> usize {
        let mut resolver = LocalResolver::new(&[], &[]);
        for index in 0..binding_count {
            resolver.declare(&format!("binding_{index}"), NameIdx(index as u32));
        }

        resolver.visible.reset_lookup_count();
        for index in 0..binding_count {
            assert_eq!(
                resolver.resolve(&format!("binding_{index}")),
                Some(SlotIdx(index as u32))
            );
        }
        resolver.visible.lookup_count()
    }

    #[test]
    fn resolution_uses_one_visibility_index_lookup_per_access() {
        let small = indexed_lookup_count(1_024);
        let large = indexed_lookup_count(8_192);

        assert_eq!(small, 1_024);
        assert_eq!(large, 8_192);
        assert_eq!(large / small, 8);
    }
}
