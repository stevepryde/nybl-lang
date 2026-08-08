use nybl::value::NyblFn;
use nybl::{NyblError, NyblHost, NyblLimits, Value};
use nybl_vm::NyblInstance;
use std::collections::BTreeMap;
use std::rc::Rc;

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
fn load_rejects_a_program_using_a_host_disabled_builtin() {
    let mut host = Host;
    let limits = NyblLimits::standard().with_disabled_builtins(["rand"]);
    let error = NyblInstance::load("pub fn roll() { return rand(6) }", &mut host, &limits)
        .err()
        .expect("load must refuse a disabled-builtin reference");
    assert_eq!(error.message, "builtin `rand` is disabled by the host");
    assert!(error.is_fatal, "disabled builtins are uncatchable");
}

#[test]
fn executed_final_public_declarations_define_the_abi() {
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
    assert!(instance.call("gone", &[], &mut host).is_err());
}

#[test]
fn top_level_return_retains_prior_root_state_and_skips_later_entries() {
    let mut host = Host;
    let mut instance = NyblInstance::load(
        "let count = 4\npub fn next() { count += 1; return count }\nreturn\npub fn skipped() {}",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();
    assert_eq!(instance.entry_points().len(), 1);
    assert_eq!(
        instance.call("next", &[], &mut host).unwrap().inspect(),
        "5"
    );
    assert_eq!(
        instance.call("next", &[], &mut host).unwrap().inspect(),
        "6"
    );
    assert!(instance.call("skipped", &[], &mut host).is_err());
}

#[test]
fn callbacks_have_instance_affinity_and_preflight_is_line_zero() {
    let source = "pub fn make() { return fn(x) { return x } }";
    let mut host = Host;
    let mut first = NyblInstance::load(source, &mut host, &NyblLimits::standard()).unwrap();
    let callback = first.call("make", &[], &mut host).unwrap();
    let arity = first.call_value(&callback, &[], &mut host).unwrap_err();
    assert_eq!(arity.line, Some(0));
    assert_eq!(
        first
            .call_value(&callback, &[Value::Int(7)], &mut host)
            .unwrap()
            .inspect(),
        "7"
    );

    let mut second = NyblInstance::load(source, &mut host, &NyblLimits::standard()).unwrap();
    let affinity = second
        .call_value(&callback, &[Value::Int(7)], &mut host)
        .unwrap_err();
    assert_eq!(affinity.line, Some(0));
    assert!(affinity.message.contains("different Nybl engine instance"));
}

#[test]
fn embedding_calls_reject_ref_parameters_as_value_only() {
    let mut host = Host;
    let mut instance = NyblInstance::load(
        r#"
let calls = 0
pub fn mutate(ref value) {
    calls = calls + 1
    value = 99
}
pub fn read_calls() { return calls }
pub fn make_ref_callback() {
    return fn(ref value) { panic("callback body ran") }
}
"#,
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();

    let entry_error = instance
        .call("mutate", &[Value::Int(1)], &mut host)
        .unwrap_err();
    assert_eq!(entry_error.line, Some(0));
    assert_eq!(
        entry_error.message,
        "argument 1 to `mutate` must be passed with `ref`"
    );
    assert_eq!(
        entry_error.friendly_hint.as_deref(),
        Some("Write `ref` before argument 1.")
    );
    assert_eq!(
        instance
            .call("read_calls", &[], &mut host)
            .unwrap()
            .inspect(),
        "0"
    );

    let callback = instance.call("make_ref_callback", &[], &mut host).unwrap();
    let callback_error = instance
        .call_value(&callback, &[Value::Int(1)], &mut host)
        .unwrap_err();
    assert_eq!(callback_error.line, Some(0));
    assert_eq!(
        callback_error.message,
        "argument 1 to `fn` must be passed with `ref`"
    );
    assert_eq!(
        callback_error.friendly_hint.as_deref(),
        Some("Write `ref` before argument 1.")
    );
}

#[test]
fn try_call_wraps_foreign_affinity_and_callback_arity_preflight_errors() {
    let mut host = Host;
    let mut first = NyblInstance::load(
        "pub fn make() { return fn() { return 7 } }",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();
    let foreign = first.call("make", &[], &mut host).unwrap();

    let mut second = NyblInstance::load(
        "pub fn probe(f) { return try_call(f) }\npub fn make_arity() { return fn(x) { return x } }",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();
    let affinity = second.call("probe", &[foreign], &mut host).unwrap();
    assert!(affinity.inspect().contains("Result::Err"));
    assert!(
        affinity
            .inspect()
            .contains("different Nybl engine instance")
    );

    let wrong_arity = second.call("make_arity", &[], &mut host).unwrap();
    let arity = second.call("probe", &[wrong_arity], &mut host).unwrap();
    assert!(arity.inspect().contains("Result::Err"));
    assert!(arity.inspect().contains("expects 1 argument"));
}

#[test]
fn uncaught_errors_unwind_transient_state_but_keep_prior_mutations() {
    let mut host = Host;
    let mut instance = NyblInstance::load(
        "let count = 0\npub fn fail() { count += 1; let local = [1, 2]; panic(\"boom\") }\npub fn read() { return count }",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();
    assert!(instance.call("fail", &[], &mut host).is_err());
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "1"
    );
    assert!(instance.call("fail", &[], &mut host).is_err());
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "2"
    );
}

#[test]
fn callback_preframe_errors_do_not_erase_live_root_state() {
    let mut host = Host;
    let mut instance = NyblInstance::load(
        "let count = 0\npub fn bump() { count += 1; return count }\npub fn read() { return count }",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();
    assert_eq!(
        instance.call("bump", &[], &mut host).unwrap().inspect(),
        "1"
    );

    let opaque_body: Rc<dyn std::any::Any> = Rc::new(17_u8);
    let external = Value::Fn(
        NyblFn::try_new_compiled(Vec::new(), Vec::new(), opaque_body, None, 0, 0).unwrap(),
    );
    assert!(instance.call_value(&external, &[], &mut host).is_err());
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "1"
    );
}

#[test]
fn step_budget_resets_for_each_call() {
    let limits = NyblLimits {
        max_steps: 20,
        max_memory: 1024 * 1024,
        ..NyblLimits::standard()
    };
    let mut host = Host;
    let mut instance = NyblInstance::load(
        "pub fn work() { let total = 0; repeat 5 { total += 1 }; return total }",
        &mut host,
        &limits,
    )
    .unwrap();
    assert_eq!(
        instance.call("work", &[], &mut host).unwrap().inspect(),
        "5"
    );
    assert_eq!(
        instance.call("work", &[], &mut host).unwrap().inspect(),
        "5"
    );
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
fn imported_module_values_are_authoritative_across_calls_and_callbacks() {
    let mut modules = BTreeMap::new();
    modules.insert(
        "state".to_string(),
        "let count = 0\nlet items = [1]\nfn next() { count += 1; items.push(count); return [count, items] }\nfn via(handle) { count += 1; return handle.count }\nfn make() { return fn() { count += 1; return count } }"
            .to_string(),
    );
    let mut host = ModuleHost { modules };
    let mut instance = NyblInstance::load(
        "use state as state\npub fn next() { return state.next() }\npub fn via() { return state.via(state) }\npub fn make() { return state.make() }",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();
    assert_eq!(
        instance.call("next", &[], &mut host).unwrap().inspect(),
        "[1, [1, 1]]"
    );
    assert_eq!(
        instance.call("next", &[], &mut host).unwrap().inspect(),
        "[2, [1, 1, 2]]"
    );
    assert_eq!(instance.call("via", &[], &mut host).unwrap().inspect(), "3");
    let callback = instance.call("make", &[], &mut host).unwrap();
    assert_eq!(
        instance
            .call_value(&callback, &[], &mut host)
            .unwrap()
            .inspect(),
        "4"
    );
    assert_eq!(instance.call("via", &[], &mut host).unwrap().inspect(), "5");
}

#[test]
fn facade_reexports_share_authoritative_values_with_the_declaring_module() {
    let mut modules = BTreeMap::new();
    modules.insert(
        "leaf".to_string(),
        "let count = 0\nlet items = [0]\nfn bump() { count += 1; items.push(count); return [count, items] }\nfn make() { return fn() { count += 1; return count } }"
            .to_string(),
    );
    modules.insert("facade".to_string(), "use leaf".to_string());
    let mut host = ModuleHost { modules };
    let mut instance = NyblInstance::load(
        "use facade as api\nuse leaf as direct\npub fn bump() { return api.bump() }\npub fn read() { return [api.count, direct.count, api.items, direct.items] }\npub fn make() { return api.make() }",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();

    assert_eq!(
        instance.call("bump", &[], &mut host).unwrap().inspect(),
        "[1, [0, 1]]"
    );
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "[1, 1, [0, 1], [0, 1]]"
    );
    let callback = instance.call("make", &[], &mut host).unwrap();
    assert_eq!(
        instance
            .call_value(&callback, &[], &mut host)
            .unwrap()
            .inspect(),
        "2"
    );
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "[2, 2, [0, 1], [0, 1]]"
    );
}

#[test]
fn flat_imports_read_current_authoritative_values_at_root_and_in_functions() {
    let mut modules = BTreeMap::new();
    modules.insert(
        "leaf".to_string(),
        "let count = 0\nfn bump() { count += 1; return count }".to_string(),
    );
    let mut host = ModuleHost { modules };
    let mut instance = NyblInstance::load(
        "use leaf\nuse leaf as direct\npub fn next() { return direct.bump() }\npub fn read_root() { return count }\npub fn read_selective() { use leaf.{count}; return count }\npub fn read_glob() { use leaf; return count }",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();

    assert_eq!(
        instance.call("next", &[], &mut host).unwrap().inspect(),
        "1"
    );
    assert_eq!(
        instance
            .call("read_root", &[], &mut host)
            .unwrap()
            .inspect(),
        "1"
    );
    assert_eq!(
        instance
            .call("read_selective", &[], &mut host)
            .unwrap()
            .inspect(),
        "1"
    );
    assert_eq!(
        instance.call("next", &[], &mut host).unwrap().inspect(),
        "2"
    );
    assert_eq!(
        instance
            .call("read_glob", &[], &mut host)
            .unwrap()
            .inspect(),
        "2"
    );
}

#[test]
fn failed_facade_load_restores_forwarded_values_and_remains_retryable() {
    let mut modules = BTreeMap::new();
    modules.insert("leaf".to_string(), "let count = 0".to_string());
    modules.insert(
        "bad".to_string(),
        "use leaf\ncount += 1\npanic(\"bad facade\")".to_string(),
    );
    let mut host = ModuleHost { modules };
    let mut instance = NyblInstance::load(
        "use leaf as leaf\npub fn load_bad() { use bad }\npub fn read() { return leaf.count }",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();

    assert!(instance.call("load_bad", &[], &mut host).is_err());
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "1"
    );
    assert!(instance.call("load_bad", &[], &mut host).is_err());
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "2"
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
fn recursive_and_cross_module_calls_restore_authoritative_environments_in_order() {
    let mut modules = BTreeMap::new();
    modules.insert(
        "a".to_string(),
        "let count = 0\nfn recurse(n) { count += 1; if n > 0 { return recurse(n - 1) }; return count }\nfn touch() { count += 1; return count }\nfn enter(other, handle) { count += 10; let inner = other.enter(handle); count += 100; return [inner, count] }\nfn read() { return count }"
            .to_string(),
    );
    modules.insert(
        "b".to_string(),
        "let count = 0\nfn enter(other) { count += 10; let inner = other.touch(); count += 100; return [inner, count] }\nfn read() { return count }"
            .to_string(),
    );
    let mut host = ModuleHost { modules };
    let mut instance = NyblInstance::load(
        "use a as a\nuse b as b\npub fn cross() { return a.enter(b, a) }\npub fn recurse() { return a.recurse(2) }\npub fn read() { return [a.read(), b.read()] }",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();

    assert_eq!(
        instance.call("cross", &[], &mut host).unwrap().inspect(),
        "[[11, 110], 111]"
    );
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "[111, 110]"
    );
    assert_eq!(
        instance.call("recurse", &[], &mut host).unwrap().inspect(),
        "114"
    );
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "[114, 110]"
    );
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

#[test]
fn fatal_step_and_call_depth_errors_leave_the_instance_reusable() {
    let mut host = Host;
    let step_limits = NyblLimits {
        max_steps: 20,
        max_memory: 1024 * 1024,
        ..NyblLimits::standard()
    };
    let mut step_instance = NyblInstance::load(
        "pub fn spin() { while true {} }\npub fn ok() { return 7 }",
        &mut host,
        &step_limits,
    )
    .unwrap();
    let step_error = step_instance.call("spin", &[], &mut host).unwrap_err();
    assert!(step_error.is_fatal);
    assert_eq!(
        step_instance.call("ok", &[], &mut host).unwrap().inspect(),
        "7"
    );

    let mut depth_instance = NyblInstance::load(
        "pub fn dive() { return dive() }\npub fn ok() { return 8 }",
        &mut host,
        &NyblLimits {
            max_steps: 1_000_000,
            max_memory: 1024 * 1024,
            ..NyblLimits::standard()
        },
    )
    .unwrap();
    let depth_error = depth_instance.call("dive", &[], &mut host).unwrap_err();
    assert!(
        depth_error
            .message
            .contains("Too many nested function calls")
    );
    assert_eq!(
        depth_instance.call("ok", &[], &mut host).unwrap().inspect(),
        "8"
    );
}

struct CountingModuleHost {
    modules: BTreeMap<String, String>,
    resolutions: usize,
}

impl NyblHost for CountingModuleHost {
    fn call(
        &mut self,
        _name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        None
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        self.resolutions += 1;
        self.modules.get(name).cloned().map(Ok)
    }
}

#[test]
fn lazy_imports_are_cached_even_when_a_later_host_changes_source() {
    let mut host = CountingModuleHost {
        modules: BTreeMap::from([(
            "dynamic".to_string(),
            "struct Item { value }\nfn Item.double(self) { return self.value * 2 }\nfn make() { return Item { value: 1 } }\nlet value = 1"
                .to_string(),
        )]),
        resolutions: 0,
    };
    let mut instance = NyblInstance::load(
        "pub fn read() { use dynamic as dynamic; return dynamic.value }\npub fn method() { use dynamic as dynamic; return dynamic.make().double() }",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();
    assert_eq!(host.resolutions, 0);
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "1"
    );
    assert_eq!(host.resolutions, 1);

    host.modules
        .insert("dynamic".to_string(), "let value = 2".to_string());
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "1"
    );
    assert_eq!(
        instance.call("method", &[], &mut host).unwrap().inspect(),
        "2"
    );
    assert_eq!(host.resolutions, 1);
}

#[test]
fn retained_types_methods_rng_and_cow_values_survive_across_calls() {
    let mut host = Host;
    let source = "struct Counter { value }\nfn Counter.inc(self) { return Counter { value: self.value + 1 } }\nlet stored = Counter { value: 3 }\nlet items = [1]\npub fn method() { stored = stored.inc(); return stored.value }\npub fn take(value) { value.push(2); return value }\npub fn get() { return items }\npub fn push() { items.push(3); return items }\npub fn random() { return rand(1000000) }\npub fn random_pair() { return [rand(1000000), rand(1000000)] }";
    let mut instance = NyblInstance::load(source, &mut host, &NyblLimits::standard()).unwrap();
    assert_eq!(
        instance.call("method", &[], &mut host).unwrap().inspect(),
        "4"
    );
    assert_eq!(
        instance.call("method", &[], &mut host).unwrap().inspect(),
        "5"
    );

    let original = Value::new_array(vec![Value::Int(1)]);
    assert_eq!(
        instance
            .call("take", std::slice::from_ref(&original), &mut host)
            .unwrap()
            .inspect(),
        "[1, 2]"
    );
    assert_eq!(original.inspect(), "[1]");

    let returned = instance.call("get", &[], &mut host).unwrap();
    let Value::Array(mut external) = returned else {
        panic!("expected array")
    };
    external.try_push(Value::Int(2), 0).unwrap();
    assert_eq!(external.len(), 2);
    assert_eq!(
        instance.call("get", &[], &mut host).unwrap().inspect(),
        "[1]"
    );
    assert_eq!(
        instance.call("push", &[], &mut host).unwrap().inspect(),
        "[1, 3]"
    );
    assert_eq!(external.len(), 2);

    let first = instance.call("random", &[], &mut host).unwrap();
    let second = instance.call("random", &[], &mut host).unwrap();
    let mut comparison = NyblInstance::load(source, &mut host, &NyblLimits::standard()).unwrap();
    assert_eq!(
        comparison
            .call("random_pair", &[], &mut host)
            .unwrap()
            .inspect(),
        format!("[{}, {}]", first.inspect(), second.inspect())
    );
}

struct NestedInstanceHost {
    nested: Option<NyblInstance>,
}

impl NyblHost for NestedInstanceHost {
    fn call(
        &mut self,
        name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        if name != "nested_call" {
            return None;
        }
        let mut nested = self.nested.take().expect("nested instance present");
        let result = nested.call("next", &[], self);
        self.nested = Some(nested);
        Some(result)
    }
}

#[test]
fn host_calls_may_nest_a_different_instance_without_crossing_state() {
    let mut plain_host = Host;
    let nested = NyblInstance::load(
        "let count = 0\npub fn next() { count += 1; return count }",
        &mut plain_host,
        &NyblLimits::standard(),
    )
    .unwrap();
    let mut host = NestedInstanceHost {
        nested: Some(nested),
    };
    let mut outer = NyblInstance::load(
        "pub fn invoke() { return nested_call() }",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();

    assert_eq!(outer.call("invoke", &[], &mut host).unwrap().inspect(), "1");
    assert_eq!(outer.call("invoke", &[], &mut host).unwrap().inspect(), "2");
}
