//! Snapshot-style tests for the transpiler.
//!
//! These assert on fragments of the emitted Rust rather than the
//! full file. Exact-match snapshots would churn every time we
//! tweak formatting, and what really matters is that the emitted
//! code has the right shape — correct op dispatch, correct fn
//! signatures, correct helper calls.
//!
//! End-to-end compilation (emitted code must actually compile and
//! produce the tree-walker's output) lives in `tests/e2e.rs` behind
//! `#[ignore]`.

use nybl_compile::{Options, modules_from_map, transpile};

fn compile(code: &str) -> String {
    transpile(code, &Options::default()).expect("transpile")
}

#[test]
fn inconsistent_or_pattern_is_rejected_before_aot_emission() {
    let error = transpile(
        "let value = match 1 { 1 | y => y, _ => 0 }",
        &Options::default(),
    )
    .expect_err("invalid pattern must fail before Rust emission");

    assert_eq!(
        error.message,
        "`or`-pattern alternative 2 binds `y`, but alternative 1 binds no names"
    );
    assert_eq!(error.line, Some(1));
    assert!(error.friendly_hint.is_some());
}

#[test]
fn targeted_parser_diagnostics_are_preserved_at_the_aot_boundary() {
    let cases = [
        (
            "let label = match 1 {\n  1 => { print(\"one\") },\n  _ => \"other\",\n}",
            "`{ ... }` after `=>` is a dictionary expression, not a match-arm block",
            2,
            8,
            "`match` arm bodies must be a single expression; put it directly after `=>`, or quote dictionary keys if you meant to return a dictionary.",
        ),
        (
            "for i in 0..3 {}",
            "`..` range syntax is not supported in expressions",
            1,
            11,
            "use `range(start, end)` instead, for example `range(0, 3)`.",
        ),
        (
            "const Y = 2\nmatch 3 { Y => 0 }",
            "`match` pattern binding `Y` looks like a constant, but a value name is required here",
            2,
            11,
            "uppercase names can't be pattern bindings. To match against the value of `Y`, bind a lowercase name and compare it in a guard: `n if n == Y => ...`",
        ),
    ];

    for (source, message, line, column, hint) in cases {
        let error =
            transpile(source, &Options::default()).expect_err("parse error must stop AOT emission");
        assert_eq!(error.message, message, "source: {source}");
        assert_eq!(error.line, Some(line), "source: {source}");
        assert_eq!(error.column, Some(column), "source: {source}");
        assert_eq!(
            error.friendly_hint.as_deref(),
            Some(hint),
            "source: {source}"
        );
    }
}

#[test]
fn top_level_try_uses_the_shared_diagnostic_constructor() {
    let source = r#"enum Result { Ok(value), Err(error) }
let value = try Result::Err("boom")"#;
    for sandbox in [false, true] {
        let rust = transpile(
            source,
            &Options {
                sandbox,
                ..Options::default()
            },
        )
        .expect("transpile");
        assert!(rust.contains("::nybl::error_messages::top_level_try_error(2)"));
        assert!(!rust.contains("NyblError::runtime(\"try encountered Err at top-level\""));
        assert!(rust.contains("if let Some(hint) = &err.friendly_hint"));
        assert!(rust.contains("eprintln!(\"hint: {}\", hint)"));
    }
}

#[test]
fn i64_min_literal_emits_exact_integer_values_and_patterns() {
    let rust = compile(
        r#"let min = -9223372036854775808
print(match min {
    -9223372036854775808 => min,
    _ => 0,
})"#,
    );
    assert!(
        rust.contains("::nybl::value::Value::Int(-9223372036854775808i64)"),
        "generated Rust lost the exact expression value"
    );
    assert!(
        rust.contains("::nybl::parser::LiteralPattern::Int(-9223372036854775808i64)"),
        "generated Rust lost the exact pattern value"
    );
    assert!(!rust.contains("Value::Number(-9223372036854775808"));
}

#[test]
fn multiline_if_expression_still_rejects_multiple_branch_values_before_aot_emission() {
    let error = transpile(
        "let value = if true {\n    1\n    2\n} else {\n    3\n}",
        &Options::default(),
    )
    .expect_err("a second branch expression must fail before Rust emission");
    assert_eq!(error.message, "Expected `}` but found `an integer`");
    assert_eq!(error.line, Some(3));
    assert_eq!(error.column, Some(5));
}

fn contains_all(haystack: &str, needles: &[&str]) {
    for n in needles {
        assert!(
            haystack.contains(n),
            "expected fragment not found: {n:?}\n---\n{haystack}\n---"
        );
    }
}

#[test]
fn empty_program_still_produces_runnable_shell() {
    let out = compile("");
    contains_all(
        &out,
        &[
            "fn run_program(ctx: &mut Ctx<'_>)",
            "pub fn run<H: ::nybl::NyblHost>",
            "fn main()",
            "::nybl_sys::StandardHost::new()",
        ],
    );
}

#[test]
fn print_42_emits_on_print_call() {
    let out = compile("print(42)");
    // The body must preflight bounded formatting and then use the fallible
    // host-print helper so generated programs propagate output failures.
    // `42` is an int literal in phase 6, so the emitted value is
    // `Value::Int`.
    contains_all(
        &out,
        &[
            "::nybl::formatting::__format_values_in(",
            "__nybl_host_print(ctx, &__nybl_message",
            "::nybl::value::Value::Int(42",
        ],
    );
}

#[test]
fn match_block_arm_values_are_parenthesized_for_rustc() {
    let out = compile(
        r#"let flag = true
let value = 40
let unguarded = match flag { true => value + 1, _ => 0 }
let guarded = match flag { true if value > 0 => value + 2, _ => 0 }
print(unguarded, guarded)"#,
    );

    let block_arm_breaks = out
        .lines()
        .filter(|line| line.contains("break 'match_arms_") && line.contains(" ({ let "))
        .count();
    assert_eq!(
        block_arm_breaks, 2,
        "guarded and unguarded block-expression arms must parenthesize the break value:\n{out}"
    );
    assert!(
        !out.lines()
            .any(|line| line.contains("break 'match_arms_") && line.contains(" { let ")),
        "a bare block after a labelled break triggers rustc's break_with_label_and_loop lint:\n{out}"
    );
}

#[test]
fn module_let_emits_authoritative_context_binding() {
    let out = compile("let x = 10");
    contains_all(
        &out,
        &[
            "let __t0 = ::nybl::value::Value::Int(10",
            "__nybl_define_binding(ctx, \"<root>\", \"x\", __t0)",
        ],
    );
}

#[test]
fn non_sandbox_lifted_fn_reads_its_root_module_binding() {
    let out = compile("let value = 10\nfn read() { return value }\nprint(read())");
    contains_all(
        &out,
        &[
            "fn __nybl_function_site_0(ctx: &mut Ctx<'_>)",
            "__nybl_read_binding(ctx, \"<root>\", \"value\", 2)?",
        ],
    );
    assert!(!out.contains("__nybl_user_value_76616c7565.clone()"));
}

#[test]
fn non_sandbox_lifted_fns_resolve_bare_imported_calls_from_context() {
    let out = compile_with_modules(
        "use inner\nfn call() { return increment(10) }\nprint(call())",
        &[("inner", "fn increment(n) { return n + 1 }")],
    )
    .expect("transpile bare imported call from a lifted function");
    contains_all(
        &out,
        &[
            "__nybl_import_binding_value(ctx, \"<root>\", \"increment\"",
            "__nybl_binding_value(ctx, \"<root>\", \"increment\")",
            "__nybl_call_named_value(ctx, __callee",
        ],
    );
}

#[test]
fn non_sandbox_named_functions_claim_first_win_before_flat_imports() {
    let out = compile_with_modules(
        "fn pick() { return 11 }\nuse b\nfn read() { return pick() }\nprint(read())",
        &[("b", "fn pick() { return 22 }")],
    )
    .expect("transpile first-win named function");
    assert!(
        out.contains("__nybl_activate_function(ctx, 0);"),
        "root function activation missing:\n{out}"
    );
    assert!(
        !out.contains("__nybl_import_binding_value(ctx, \"<root>\", \"pick\""),
        "a statically losing flat import should not become binding storage:\n{out}"
    );
    contains_all(
        &out,
        &[
            "__nybl_has_binding(ctx, module_path, name)",
            "__nybl_binding_value(ctx, \"<root>\", \"pick\")",
            "ctx.active_function_sites",
        ],
    );
}

