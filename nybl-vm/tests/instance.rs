use nybl::value::NyblFn;
use nybl::{NyblError, NyblHost, NyblLimits, StructShape, Value};
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

#[test]
fn prepared_and_batch_calls_preserve_state_limits_and_affinity() {
    let limits = NyblLimits {
        max_steps: 20,
        max_memory: 1024 * 1024,
        ..NyblLimits::standard()
    };
    let source = "let total = 0\npub fn work(n) { repeat 5 { total += 1 }; total += n; return total }\npub fn read() { return total }";
    let mut host = Host;
    let mut instance = NyblInstance::load(source, &mut host, &limits).unwrap();
    let work = instance.prepare_entry("work").unwrap();
    assert_eq!(work.name(), "work");
    assert_eq!(work.arity(), 1);
    assert!(!work.is_variadic());
    assert_eq!(
        instance
            .call_prepared(&work, &[Value::Int(1)], &mut host)
            .unwrap()
            .inspect(),
        "6"
    );
    let calls = [[Value::Int(2)], [Value::Int(3)]];
    let values = instance.call_batch(&work, &calls, &mut host).unwrap();
    assert_eq!(
        values.iter().map(Value::inspect).collect::<Vec<_>>(),
        ["13", "21"]
    );

    let mut other = NyblInstance::load(source, &mut host, &limits).unwrap();
    let error = other
        .call_prepared(&work, &[Value::Int(1)], &mut host)
        .unwrap_err();
    assert!(error.message.contains("different Nybl engine instance"));
}

#[test]
fn batch_stops_on_error_and_keeps_prior_mutations() {
    let mut host = Host;
    let mut instance = NyblInstance::load(
        "let total = 0\npub fn add(n) { total += n; if n == 2 { panic(\"stop\") }; return total }\npub fn read() { return total }",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();
    let add = instance.prepare_entry("add").unwrap();
    let calls = [[Value::Int(1)], [Value::Int(2)], [Value::Int(4)]];
    assert_eq!(
        instance
            .call_batch(&add, &calls, &mut host)
            .unwrap_err()
            .message,
        "stop"
    );
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "3"
    );
}

#[test]
fn batch_retains_persistent_memory_accounting() {
    let limits = NyblLimits {
        max_steps: 100,
        max_memory: 64,
        ..NyblLimits::standard()
    };
    let mut host = Host;
    let mut instance = NyblInstance::load(
        "let values = []\npub fn keep(value) { values.push(value); return values.len() }",
        &mut host,
        &limits,
    )
    .unwrap();
    let keep = instance.prepare_entry("keep").unwrap();
    let calls = (0..32).map(|n| [Value::Int(n)]).collect::<Vec<_>>();
    let error = instance.call_batch(&keep, &calls, &mut host).unwrap_err();
    assert!(error.is_fatal);
    assert!(error.message.contains("Memory limit"));
}

struct ValueHost {
    value: i64,
    seen: Vec<i64>,
}

impl NyblHost for ValueHost {
    fn call(&mut self, name: &str, args: &[Value], line: u32) -> Option<Result<Value, NyblError>> {
        match name {
            "host_value" => Some(Ok(Value::Int(self.value))),
            "observe" => Some(match args {
                [Value::Int(value)] => {
                    self.seen.push(*value);
                    Ok(Value::Int(*value))
                }
                _ => Err(NyblError::runtime("observe expects one Int", line)),
            }),
            _ => None,
        }
    }
}

#[test]
fn prepared_entries_do_not_cache_host_specific_dispatch() {
    let source = "pub fn read() { return host_value() }";
    let limits = NyblLimits::standard();

    let mut walker_host_a = ValueHost {
        value: 11,
        seen: Vec::new(),
    };
    let mut walker = nybl::NyblInstance::load(source, &mut walker_host_a, &limits).unwrap();
    let walker_entry = walker.prepare_entry("read").unwrap();
    assert_eq!(
        walker
            .call_prepared(&walker_entry, &[], &mut walker_host_a)
            .unwrap()
            .inspect(),
        "11"
    );
    let mut walker_host_b = ValueHost {
        value: 22,
        seen: Vec::new(),
    };
    assert_eq!(
        walker
            .call_batch(&walker_entry, &[Vec::<Value>::new()], &mut walker_host_b)
            .unwrap()[0]
            .inspect(),
        "22"
    );

    let mut vm_host_a = ValueHost {
        value: 33,
        seen: Vec::new(),
    };
    let mut vm = NyblInstance::load(source, &mut vm_host_a, &limits).unwrap();
    let vm_entry = vm.prepare_entry("read").unwrap();
    assert_eq!(
        vm.call_prepared(&vm_entry, &[], &mut vm_host_a)
            .unwrap()
            .inspect(),
        "33"
    );
    let mut vm_host_b = ValueHost {
        value: 44,
        seen: Vec::new(),
    };
    assert_eq!(
        vm.call_batch(&vm_entry, &[Vec::<Value>::new()], &mut vm_host_b)
            .unwrap()[0]
            .inspect(),
        "44"
    );
}

