//! Host-disabled builtins at the AOT boundary: transpile time is this
//! engine's earliest detectable point, so definite references refuse to
//! transpile; unprovable references compile into the same fatal error
//! the walker and VM raise at their runtime dispatch sites.

use std::collections::BTreeSet;

use nybl_compile::{Options, modules_from_map, transpile};

fn library_options() -> Options {
    Options {
        emit_main: false,
        use_nybl_sys: false,
        disabled_builtins: BTreeSet::from(["rand".to_string()]),
        ..Options::default()
    }
}

#[test]
fn transpile_refuses_a_direct_disabled_builtin_call() {
    let error = transpile("let x = rand(5)", &library_options())
        .expect_err("a definite `rand` reference must refuse to transpile");
    assert_eq!(error.message, "builtin `rand` is disabled by the host");
    assert!(error.is_fatal, "disabled builtins are uncatchable");
}

#[test]
fn transpile_refuses_a_disabled_builtin_inside_an_imported_module() {
    let error = transpile(
        "use helper.{value}\nprint(value)",
        &Options {
            module_resolver: Some(modules_from_map([("helper", "let value = rand(3)")])),
            ..library_options()
        },
    )
    .expect_err("module builtins are checked at transpile time");
    assert_eq!(error.message, "builtin `rand` is disabled by the host");
}

#[test]
fn function_declaration_shadows_a_disabled_builtin() {
    transpile(
        "fn rand(n) { return n }\nlet value = rand(3)",
        &library_options(),
    )
    .expect("the lexical function, not the disabled builtin, is called");
}

#[test]
fn call_before_shadowing_function_keeps_the_runtime_backstop() {
    let source = transpile(
        "let value = rand(3)\nfn rand(n) { return n }",
        &library_options(),
    )
    .expect("source-order ambiguity is checked at runtime");
    assert!(source.contains("disabled_builtin_error"));
}

#[test]
fn unprovable_references_compile_into_the_fatal_backstop_error() {
    // The glob import could bind `rand`, so the static pass cannot
    // refuse the program; the call site must compile into the fatal
    // disabled-builtin error instead of a builtin invocation.
    let source = transpile(
        "use helper\nlet x = rand(3)",
        &Options {
            module_resolver: Some(modules_from_map([("helper", "let unrelated = 1")])),
            ..library_options()
        },
    )
    .expect("an unprovable reference must still transpile");
    assert!(
        source.contains("disabled_builtin_error"),
        "generated code must carry the backstop error"
    );
    assert!(
        !source.contains("builtin_rand("),
        "generated code must not invoke the disabled builtin"
    );
}