#[test]
fn flat_value_imports_emit_origin_aliases_through_facades() {
    let out = compile_with_modules(
        "use facade\nfn read() { return count }\nprint(read())",
        &[
            ("facade", "use leaf"),
            (
                "leaf",
                "let count = 0\nfn bump() { count += 1; return count }",
            ),
        ],
    )
    .expect("transpile live facade value");
    contains_all(
        &out,
        &[
            "__nybl_import_binding_alias(ctx, \"facade\", \"count\", \"leaf\", \"count\")",
            "__nybl_import_binding_alias(ctx, \"<root>\", \"count\", \"facade\", \"count\")",
            "__nybl_resolve_binding_key(ctx, module_path, name)",
            "__nybl_binding_mut_option(ctx, \"leaf\", \"count\")",
        ],
    );
}

#[test]
fn compound_assign_routes_through_ops() {
    let out = compile("let x = 1\nx += 5");
    contains_all(
        &out,
        &[
            "__nybl_binding_mut_option(ctx, \"<root>\", \"x\")",
            "::nybl::ops::add_in(&",
        ],
    );
}

#[test]
fn binary_ops_use_ops_module() {
    let programs = [
        ("print(1 + 2)", "::nybl::ops::add"),
        ("print(1 - 2)", "::nybl::ops::sub"),
        ("print(1 * 2)", "::nybl::ops::mul"),
        ("print(1 / 2)", "::nybl::ops::div"),
        ("print(1 % 2)", "::nybl::ops::rem"),
        ("print(1 < 2)", "::nybl::ops::lt"),
        ("print(1 > 2)", "::nybl::ops::gt"),
        ("print(1 <= 2)", "::nybl::ops::lt_eq"),
        ("print(1 >= 2)", "::nybl::ops::gt_eq"),
        ("print(1 == 2)", "::nybl::ops::eq"),
        ("print(1 != 2)", "::nybl::ops::not_eq"),
    ];
    for (src, op) in programs {
        let out = compile(src);
        assert!(
            out.contains(op),
            "expected {op} in output for `{src}`:\n{out}"
        );
    }
}

#[test]
fn short_circuit_and_uses_is_truthy() {
    let out = compile("print(true && false)");
    // Short-circuit `&&` should branch on the left's truthiness
    // without running the full `ops::and` path (there isn't one).
    contains_all(&out, &["if __l.is_truthy()", "::nybl::value::Value::Bool("]);
}