#[test]
fn batch_matches_repeated_calls_for_results_state_and_host_effects() {
    let source = "let total = 0\npub fn step(n) { total += observe(n); return total }\npub fn read() { return total }";
    let calls = vec![
        vec![Value::Int(2)],
        vec![Value::Int(3)],
        vec![Value::Int(5)],
    ];
    let limits = NyblLimits::standard();

    let mut walker_repeated_host = ValueHost {
        value: 0,
        seen: Vec::new(),
    };
    let mut walker_repeated =
        nybl::NyblInstance::load(source, &mut walker_repeated_host, &limits).unwrap();
    let walker_repeated_values = calls
        .iter()
        .map(|args| {
            walker_repeated
                .call("step", args, &mut walker_repeated_host)
                .unwrap()
        })
        .collect::<Vec<_>>();
    let walker_repeated_final = walker_repeated
        .call("read", &[], &mut walker_repeated_host)
        .unwrap();

    let mut walker_batch_host = ValueHost {
        value: 0,
        seen: Vec::new(),
    };
    let mut walker_batch =
        nybl::NyblInstance::load(source, &mut walker_batch_host, &limits).unwrap();
    let walker_entry = walker_batch.prepare_entry("step").unwrap();
    let walker_batch_values = walker_batch
        .call_batch(&walker_entry, &calls, &mut walker_batch_host)
        .unwrap();
    let walker_batch_final = walker_batch
        .call("read", &[], &mut walker_batch_host)
        .unwrap();
    assert_eq!(
        walker_batch_values
            .iter()
            .map(Value::inspect)
            .collect::<Vec<_>>(),
        walker_repeated_values
            .iter()
            .map(Value::inspect)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        walker_batch_final.inspect(),
        walker_repeated_final.inspect()
    );
    assert_eq!(walker_batch_host.seen, walker_repeated_host.seen);

    let mut vm_repeated_host = ValueHost {
        value: 0,
        seen: Vec::new(),
    };
    let mut vm_repeated = NyblInstance::load(source, &mut vm_repeated_host, &limits).unwrap();
    let vm_repeated_values = calls
        .iter()
        .map(|args| {
            vm_repeated
                .call("step", args, &mut vm_repeated_host)
                .unwrap()
        })
        .collect::<Vec<_>>();
    let vm_repeated_final = vm_repeated
        .call("read", &[], &mut vm_repeated_host)
        .unwrap();

    let mut vm_batch_host = ValueHost {
        value: 0,
        seen: Vec::new(),
    };
    let mut vm_batch = NyblInstance::load(source, &mut vm_batch_host, &limits).unwrap();
    let vm_entry = vm_batch.prepare_entry("step").unwrap();
    let vm_batch_values = vm_batch
        .call_batch(&vm_entry, &calls, &mut vm_batch_host)
        .unwrap();
    let vm_batch_final = vm_batch.call("read", &[], &mut vm_batch_host).unwrap();
    assert_eq!(
        vm_batch_values
            .iter()
            .map(Value::inspect)
            .collect::<Vec<_>>(),
        vm_repeated_values
            .iter()
            .map(Value::inspect)
            .collect::<Vec<_>>()
    );
    assert_eq!(vm_batch_final.inspect(), vm_repeated_final.inspect());
    assert_eq!(vm_batch_host.seen, vm_repeated_host.seen);
}

