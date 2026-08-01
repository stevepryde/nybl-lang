//! Stateful host-callable tree-walker instances and shared ABI metadata.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{format, string::String, vec::Vec};

use crate::builtins::error;
use crate::memory::MemoryContext;
use crate::{NyblError, NyblHost, NyblLimits, ReplSession, Value};
use core::cell::Cell;

/// A public root function exposed by a loaded [`crate::NyblInstance`].
///
/// Entries are returned in final declaration order. Fields are intentionally
/// private so future ABI metadata can be added without making struct literals
/// a compatibility constraint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryPoint {
    pub(crate) name: String,
    /// Required positional count. Variadic entries accept any larger count.
    pub(crate) arity: usize,
    pub(crate) variadic: bool,
}

impl EntryPoint {
    #[doc(hidden)]
    pub fn __new(name: String, arity: usize) -> Self {
        Self {
            name,
            arity,
            variadic: false,
        }
    }

    #[doc(hidden)]
    pub fn __new_variadic(name: String, required_arity: usize) -> Self {
        Self {
            name,
            arity: required_arity,
            variadic: true,
        }
    }

    /// Source name of the public root function.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Required positional arguments. For a variadic entry this is the
    /// minimum; use [`Self::is_variadic`] or [`Self::accepts_arity`] when
    /// validating a call.
    pub const fn arity(&self) -> usize {
        self.arity
    }

    /// Whether this entry accepts arbitrarily many arguments after its fixed
    /// positional prefix.
    pub const fn is_variadic(&self) -> bool {
        self.variadic
    }

    /// Maximum accepted arity, or `None` for a variadic entry.
    pub const fn max_arity(&self) -> Option<usize> {
        if self.variadic {
            None
        } else {
            Some(self.arity)
        }
    }

    /// Whether `count` positional arguments satisfy this entry's arity.
    pub const fn accepts_arity(&self, count: usize) -> bool {
        if self.variadic {
            count >= self.arity
        } else {
            count == self.arity
        }
    }
}

/// A loaded tree-walker program whose globals, imports, functions, types,
/// methods, RNG state, and returned callbacks remain live across calls.
///
/// Hosts are deliberately borrowed only for `load` and each call. An
/// instance never stores a host reference, so callers may use a different
/// compatible host for later operations.
pub struct NyblInstance {
    session: ReplSession,
    entries: Vec<EntryPoint>,
    limits: NyblLimits,
    in_operation: Cell<bool>,
    memory: MemoryContext,
}

impl NyblInstance {
    /// Parse and evaluate a program, retaining its module state for later
    /// calls to root-level `pub fn` declarations.
    pub fn load(
        source: &str,
        host: &mut dyn NyblHost,
        limits: &NyblLimits,
    ) -> Result<Self, NyblError> {
        let stmts = crate::parse(source)?;
        let mut session = ReplSession::new();
        let memory = MemoryContext::__new(limits.max_memory);
        session.run_stmts_in(&stmts, host, limits, &memory)?;
        if memory.__exceeded() {
            return Err(memory_limit_error());
        }
        let entries = session.instance_entries();
        Ok(Self {
            session,
            entries,
            limits: limits.clone(),
            in_operation: Cell::new(false),
            memory,
        })
    }

    /// Public root functions in final surviving declaration order.
    pub fn entry_points(&self) -> &[EntryPoint] {
        &self.entries
    }