#[test]
fn if_else_uses_is_truthy() {
    let out = compile(r#"if true { print("y") } else { print("n") }"#);
    contains_all(&out, &["if (", ").is_truthy()", "else {"]);
}

#[test]
fn while_loop_tests_is_truthy_each_iteration() {
    let out = compile("let i = 0\nwhile i < 5 { i = i + 1 }");
    contains_all(&out, &["while (", ").is_truthy()"]);
}

#[test]
fn repeat_parses_count_and_iterates() {
    let out = compile("repeat 4 { let x = 1 }");
    contains_all(
        &out,
        &[
            "::nybl::value::Value::Number(n) => n as i64",
            "for _ in 0..",
        ],
    );
}

#[test]
fn for_in_uses_iter_protocol_helpers() {
    // The AOT now emits calls to `__nybl_iter_start` /
    // `__nybl_iter_step` so for-loops transparently handle
    // Array/Str/Dict (fast path), Value::Iter, and user types
    // with an `.iter()` method through one uniform loop body.
    let out = compile("for x in [1, 2, 3] { print(x) }");
    contains_all(&out, &["__nybl_iter_start(", "__nybl_iter_step(", "loop"]);
}

#[test]
fn fn_decl_emits_nybl_prefixed_fn() {
    let out = compile("fn double(x) { return x * 2 }\nprint(double(5))");
    contains_all(
        &out,
        &[
            "fn __nybl_function_site_0(ctx: &mut Ctx<'_>, __nybl_param_0: ::nybl::value::Value)",
            "let mut __nybl_user_value_78: ::nybl::value::Value = __nybl_param_0;",
            "Result<::nybl::value::Value, ::nybl::error::NyblError>",
            "__nybl_guarded_function_site_0(ctx,",
        ],
    );
}

#[test]
fn mixed_mode_function_sites_branch_on_active_site_before_call_dispatch() {
    let out =
        compile("fn f() { return 1 }\nif false { fn f(ref value) { value = 2 } }\nprint(f())");
    contains_all(
        &out,
        &[
            "match __nybl_active_ref_function_site(ctx, \"<root>\", \"f\")",
            "::std::option::Option::Some(__nybl_site)",
            "__nybl_function_site_value(ctx, __nybl_site, 3)?",
            "match ctx.host.call(\"f\", &[], 3)",
        ],
    );
}

#[test]
fn exact_value_self_preflights_before_argument_evaluation() {
    let out = compile(
        "fn side() { print(\"ARG\"); return 1 }\nfn f(value) { return f(side(), side()) }\nf(1)",
    );
    let body_start = out
        .find("fn __nybl_function_site_1(")
        .expect("second declaration body");
    let body_end = out[body_start..]
        .find("\n}\n")
        .map(|offset| body_start + offset)
        .expect("second declaration body end");
    let body = &out[body_start..body_end];
    assert!(
        body.contains(
            "return Err(::nybl::error::NyblError::runtime(format!(\"`f` expects 1 argument, but got 2\"), 2))"
        ),
        "wrong-arity exact calls must emit their arity error"
    );
    assert!(
        !body.contains("__nybl_guarded_function_site_0(ctx"),
        "wrong-arity exact calls must not evaluate argument expressions:\n{body}"
    );
}

#[test]
fn duplicate_parameters_rebind_in_order_without_duplicate_rust_arguments() {
    let out = compile("fn pick(value, value) { return value }");
    contains_all(
        &out,
        &[
            "__nybl_param_0: ::nybl::value::Value",
            "__nybl_param_1: ::nybl::value::Value",
            "let mut __nybl_user_value_76616c7565: ::nybl::value::Value = __nybl_param_0;",
            "let mut __nybl_user_value_76616c7565: ::nybl::value::Value = __nybl_param_1;",
        ],
    );
    assert!(!out.contains("mut __nybl_user_value_76616c7565: ::nybl::value::Value,"));
}

#[test]
fn duplicate_lambda_ref_positions_copy_out_the_canonical_binding() {
    let cases = [
        (
            "let callback = fn(ref value, value) { value += 1 }",
            vec!["__nybl_lambda_arg_0 = __nybl_user_value_76616c7565.clone();"],
        ),
        (
            "let callback = fn(value, ref value) { value += 1 }",
            vec!["__nybl_lambda_arg_1 = __nybl_user_value_76616c7565.clone();"],
        ),
        (
            "let callback = fn(ref value, ref value) { value += 1 }",
            vec![
                "__nybl_lambda_arg_0 = __nybl_user_value_76616c7565.clone();",
                "__nybl_lambda_arg_1 = __nybl_user_value_76616c7565.clone();",
            ],
        ),
    ];

    for (source, expected_syncs) in cases {
        for output in [compile(source), compile_sandbox(source)] {
            contains_all(&output, &expected_syncs);
        }
    }
}

#[test]
fn unknown_call_falls_back_to_host() {
    // `readline` isn't in the builtin list and isn't declared in
    // the program, so the emit should route through ctx.host.call
    // and raise a "not found" error on the None branch.
    //
    // Tech-debt-4: the emitted error text now flows through
    // `nybl::error_messages::function_not_found`, so the snapshot
    // looks for the call site rather than a raw `format!` string.
    let out = compile(r#"let s = readline("> ")"#);
    contains_all(
        &out,
        &[
            "ctx.host.call(\"readline\",",
            "::nybl::error_messages::function_not_found(\"readline\")",
        ],
    );
    // It should NOT try to call a nonexistent user-fn symbol.
    assert!(
        !out.contains("__nybl_user_fn_n726561646c696e65"),
        "unknown name should not emit a user-fn dispatch:\n{out}"
    );
}

#[test]
fn index_read_uses_ops_index_get() {
    let out = compile("let a = [1, 2]\nprint(a[0])");
    contains_all(&out, &["::nybl::ops::index_get_in(&__o, &__i,"]);
}

#[test]
fn range_snapshots_memory_before_mutably_borrowing_rng_state() {
    let out = compile("let values = range(3)");
    contains_all(
        &out,
        &[
            "let __nybl_memory = ctx.memory.clone();",
            "&mut ctx.rand_state, &__nybl_memory",
        ],
    );
    assert!(!out.contains("&mut ctx.rand_state, &ctx.memory"));
}

#[test]
fn array_and_dict_literals_use_fallible_line_aware_constructors() {
    let arr = compile("let a = [1, 2, 3]");
    contains_all(
        &arr,
        &[
            "::nybl::value::Value::__try_new_array_in(vec![",
            "], 1, &ctx.memory)?",
        ],
    );

    let dct = compile("\nlet d = {\"a\": 1, \"b\": 2}");
    contains_all(
        &dct,
        &[
            "::nybl::value::Value::__try_new_dict_in(vec![",
            "], 2, &ctx.memory)?",
        ],
    );
}

#[test]
fn composite_values_propagate_depth_errors_with_source_lines() {
    let out = compile(
        r#"struct Point { x }
enum Shape { Circle(r), Rect { w } }
let p = Point { x: [1] }
let c = Shape::Circle({"r": 2})
let r = Shape::Rect { w: [3] }"#,
    );
    contains_all(
        &out,
        &[
            "Value::__try_new_struct_in(",
            "], 3, &ctx.memory)?",
            "Value::__try_new_enum_tuple_in(",
            "], 4, &ctx.memory)?",
            "Value::__try_new_enum_struct_in(",
            "], 5, &ctx.memory)?",
        ],
    );
}

#[test]
fn lambda_records_opaque_capture_depth_before_move() {
    let out = compile("let captured = [1]\nlet f = fn() { return captured }");
    let depth = out
        .find("let __opaque_body_depth = 0u16.max(__cap_0.ownership_depth())")
        .expect("capture depth should be computed");
    let closure = out[depth..]
        .find("Rc::new(move")
        .map(|offset| depth + offset)
        .expect("capture should move into the callable");
    assert!(
        depth < closure,
        "capture depth must be computed before captures move into the opaque callable"
    );
    contains_all(
        &out,
        &[
            "__nybl_wrap_callable(",
            "__opaque_body_depth, 2, __callable)?",
            "NyblFn::try_new_compiled_in_module_with_origin_and_modes(",
            "param_modes,",
            "opaque_body_depth,",
            "line,",
        ],
    );
}

#[test]
fn lambda_captures_namespaced_constructors_for_depth_accounting() {
    let out = compile_with_modules(
        "use shapes as s\nlet f = fn() { return [s.Point { x: 1 }, s.Maybe::Some(2)] }",
        &[(
            "shapes",
            "struct Point { x }\nenum Maybe { Some(value), None }",
        )],
    )
    .expect("transpile namespaced constructors in lambda");
    assert_eq!(
        out.matches("let __cap_0 = __nybl_read_binding(ctx, \"<root>\", \"s\", 2)?;")
            .count(),
        1,
        "the module alias should be captured exactly once:\n{out}"
    );
    contains_all(
        &out,
        &[
            "let __opaque_body_depth = 0u16.max(__cap_0.ownership_depth())",
            "__nybl_validate_namespace_type(&__nybl_user_value_73, \"s\", \"Point\", 2)?",
            "__nybl_validate_namespace_type(&__nybl_user_value_73, \"s\", \"Maybe\", 2)?",
        ],
    );
}

#[test]
fn nested_lambda_shadowing_does_not_capture_same_named_outer_scope_value() {
    let out = compile("let x = [1]\nlet outer = fn(x) { return fn() { return x } }");
    assert_eq!(
        out.matches("let __cap_0 = __nybl_user_value_78.clone();")
            .count(),
        1,
        "only the inner lambda should capture the outer lambda parameter:\n{out}"
    );
    contains_all(
        &out,
        &[
            "let __opaque_body_depth = 0u16; let __callable",
            "let __opaque_body_depth = 0u16.max(__cap_0.ownership_depth())",
        ],
    );
}

#[test]
fn rust_keyword_idents_are_mangled() {
    let out = compile("let type = 5\nprint(type)");
    contains_all(
        &out,
        &[
            "__nybl_define_binding(ctx, \"<root>\", \"type\"",
            "__nybl_read_binding(ctx, \"<root>\", \"type\", 2)?",
        ],
    );
    assert!(!out.contains("let mut r#type:"));
}

#[test]
fn every_user_identifier_uses_one_collision_free_namespace() {
    let out = compile(
        r#"fn yield(crate, super, ctx, nybl_self) {
    return crate + super + ctx + nybl_self
}
let __t0 = 1
let __l = 2
let ctx = 3
let nybl_self = 4
let crate = 5
let super = 6
let __nybl_user_value_5f5f7430 = 7
print(yield(crate, super, ctx, nybl_self), __t0, __l, __nybl_user_value_5f5f7430)"#,
    );

    contains_all(
        &out,
        &[
            "fn __nybl_function_site_0(",
            "mut __nybl_user_value_6372617465:",
            "mut __nybl_user_value_7375706572:",
            "mut __nybl_user_value_637478:",
            "mut __nybl_user_value_6e79626c5f73656c66:",
            "__nybl_define_binding(ctx, \"<root>\", \"__t0\"",
            "__nybl_define_binding(ctx, \"<root>\", \"__l\"",
            "__nybl_define_binding(ctx, \"<root>\", \"__nybl_user_value_5f5f7430\"",
        ],
    );
    assert!(!out.contains("let mut __t0:"));
    assert!(!out.contains("let mut __l:"));
    assert!(!out.contains("mut crate: ::nybl::value::Value"));
    assert!(!out.contains("mut super: ::nybl::value::Value"));
    assert!(!out.contains("r#yield"));
}

#[test]
fn module_paths_that_only_differ_by_dot_and_underscore_do_not_collide() {
    let out = compile_with_modules(
        "use a.b as dotted\nuse a_b as underscored\nprint(dotted.helper(), dotted.ctx, underscored.helper(), underscored.yield)",
        &[
            ("a.b", "let ctx = 10\nfn helper() { return 1 }"),
            ("a_b", "let yield = 20\nfn helper() { return 2 }"),
        ],
    )
    .expect("transpile both formerly-colliding modules");

    contains_all(
        &out,
        &[
            "fn __mod_612e62_load(",
            "fn __mod_615f62_load(",
            "struct NyblModule612e62Exports {",
            "struct NyblModule615f62Exports {",
            "module_path: \"a.b\", name: \"helper\"",
            "module_path: \"a_b\", name: \"helper\"",
        ],
    );
    assert!(!out.contains("fn __mod_a_b_load("));
    assert!(!out.contains("struct NyblModulea_bExports"));
}

#[test]
fn method_call_on_ident_emits_back_assign_for_mutating() {
    // `push` is mutating, so the emitted code must carry the
    // mutated array back into the source binding.
    let out = compile("let a = [1, 2]\na.push(3)");
    contains_all(
        &out,
        &[
            "__nybl_call_method(ctx, &",
            "\"push\"",
            "if let Some(__new_obj) = __mutated",
            "__nybl_binding_mut_option(ctx, \"<root>\", \"a\")",
            "= __new_obj",
        ],
    );
}

#[test]
fn method_call_on_ident_skips_back_assign_for_pure() {
    // `len` is pure, so the mutated slot is discarded with `_`.
    let out = compile("let a = [1, 2, 3]\nprint(a.len())");
    assert!(
        out.contains("let (__r, _) = __nybl_call_method(ctx, &"),
        "expected pure-method discard in:\n{out}"
    );
    assert!(
        !out.contains("if let Some(__new_obj)"),
        "pure method shouldn't emit the back-assign branch:\n{out}"
    );
}

#[test]
fn method_call_on_literal_has_no_back_assign() {
    // `[1,2,3].push(...)` has no target ident — the mutation is
    // observed and then discarded, same as in the tree-walker.
    // The emission tries user methods first, then falls through
    // to the builtin with `let (__r, _) = ...` for the
    // no-back-assign branch.
    let out = compile("print([1, 2, 3].push(4))");
    assert!(
        out.contains("let (__r, _) = __nybl_call_method(ctx, &"),
        "expected literal-receiver discard in:\n{out}"
    );
}

#[test]
fn string_interp_builds_string_via_format() {
    let out = compile(
        r#"let name = "nybl"
print("hi {name}!")"#,
    );
    contains_all(
        &out,
        &[
            "::std::string::String::new()",
            "__s.push_str(\"hi \")",
            "__s.push_str(&format!(\"{}\", __nybl_read_binding(ctx, \"<root>\", \"name\", 2)?))",
            "__s.push_str(\"!\")",
            "::nybl::value::Value::__new_str_in(__s, &ctx.memory)",
        ],
    );
}

#[test]
fn index_assign_routes_through_ops_index_set() {
    let out = compile("let a = [1, 2, 3]\na[0] = 99");
    contains_all(
        &out,
        &[
            "__nybl_binding_mut_option(ctx, \"<root>\", \"a\")",
            "::nybl::ops::index_set_in(",
        ],
    );
}

#[test]
fn compound_index_assign_reads_then_writes() {
    let out = compile("let a = [1, 2]\na[0] += 5");
    contains_all(
        &out,
        &[
            "__nybl_binding_mut_option(ctx, \"<root>\", \"a\")",
            "::nybl::ops::index_get_in(",
            "::nybl::ops::add_in(",
            "::nybl::ops::index_set_in(",
        ],
    );
}

#[test]
fn index_assign_on_non_ident_is_rejected() {
    // The tree-walker rejects `[1,2][0] = 3` too — the error
    // message matches, for differential-harness peace of mind.
    let err = transpile("[1, 2][0] = 3", &Options::default()).unwrap_err();
    assert!(
        err.message.contains("Can only assign to indexed variables"),
        "got: {}",
        err.message
    );
}

#[test]
fn const_container_assignment_is_rejected_before_aot_emission() {
    let cases = [
        "const VALUES = [1, 2]\nVALUES[0] = 9",
        "const VALUES = [1, 2]\nVALUES[0] += 9",
        "const LOOKUP = {\"n\": 1}\nLOOKUP[\"n\"] = 9",
        "const LOOKUP = {\"n\": 1}\nLOOKUP[\"n\"] += 9",
        "struct Counter { n }\nconst COUNTER = Counter { n: 1 }\nCOUNTER.n = 9",
        "struct Counter { n }\nconst COUNTER = Counter { n: 1 }\n(COUNTER).n += 9",
        "const GRID = [[1]]\nGRID[0][0] = 9",
    ];

    for source in cases {
        let err = transpile(source, &Options::default()).unwrap_err();
        assert!(
            err.message.contains("can't reassign") && err.message.contains("constant"),
            "source: {source}\nerror: {err}"
        );
        assert_eq!(
            err.friendly_hint.as_deref(),
            Some("constants are immutable. Use `let` if you want a mutable binding."),
            "source: {source}"
        );
    }
}

#[test]
fn const_array_mutation_emits_the_shared_runtime_guard() {
    for method_call in [
        "VALUES.push(4)",
        "VALUES.pop()",
        "VALUES.insert(0, 4)",
        "VALUES.remove(0)",
        "VALUES.reverse()",
        "VALUES.sort()",
    ] {
        let source = format!("const VALUES = [3, 1, 2]\n{method_call}");
        let output = compile(&source);
        assert!(
            output.contains("::nybl::methods::reject_constant_array_mutation(\"VALUES\","),
            "source: {source}\noutput: {output}"
        );
    }

    let output = compile(
        r#"fn mutate() {
    const VALUES = [3, 1, 2]
    VALUES.sort()
}
mutate()"#,
    );
    assert!(
        output.contains("::nybl::methods::reject_constant_array_mutation(\"VALUES\", \"sort\", 3)"),
        "output: {output}"
    );
}