#[test]
fn preparing_ref_entry_is_rejected_before_execution() {
    let mut host = Host;
    let instance = NyblInstance::load(
        "pub fn update(ref value) { value = 2 }",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();
    let error = instance.prepare_entry("update").unwrap_err();
    assert!(error.message.contains("cannot be called from the host"));
}

#[test]
fn prepared_batches_are_deterministic_across_walker_and_vm() {
    let source = "pub fn roll(n) { return rand(n) }";
    let calls = [
        [Value::Int(100)],
        [Value::Int(100)],
        [Value::Int(100)],
        [Value::Int(100)],
    ];
    let limits = NyblLimits::standard();

    let mut walker_host = Host;
    let mut walker = nybl::NyblInstance::load(source, &mut walker_host, &limits).unwrap();
    let walker_entry = walker.prepare_entry("roll").unwrap();
    let walker_values = walker
        .call_batch(&walker_entry, &calls, &mut walker_host)
        .unwrap();

    let mut vm_host = Host;
    let mut vm = NyblInstance::load(source, &mut vm_host, &limits).unwrap();
    let vm_entry = vm.prepare_entry("roll").unwrap();
    let vm_values = vm.call_batch(&vm_entry, &calls, &mut vm_host).unwrap();

    assert_eq!(
        walker_values.iter().map(Value::inspect).collect::<Vec<_>>(),
        vm_values.iter().map(Value::inspect).collect::<Vec<_>>()
    );
}

#[test]
fn prepared_variadic_entries_keep_minimum_arity_across_engines() {
    let source = "pub fn collect(first, ..rest) { return rest.len() }";
    let calls = vec![
        vec![Value::Int(1)],
        vec![Value::Int(1), Value::Int(2), Value::Int(3)],
    ];
    let limits = NyblLimits::standard();

    let mut walker_host = Host;
    let mut walker = nybl::NyblInstance::load(source, &mut walker_host, &limits).unwrap();
    let walker_entry = walker.prepare_entry("collect").unwrap();
    assert_eq!(walker_entry.arity(), 1);
    assert!(walker_entry.is_variadic());
    let walker_values = walker
        .call_batch(&walker_entry, &calls, &mut walker_host)
        .unwrap();

    let mut vm_host = Host;
    let mut vm = NyblInstance::load(source, &mut vm_host, &limits).unwrap();
    let vm_entry = vm.prepare_entry("collect").unwrap();
    let vm_values = vm.call_batch(&vm_entry, &calls, &mut vm_host).unwrap();

    assert_eq!(
        walker_values.iter().map(Value::inspect).collect::<Vec<_>>(),
        ["0", "2"]
    );
    assert_eq!(
        walker_values.iter().map(Value::inspect).collect::<Vec<_>>(),
        vm_values.iter().map(Value::inspect).collect::<Vec<_>>()
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

// ─── Instance groups and the tick-hook line ───────────────────────

#[derive(Clone, Copy, Debug)]
enum Engine {
    Walker,
    Vm,
}

const ENGINE_PAIRS: [(Engine, Engine); 4] = [
    (Engine::Walker, Engine::Walker),
    (Engine::Vm, Engine::Vm),
    (Engine::Walker, Engine::Vm),
    (Engine::Vm, Engine::Walker),
];

impl Engine {
    /// `repeat` iterations that cost about `units` of `max_steps`: one
    /// walker step per iteration, two VM instructions at `STEP_SCALE` 8.
    fn iterations(self, units: i64) -> Value {
        Value::Int(match self {
            Engine::Walker => units,
            Engine::Vm => units * 4,
        })
    }
}

/// How the two instances of a scenario are loaded.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Grouping {
    Shared,
    Separate,
    Ungrouped,
}

const GROUPINGS: [Grouping; 3] = [Grouping::Shared, Grouping::Separate, Grouping::Ungrouped];

enum Script {
    Walker(nybl::NyblInstance),
    Vm(NyblInstance),
}

impl Script {
    fn load(
        engine: Engine,
        source: &str,
        host: &mut dyn NyblHost,
        limits: &NyblLimits,
        group: Option<&nybl::InstanceGroup>,
    ) -> Result<Self, NyblError> {
        Ok(match (engine, group) {
            (Engine::Walker, Some(group)) => {
                Script::Walker(nybl::NyblInstance::load_in(source, host, group)?)
            }
            (Engine::Walker, None) => {
                Script::Walker(nybl::NyblInstance::load(source, host, limits)?)
            }
            (Engine::Vm, Some(group)) => Script::Vm(NyblInstance::load_in(source, host, group)?),
            (Engine::Vm, None) => Script::Vm(NyblInstance::load(source, host, limits)?),
        })
    }

    fn call(
        &mut self,
        name: &str,
        args: &[Value],
        host: &mut dyn NyblHost,
    ) -> Result<Value, NyblError> {
        match self {
            Script::Walker(instance) => instance.call(name, args, host),
            Script::Vm(instance) => instance.call(name, args, host),
        }
    }

    fn batch(
        &mut self,
        name: &str,
        calls: &[Vec<Value>],
        host: &mut dyn NyblHost,
    ) -> Result<Vec<Value>, NyblError> {
        match self {
            Script::Walker(instance) => {
                let entry = instance.prepare_entry(name)?;
                instance.call_batch(&entry, calls, host)
            }
            Script::Vm(instance) => {
                let entry = instance.prepare_entry(name)?;
                instance.call_batch(&entry, calls, host)
            }
        }
    }
}

/// Load instance A and instance B of one scenario.
fn load_pair(
    engines: (Engine, Engine),
    sources: (&str, &str),
    limits: &NyblLimits,
    grouping: Grouping,
) -> (Script, Script) {
    let shared = nybl::InstanceGroup::new(limits);
    let other = nybl::InstanceGroup::new(limits);
    let (group_a, group_b) = match grouping {
        Grouping::Shared => (Some(&shared), Some(&shared)),
        Grouping::Separate => (Some(&shared), Some(&other)),
        Grouping::Ungrouped => (None, None),
    };
    let a = Script::load(engines.0, sources.0, &mut Host, limits, group_a).unwrap();
    let b = Script::load(engines.1, sources.1, &mut Host, limits, group_b).unwrap();
    (a, b)
}

/// Host for instance A whose `ask()` runs `entry` on instance B (or a batch
/// of it) and records B's result.
struct AskHost {
    b: Option<Script>,
    entry: &'static str,
    args: Vec<Value>,
    batch: Option<Vec<Vec<Value>>>,
    /// Hand A `none` instead of B's error.
    swallow: bool,
    /// B's outcomes. Values are not retained, so a value B returns has a
    /// single owner once A holds it.
    results: Vec<Result<(), NyblError>>,
    /// Module sources A's `use` resolves.
    modules: BTreeMap<String, String>,
}

impl AskHost {
    fn new(b: Script, entry: &'static str, args: Vec<Value>) -> Self {
        Self {
            b: Some(b),
            entry,
            args,
            batch: None,
            swallow: false,
            results: Vec::new(),
            modules: BTreeMap::new(),
        }
    }

    fn b_result(&self) -> &Result<(), NyblError> {
        self.results.last().expect("A asked B")
    }
}

impl NyblHost for AskHost {
    fn call(
        &mut self,
        name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        if name != "ask" {
            return None;
        }
        let mut b = self.b.take().expect("B is not already running");
        let result = match &self.batch {
            Some(calls) => b.batch(self.entry, calls, &mut Host).map(Value::new_array),
            None => b.call(self.entry, &self.args, &mut Host),
        };
        self.b = Some(b);
        self.results
            .push(result.as_ref().map(|_| ()).map_err(Clone::clone));
        Some(match result {
            Err(_) if self.swallow => Ok(Value::None),
            other => other,
        })
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        self.modules.get(name).cloned().map(Ok)
    }
}

const RECURSE_THEN_ASK: &str =
    "fn down(n) { if n > 0 { return down(n - 1) } return ask() }\npub fn run(n) { return down(n) }";
const RECURSE: &str =
    "fn down(n) { if n > 0 { return down(n - 1) } return 0 }\npub fn run(n) { return down(n) }";

/// Recursing walker scenarios need more native stack than a debug test
/// thread's default; RUN-002 sizes debug threads at 24 MiB.
fn on_large_stack(test: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(24 * 1024 * 1024)
        .spawn(test)
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn grouped_nesting_budget_bounds_the_whole_chain_of_instances() {
    on_large_stack(grouped_nesting_scenarios);
}

fn grouped_nesting_scenarios() {
    // A `down` frame charges about seven units: 15 frames fit a budget of
    // 200 alone, 30 do not.
    let limits = NyblLimits::standard().with_max_nesting(200);
    for engines in ENGINE_PAIRS {
        for grouping in GROUPINGS {
            let (mut a, b) = load_pair(engines, (RECURSE_THEN_ASK, RECURSE), &limits, grouping);
            let mut host = AskHost::new(b, "run", vec![Value::Int(15)]);
            let result = a.call("run", &[Value::Int(15)], &mut host);
            let context = format!("{engines:?} {grouping:?}");
            if grouping == Grouping::Shared {
                let error = host.b_result().as_ref().expect_err(&context);
                assert_eq!(
                    error.message,
                    nybl::error_messages::NESTING_MESSAGE,
                    "{context}"
                );
                assert!(!error.is_fatal, "{context}: nesting errors are catchable");
                assert_eq!(result.unwrap_err().message, error.message, "{context}");
            } else {
                assert_eq!(result.unwrap().inspect(), "0", "{context}");
            }
        }
    }
}

#[test]
fn grouped_call_depth_cap_bounds_the_whole_chain_of_instances() {
    on_large_stack(grouped_call_depth_scenarios);
}

fn grouped_call_depth_scenarios() {
    // 41 frames fit the fixed 64-frame cap alone; 82 do not.
    let limits = NyblLimits::standard();
    for engines in ENGINE_PAIRS {
        for grouping in GROUPINGS {
            let (mut a, b) = load_pair(engines, (RECURSE_THEN_ASK, RECURSE), &limits, grouping);
            let mut host = AskHost::new(b, "run", vec![Value::Int(40)]);
            let result = a.call("run", &[Value::Int(40)], &mut host);
            let context = format!("{engines:?} {grouping:?}");
            if grouping == Grouping::Shared {
                let error = host.b_result().as_ref().expect_err(&context);
                assert_eq!(
                    error.message,
                    nybl::error_messages::CALL_DEPTH_MESSAGE,
                    "{context}"
                );
                assert!(!error.is_fatal, "{context}");
                assert!(result.is_err(), "{context}");
            } else {
                assert_eq!(result.unwrap().inspect(), "0", "{context}");
            }
        }
    }
}

const SPEND_ASK_SPEND: &str = "pub fn run(before, after) { repeat before { }\n ask()\n repeat after { }\n return 1 }\npub fn spend(n) { repeat n { } return 1 }";
const SPEND: &str = "pub fn spend(n) { repeat n { } return 1 }";

fn is_step_error(error: &NyblError) -> bool {
    error.is_fatal && error.message == nybl::error_messages::STEP_LIMIT_MESSAGE
}

#[test]
fn grouped_steps_bound_the_whole_call_across_engines() {
    let limits = NyblLimits {
        max_steps: 1_000,
        ..NyblLimits::standard()
    };
    for (ea, eb) in ENGINE_PAIRS {
        for grouping in GROUPINGS {
            let context = format!("{ea:?}/{eb:?} {grouping:?}");
            // About 600 + 600 units: each fits alone, the sum does not.
            let (mut a, b) = load_pair((ea, eb), (SPEND_ASK_SPEND, SPEND), &limits, grouping);
            let mut host = AskHost::new(b, "spend", vec![eb.iterations(600)]);
            host.swallow = true;
            let result = a.call("run", &[ea.iterations(600), ea.iterations(10)], &mut host);
            if grouping == Grouping::Shared {
                let b_error = host.b_result().as_ref().expect_err(&context);
                assert!(is_step_error(b_error), "{context}: {b_error:?}");
                // The host swallowed B's error; A's next tick still fails.
                let a_error = result.expect_err(&context);
                assert!(is_step_error(&a_error), "{context}: {a_error:?}");
            } else {
                assert!(host.b_result().is_ok(), "{context}");
                assert_eq!(result.unwrap().inspect(), "1", "{context}");
            }
            // After a failed or finished outermost operation, the next
            // outermost operation starts from zero.
            let mut b = host.b.take().unwrap();
            for _ in 0..2 {
                assert!(
                    b.call("spend", &[eb.iterations(900)], &mut Host).is_ok(),
                    "{context}"
                );
                assert!(
                    a.call("spend", &[ea.iterations(900)], &mut Host).is_ok(),
                    "{context}"
                );
            }

            // About 300 + 300 units fit together, so neither engine charges
            // the other more than the steps it actually used.
            let (mut a, b) = load_pair((ea, eb), (SPEND_ASK_SPEND, SPEND), &limits, grouping);
            let mut host = AskHost::new(b, "spend", vec![eb.iterations(300)]);
            let result = a.call("run", &[ea.iterations(300), ea.iterations(10)], &mut host);
            assert!(host.b_result().is_ok(), "{context}: {:?}", host.b_result());
            assert_eq!(result.unwrap().inspect(), "1", "{context}");
        }
    }
}

#[test]
fn nested_batch_counts_every_item_toward_the_enclosing_operation() {
    let limits = NyblLimits {
        max_steps: 1_000,
        ..NyblLimits::standard()
    };
    for (ea, eb) in ENGINE_PAIRS {
        for grouping in GROUPINGS {
            let context = format!("{ea:?}/{eb:?} {grouping:?}");
            // Four items of about 300 units each: each fits alone, all four
            // together do not.
            let items = vec![vec![eb.iterations(300)]; 4];
            let (mut a, b) = load_pair((ea, eb), (SPEND_ASK_SPEND, SPEND), &limits, grouping);
            let mut host = AskHost::new(b, "spend", Vec::new());
            host.batch = Some(items.clone());
            let result = a.call("run", &[Value::Int(0), Value::Int(0)], &mut host);
            if grouping == Grouping::Shared {
                let b_error = host.b_result().as_ref().expect_err(&context);
                assert!(is_step_error(b_error), "{context}: {b_error:?}");
                assert!(result.is_err(), "{context}");
            } else {
                assert!(host.b_result().is_ok(), "{context}");
                assert!(result.is_ok(), "{context}");
            }
            // The same batch as an outermost operation resets per item.
            let mut b = host.b.take().unwrap();
            assert_eq!(
                b.batch("spend", &items, &mut Host).unwrap().len(),
                4,
                "{context}"
            );
        }
    }
}

const KEEP: &str = "let kept = none\npub fn keep(n) { let items = []\n repeat n { items.push(n) }\n kept = items\n return 1 }\npub fn drop_it() { kept = none }";

#[test]
fn grouped_memory_bounds_the_total_retained_by_all_instances() {
    for (ea, eb) in ENGINE_PAIRS {
        let context = format!("{ea:?}/{eb:?}");
        // Measure what one retained value costs, then allow one and a half.
        let probe_limits = NyblLimits {
            max_steps: 100_000,
            ..NyblLimits::standard()
        };
        let probe = nybl::InstanceGroup::new(&probe_limits);
        let mut one = Script::load(ea, KEEP, &mut Host, &probe_limits, Some(&probe)).unwrap();
        one.call("keep", &[Value::Int(1_000)], &mut Host).unwrap();
        let retained = probe.__memory().__used();
        assert!(retained > 0, "{context}");
        let limits = NyblLimits {
            max_memory: retained + retained / 2,
            ..probe_limits
        };

        for grouping in GROUPINGS {
            let context = format!("{context} {grouping:?}");
            let (mut a, mut b) = load_pair((ea, eb), (KEEP, KEEP), &limits, grouping);
            assert!(
                a.call("keep", &[Value::Int(1_000)], &mut Host).is_ok(),
                "{context}"
            );
            let second = b.call("keep", &[Value::Int(1_000)], &mut Host);
            if grouping == Grouping::Shared {
                let error = second.expect_err(&context);
                assert!(
                    error.is_fatal && error.message.contains("Memory limit"),
                    "{context}"
                );
                // Releasing A's value brings the group back under budget.
                a.call("drop_it", &[], &mut Host).unwrap();
                assert!(
                    b.call("keep", &[Value::Int(1_000)], &mut Host).is_ok(),
                    "{context}"
                );
            } else {
                assert!(second.is_ok(), "{context}");
            }
        }
    }
}

#[test]
fn a_value_handed_between_grouped_instances_is_charged_once_without_a_copy() {
    // 100 pushes leave a backing with spare capacity; copying it on the
    // first mutation would allocate an exact-size backing and then grow it.
    let make = "pub fn make() { let items = []\n repeat 100 { items.push(1) }\n return items }";
    let take = "let kept = none\npub fn take() { let v = ask()\n v.push(0)\n kept = v\n return 1 }";
    let alone = "let kept = none\nfn make() { let items = []\n repeat 100 { items.push(1) }\n return items }\npub fn take() { let v = make()\n v.push(0)\n kept = v\n return 1 }";
    let limits = NyblLimits::standard();
    for (ea, eb) in ENGINE_PAIRS {
        let context = format!("{ea:?}/{eb:?}");
        let oracle_group = nybl::InstanceGroup::new(&limits);
        let mut oracle = Script::load(ea, alone, &mut Host, &limits, Some(&oracle_group)).unwrap();
        oracle.call("take", &[], &mut Host).unwrap();
        let expected = oracle_group.__memory().__used();

        let group = nybl::InstanceGroup::new(&limits);
        let mut a = Script::load(ea, take, &mut Host, &limits, Some(&group)).unwrap();
        let b = Script::load(eb, make, &mut Host, &limits, Some(&group)).unwrap();
        let mut host = AskHost::new(b, "make", Vec::new());
        a.call("take", &[], &mut host).unwrap();
        assert_eq!(group.__memory().__used(), expected, "{context}");
    }
}

#[test]
fn grouped_instances_run_under_the_group_disabled_builtins() {
    let group = nybl::InstanceGroup::new(&NyblLimits::standard().with_disabled_builtins(["rand"]));
    for engine in [Engine::Walker, Engine::Vm] {
        let error = Script::load(
            engine,
            "pub fn roll() { return rand(6) }",
            &mut Host,
            &NyblLimits::standard(),
            Some(&group),
        )
        .err()
        .expect("the group's deny list applies");
        assert_eq!(
            error.message, "builtin `rand` is disabled by the host",
            "{engine:?}"
        );
    }
    let program = nybl_vm::CompiledScript::compile("pub fn roll() { return rand(6) }").unwrap();
    assert!(NyblInstance::from_compiled_in(&program, &mut Host, &group).is_err());
}

/// Host for instance A whose `ask()` runs two operations in a row on B:
/// `first`, then `run`, both with `n`.
struct SiblingHost {
    b: Option<Script>,
    first: &'static str,
    n: i64,
    results: Vec<Result<(), NyblError>>,
}

impl NyblHost for SiblingHost {
    fn call(
        &mut self,
        name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        if name != "ask" {
            return None;
        }
        let mut b = self.b.take().expect("B is not already running");
        for entry in [self.first, "run"] {
            let result = b.call(entry, &[Value::Int(self.n)], &mut Host);
            self.results.push(result.map(|_| ()));
        }
        self.b = Some(b);
        Some(Ok(Value::Int(0)))
    }
}

/// `run` and `fail` recurse `n` frames and call a host hook (`print`) from
/// the deepest one; `fail` then raises a catchable error there. An error
/// unwinds without running another statement, so after `fail` the deepest
/// counts are the last ones B published.
const RECURSE_PRINT: &str = "fn down(n, fail) { if n > 0 { return down(n - 1, fail) }\n print(\"bottom\")\n if fail { panic(\"bottom\") }\n return 0 }\npub fn run(n) { return down(n, false) }\npub fn fail(n) { return down(n, true) }";

#[test]
fn sibling_nested_operations_from_one_hook_each_start_from_the_hook_caller() {
    on_large_stack(sibling_nested_scenarios);
}

fn sibling_nested_scenarios() {
    // Each B operation alone fits the budget from A's shallow frame; two
    // stacked would not (see the nesting and call-depth scenarios above).
    // The nesting budget, then the call-depth cap.
    let cases = [
        (NyblLimits::standard().with_max_nesting(200), 15),
        (NyblLimits::standard(), 40),
    ];
    for (limits, n) in &cases {
        for engines in ENGINE_PAIRS {
            for first in ["run", "fail"] {
                let context = format!("{engines:?} n={n} first={first}");
                let (mut a, b) = load_pair(
                    engines,
                    ("pub fn run() { return ask() }", RECURSE_PRINT),
                    limits,
                    Grouping::Shared,
                );
                let mut host = SiblingHost {
                    b: Some(b),
                    first,
                    n: *n,
                    results: Vec::new(),
                };
                assert_eq!(
                    a.call("run", &[], &mut host).unwrap().inspect(),
                    "0",
                    "{context}"
                );
                let [first_result, second] = &host.results[..] else {
                    panic!("{context}: two nested operations ran")
                };
                if first == "run" {
                    assert!(first_result.is_ok(), "{context}: {first_result:?}");
                } else {
                    let error = first_result.as_ref().expect_err(&context);
                    assert_eq!(error.message, "bottom", "{context}");
                }
                assert!(second.is_ok(), "{context}: {second:?}");
            }
        }
    }
}

#[test]
fn a_nested_call_from_a_module_top_level_continues_the_group_step_count() {
    let limits = NyblLimits {
        max_steps: 1_000,
        ..NyblLimits::standard()
    };
    for (ea, eb) in ENGINE_PAIRS {
        let context = format!("{ea:?}/{eb:?}");
        // About 100 units before the import and 300 in the module's top
        // level leave too few for B's 600.
        let (mut a, b) = load_pair(
            (ea, eb),
            (
                "pub fn run(before) { repeat before { }\n use m\n return 1 }",
                SPEND,
            ),
            &limits,
            Grouping::Shared,
        );
        let mut host = AskHost::new(b, "spend", vec![eb.iterations(600)]);
        host.swallow = true;
        host.modules.insert(
            "m".to_string(),
            format!("repeat {} {{ }}\nask()", ea.iterations(300).inspect()),
        );
        let result = a.call("run", &[ea.iterations(100)], &mut host);
        let b_error = host.b_result().as_ref().expect_err(&context);
        assert!(is_step_error(b_error), "{context}: {b_error:?}");
        assert!(result.is_err(), "{context}");
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Hook {
    Call,
    Tick,
    Print,
    Method,
    Resolve,
}

/// Host for instance A that runs `spend(units)` on B once, from `hook`.
struct HookHost {
    b: Option<Script>,
    hook: Hook,
    units: Value,
    /// `arm()` was called; the next tick runs B.
    armed: bool,
    results: Vec<Result<(), NyblError>>,
}

impl HookHost {
    /// Run B from `hook` when it is this host's hook. B's error is recorded
    /// and not handed to A.
    fn nest(&mut self, hook: Hook) {
        if hook == self.hook {
            let mut b = self.b.take().expect("B is not already running");
            let result = b.call("spend", std::slice::from_ref(&self.units), &mut Host);
            self.b = Some(b);
            self.results.push(result.map(|_| ()));
        }
    }
}

impl NyblHost for HookHost {
    fn call(
        &mut self,
        name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        match name {
            "ask" => self.nest(Hook::Call),
            "arm" => self.armed = true,
            _ => return None,
        }
        Some(Ok(Value::None))
    }

    fn on_tick(&mut self) -> Result<(), NyblError> {
        if core::mem::take(&mut self.armed) {
            self.nest(Hook::Tick);
        }
        Ok(())
    }

    fn on_print(&mut self, _message: &str) {
        self.nest(Hook::Print);
    }

    fn call_method(
        &mut self,
        _receiver: &nybl::HostValue,
        method: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        (method == "ask").then(|| {
            self.nest(Hook::Method);
            Ok(Value::None)
        })
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        (name == "m").then(|| {
            self.nest(Hook::Resolve);
            Ok("let loaded = 1".to_string())
        })
    }
}

#[test]
fn every_host_hook_continues_the_group_step_count() {
    let limits = NyblLimits {
        max_steps: 1_000,
        ..NyblLimits::standard()
    };
    let hooks = [
        (Hook::Call, "ask()"),
        (Hook::Tick, "arm()"),
        (Hook::Print, "print(\"x\")"),
        (Hook::Method, "h.ask()"),
        (Hook::Resolve, "use m"),
    ];
    for (hook, trigger) in hooks {
        let source = format!(
            "pub fn run(h, before, after) {{ repeat before {{ }}\n {trigger}\n repeat after {{ }}\n return 1 }}"
        );
        for (ea, eb) in ENGINE_PAIRS {
            for b_units in [400, 700] {
                let context = format!("{hook:?} {ea:?}/{eb:?} B {b_units}");
                let (mut a, b) = load_pair((ea, eb), (&source, SPEND), &limits, Grouping::Shared);
                let mut host = HookHost {
                    b: Some(b),
                    hook,
                    units: eb.iterations(b_units),
                    armed: false,
                    results: Vec::new(),
                };
                let args = [
                    Value::new_host("handle", ()),
                    ea.iterations(400),
                    ea.iterations(400),
                ];
                let result = a.call("run", &args, &mut host);
                let [b_result] = &host.results[..] else {
                    panic!("{context}: B ran once")
                };
                if b_units == 400 {
                    // About 400 + 400 fit; A resumes from B's count, so its
                    // last 400 do not.
                    assert!(b_result.is_ok(), "{context}: {b_result:?}");
                } else {
                    // About 400 + 700 do not fit.
                    let error = b_result.as_ref().expect_err(&context);
                    assert!(is_step_error(error), "{context}: {error:?}");
                }
                let a_error = result.expect_err(&context);
                assert!(is_step_error(&a_error), "{context}: {a_error:?}");
            }
        }
    }
}

/// Host whose `on_tick` fails on its `fail_on`-th call with `line`.
struct FailingTickHost {
    ticks: u64,
    fail_on: u64,
    line: u32,
    modules: BTreeMap<String, String>,
}

impl FailingTickHost {
    fn new(fail_on: u64, line: u32) -> Self {
        Self {
            ticks: 0,
            fail_on,
            line,
            modules: BTreeMap::new(),
        }
    }
}

impl NyblHost for FailingTickHost {
    fn call(
        &mut self,
        _name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        None
    }

    fn on_tick(&mut self) -> Result<(), NyblError> {
        self.ticks += 1;
        if self.ticks == self.fail_on {
            Err(NyblError::fatal("host stop", self.line))
        } else {
            Ok(())
        }
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        self.modules.get(name).cloned().map(Ok)
    }
}

/// Load `source` with an inert tick hook, then call `run` with the hook
/// armed to fail on its `host.fail_on`-th tick.
fn run_with_tick_host(
    engine: Engine,
    source: &str,
    host: &mut FailingTickHost,
    limits: &NyblLimits,
) -> NyblError {
    let fail_on = core::mem::replace(&mut host.fail_on, u64::MAX);
    let mut instance = Script::load(engine, source, host, limits, None).unwrap();
    host.ticks = 0;
    host.fail_on = fail_on;
    instance.call("run", &[], host).unwrap_err()
}

const NESTED_LOOPS: &str = "fn inner(n) {
  let total = 0
  repeat n {
    total += 1
  }
  return total
}
pub fn run() {
  let count = 0
  while true {
    count += inner(5)
  }
}";

#[test]
fn line_less_tick_hook_error_reports_the_outermost_active_loop_header() {
    for engine in [Engine::Walker, Engine::Vm] {
        for fail_on in [3, 40, 41, 200] {
            let mut host = FailingTickHost::new(fail_on, 0);
            let error =
                run_with_tick_host(engine, NESTED_LOOPS, &mut host, &NyblLimits::standard());
            assert_eq!(error.message, "host stop");
            // `while true` is on line 10.
            assert_eq!(error.line, Some(10), "{engine:?} tick {fail_on}");
            assert_eq!(error.column, None);
        }
    }
}

#[test]
fn line_less_tick_hook_error_without_a_loop_reports_the_current_statement() {
    let source = "pub fn run() {\n  let a = 1\n  let b = a + 1\n  let c = b + 1\n  return c\n}";
    for engine in [Engine::Walker, Engine::Vm] {
        let mut host = FailingTickHost::new(2, 0);
        let error = run_with_tick_host(engine, source, &mut host, &NyblLimits::standard());
        assert_eq!(error.message, "host stop");
        let line = error.line.expect("a line is reported");
        assert!((2..=5).contains(&line), "{engine:?}: line {line}");
    }
}

#[test]
fn tick_hook_error_with_an_explicit_line_is_unchanged() {
    for engine in [Engine::Walker, Engine::Vm] {
        let mut host = FailingTickHost::new(40, 77);
        let error = run_with_tick_host(engine, NESTED_LOOPS, &mut host, &NyblLimits::standard());
        assert_eq!(error.line, Some(77), "{engine:?}");
    }
}

#[test]
fn tick_hook_error_in_a_module_loop_has_the_step_limit_error_owner_and_line() {
    let root = "use lib\npub fn run() {\n  return spin()\n}";
    let lib = "let ready = true\nfn spin() {\n  let n = 0\n  while ready {\n    n += 1\n  }\n}";
    for engine in [Engine::Walker, Engine::Vm] {
        let mut builtin_host = FailingTickHost::new(u64::MAX, 0);
        builtin_host.modules.insert("lib".into(), lib.into());
        let limits = NyblLimits {
            max_steps: 500,
            ..NyblLimits::standard()
        };
        let builtin = run_with_tick_host(engine, root, &mut builtin_host, &limits);
        assert!(is_step_error(&builtin), "{engine:?}: {builtin:?}");

        let mut hook_host = FailingTickHost::new(100, 0);
        hook_host.modules.insert("lib".into(), lib.into());
        let hooked = run_with_tick_host(engine, root, &mut hook_host, &NyblLimits::standard());
        assert_eq!(hooked.message, "host stop");
        let owner = |error: &NyblError| {
            error
                .source_context
                .as_ref()
                .map(|context| context.module_path.clone())
        };
        assert_eq!(owner(&builtin).as_deref(), Some("lib"), "{engine:?}");
        assert_eq!(owner(&hooked), owner(&builtin), "{engine:?}");
        // `while ready` is on line 4 of the module.
        assert_eq!(builtin.line, Some(4), "{engine:?}");
        assert_eq!(hooked.line, builtin.line, "{engine:?}");
    }
}

// ─── Host-built struct shapes ─────────────────────────────────────

const POINT_SCRIPT: &str = "struct Point { x, y }
pub fn bump(p) { p.x = p.x + 10\n return p }
pub fn same(p) { return p == Point { x: 1, y: 2 } }
pub fn is_point(p) { let r = match p { Point { x, y } => true, _ => false }\n return r }";

fn point(module_path: &str, x: i64, y: i64) -> Value {
    let shape = StructShape::new(module_path, "Point", ["x", "y"]);
    Value::try_new_struct_from_shape(&shape, vec![Value::Int(x), Value::Int(y)], 0).unwrap()
}

#[test]
fn host_built_struct_values_are_read_written_and_returned_by_scripts() {
    let original = point(nybl::value::ROOT_MODULE_PATH, 1, 2);
    for engine in [Engine::Walker, Engine::Vm] {
        let mut script = Script::load(
            engine,
            POINT_SCRIPT,
            &mut Host,
            &NyblLimits::standard(),
            None,
        )
        .unwrap();
        let returned = script
            .call("bump", std::slice::from_ref(&original), &mut Host)
            .unwrap();
        let Value::Struct(returned) = returned else {
            panic!("{engine:?}: expected a struct, got {returned:?}")
        };
        assert!(
            matches!(returned.values(), [Value::Int(11), Value::Int(2)]),
            "{engine:?}"
        );
        assert!(
            matches!(returned.field("x"), Some(Value::Int(11))),
            "{engine:?}"
        );
        assert!(
            matches!(returned.field("y"), Some(Value::Int(2))),
            "{engine:?}"
        );
        // The script's write detached its copy; the host's value is untouched.
        let Value::Struct(original) = &original else {
            unreachable!()
        };
        assert!(
            matches!(original.values(), [Value::Int(1), Value::Int(2)]),
            "{engine:?}"
        );
    }
}

#[test]
fn host_built_struct_identity_is_module_path_and_type_name() {
    let root = point(nybl::value::ROOT_MODULE_PATH, 1, 2);
    let elsewhere = point("elsewhere", 1, 2);
    for engine in [Engine::Walker, Engine::Vm] {
        let mut script = Script::load(
            engine,
            POINT_SCRIPT,
            &mut Host,
            &NyblLimits::standard(),
            None,
        )
        .unwrap();
        let mut ask = |entry: &str, value: &Value| {
            script
                .call(entry, std::slice::from_ref(value), &mut Host)
                .unwrap()
                .inspect()
        };
        assert_eq!(ask("same", &root), "true", "{engine:?}");
        assert_eq!(ask("is_point", &root), "true", "{engine:?}");
        assert_eq!(ask("same", &elsewhere), "false", "{engine:?}");
        assert_eq!(ask("is_point", &elsewhere), "false", "{engine:?}");
    }
}

#[test]
fn script_struct_values_charge_for_their_values_not_their_names() {
    let make = |type_name: &str, first: &str, second: &str| {
        format!(
            "struct {type_name} {{ {first}, {second} }}\nlet kept = []\npub fn make(n) {{ repeat n {{ kept.push({type_name} {{ {first}: 1, {second}: 2 }}) }}\n return kept.len() }}"
        )
    };
    let short = make("P", "a", "b");
    let long = make(
        "PointWithAVeryLongDescriptiveTypeName",
        "first_field_with_a_long_descriptive_name",
        "second_field_with_a_long_descriptive_name",
    );
    let limits = NyblLimits::standard();
    for engine in [Engine::Walker, Engine::Vm] {
        let retained = |source: &str| {
            let group = nybl::InstanceGroup::new(&limits);
            let mut script =
                Script::load(engine, source, &mut Host, &limits, Some(&group)).unwrap();
            let before = group.__memory().__used();
            script.call("make", &[Value::Int(50)], &mut Host).unwrap();
            group.__memory().__used() - before
        };
        let short_bytes = retained(&short);
        assert!(short_bytes > 0, "{engine:?}");
        assert_eq!(retained(&long), short_bytes, "{engine:?}");
    }
}

const SHAPE_METHODS_SCRIPT: &str = "struct Point { x, y }
fn Point.sum(self) { return self.x + self.y }
fn Point.scale(ref self, k) { self.x = self.x * k\n self.y = self.y * k }
pub fn sum(p) { return p.sum() }
pub fn scaled(p) { p.scale(3)\n return [p.sum(), p] }";

#[test]
fn host_built_struct_values_dispatch_script_methods() {
    let original = point(nybl::value::ROOT_MODULE_PATH, 1, 2);
    for engine in [Engine::Walker, Engine::Vm] {
        let mut script = Script::load(
            engine,
            SHAPE_METHODS_SCRIPT,
            &mut Host,
            &NyblLimits::standard(),
            None,
        )
        .unwrap();
        let mut ask = |entry: &str| {
            script
                .call(entry, std::slice::from_ref(&original), &mut Host)
                .unwrap()
                .inspect()
        };
        assert_eq!(ask("sum"), "3", "{engine:?}");
        assert_eq!(ask("scaled"), "[9, Point { x: 3, y: 6 }]", "{engine:?}");
    }
}

#[test]
fn caught_runtime_errors_of_one_instance_share_one_shape() {
    let source = "fn fail() { panic(\"stop\") }
pub fn caught() { let r = try_call(fail)
 let e = match r { Result::Err(e) => e, _ => none }
 return e }";
    for engine in [Engine::Walker, Engine::Vm] {
        let mut script =
            Script::load(engine, source, &mut Host, &NyblLimits::standard(), None).unwrap();
        let first = script.call("caught", &[], &mut Host).unwrap();
        let second = script.call("caught", &[], &mut Host).unwrap();
        let (Value::Struct(first), Value::Struct(second)) = (&first, &second) else {
            panic!("{engine:?}: expected two RuntimeError structs, got {first:?} and {second:?}")
        };
        assert_eq!(first.shape().type_name(), "RuntimeError", "{engine:?}");
        assert!(
            matches!(first.field("message"), Some(Value::Str(_))),
            "{engine:?}"
        );
        // One shared shape means one field-name allocation for both values.
        assert!(
            std::ptr::eq(first.shape().field_names(), second.shape().field_names()),
            "{engine:?}"
        );
    }
}