    /// Call a public root function by its dedicated ABI name.
    pub fn call(
        &mut self,
        name: &str,
        args: &[Value],
        host: &mut dyn NyblHost,
    ) -> Result<Value, NyblError> {
        let _operation = OperationGuard::begin(&self.in_operation)?;
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| error(0, format!("Public entry point `{name}` was not found")))?;
        if !entry.accepts_arity(args.len()) {
            return Err(error(
                0,
                if entry.variadic {
                    format!(
                        "`{}` expects at least {} arguments, but got {}",
                        name,
                        entry.arity,
                        args.len(),
                    )
                } else {
                    format!(
                        "`{}` expects {} argument{}, but got {}",
                        name,
                        entry.arity,
                        if entry.arity == 1 { "" } else { "s" },
                        args.len(),
                    )
                },
            ));
        }
        if self.memory.__exceeded() {
            return Err(memory_limit_error());
        }
        let result = self
            .session
            .call_named(name, args, host, &self.limits, &self.memory);
        if self.memory.__exceeded() {
            Err(memory_limit_error())
        } else {
            result
        }
    }

    /// Invoke a callback value created by this instance.
    pub fn call_value(
        &mut self,
        callable: &Value,
        args: &[Value],
        host: &mut dyn NyblHost,
    ) -> Result<Value, NyblError> {
        let _operation = OperationGuard::begin(&self.in_operation)?;
        self.session
            .validate_instance_callable(callable, args.len())?;
        if self.memory.__exceeded() {
            return Err(memory_limit_error());
        }
        let result = self.session.call_fn_in(
            callable.clone(),
            args.to_vec(),
            host,
            &self.limits,
            &self.memory,
        );
        if self.memory.__exceeded() {
            Err(memory_limit_error())
        } else {
            result
        }
    }
}

fn memory_limit_error() -> NyblError {
    NyblError::fatal("Memory limit exceeded", 0)
}

struct OperationGuard<'a>(&'a Cell<bool>);

impl<'a> OperationGuard<'a> {
    fn begin(flag: &'a Cell<bool>) -> Result<Self, NyblError> {
        if flag.replace(true) {
            return Err(error(0, "A Nybl instance cannot be re-entered"));
        }
        Ok(Self(flag))
    }
}

impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    use alloc::string::ToString;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    struct Host;
    impl NyblHost for Host {
        fn call(
            &mut self,
            _name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, NyblError>> {
            None
        }
    }

    #[test]
    fn final_public_declarations_define_the_abi() {
        let mut host = Host;
        let mut instance = NyblInstance::load(
            "pub fn first() { return 1 }\npub fn gone() {}\nfn gone() {}\npub fn first(x) { return x }\npub fn last() { return 3 }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        let entries: Vec<(&str, usize)> = instance
            .entry_points()
            .iter()
            .map(|entry| (entry.name(), entry.arity()))
            .collect();
        assert_eq!(entries, vec![("first", 1), ("last", 0)]);
        assert_eq!(
            instance
                .call("first", &[Value::Int(9)], &mut host)
                .unwrap()
                .inspect(),
            "9"
        );
        assert_eq!(
            instance.call("last", &[], &mut host).unwrap().inspect(),
            "3"
        );
        assert!(instance.call("gone", &[], &mut host).is_err());
    }

    #[test]
    fn calls_retain_and_mutate_root_globals() {
        let mut host = Host;
        let mut instance = NyblInstance::load(
            "let count = 0\npub fn next() { count += 1; return count }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "1"
        );
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "2"
        );
    }

    #[test]
    fn host_calls_reject_ref_bearing_entries_and_callbacks_before_execution() {
        let mut host = Host;
        let mut instance = NyblInstance::load(
            "let calls = 0\npub fn update(ref value) { calls += 1; value = 9 }\npub fn make() { return fn(ref value) { calls += 1; value = 8 } }\npub fn read() { return calls }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();

        let entry_error = instance
            .call("update", &[Value::Int(1)], &mut host)
            .unwrap_err();
        assert_eq!(
            entry_error.message,
            "argument 1 to `update` must be passed with `ref`"
        );
        assert_eq!(
            instance.call("read", &[], &mut host).unwrap().inspect(),
            "0"
        );

        let callback = instance.call("make", &[], &mut host).unwrap();
        let callback_error = instance
            .call_value(&callback, &[Value::Int(1)], &mut host)
            .unwrap_err();
        assert_eq!(
            callback_error.message,
            "argument 1 to `fn` must be passed with `ref`"
        );
        assert_eq!(
            instance.call("read", &[], &mut host).unwrap().inspect(),
            "0"
        );
    }

    #[test]
    fn executed_entries_are_dedicated_and_early_return_is_final() {
        let mut host = Host;
        let mut instance = NyblInstance::load(
            "pub fn entry() { fn entry(x) { return x }; return 1 }\nreturn\npub fn skipped() {}",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(instance.entry_points().len(), 1);
        assert_eq!(
            instance.call("entry", &[], &mut host).unwrap().inspect(),
            "1"
        );
        assert_eq!(
            instance.call("entry", &[], &mut host).unwrap().inspect(),
            "1"
        );
        assert!(instance.call("skipped", &[], &mut host).is_err());
    }

    #[test]
    fn top_level_return_skips_a_stripped_tail_expression() {
        let mut host = Host;
        let mut instance = NyblInstance::load(
            "pub fn ok() { return 1 }\nreturn\npanic(\"must not run\")",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(instance.call("ok", &[], &mut host).unwrap().inspect(), "1");
    }

    #[test]
    fn callbacks_are_bound_to_the_creating_instance() {
        let source = "pub fn make() { return fn() { return 7 } }";
        let mut host = Host;
        let mut first = NyblInstance::load(source, &mut host, &NyblLimits::standard()).unwrap();
        let callback = first.call("make", &[], &mut host).unwrap();
        assert_eq!(
            first
                .call_value(&callback, &[], &mut host)
                .unwrap()
                .inspect(),
            "7"
        );
        let mut second = NyblInstance::load(source, &mut host, &NyblLimits::standard()).unwrap();
        let error = second.call_value(&callback, &[], &mut host).unwrap_err();
        assert_eq!(error.line, Some(0));
        assert!(error.message.contains("different Nybl engine instance"));
    }

    struct ModuleHost {
        modules: BTreeMap<String, String>,
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

        fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
            self.modules.get(name).cloned().map(Ok)
        }
    }

    #[test]
    fn module_globals_are_live_through_aliases_and_facades() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "leaf".to_string(),
            "let count = 0\nlet items = [1]\nfn bump() { count += 1; return count }".to_string(),
        );
        modules.insert(
            "facade".to_string(),
            "use leaf\nfn read() { return count }\nfn inc() { count += 1; return count }\nfn add() { items.push(2); return items }"
                .to_string(),
        );
        let mut host = ModuleHost { modules };
        let mut instance = NyblInstance::load(
            "use facade as f\npub fn next() { let before = f.read(); let value = f.inc(); let after = f.read(); let items = f.add(); return [before, value, after, f.count, items, f.items] }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[0, 1, 1, 1, [1, 2], [1, 2]]"
        );
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[1, 2, 2, 2, [1, 2, 2], [1, 2, 2]]"
        );
    }

    #[test]
    fn facade_local_overwrite_returns_the_forwarded_value_to_its_origin() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "leaf".to_string(),
            "let count = 7\nfn read() { return count }\nfn bump() { count += 1; return count }"
                .to_string(),
        );
        modules.insert(
            "facade".to_string(),
            "use leaf\nlet count = 100\nfn own_bump() { count += 1; return count }".to_string(),
        );
        let mut host = ModuleHost { modules };
        let mut instance = NyblInstance::load(
            "use facade as facade\nuse leaf as leaf\npub fn bump() { return facade.own_bump() }\npub fn leaf_read() { return leaf.read() }\npub fn leaf_bump() { return leaf.bump() }\npub fn read() { return [facade.count, leaf.count] }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();

        assert_eq!(
            instance.call("read", &[], &mut host).unwrap().inspect(),
            "[100, 7]"
        );
        assert_eq!(
            instance
                .call("leaf_read", &[], &mut host)
                .unwrap()
                .inspect(),
            "7"
        );
        assert_eq!(
            instance
                .call("leaf_bump", &[], &mut host)
                .unwrap()
                .inspect(),
            "8"
        );
        assert_eq!(
            instance.call("bump", &[], &mut host).unwrap().inspect(),
            "101"
        );
        assert_eq!(
            instance.call("read", &[], &mut host).unwrap().inspect(),
            "[101, 8]"
        );
    }

    #[test]
    fn module_compatibility_snapshots_do_not_force_named_cow_detaches() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "leaf".to_string(),
            "let items = [1, 2, 3]\nfn pop() { items.pop() }".to_string(),
        );
        modules.insert(
            "facade".to_string(),
            "use leaf\nfn pop() { items.pop() }".to_string(),
        );
        let mut host = ModuleHost { modules };
        let mut instance = NyblInstance::load(
            "use leaf as leaf\nuse facade as facade\npub fn facade_pop() { facade.pop() }\npub fn direct_pop() { leaf.pop() }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        let loaded_bytes = instance.memory.__used();
        assert!(loaded_bytes > 0);

        instance.call("facade_pop", &[], &mut host).unwrap();
        assert_eq!(instance.memory.__used(), loaded_bytes);
        instance.call("direct_pop", &[], &mut host).unwrap();
        assert_eq!(instance.memory.__used(), loaded_bytes);
    }

    #[test]
    fn recursive_module_snapshots_do_not_retain_replaced_instance_receipts() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "leaf".to_string(),
            "let items = [[\"abcdefghijklmnopqrstuvwxyz0123456789\"]]\nlet captured = \"zyxwvutsrqponmlkjihgfedcba9876543210\"\nlet callback = fn() { return captured }\nfn clear() { items = []; captured = none; callback = none }"
                .to_string(),
        );
        let mut host = ModuleHost { modules };
        let mut instance = NyblInstance::load(
            "use leaf as leaf\npub fn clear() { leaf.clear() }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        let loaded_bytes = instance.memory.__used();
        assert!(loaded_bytes > 0);

        instance.call("clear", &[], &mut host).unwrap();
        let cleared_bytes = instance.memory.__used();
        assert!(cleared_bytes < loaded_bytes);

        let mut empty_modules = BTreeMap::new();
        empty_modules.insert(
            "leaf".to_string(),
            "let items = []\nlet captured = none\nlet callback = none\nfn clear() { items = []; captured = none; callback = none }"
                .to_string(),
        );
        let mut empty_host = ModuleHost {
            modules: empty_modules,
        };
        let empty = NyblInstance::load(
            "use leaf as leaf\npub fn clear() { leaf.clear() }",
            &mut empty_host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(cleared_bytes, empty.memory.__used());
    }

    #[test]
    fn lazy_modules_borrow_the_currently_active_origin_on_success_and_error() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "a".to_string(),
            "let count = 0\nlet items = [0]\nfn load_b() { count += 1; items.push(1); use b; return [count, items] }\nfn load_bad() { count += 1; items.push(2); use bad }\nfn read() { return [count, items] }"
                .to_string(),
        );
        modules.insert(
            "b".to_string(),
            "use a\ncount += 10\nitems.push(10)".to_string(),
        );
        modules.insert(
            "bad".to_string(),
            "use a.{count, items}\ncount += 100\nitems.push(100)\npanic(\"bad lazy module\")"
                .to_string(),
        );
        let mut host = ModuleHost { modules };
        let mut instance = NyblInstance::load(
            "use a as a\npub fn load_b() { return a.load_b() }\npub fn load_bad() { return a.load_bad() }\npub fn read() { return a.read() }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();

        assert_eq!(
            instance.call("load_b", &[], &mut host).unwrap().inspect(),
            "[11, [0, 1, 10]]"
        );
        assert!(instance.call("load_bad", &[], &mut host).is_err());
        assert_eq!(
            instance.call("read", &[], &mut host).unwrap().inspect(),
            "[112, [0, 1, 10, 2, 100]]"
        );
    }

    struct WrappingModuleHost {
        modules: BTreeMap<String, String>,
    }

    impl NyblHost for WrappingModuleHost {
        fn call(
            &mut self,
            name: &str,
            args: &[Value],
            line: u32,
        ) -> Option<Result<Value, NyblError>> {
            if name != "wrap" {
                return None;
            }
            Some(
                crate::value::NyblModule::try_new(
                    "host.wrapper".to_string(),
                    vec![("child".to_string(), args[0].clone())],
                    Vec::new(),
                    line,
                )
                .map(Value::Module),
            )
        }

        fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
            self.modules.get(name).cloned().map(Ok)
        }
    }

    #[test]
    fn module_snapshots_externalize_host_module_binding_graphs() {
        let root = "use leaf as leaf\npub fn clear() { leaf.clear() }";
        let mut modules = BTreeMap::new();
        modules.insert(
            "leaf".to_string(),
            "let child = [\"abcdefghijklmnopqrstuvwxyz0123456789\"]\nlet wrapped = wrap(child)\nfn clear() { child = none; wrapped = none }"
                .to_string(),
        );
        let mut host = WrappingModuleHost { modules };
        let mut instance = NyblInstance::load(root, &mut host, &NyblLimits::standard()).unwrap();
        let loaded_bytes = instance.memory.__used();
        assert!(loaded_bytes > 0);
        instance.call("clear", &[], &mut host).unwrap();
        let cleared_bytes = instance.memory.__used();
        assert!(cleared_bytes < loaded_bytes);

        let mut empty_modules = BTreeMap::new();
        empty_modules.insert(
            "leaf".to_string(),
            "let child = none\nlet wrapped = none\nfn clear() { child = none; wrapped = none }"
                .to_string(),
        );
        let mut empty_host = WrappingModuleHost {
            modules: empty_modules,
        };
        let empty = NyblInstance::load(root, &mut empty_host, &NyblLimits::standard()).unwrap();
        assert_eq!(cleared_bytes, empty.memory.__used());
    }

    #[test]
    fn callbacks_keep_module_globals_live_instead_of_capturing_snapshots() {
        let mut host = Host;
        let mut instance = NyblInstance::load(
            "let count = 0\npub fn make() { return fn() { count += 1; return count } }\npub fn inc() { count += 1; return count }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        let callback = instance.call("make", &[], &mut host).unwrap();
        assert_eq!(
            instance
                .call_value(&callback, &[], &mut host)
                .unwrap()
                .inspect(),
            "1"
        );
        assert_eq!(instance.call("inc", &[], &mut host).unwrap().inspect(), "2");
        assert_eq!(
            instance
                .call_value(&callback, &[], &mut host)
                .unwrap()
                .inspect(),
            "3"
        );
    }

    #[test]
    fn module_handles_read_the_active_defining_environment() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "state".to_string(),
            "let count = 0\nlet items = [1]\nfn via(handle) { count += 1; items.push(2); return [handle.count, handle.items] }"
                .to_string(),
        );
        let mut host = ModuleHost { modules };
        let mut instance = NyblInstance::load(
            "use state as state\npub fn next() { return state.via(state) }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[1, [1, 2]]"
        );
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[2, [1, 2, 2]]"
        );
    }

    #[test]
    fn active_module_handles_fall_back_to_callable_export_metadata() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "state".to_string(),
            "fn helper() { return 7 }\nfn via(handle) { return handle.helper() }".to_string(),
        );
        let mut host = ModuleHost { modules };
        let mut instance = NyblInstance::load(
            "use state as state\npub fn call() { return state.via(state) }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(
            instance.call("call", &[], &mut host).unwrap().inspect(),
            "7"
        );
    }

    #[test]
    fn failed_facade_initialization_restores_dependency_values() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "leaf".to_string(),
            "let count = 0\nfn bump() { count += 1; return count }".to_string(),
        );
        modules.insert("bad".to_string(), "use leaf\nmissing()".to_string());
        modules.insert("bad_signal".to_string(), "use leaf\nbreak".to_string());
        let mut host = ModuleHost { modules };
        let mut instance = NyblInstance::load(
            "pub fn fail() { use bad }\npub fn fail_signal() { use bad_signal }\npub fn recover() { use leaf as leaf; return leaf.bump() }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert!(instance.call("fail", &[], &mut host).is_err());
        assert_eq!(
            instance.call("recover", &[], &mut host).unwrap().inspect(),
            "1"
        );
        assert!(instance.call("fail_signal", &[], &mut host).is_err());
        assert_eq!(
            instance.call("recover", &[], &mut host).unwrap().inspect(),
            "2"
        );
    }

    #[test]
    fn facade_handles_read_forwarded_bindings_from_the_active_facade() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "leaf".to_string(),
            "let count = 0\nlet items = [1]".to_string(),
        );
        modules.insert(
            "facade".to_string(),
            "use leaf\nfn via(handle) { count += 1; items.push(2); return [handle.count, handle.items] }"
                .to_string(),
        );
        let mut host = ModuleHost { modules };
        let mut instance = NyblInstance::load(
            "use facade as facade\npub fn next() { return facade.via(facade) }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[1, [1, 2]]"
        );
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[2, [1, 2, 2]]"
        );
    }

    #[test]
    fn origin_module_handles_find_values_moved_into_an_active_importer() {
        let mut modules = BTreeMap::new();
        modules.insert("leaf".to_string(), "let count = 0".to_string());
        modules.insert("facade".to_string(), "use leaf".to_string());
        let mut host = ModuleHost { modules };
        let mut instance = NyblInstance::load(
            "use leaf as leaf\nuse facade\npub fn next() { count += 1; return [count, leaf.count] }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[1, 1]"
        );
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[2, 2]"
        );
    }

    struct RetainingHost {
        retained: Option<Value>,
    }

    impl NyblHost for RetainingHost {
        fn call(
            &mut self,
            name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, NyblError>> {
            if name != "retain_large" {
                return None;
            }
            self.retained = Some(Value::new_str("x".repeat(16 * 1024)));
            Some(Ok(Value::None))
        }
    }

    #[test]
    fn host_allocations_are_untracked_and_final_return_is_checked() {
        let limits = NyblLimits {
            max_steps: 100,
            max_memory: 32,
        };
        let mut host = RetainingHost { retained: None };
        let mut instance = NyblInstance::load(
            "pub fn host_only() { retain_large() }\npub fn too_large() { return \"abcdefghijklmnopqrstuvwxyz0123456789\" }",
            &mut host,
            &limits,
        )
        .unwrap();
        instance.call("host_only", &[], &mut host).unwrap();
        assert!(host.retained.is_some());
        let error = instance.call("too_large", &[], &mut host).unwrap_err();
        assert!(error.is_fatal);
        assert!(error.message.contains("Memory limit"));
        assert_eq!(instance.memory.__used(), 0);
        instance.call("host_only", &[], &mut host).unwrap();
    }

    struct ExternalValueHost {
        value: Option<Value>,
    }

    impl NyblHost for ExternalValueHost {
        fn call(
            &mut self,
            name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, NyblError>> {
            (name == "take_external").then(|| Ok(self.value.take().unwrap_or(Value::None)))
        }
    }

    #[test]
    fn external_host_values_are_free_until_the_instance_first_mutates_them() {
        let external = Value::new_array((0..256).map(Value::Int).collect());
        let limits = NyblLimits {
            max_steps: 100,
            max_memory: 64,
        };
        let mut host = ExternalValueHost {
            value: Some(external),
        };
        let mut instance = NyblInstance::load(
            "let stored = none\npub fn keep() { stored = take_external() }\npub fn mutate() { stored.push(256) }\npub fn harmless() { return 1 }",
            &mut host,
            &limits,
        )
        .unwrap();

        instance.call("keep", &[], &mut host).unwrap();
        assert_eq!(instance.memory.__used(), 0);

        let mutation_error = instance.call("mutate", &[], &mut host).unwrap_err();
        assert!(mutation_error.is_fatal);
        assert!(mutation_error.message.contains("Memory limit"));
        assert!(instance.memory.__used() > limits.max_memory);

        let poisoned_error = instance.call("harmless", &[], &mut host).unwrap_err();
        assert!(poisoned_error.is_fatal);
        assert!(poisoned_error.message.contains("Memory limit"));
    }

    #[test]
    fn returned_values_keep_their_instance_receipt_until_the_last_owner_drops() {
        let mut host = Host;
        let mut instance = NyblInstance::load(
            "pub fn make() { return [1, 2, 3, 4] }\npub fn harmless() { return none }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(instance.memory.__used(), 0);

        let retained = instance.call("make", &[], &mut host).unwrap();
        let retained_bytes = instance.memory.__used();
        assert!(retained_bytes > 0);
        instance.call("harmless", &[], &mut host).unwrap();
        assert_eq!(instance.memory.__used(), retained_bytes);

        let second_owner = retained.clone();
        drop(retained);
        assert_eq!(instance.memory.__used(), retained_bytes);
        drop(second_owner);
        assert_eq!(instance.memory.__used(), 0);
    }

    #[test]
    fn interleaved_instances_keep_independent_memory_accounts() {
        let mut host = Host;
        let source = "pub fn make(x) { return [x, x] }";
        let limits = NyblLimits::standard();
        let mut first = NyblInstance::load(source, &mut host, &limits).unwrap();
        let mut second = NyblInstance::load(source, &mut host, &limits).unwrap();

        let first_value = first.call("make", &[Value::Int(1)], &mut host).unwrap();
        let first_bytes = first.memory.__used();
        assert!(first_bytes > 0);
        assert_eq!(second.memory.__used(), 0);

        let second_value = second.call("make", &[Value::Int(2)], &mut host).unwrap();
        let second_bytes = second.memory.__used();
        assert!(second_bytes > 0);
        assert_eq!(first.memory.__used(), first_bytes);

        drop(first_value);
        assert_eq!(first.memory.__used(), 0);
        assert_eq!(second.memory.__used(), second_bytes);
        drop(second_value);
        assert_eq!(second.memory.__used(), 0);
    }

    struct HookAllocatingHost {
        retained: RefCell<Vec<Value>>,
    }

    impl HookAllocatingHost {
        fn retain_large(&self) {
            self.retained
                .borrow_mut()
                .push(Value::new_str("x".repeat(16 * 1024)));
        }
    }

    impl NyblHost for HookAllocatingHost {
        fn call(
            &mut self,
            name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, NyblError>> {
            self.retain_large();
            (name == "host_value").then_some(Ok(Value::None))
        }

        fn on_print(&mut self, _message: &str) {
            self.retain_large();
        }

        fn function_hint(&self) -> &str {
            self.retain_large();
            "host hint"
        }

        fn on_tick(&mut self) -> Result<(), NyblError> {
            self.retain_large();
            Ok(())
        }

        fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
            self.retain_large();
            (name == "hook").then(|| Ok(String::new()))
        }
    }

    #[test]
    fn every_host_hook_leaves_instance_accounting_unchanged() {
        let limits = NyblLimits {
            max_steps: 100,
            max_memory: 64,
        };
        let mut host = HookAllocatingHost {
            retained: RefCell::new(Vec::new()),
        };
        let mut instance = NyblInstance::load(
            "use hook\npub fn print_it() { print(\"ok\") }\npub fn host_it() { host_value() }\npub fn hint_it() { missing() }",
            &mut host,
            &limits,
        )
        .unwrap();
        assert_eq!(instance.memory.__used(), 0);

        instance.call("print_it", &[], &mut host).unwrap();
        instance.call("host_it", &[], &mut host).unwrap();
        let error = instance.call("hint_it", &[], &mut host).unwrap_err();
        assert!(!error.is_fatal);
        assert!(
            error
                .friendly_hint
                .as_deref()
                .is_some_and(|hint| hint.contains("host hint"))
        );
        assert_eq!(instance.memory.__used(), 0);
        assert!(host.retained.borrow().len() >= 8);
    }

    #[test]
    fn reentry_rejection_precedes_target_and_arity_preflight() {
        let mut host = Host;
        let mut instance =
            NyblInstance::load("pub fn entry() {}", &mut host, &NyblLimits::standard()).unwrap();
        instance.in_operation.set(true);
        let error = instance
            .call("missing", &[Value::None], &mut host)
            .unwrap_err();
        instance.in_operation.set(false);
        assert_eq!(error.line, Some(0));
        assert!(error.message.contains("cannot be re-entered"));
    }
}