#[test]
fn const_non_array_mutator_names_still_emit_dynamic_dispatch() {
    let output = compile(
        r#"struct Accumulator { total }
fn Accumulator.push(self, value) { return self.total + value }
const ACCUMULATOR = Accumulator { total: 7 }
print(ACCUMULATOR.push(5))"#,
    );
    contains_all(
        &output,
        &[
            "__nybl_try_user_method",
            "::nybl::methods::reject_constant_array_mutation(\"ACCUMULATOR\", \"push\"",
        ],
    );

    compile(
        r#"const LOOKUP = {"n": 1}
LOOKUP.remove("n")"#,
    );
}

#[test]
fn const_index_reads_in_mutable_targets_still_emit_aot() {
    let out = compile("const INDEX = 0\nlet values = [1]\nvalues[INDEX] += 2");
    contains_all(
        &out,
        &[
            "__nybl_read_binding(ctx, \"<root>\", \"INDEX\", 3)?",
            "__nybl_binding_mut_option(ctx, \"<root>\", \"values\")",
            "::nybl::ops::index_get_in(",
            "::nybl::ops::index_set_in(",
        ],
    );
}

// ─── Sandbox mode ─────────────────────────────────────────────────

fn compile_sandbox(code: &str) -> String {
    transpile(
        code,
        &Options {
            sandbox: true,
            ..Options::default()
        },
    )
    .expect("transpile")
}

#[test]
fn sandbox_off_by_default_emits_no_tick_helper() {
    let out = compile("while true { }");
    for sandbox_only in ["__nybl_tick", "NyblInstance", "call_depth"] {
        assert!(
            !out.contains(sandbox_only),
            "non-sandbox build shouldn't emit {sandbox_only}:\n{out}",
        );
    }
    assert!(
        out.contains("memory: ::nybl::memory::MemoryContext::__untracked()"),
        "non-sandbox init should use an explicit untracked context:\n{out}"
    );
    assert!(!out.contains("ActiveMemoryGuard"));
    assert!(!out.contains("nybl_memory_"));
}

#[test]
fn lambda_final_memory_checks_are_sandbox_only() {
    let source = "let callback = fn() { return [1] }";
    let native = compile(source);
    assert!(
        !native.contains("__nybl_check_memory"),
        "native output must not pay sandbox memory-check overhead:\n{native}"
    );

    let sandbox = compile_sandbox(source);
    assert!(
        sandbox.contains("let value = __nybl_value_result?; __nybl_check_memory(ctx, 1)?;"),
        "sandbox lambda must check memory before exposing its outcome:\n{sandbox}"
    );
}

#[test]
fn sandbox_on_emits_tick_helper_and_limits_param() {
    let out = compile_sandbox("while true { }");
    let flat = norm(&out);
    for needle in [
        "fn __nybl_tick(ctx: &mut Ctx<'_>, line: u32)",
        "ctx.max_steps",
        "pub struct NyblInstance",
        "let memory = ::nybl::memory::MemoryContext::__new(limits.max_memory);",
        "call_depth: 0",
        "pub fn run<H: ::nybl::NyblHost>( host: &mut H, limits: &::nybl::NyblLimits, )",
        "let limits = ::nybl::NyblLimits::standard();",
    ] {
        assert!(
            flat.contains(needle),
            "expected fragment not found: {needle:?}\n---\n{out}\n---"
        );
    }
    assert!(!out.contains("ActiveMemoryGuard"));
    assert!(!out.contains("nybl_memory_"));
}

#[test]
fn sandbox_state_is_persistent_while_operation_context_is_ephemeral() {
    let sandbox = compile_sandbox("print(1)");
    contains_all(
        &sandbox,
        &[
            "struct __NyblState",
            "struct Ctx<'h>",
            "state: &'h mut __NyblState",
            "impl<'h> ::core::ops::Deref for Ctx<'h>",
            "let mut state = __NyblState",
            "state: &mut state",
            "sandbox embedders should use `run` and the generated `NyblInstance`",
            "layout\n/// is not a supported embedding API",
        ],
    );
    for public_internal in [
        "pub struct __NyblState",
        "pub struct Ctx<'h>",
        "pub struct AotClosure",
        "pub callable:",
    ] {
        assert!(
            !sandbox.contains(public_internal),
            "sandbox output exposed generated internal {public_internal:?}"
        );
    }
    let unsandboxed = compile("print(1)");
    assert!(!unsandboxed.contains("__NyblState"));
    contains_all(
        &unsandboxed,
        &[
            "pub struct Ctx<'h>",
            "pub struct AotClosure",
            "pub callable:",
            "pub reached_function_sites: ::std::vec::Vec<bool>",
            "const __NYBL_FUNCTION_SITES: &[__NyblFunctionSite]",
        ],
    );
    assert!(!sandbox.contains("pub reached_function_sites"));
}

#[test]
fn sandbox_catalogues_repeated_function_declarations_by_unique_site() {
    let out = compile_sandbox("pub fn entry(x) { return x }\npub fn entry(x, y) { return y }");
    contains_all(
        &out,
        &[
            "__NyblFunctionSite { id: 0, module_path: \"<root>\", name: \"entry\", params: &[\"x\"], param_modes: &[::nybl::parser::ParamMode::Value], is_public: true, abi_eligible: true, line: 1 }",
            "__NyblFunctionSite { id: 1, module_path: \"<root>\", name: \"entry\", params: &[\"x\", \"y\"], param_modes: &[::nybl::parser::ParamMode::Value, ::nybl::parser::ParamMode::Value], is_public: true, abi_eligible: true, line: 2 }",
        ],
    );
}

#[test]
fn sandbox_emits_tick_at_while_iteration() {
    let out = norm(&compile_sandbox("while true { let x = 1 }"));
    assert!(
        out.contains("while (::nybl::value::Value::Bool(true)).is_truthy() { __nybl_tick(ctx,"),
        "expected tick at top of while body:\n{out}"
    );
}

