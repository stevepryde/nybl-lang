use std::collections::BTreeMap;

use nybl::{NyblError, NyblHost, NyblLimits, Value};
use nybl_vm::NyblInstance;

#[derive(Default)]
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
fn rest_parameters_work_for_named_functions_lambdas_methods_and_instances() {
    let source = r#"
struct Box { value }
fn Box.join(self, prefix, ..items) { return [self.value, prefix, items] }
struct Counter { value }
fn Counter.add(ref self, ..items) { self.value += items.len(); return items }
fn collect(first, ..items) { return [first, items] }
pub fn named() { return collect(1, 2, 3) }
pub fn lambda() {
    let collect_all = fn(..items) { return items }
    return collect_all("a", "b")
}
pub fn method() {
    let box = Box { value: 7 }
    return box.join("x", 8, 9)
}
pub fn ref_method() {
    let counter = Counter { value: 10 }
    let items = counter.add(1, 2, 3)
    return [counter.value, items]
}
pub fn variadic(first, ..items) { return [first, items] }
"#;
    let mut host = ModuleHost::default();
    let mut instance = NyblInstance::load(source, &mut host, &NyblLimits::standard()).unwrap();

    let variadic = instance
        .entry_points()
        .iter()
        .find(|entry| entry.name() == "variadic")
        .unwrap();
    assert_eq!(variadic.arity(), 1);
    assert!(variadic.is_variadic());
    assert!(variadic.accepts_arity(3));

    assert_eq!(
        instance.call("named", &[], &mut host).unwrap().inspect(),
        "[1, [2, 3]]"
    );
    assert_eq!(
        instance.call("lambda", &[], &mut host).unwrap().inspect(),
        "[\"a\", \"b\"]"
    );
    assert_eq!(
        instance.call("method", &[], &mut host).unwrap().inspect(),
        "[7, \"x\", [8, 9]]"
    );
    assert_eq!(
        instance
            .call("ref_method", &[], &mut host)
            .unwrap()
            .inspect(),
        "[13, [1, 2, 3]]"
    );
    assert_eq!(
        instance
            .call(
                "variadic",
                &[Value::Int(1), Value::Int(2), Value::Int(3)],
                &mut host,
            )
            .unwrap()
            .inspect(),
        "[1, [2, 3]]"
    );
    let too_few = instance.call("variadic", &[], &mut host).unwrap_err();
    assert!(too_few.message.contains("at least 1 argument"));
}

#[test]
fn rest_extras_are_value_only_and_preflight_before_evaluation() {
    let source = r#"
let calls = 0
fn tick() { calls += 1; return calls }
fn collect(..items) { return items }
pub fn attempt() {
    let target = 0
    return collect(tick(), ref target)
}
pub fn calls() { return calls }
"#;
    let mut host = ModuleHost::default();
    let mut instance = NyblInstance::load(source, &mut host, &NyblLimits::standard()).unwrap();
    let error = instance.call("attempt", &[], &mut host).unwrap_err();
    assert!(error.message.contains("argument 2"));
    assert_eq!(
        instance.call("calls", &[], &mut host).unwrap().inspect(),
        "0"
    );
}

#[test]
fn explicit_surfaces_filter_all_import_forms_and_can_reexport() {
    let mut modules = BTreeMap::new();
    modules.insert(
        "leaf".to_string(),
        r#"
let visible = 1
let hidden = 2
let _shown = 3
struct Visible { value }
struct Hidden { value }
fn read_hidden() { return hidden }
fn gather(..items) { return items }
pub { visible, _shown, read_hidden, gather, Visible }
"#
        .to_string(),
    );
    modules.insert(
        "facade".to_string(),
        "use leaf.{visible, read_hidden, gather}\npub { visible, read_hidden, gather }".to_string(),
    );
    let source = r#"
use leaf as leaf
use leaf
use facade as facade
pub fn read() {
    return [leaf.visible, visible, _shown, leaf.read_hidden(), facade.visible, facade.read_hidden()]
}
pub fn make_types() { return [leaf.Visible { value: 4 }, Visible { value: 5 }] }
pub fn hidden_alias() { return leaf.hidden }
pub fn hidden_glob() { return hidden }
pub fn gathered() { return facade.gather(6, 7) }
"#;
    let mut host = ModuleHost { modules };
    let mut instance = NyblInstance::load(source, &mut host, &NyblLimits::standard()).unwrap();
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "[1, 1, 3, 2, 1, 2]"
    );
    assert_eq!(
        instance
            .call("make_types", &[], &mut host)
            .unwrap()
            .inspect(),
        "[Visible { value: 4 }, Visible { value: 5 }]"
    );
    assert!(instance.call("hidden_alias", &[], &mut host).is_err());
    assert!(instance.call("hidden_glob", &[], &mut host).is_err());
    assert_eq!(
        instance.call("gathered", &[], &mut host).unwrap().inspect(),
        "[6, 7]"
    );

    let denied = match NyblInstance::load(
        "use leaf.{hidden}\npub fn read() { return hidden }",
        &mut host,
        &NyblLimits::standard(),
    ) {
        Ok(_) => panic!("private explicit-surface name was imported"),
        Err(error) => error,
    };
    assert!(denied.message.contains("isn't exported"));

    let denied_type = match NyblInstance::load(
        "use leaf.{Hidden}\npub fn make() { return Hidden { value: 1 } }",
        &mut host,
        &NyblLimits::standard(),
    ) {
        Ok(_) => panic!("private explicit-surface type was imported"),
        Err(error) => error,
    };
    assert!(denied_type.message.contains("isn't exported"));
}

#[test]
fn modules_without_a_surface_keep_legacy_private_access() {
    let mut modules = BTreeMap::new();
    modules.insert("legacy".to_string(), "let _private = 9".to_string());
    let mut host = ModuleHost { modules };
    let mut instance = NyblInstance::load(
        "use legacy.{_private}\npub fn read() { return _private }",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "9"
    );
}

#[test]
fn compiler_and_disassembler_retain_surface_and_rest_metadata() {
    let ast = nybl::parse("pub { gather }\nfn gather(first, ..items) { return items }").unwrap();
    let chunk = nybl_vm::compile(&ast).unwrap();
    assert_eq!(
        chunk.public_surface.as_deref(),
        Some(["gather".to_string()].as_slice())
    );
    assert_eq!(
        chunk.functions[0].param_modes,
        vec![
            nybl::parser::ParamMode::Value,
            nybl::parser::ParamMode::Rest
        ]
    );
    let rendered = nybl_vm::disassemble(&chunk);
    assert!(rendered.contains("pub {gather}"));
    assert!(rendered.contains("gather(first, ..items)"));
}