/// Normalize whitespace runs to single spaces so we can do
/// position-insensitive substring checks on the pretty-printed
/// output.
fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out
}

#[test]
fn sandbox_emits_tick_at_repeat_and_for() {
    let repeat = norm(&compile_sandbox("repeat 3 { let x = 1 }"));
    assert!(
        repeat.contains(".max(0)) { __nybl_tick(ctx,"),
        "expected tick at top of repeat iteration:\n{repeat}"
    );

    let forin = norm(&compile_sandbox("for x in [1, 2] { let y = x }"));
    // The for-in body starts with `loop { __nybl_tick(ctx,` now
    // that the emitter uses the iter-protocol helpers
    // (`__nybl_iter_start` / `__nybl_iter_step`).
    assert!(
        forin.contains("loop { __nybl_tick(ctx,"),
        "expected tick at top of for-in iteration body:\n{forin}"
    );
}

#[test]
fn sandbox_emits_tick_at_fn_entry() {
    let out = norm(&compile_sandbox("fn foo() { return 1 }\nprint(foo())"));
    assert!(
        out.contains(
            "fn __nybl_function_site_0(ctx: &mut Ctx<'_>) -> Result<::nybl::value::Value, ::nybl::error::NyblError> { __nybl_tick(ctx,"
        ),
        "expected tick at function entry:\n{out}"
    );
}

#[test]
fn sandbox_run_program_ticks_once_on_entry() {
    let out = norm(&compile_sandbox("print(1)"));
    // Top-level `run_program` is the program-scope equivalent of a
    // function entry, so it ticks once before doing anything else.
    assert!(
        out.contains(
            "fn run_program(ctx: &mut Ctx<'_>) -> Result<(), ::nybl::error::NyblError> { __nybl_tick(ctx,"
        ),
        "expected tick at run_program entry:\n{out}"
    );
}

#[test]
fn module_name_wraps_output_and_skips_main() {
    let opts = Options {
        module_name: Some("my_prog".into()),
        ..Options::default()
    };
    let out = transpile("print(1)", &opts).unwrap();
    assert!(
        out.starts_with("pub mod my_prog {\n"),
        "expected module wrapper prefix:\n{out}"
    );
    assert!(
        out.trim_end().ends_with('}'),
        "expected closing `}}`:\n{out}"
    );
    assert!(
        !out.contains("fn main()"),
        "module mode should skip main:\n{out}"
    );
    // The run fn should still be there, now addressed as
    // `my_prog::run`.
    assert!(
        out.contains("pub fn run<H: ::nybl::NyblHost>"),
        "expected pub fn run:\n{out}"
    );
}

#[test]
fn options_without_main_skip_entry_point() {
    let opts = Options {
        emit_main: false,
        use_nybl_sys: false,
        sandbox: false,
        module_name: None,
        module_resolver: None,
    };
    let out = transpile("print(1)", &opts).unwrap();
    assert!(
        !out.contains("fn main()"),
        "expected no main() when emit_main is false:\n{out}"
    );
    // But the library entry point (`run`) should still be there so
    // the emitter's output is usable.
    assert!(
        out.contains("pub fn run<H: ::nybl::NyblHost>"),
        "expected `run` to still be emitted:\n{out}"
    );
}

// ─── Cross-module type identity ───────────────────────────────────
//
// Runtime registries are nested by declaration module and type name, so
// same-named types in different modules coexist without shape folding.

fn compile_with_modules(
    code: &str,
    modules: &[(&str, &str)],
) -> Result<String, ::nybl::error::NyblError> {
    let resolver = modules_from_map(modules.iter().map(|(k, v)| (*k, *v)));
    transpile(
        code,
        &Options {
            emit_main: false,
            use_nybl_sys: false,
            sandbox: false,
            module_name: None,
            module_resolver: Some(resolver),
        },
    )
}

fn module_slug(module: &str) -> String {
    module
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn module_load_marker(module: &str) -> String {
    format!("fn __mod_{}_load(", module_slug(module))
}

fn module_export_fields(generated: &str, module: &str) -> Vec<String> {
    let slug = module_slug(module);
    let marker = format!("struct NyblModule{slug}Exports {{");
    let body = generated
        .split_once(&marker)
        .unwrap_or_else(|| panic!("missing exports struct for `{module}`:\n{generated}"))
        .1;
    body.lines()
        .skip(1)
        .take_while(|line| line.trim() != "}")
        .map(|line| {
            line.trim()
                .split_once(':')
                .expect("exports struct field")
                .0
                .to_string()
        })
        .collect()
}

#[test]
fn nested_import_discovery_reaches_every_statement_body_and_lambda_expression() {
    let source = r#"fn exercise() {
    use fn_dep as fn_module
    if true {
        use if_dep as if_module
    } else if false {
        use else_if_dep as else_if_module
    } else {
        use else_dep as else_module
    }
    while false {
        use while_dep as while_module
    }
    repeat 0 {
        use repeat_dep as repeat_module
    }
    for item in [] {
        use for_dep as for_module
    }
}
struct Holder { value }
fn Holder.exercise(self) {
    use method_dep as method_module
}
let direct = fn() {
    use lambda_dep as lambda_module
    return none
}
let from_match = match 1 {
    1 => fn() {
        use match_lambda_dep as match_module
        return none
    },
    _ => fn() { return none },
}"#;
    let modules = [
        "fn_dep",
        "if_dep",
        "else_if_dep",
        "else_dep",
        "while_dep",
        "repeat_dep",
        "for_dep",
        "method_dep",
        "lambda_dep",
        "match_lambda_dep",
    ]
    .map(|name| (name, "let value = 1"));

    let out = compile_with_modules(source, &modules).expect("discover every nested import");
    for (module, _) in modules {
        let marker = module_load_marker(module);
        assert!(
            out.contains(&marker),
            "missing nested module `{module}` ({marker}) in generated output"
        );
    }
}

#[test]
fn repeated_nested_imports_resolve_once_but_emit_per_runtime_scope() {
    let calls = std::rc::Rc::new(std::cell::RefCell::new(0_u32));
    let resolver_calls = std::rc::Rc::clone(&calls);
    let resolver: nybl_compile::ModuleResolver =
        std::rc::Rc::new(std::cell::RefCell::new(move |name: &str| {
            if name == "shared" {
                *resolver_calls.borrow_mut() += 1;
                Some(Ok("fn helper(n) { return n + 10 }".to_string()))
            } else {
                None
            }
        }));
    let source = r#"fn one() { use shared; return helper(1) }
fn two() { use shared; return helper(2) }"#;
    let out = transpile(
        source,
        &Options {
            emit_main: false,
            use_nybl_sys: false,
            module_resolver: Some(resolver),
            ..Options::default()
        },
    )
    .expect("transpile two function-local imports");

    assert_eq!(*calls.borrow(), 1, "resolver must run once per module path");
    let load_site = format!("= __mod_{}_load(ctx)?;", module_slug("shared"));
    assert_eq!(
        out.matches(&load_site).count(),
        2,
        "each function still needs its own runtime binding site:\n{out}"
    );
}

#[test]
fn nested_imports_do_not_become_module_re_exports() {
    let out = compile_with_modules(
        "use wrapper as wrapper_module",
        &[
            ("dep", "let value = 41\nlet hidden = 99"),
            (
                "wrapper",
                r#"fn load() {
    use dep.{value} as chosen
    return chosen.value + 1
}
let own = 7"#,
            ),
        ],
    )
    .expect("transpile a module with a function-local selective alias");

    assert!(out.contains(&module_load_marker("dep")));
    assert_eq!(
        module_export_fields(&out, "wrapper"),
        ["__nybl_user_value_6f776e", "__nybl_user_value_6c6f6164"]
    );
}

#[test]
fn function_local_module_alias_does_not_leak_into_a_sibling_function() {
    let error = compile_with_modules(
        r#"fn seed_alias() {
    use types as t
    return none
}
fn lacks_import() {
    return t.Point { value: 42 }
}"#,
        &[("types", "struct Point { value }")],
    )
    .expect_err("a function-local module alias must not reach a sibling function");

    assert_eq!(error.message, "Struct `Point` is not declared");
    assert_eq!(error.line, Some(6));
}

#[test]
fn module_alias_conflicting_with_named_function_is_rejected() {
    let error = compile_with_modules(
        r#"fn dep() { return 1 }
use dep as dep"#,
        &[("dep", "let value = 42")],
    )
    .expect_err("a module alias must not replace a named function");

    assert!(error.message.contains("would shadow an existing binding"));
    assert_eq!(error.line, Some(2));
}

#[test]
fn declaration_alias_binding_precedes_same_named_parameter_binding() {
    let output = compile_with_modules(
        r#"use types as t
fn build(t) { return t.Point { value: 42 } }"#,
        &[("types", "struct Point { value }")],
    )
    .expect("transpile declaration alias shadowed by a parameter");
    let function = output
        .split_once("fn __nybl_function_site_0(")
        .expect("lifted build function")
        .1;
    let parameter_binding = function
        .find("let mut __nybl_user_value_74: ::nybl::value::Value = __nybl_param_0;")
        .expect("Nybl parameter binding");
    assert_eq!(
        parameter_binding,
        function.find("let mut __nybl_user_value_74").unwrap()
    );
    assert!(
        !function[..function.find("fn __nybl_try_user_method").unwrap()]
            .contains("ctx.module_aliases.get"),
        "a same-named parameter must suppress declaration-alias restoration:\n{function}"
    );
}

#[test]
fn module_loader_clears_declaration_alias_context_after_failure() {
    let output = compile_with_modules(
        "use holder",
        &[
            ("holder", "use types as dep\nlet broken = 1 / 0"),
            ("types", "let value = 42"),
        ],
    )
    .expect("transpile failing module body");
    let loader = output
        .split_once(&module_load_marker("holder"))
        .expect("holder loader")
        .1;
    let loader = loader
        .split_once("fn ")
        .map_or(loader, |(loader, _)| loader);
    assert!(loader.contains("let __load_result = (||"));
    assert!(loader.contains("if __load_result.is_err()"));
    assert!(loader.contains("ctx.module_cache.remove(\"holder\")"));
    assert!(loader.contains("ctx.module_aliases.retain(|(module, _), _| module != \"holder\")"));
    assert!(
        loader.contains(
            "let __saved_type_bindings = ctx.module_type_bindings.get(\"holder\").cloned()"
        )
    );
    assert!(loader.contains("ctx.module_type_bindings.remove(\"holder\")"));
}

#[test]
fn bare_types_resolve_from_source_ordered_runtime_context() {
    let output = compile(
        r#"fn build() { return Point { value: 42 } }
fn label(value) { return match value { Point { value } => value, _ => 0 } }
let before = try_call(build)
struct Point { value }
let after = build()"#,
    );

    contains_all(
        &output,
        &[
            "let mut __nybl_type_bindings = __nybl_callable_type_bindings(ctx, \"<root>\")",
            "__nybl_resolve_bare_type(&__nybl_type_bindings, \"Point\", true, 1)",
            "__nybl_bind_type(&mut __nybl_type_bindings, \"Point\", \"<root>\")",
            "__nybl_publish_type_binding(ctx, \"<root>\", \"Point\", \"<root>\")",
            "(::std::option::Option::None, __tn) => __nybl_type_bindings.iter().rev()",
        ],
    );
}

#[test]
fn bare_type_imports_publish_declaration_origins_at_the_use_site() {
    let output = compile_with_modules(
        r#"fn build() { return Point { value: 42 } }
let before = try_call(build)
use facade.{Point}
let after = build()"#,
        &[("types", "struct Point { value }"), ("facade", "use types")],
    )
    .expect("transpile source-ordered type import");

    contains_all(
        &output,
        &[
            "__nybl_bind_imported_type(&mut __nybl_type_bindings, \"Point\", \"types\")",
            "__nybl_publish_imported_type_binding(ctx, \"<root>\", \"Point\", \"types\")",
        ],
    );
}

#[test]
fn type_publication_emits_one_incremental_update_per_declaration() {
    const DECLARATIONS: usize = 256;
    let source = (0..DECLARATIONS)
        .map(|index| format!("struct Type{index} {{}}"))
        .collect::<Vec<_>>()
        .join("\n");

    for sandbox in [false, true] {
        let output = transpile(
            &source,
            &Options {
                sandbox,
                ..Options::default()
            },
        )
        .expect("transpile type-heavy source");
        let run_program = output.split_once("fn run_program").expect("run program").1;
        assert_eq!(
            run_program
                .matches("__nybl_publish_type_binding(ctx, \"<root>\"")
                .count(),
            DECLARATIONS,
        );
        assert!(!output.contains("__nybl_flatten_type_bindings"));
        assert!(!output.contains("__nybl_publish_type_bindings"));
        assert!(
            output.contains(
                "type __NyblTypeBindings = ::std::vec::Vec<::std::rc::Rc<__NyblTypeFrame>>"
            )
        );
        assert!(output.contains("::std::rc::Rc::make_mut(bindings)"));
    }
}

#[test]
fn type_free_callables_and_loops_do_not_clone_runtime_type_context() {
    let output = compile(
        r#"fn hot(n) {
    let total = 0
    repeat n { total += 1 }
    return match total { 0 => 0, value => value }
}
let closure = fn(n) {
    let total = 0
    repeat n { total += 1 }
    return total
}
print(hot(3), closure(2))"#,
    );

    assert!(!output.contains("let mut __nybl_type_bindings = __nybl_callable_type_bindings"));
    assert!(!output.contains("let mut __nybl_type_bindings = __nybl_type_bindings.clone()"));
}

#[test]
fn method_free_program_omits_slot_table_and_registration_machinery() {
    let output = compile("let values = [1, 2]\nprint(values.len())");

    assert!(!output.contains("method_slots:"));
    assert!(!output.contains("__nybl_method_adapter_"));
    contains_all(
        &output,
        &[
            "type __NyblMethodFn =",
            "let _ = (ctx, obj, method, args, line);",
            "fn __nybl_try_user_method(",
            "fn __nybl_preflight_user_method(",
            "Ok(None)",
        ],
    );
}

#[test]
fn method_sites_are_unique_but_share_one_dense_key_slot() {
    let output = compile(
        r#"struct Item { value }
if true { fn Item.read(self) { return 1 } }
if false { fn Item.read(self, extra) { return extra } }
let install = fn() { fn Item.read(self) { return 3 } }"#,
    );

    contains_all(
        &output,
        &[
            "pub method_slots: [::std::option::Option<__NyblMethodFn>; 1]",
            "fn __nybl_method_site_0(",
            "fn __nybl_method_adapter_0(",
            "fn __nybl_method_site_1(",
            "fn __nybl_method_adapter_1(",
            "fn __nybl_method_site_2(",
            "fn __nybl_method_adapter_2(",
            "ctx.method_slots[0] = ::std::option::Option::Some(__nybl_method_adapter_0)",
            "ctx.method_slots[0] = ::std::option::Option::Some(__nybl_method_adapter_1)",
            "ctx.method_slots[0] = ::std::option::Option::Some(__nybl_method_adapter_2)",
            "Some(adapter) => Ok(Some(adapter(ctx, obj, args, line)?))",
        ],
    );
}

#[test]
fn failed_module_with_methods_snapshots_only_its_dense_slots() {
    let output = compile_with_modules(
        "fn retry() { use bad }\nuse dep",
        &[
            ("dep", "fn Shared.read(self) { return 1 }"),
            (
                "bad",
                "use dep\nfn Own.read(self) { return 2 }\nlet boom = 1 / 0",
            ),
        ],
    )
    .expect("transpile method-bearing failing module");

    contains_all(
        &output,
        &[
            "let __saved_method_slots = [ctx.method_slots[0]];",
            "ctx.method_slots[0] = __saved_method_slots[0];",
            "let __saved_method_slots = [ctx.method_slots[1]];",
            "ctx.method_slots[1] = ::std::option::Option::Some",
            "filter(|site| site.module_path == \"bad\").map(|site| (site.id, ctx.reached_function_sites[site.id])).collect();",
            "for (__site, __reached) in __saved_reached_function_sites { ctx.reached_function_sites[__site] = __reached; }",
        ],
    );
    assert!(
        !output.contains("let __saved_method_slots = [ctx.method_slots[0], ctx.method_slots[1]];")
    );
}

#[test]
fn nested_type_sites_emit_static_descriptors_and_o1_runtime_lookup() {
    let output = compile(
        r#"if true {
    struct Point { x, y }
    enum Signal { Idle, Pair(left, right), Named { value } }
    print(Point { x: 1, y: 2 }.x)
    print(Signal::Pair(3, 4))
}"#,
    );

    contains_all(
        &output,
        &[
            "const __NYBL_STRUCT_SITE_0: &'static [&'static str] = &[\"x\", \"y\"]",
            "const __NYBL_ENUM_SITE_1: &'static [(&'static str, __NyblDynamicVariantShape)]",
            "__NyblDynamicVariantShape::Tuple(&[\"left\", \"right\"])",
            "__nybl_register_struct(ctx, \"<root>\", \"Point\", __NYBL_STRUCT_SITE_0",
            "__nybl_register_enum(ctx, \"<root>\", \"Signal\", __NYBL_ENUM_SITE_1",
            ".get(module)\n        .and_then(|defs| defs.get(type_name))",
            "__nybl_struct_fields(ctx, &__module_path, \"Point\"",
            "__nybl_enum_variant_shape(ctx, &__module_path, \"Signal\", \"Pair\"",
        ],
    );
    assert!(!output.contains("match __module_path.as_str()"));
    assert!(!output.contains("let __declared_fields: &'static"));
}

#[test]
fn recursive_type_catalogue_reaches_lambdas_through_expression_edges() {
    let output = compile(
        r#"let maker = ({"callable": [fn() {
    let before = try_call(fn() { return Deep { value: 1 } })
    struct Deep { value }
    enum Nested { Value(item) }
    return match Nested::Value(Deep { value: 2 }) {
        Nested::Value(item) => item,
        _ => none,
    }
}]}["callable"])[0]
print(maker().value)"#,
    );

    contains_all(
        &output,
        &[
            "const __NYBL_STRUCT_SITE_0",
            "const __NYBL_ENUM_SITE_1",
            "__nybl_register_struct(ctx, \"<root>\", \"Deep\"",
            "__nybl_register_enum(ctx, \"<root>\", \"Nested\"",
        ],
    );
}

#[test]
fn typed_module_failure_emits_own_registry_snapshot_and_restore() {
    let output = compile_with_modules(
        "fn retry() { use bad }",
        &[(
            "bad",
            "struct Own { value }\nenum OwnSignal { Value(item) }\nlet boom = 1 / 0",
        )],
    )
    .expect("transpile typed failing module");

    contains_all(
        &output,
        &[
            "let __saved_type_defs = __nybl_take_module_type_defs(ctx, \"bad\")",
            "__nybl_clear_module_type_defs(ctx, \"bad\")",
            "__nybl_restore_module_type_defs(ctx, \"bad\", __saved_type_defs)",
            "structs: ctx.struct_defs.remove(module)",
            "enums: ctx.enum_defs.remove(module)",
        ],
    );
}

#[test]
fn flat_module_reexport_publishes_live_context_for_lifted_callables() {
    let output = compile_with_modules(
        r#"use wrapper
fn build() { return api.Point { value: 42 } }
let value = build()"#,
        &[
            ("types", "struct Point { value }"),
            ("wrapper", "use types as api"),
        ],
    )
    .expect("transpile a flat module-valued re-export");

    contains_all(
        &output,
        &[
            "ctx.module_aliases.insert((\"wrapper\".to_string(), \"api\".to_string())",
            "ctx.module_aliases.insert((\"<root>\".to_string(), \"api\".to_string())",
            "__nybl_binding_value(ctx, \"<root>\", \"api\").ok_or_else",
            "isn't a module alias in scope",
        ],
    );
}

#[test]
fn module_alias_copy_and_assignment_sync_generated_context() {
    let output = compile_with_modules(
        r#"use first as api
use second as other
let copy = api
api = other
fn from_copy() { return copy.First { value: 1 } }
fn from_api() { return api.Second { value: 2 } }"#,
        &[
            ("first", "struct First { value }"),
            ("second", "struct Second { value }"),
        ],
    )
    .expect("transpile module alias copy and reassignment");

    contains_all(
        &output,
        &[
            "__nybl_read_binding(ctx, \"<root>\", \"copy\", 0)?",
            "matches!(&",
            "(\"<root>\".to_string(), \"copy\".to_string())",
            "__nybl_read_binding(ctx, \"<root>\", \"api\", 0)?",
            "ctx.module_aliases.remove(&(\"<root>\".to_string(), \"api\".to_string()))",
        ],
    );
}

#[test]
fn block_local_module_alias_does_not_leak_into_the_enclosing_scope() {
    let error = compile_with_modules(
        r#"if true {
    use types as t
}
let leaked = t.Point { value: 42 }"#,
        &[("types", "struct Point { value }")],
    )
    .expect_err("a block-local module alias must not reach its enclosing scope");

    assert_eq!(error.message, "Struct `Point` is not declared");
    assert_eq!(error.line, Some(4));
}

#[test]
fn lambda_local_module_alias_does_not_leak_into_a_sibling_lambda() {
    let error = compile_with_modules(
        r#"let seed_alias = fn() {
    use types as t
    return none
}
let lacks_import = fn() {
    return t.Point { value: 42 }
}"#,
        &[("types", "struct Point { value }")],
    )
    .expect_err("a lambda-local module alias must not reach a sibling lambda");

    assert_eq!(error.message, "Struct `Point` is not declared");
    assert_eq!(error.line, Some(6));
}

#[test]
fn nested_import_bindings_shadow_outer_frames_but_not_the_current_frame() {
    let out = compile_with_modules(
        r#"use first as types
let selected = 1
let globbed = 1
if true {
    use second as types
    use values.{selected}
    use values
    let point = types.Point { second: selected + globbed }
}
if true {
    let selected = 10
    use values.{selected}
    let globbed = 20
    use values
}"#,
        &[
            ("first", "struct Point { first }"),
            ("second", "struct Point { second }"),
            ("values", "let selected = 2\nlet globbed = 3"),
        ],
    )
    .expect("nested imports may shadow outer frames and keep current-frame bindings");

    assert!(out.contains("__nybl_validate_namespace_type(&__nybl_user_value_7479706573"));
    assert!(out.contains("\"types\", \"Point\", 8)?; \"second\".to_string()"));
    assert!(out.contains(
        "let __declared_fields = __nybl_struct_fields(ctx, &__module_path, \"Point\", 8)?;"
    ));
}

#[test]
fn same_scope_type_imports_are_first_win_for_selective_and_glob_forms() {
    compile_with_modules(
        r#"if true {
    use first.{Point}
    use second.{Point}
    let selected = Point { first: 1 }
}
if true {
    use first
    use second
    let globbed = Point { first: 2 }
}"#,
        &[
            ("first", "struct Point { first }"),
            ("second", "struct Point { second }"),
        ],
    )
    .expect("the first type import in each frame must retain the bare name");
}

#[test]
fn selective_alias_type_shape_limits_construction_and_pattern_resolution() {
    let construction_error = compile_with_modules(
        "use types.{A} as narrowed\nlet bad = narrowed.B { value: 1 }",
        &[("types", "struct A { value }\nstruct B { value }")],
    )
    .expect_err("an unselected type must not resolve through a selective alias");
    assert_eq!(construction_error.message, "Struct `B` is not declared");
    assert_eq!(construction_error.line, Some(2));

    let out = compile_with_modules(
        r#"use types as all
use types.{A} as narrowed
let value = all.B { value: 1 }
let result = match value {
    narrowed.B { value } => "matched",
    _ => "missed",
}"#,
        &[("types", "struct A { value }\nstruct B { value }")],
    )
    .expect("an unselected pattern type remains a legal non-matching pattern");

    assert!(out.contains("Option::Some(\"narrowed\"), __tn"));
    assert!(out.contains("__module.type_origin(__tn).map(str::to_string)"));
}

#[test]
fn lazy_import_edges_do_not_create_false_cycles() {
    let out = compile_with_modules(
        "use a as root_a",
        &[
            (
                "a",
                "let value = 10\nfn load() { use b as nested_b; return nested_b.value }",
            ),
            ("b", "use a as parent\nlet value = 32"),
        ],
    )
    .expect("a lazy a -> b edge must not form an eager b -> a cycle");

    assert!(out.contains(&module_load_marker("a")));
    assert!(out.contains(&module_load_marker("b")));
}

#[test]
fn nested_import_missing_and_eager_cycle_diagnostics_keep_source_lines() {
    let missing = compile_with_modules("fn run() {\n    let before = 1\n    use absent\n}", &[])
        .expect_err("nested missing module must fail during graph discovery");
    assert_eq!(missing.message, "Module `absent` not found");
    assert_eq!(missing.line, Some(3));

    let cycle = compile_with_modules(
        "fn run() {\n    use a\n}",
        &[("a", "use b"), ("b", "use a")],
    )
    .expect_err("top-level module imports still form an eager cycle");
    assert_eq!(cycle.message, "Circular import: module `a`");
    assert_eq!(cycle.line, Some(1));
}

#[test]
fn aliased_use_constructs_a_depth_checked_module_value() {
    let out = compile_with_modules("\nuse helpers as h", &[("helpers", "let x = [1]")])
        .expect("transpile aliased module");
    contains_all(
        &out,
        &[
            "Value::new_module_with_type_exports(\"helpers\".to_string()",
            "NyblTypeExports::from_origins(::std::vec![])",
            ", 2)?;",
        ],
    );
    assert!(
        !out.contains("NyblModule {"),
        "generated code must not bypass NyblModule's checked constructor:\n{out}"
    );
}

#[test]
fn aliased_facade_emits_the_declaration_module_for_reexported_types() {
    let out = compile_with_modules(
        "use facade as api\nlet value = api.Point { x: 1 }",
        &[("leaf", "struct Point { x }"), ("facade", "use leaf")],
    )
    .expect("transpile an aliased facade type");

    contains_all(
        &out,
        &[
            "Value::new_module_with_type_exports(\"facade\".to_string()",
            "(\"Point\".to_string(), \"leaf\".to_string())",
            "m.type_origin(type_name).map(str::to_string)",
        ],
    );
}

#[test]
fn diamond_imports_emit_shared_dependency_bindings_in_each_module_scope() {
    let out = compile_with_modules(
        "use left\nuse right\nprint(one, two)",
        &[
            ("shared", "fn helper(n) { return n + 10 }"),
            ("left", "use shared\nlet one = helper(1)"),
            ("right", "use shared\nlet two = helper(2)"),
        ],
    )
    .expect("transpile diamond import");

    assert_eq!(
        out.matches("= __mod_736861726564_load(ctx)?;").count(),
        2,
        "each dependent module needs its own shared-import binding declarations:\n{out}"
    );
}

#[test]
fn import_idempotency_is_local_to_the_generated_binding_scope() {
    let repeated = compile_with_modules(
        "use shared\nuse shared\nprint(helper(1))",
        &[("shared", "fn helper(n) { return n + 10 }")],
    )
    .expect("transpile repeated plain import");
    assert_eq!(
        repeated.matches("= __mod_736861726564_load(ctx)?;").count(),
        1,
        "a repeated plain glob in one scope should remain a no-op:\n{repeated}"
    );

    let distinct_functions = compile_with_modules(
        r#"use shared as shared_module
fn one() { use shared; return helper(1) }
fn two() { use shared; return helper(2) }"#,
        &[("shared", "fn helper(n) { return n + 10 }")],
    )
    .expect("transpile function-local imports");
    assert_eq!(
        distinct_functions
            .matches("= __mod_736861726564_load(ctx)?;")
            .count(),
        3,
        "the root alias and both function scopes must each emit their own load/bind site:\n{distinct_functions}"
    );

    let distinct_lambdas = compile_with_modules(
        r#"use shared as shared_module
let one = fn() { use shared; return helper(1) }
let two = fn() { use shared; return helper(2) }"#,
        &[("shared", "fn helper(n) { return n + 10 }")],
    )
    .expect("transpile lambda-local imports");
    assert_eq!(
        distinct_lambdas
            .matches("= __mod_736861726564_load(ctx)?;")
            .count(),
        3,
        "the root alias and both lambda scopes must each emit their own load/bind site:\n{distinct_lambdas}"
    );

    let aliases = compile_with_modules(
        "use shared as first\nuse shared as second",
        &[("shared", "let value = 1")],
    )
    .expect("transpile repeated aliases");
    assert_eq!(
        aliases.matches("= __mod_736861726564_load(ctx)?;").count(),
        2,
        "aliases are shaped bindings and must never enter the plain-glob idempotency cache:\n{aliases}"
    );
}

#[test]
fn nested_import_effective_exports_match_the_exact_use_shape() {
    let out = compile_with_modules(
        r#"use aliased as aliased_module
use selected as selected_module
use globbed as globbed_module
use selected_alias as selected_alias_module
use mixed as mixed_module
use chained as chained_module
use hygienic as hygienic_module"#,
        &[
            (
                "shared",
                r#"let public = 1
let _private = 2
fn helper(n) { return n + 1 }
fn _hidden(n) { return n - 1 }
struct Thing { value }"#,
            ),
            ("aliased", "use shared as dep\nlet value = dep.public"),
            ("selected", "use shared.{_private, helper}"),
            ("globbed", "use shared"),
            ("selected_alias", "use shared.{_private, Thing} as chosen"),
            ("mixed", "use shared.{_private}\nuse shared"),
            ("chained", "use aliased as layer"),
            ("hygienic", "use shared as ctx"),
        ],
    )
    .expect("transpile shaped nested imports");

    assert_eq!(
        module_export_fields(&out, "aliased"),
        ["__nybl_user_value_646570", "__nybl_user_value_76616c7565",]
    );
    assert_eq!(
        module_export_fields(&out, "selected"),
        [
            "__nybl_user_value_5f70726976617465",
            "__nybl_user_value_68656c706572",
        ]
    );
    assert_eq!(
        module_export_fields(&out, "globbed"),
        [
            "__nybl_user_value_7075626c6963",
            "__nybl_user_value_68656c706572",
        ]
    );
    assert_eq!(
        module_export_fields(&out, "selected_alias"),
        ["__nybl_user_value_63686f73656e"]
    );
    assert_eq!(
        module_export_fields(&out, "mixed"),
        [
            "__nybl_user_value_5f70726976617465",
            "__nybl_user_value_7075626c6963",
            "__nybl_user_value_68656c706572",
        ]
    );
    assert_eq!(
        module_export_fields(&out, "chained"),
        ["__nybl_user_value_6c61796572"]
    );
    assert_eq!(
        module_export_fields(&out, "hygienic"),
        ["__nybl_user_value_637478"]
    );
}

#[test]
fn same_named_struct_same_shape_across_modules_is_ok() {
    // Two different modules both declare `struct Point { x, y }`.
    // Same shape → idempotent, mirrors the walker's re-import
    // behaviour.
    let out = compile_with_modules(
        "use geom_a\nuse geom_b",
        &[
            ("geom_a", "struct Point { x, y }"),
            ("geom_b", "struct Point { x, y }"),
        ],
    );
    assert!(out.is_ok(), "expected success, got {:?}", out.err());
}

#[test]
fn same_named_struct_different_shape_across_modules_coexist() {
    // Phase 2b — module-qualified types. Two modules may
    // independently declare `Point` with different fields; the
    // AOT transpiles fine because the resulting types live at
    // distinct identities `(geom_a, Point)` and `(geom_b, Point)`.
    let src = compile_with_modules(
        "use geom_a as a\nuse geom_b as b",
        &[
            ("geom_a", "struct Point { x, y }"),
            ("geom_b", "struct Point { x, y, z }"),
        ],
    )
    .expect("phase 2b should accept same-name, different-shape types");
    // The two module paths show up in the generated Rust as
    // string literals for the type identities.
    assert!(
        src.contains("\"geom_a\"") && src.contains("\"geom_b\""),
        "expected both module paths to surface as identity literals",
    );
}

#[test]
fn same_named_enum_different_variants_across_modules_coexist() {
    // Phase 2b — enum shapes follow the same identity rule as
    // structs. Two `Tag` enums with different variants coexist
    // at `(a, Tag)` and `(b, Tag)`; the AOT happily transpiles.
    let src = compile_with_modules(
        "use a as pa\nuse b as pb",
        &[
            ("a", "enum Tag { Red, Green }"),
            ("b", "enum Tag { Red, Green, Blue }"),
        ],
    )
    .expect("phase 2b should accept same-name, different-variant enums");
    assert!(
        src.contains("\"a\"") && src.contains("\"b\""),
        "expected both module paths to surface as identity literals",
    );
}

#[test]
fn same_named_enum_same_variants_across_modules_is_ok() {
    let out = compile_with_modules(
        "use a\nuse b",
        &[
            ("a", "enum Tag { Red, Green }"),
            ("b", "enum Tag { Red, Green }"),
        ],
    );
    assert!(out.is_ok(), "expected success, got {:?}", out.err());
}

#[test]
fn root_program_and_module_can_declare_same_name_type() {
    // Phase 2b — the root program's `Point` lives at
    // `(<root>, Point)`; the imported module's version lives
    // at `(geom, Point)`. Both constructions compile without
    // clashing, and the emitted Rust carries each identity as
    // a string literal inside its `Value::new_struct` call.
    let src = compile_with_modules(
        r#"struct Point { x, y }
use geom as g
let a = Point { x: 1, y: 2 }
let b = g.Point { x: 1, y: 2, z: 3 }"#,
        &[("geom", "struct Point { x, y, z }")],
    )
    .expect("phase 2b should accept same-name types across root and module");
    assert!(
        src.contains("\"<root>\"") && src.contains("\"geom\""),
        "expected both identities to surface as literals, got:\n{src}",
    );
}
