//! Round-trip tests for the bytecode compiler (step 2a).
//!
//! For each sample program we compile it through `nybl-lang`'s parser,
//! feed the AST to `nybl-vm`'s compiler, and assert the disassembly
//! matches an expected snapshot. This is not a semantic test — that
//! comes in step 2b via the differential harness — but it pins the
//! emitted shape so future instruction-set changes are visible in the
//! diff.

use nybl::parse;
use nybl_vm::chunk::{LocalScopeIdx, LocalScopeSnapshot, RefArgTarget, SlotIdx};
use nybl_vm::{Constant, Instr, LoopStateKind, compile, disassemble, validate_chunk};
use std::sync::Arc;

fn disasm(source: &str) -> String {
    let ast = parse(source).expect("parse");
    let chunk = compile(&ast).expect("compile");
    disassemble(&chunk)
}

fn assert_disasm(source: &str, expected: &str) {
    let actual = disasm(source);
    let normalize = |s: &str| -> String {
        s.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let actual = normalize(&actual);
    let expected = normalize(expected);
    if actual != expected {
        panic!("disassembly mismatch.\n=== actual ===\n{actual}\n=== expected ===\n{expected}\n");
    }
}

#[test]
fn empty_program_is_just_halt() {
    assert_disasm("", "0: Halt");
}

#[test]
fn numeric_literal_and_print() {
    assert_disasm(
        "print(42)",
        r#"
        0: PrepareCall print/#0
        1: LoadConst 42
        2: CallPrepared #0
        3: Pop
        4: Halt
        "#,
    );
}

#[test]
fn let_and_use() {
    assert_disasm(
        "let x = 10\nprint(x)",
        r#"
        0: LoadConst 10
        1: DefineLocal x
        2: PrepareCall print/#0
        3: LoadVar x
        4: CallPrepared #0
        5: Pop
        6: Halt
        "#,
    );
}

#[test]
fn arithmetic_chain() {
    assert_disasm(
        "print(1 + 2 * 3)",
        r#"
        0: PrepareCall print/#0
        1: LoadConst 1
        2: LoadConst 2
        3: LoadConst 3
        4: Mul
        5: Add
        6: CallPrepared #0
        7: Pop
        8: Halt
        "#,
    );
}

#[test]
fn all_binary_ops_have_direct_opcodes() {
    // Smoke test: every primitive binary operator lowers to one opcode.
    // If this ever requires more than `compile_expr + compile_expr + one op`,
    // something subtle has changed in the op set.
    for (src, op) in [
        ("print(1 + 2)", "Add"),
        ("print(1 - 2)", "Sub"),
        ("print(1 * 2)", "Mul"),
        ("print(1 / 2)", "Div"),
        ("print(1 % 2)", "Rem"),
        ("print(1 == 2)", "Eq"),
        ("print(1 != 2)", "NotEq"),
        ("print(1 < 2)", "Lt"),
        ("print(1 > 2)", "Gt"),
        ("print(1 <= 2)", "LtEq"),
        ("print(1 >= 2)", "GtEq"),
    ] {
        let d = disasm(src);
        assert!(
            d.contains(&format!("{op}\n")) || d.contains(&format!("{op} ")),
            "expected {op} in disassembly for `{src}`:\n{d}"
        );
    }
}

#[test]
fn unary_ops() {
    assert_disasm(
        "print(-5)\nprint(!true)",
        r#"
        0: PrepareCall print/#0
        1: LoadConst 5
        2: Neg
        3: CallPrepared #0
        4: Pop
        5: PrepareCall print/#1
        6: LoadTrue
        7: Not
        8: CallPrepared #1
        9: Pop
        10: Halt
        "#,
    );
}

#[test]
fn short_circuit_and() {
    // `a && b` compiles to:
    //   <a>; TruthyToBool; JumpIfFalsePeek(end); Pop; <b>; TruthyToBool; end:
    assert_disasm(
        "print(true && false)",
        r#"
        0: PrepareCall print/#0
        1: LoadTrue
        2: TruthyToBool
        3: JumpIfFalsePeek -> 7
        4: Pop
        5: LoadFalse
        6: TruthyToBool
        7: CallPrepared #0
        8: Pop
        9: Halt
        "#,
    );
}

#[test]
fn short_circuit_or() {
    assert_disasm(
        "print(true || false)",
        r#"
        0: PrepareCall print/#0
        1: LoadTrue
        2: TruthyToBool
        3: JumpIfTruePeek -> 7
        4: Pop
        5: LoadFalse
        6: TruthyToBool
        7: CallPrepared #0
        8: Pop
        9: Halt
        "#,
    );
}

#[test]
fn if_else() {
    assert_disasm(
        r#"if true { print("yes") } else { print("no") }"#,
        r#"
        0: LoadTrue
        1: JumpIfFalse -> 9
        2: PushScope
        3: PrepareCall print/#0
        4: LoadConst "yes"
        5: CallPrepared #0
        6: Pop
        7: PopScope
        8: Jump -> 15
        9: PushScope
        10: PrepareCall print/#1
        11: LoadConst "no"
        12: CallPrepared #1
        13: Pop
        14: PopScope
        15: Halt
        "#,
    );
}

#[test]
fn while_with_break_and_continue() {
    assert_disasm(
        r#"while true {
            if false { continue }
            if false { break }
        }"#,
        r#"
        0: LoadTrue
        1: JumpIfFalse -> 19
        2: PushScope
        3: LoadFalse
        4: JumpIfFalse -> 10
        5: PushScope
        6: PopScope
        7: PopScope
        8: Jump -> 0
        9: PopScope
        10: LoadFalse
        11: JumpIfFalse -> 17
        12: PushScope
        13: PopScope
        14: PopScope
        15: Jump -> 19
        16: PopScope
        17: PopScope
        18: Jump -> 0
        19: Halt
        "#,
    );
}

#[test]
fn for_over_array() {
    assert_disasm(
        "for x in [1, 2] { print(x) }",
        r#"
        0: LoadConst 1
        1: LoadConst 2
        2: MakeArray 2
        3: MakeIter
        4: IterNext -> 13
        5: PushScope
        6: DefineLocal x
        7: PrepareCall print/#0
        8: LoadVar x
        9: CallPrepared #0
        10: Pop
        11: PopScope
        12: Jump -> 4
        13: Halt
        "#,
    );
}

#[test]
fn break_discards_only_the_broken_loops_sidecar() {
    let ast = parse(
        r#"for outer in [1] {
    while true { break }
    break
}
repeat 2 {
    if true { break }
}"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let cleanups: Vec<LoopStateKind> = chunk
        .code
        .iter()
        .filter_map(|instr| match instr {
            nybl_vm::Instr::PopLoopState(kind) => Some(*kind),
            _ => None,
        })
        .collect();

    // The inner while owns no stack sidecar, so its break must not consume
    // the enclosing for iterator. The outer for and repeat each clean up
    // exactly their own state, including the repeat break nested in `if`.
    assert_eq!(cleanups, [LoopStateKind::Iterator, LoopStateKind::Repeat]);
}

#[test]
fn continue_preserves_loop_sidecars() {
    let d = disasm("for x in [1] { continue }\nrepeat 1 { continue }");
    assert!(
        !d.contains("PopLoopState"),
        "continue must preserve loop state:\n{d}"
    );
}

#[test]
fn broken_for_and_repeat_emit_typed_cleanup() {
    let d = disasm("for x in [1] { break }\nrepeat 1 { break }");
    assert!(
        d.contains("PopLoopState iterator"),
        "for break missing iterator cleanup:\n{d}"
    );
    assert!(
        d.contains("PopLoopState repeat"),
        "repeat break missing counter cleanup:\n{d}"
    );
}

#[test]
fn top_level_loop_control_unwinds_runtime_scopes_before_transfer() {
    let break_ast = parse("for item in [1] { if true { break } }").expect("parse");
    let break_chunk = compile(&break_ast).expect("compile");
    assert!(
        break_chunk.code.windows(4).any(|window| matches!(
            window,
            [
                nybl_vm::Instr::PopScope,
                nybl_vm::Instr::PopScope,
                nybl_vm::Instr::PopLoopState(LoopStateKind::Iterator),
                nybl_vm::Instr::Jump(_),
            ]
        )),
        "break must unwind nested scopes before its iterator sidecar and jump"
    );

    let continue_ast = parse("repeat 2 { if true { continue } }").expect("parse");
    let continue_chunk = compile(&continue_ast).expect("compile");
    assert!(
        continue_chunk.code.windows(3).any(|window| matches!(
            window,
            [
                nybl_vm::Instr::PopScope,
                nybl_vm::Instr::PopScope,
                nybl_vm::Instr::Jump(_),
            ]
        )),
        "continue must unwind nested scopes before jumping"
    );
    assert!(
        !continue_chunk
            .code
            .iter()
            .any(|instr| matches!(instr, nybl_vm::Instr::PopLoopState(_))),
        "continue must preserve its loop sidecar"
    );
}

#[test]
fn function_loop_blocks_do_not_emit_runtime_scope_cleanup() {
    let ast = parse(
        r#"fn stop() {
  for item in [1] { if true { break } }
}"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let body = &chunk.functions[0].chunk.code;

    assert!(
        body.iter()
            .any(|instr| matches!(instr, nybl_vm::Instr::PopLoopState(LoopStateKind::Iterator))),
        "function break must retain iterator cleanup"
    );
    assert!(
        !body
            .iter()
            .any(|instr| matches!(instr, nybl_vm::Instr::PopScope)),
        "slot-resolved function blocks must not invent runtime scope cleanup"
    );
}

#[test]
fn repeat_loop() {
    assert_disasm(
        "repeat 3 { print(1) }",
        r#"
        0: LoadConst 3
        1: MakeRepeatCount
        2: RepeatNext -> 10
        3: PushScope
        4: PrepareCall print/#0
        5: LoadConst 1
        6: CallPrepared #0
        7: Pop
        8: PopScope
        9: Jump -> 2
        10: Halt
        "#,
    );
}

#[test]
fn function_declaration_and_call() {
    let d = disasm(
        r#"fn double(x) { return x * 2 }
print(double(5))"#,
    );
    assert!(
        d.contains("DefineFn #0 (double)"),
        "top-level missing fn def:\n{d}"
    );
    assert!(
        d.contains("PrepareCall double/#1"),
        "call site missing:\n{d}"
    );
    assert!(
        d.contains("fn #0 double(x):"),
        "nested body header missing:\n{d}"
    );
    // Param `x` resolves to the function's slot 0 (the compile-
    // time `LocalResolver` assigns params in declaration order),
    // so the body reads it via `LoadLocal @0` rather than the
    // old name-keyed `LoadVar`.
    assert!(d.contains("LoadLocal @0"), "body body missing:\n{d}");
    assert!(d.contains("Mul"), "body op missing:\n{d}");
    assert!(d.contains("Return"), "body return missing:\n{d}");
}

#[test]
fn duplicate_parameter_positions_map_to_canonical_binding_slots() {
    let ast = parse(
        r#"fn rebind(ref same, same, other, ref other) {
    same = other
    return same
}"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    validate_chunk(&chunk).expect("compiler-produced duplicate parameter metadata should validate");
    let function = &chunk.functions[0];

    assert_eq!(function.params, ["same", "same", "other", "other"]);
    assert_eq!(
        function.param_modes,
        [
            nybl::parser::ParamMode::Ref,
            nybl::parser::ParamMode::Value,
            nybl::parser::ParamMode::Value,
            nybl::parser::ParamMode::Ref,
        ]
    );
    assert_eq!(
        function.chunk.parameter_slots,
        [SlotIdx(0), SlotIdx(0), SlotIdx(1), SlotIdx(1)]
    );
    assert_eq!(function.slot_count, 2);
    assert_eq!(function.chunk.slot_count, 2);
    assert!(function.chunk.code.iter().all(|instruction| {
        !matches!(
            instruction,
            Instr::LoadLocal(slot) | Instr::StoreLocal(slot) if slot.0 >= 2
        )
    }));
}

#[test]
fn function_local_index_preserves_shadowing_scope_exit_and_unresolved_fallback() {
    let ast = parse(
        r#"fn inspect(ref value) {
    let local = 1
    let local = 2
    print(local)
    if true {
        let value = 3
        print(value)
    }
    print(value)
    print(missing)
}"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let function = &chunk.functions[0];
    let local_loads: Vec<SlotIdx> = function
        .chunk
        .code
        .iter()
        .filter_map(|instr| match instr {
            Instr::LoadLocal(slot) => Some(*slot),
            _ => None,
        })
        .collect();

    assert_eq!(function.param_modes, [nybl::parser::ParamMode::Ref]);
    assert_eq!(local_loads, [SlotIdx(2), SlotIdx(3), SlotIdx(0)]);
    assert!(function.chunk.code.iter().any(|instr| {
        matches!(
            instr,
            Instr::LoadVar(name) if function.chunk.names[name.0 as usize] == "missing"
        )
    }));
}

#[test]
fn ref_arguments_keep_parameter_and_local_slot_targets() {
    let ast = parse(
        r#"fn sink(ref value) {}
fn caller(ref parameter) {
    let local = 1
    sink(ref parameter)
    sink(ref local)
}"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let caller = chunk
        .functions
        .iter()
        .find(|function| function.name == "caller")
        .expect("caller function");
    let slots: Vec<SlotIdx> = caller
        .chunk
        .call_sites
        .iter()
        .filter_map(|site| match site.ref_targets.as_slice() {
            [Some(RefArgTarget::Binding(target))] => target.slot_idx(),
            _ => None,
        })
        .collect();

    assert_eq!(slots, [SlotIdx(0), SlotIdx(1)]);
}

#[test]
fn import_clash_metadata_shares_one_sorted_scope_with_exact_frontiers() {
    let ast = parse(
        r#"fn metadata(zebra) {
    let beta = 1
    use first
    let alpha = 2
    let beta = 3
    use second
}"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let body = &chunk.functions[0].chunk;
    let names = &body.local_scopes[0];
    let resolved: Vec<(&str, u32)> = names
        .entries
        .iter()
        .map(|entry| (body.name(entry.name), entry.first_binding))
        .collect();

    assert_eq!(body.local_scopes.len(), 1);
    assert_eq!(names.binding_count, 4);
    assert_eq!(resolved, [("alpha", 2), ("beta", 1), ("zebra", 0)]);
    assert_eq!(
        body.use_specs
            .iter()
            .map(|spec| spec.local_scope)
            .collect::<Vec<_>>(),
        [
            Some(LocalScopeSnapshot {
                scope: LocalScopeIdx(0),
                binding_count: 2,
            }),
            Some(LocalScopeSnapshot {
                scope: LocalScopeIdx(0),
                binding_count: 4,
            }),
        ]
    );
}

#[test]
fn functions_without_local_uses_retain_no_scope_metadata() {
    let ast = parse(
        r#"fn no_import(parameter) {
    let local = parameter
    if true {
        let nested = local
        print(nested)
    }
}"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");

    assert!(chunk.functions[0].chunk.local_scopes.is_empty());
}

#[test]
fn match_in_named_fn_emits_runtime_scope_pair() {
    let ast = parse(
        r#"fn read(value) {
  return match value { bound => bound }
}"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let body = &chunk.functions[0].chunk.code;

    assert!(
        body.iter()
            .any(|instr| matches!(instr, nybl_vm::Instr::PushScope)),
        "named-function match must open a runtime binding scope"
    );
    assert!(
        body.iter()
            .any(|instr| matches!(instr, nybl_vm::Instr::PopScope)),
        "named-function match must close its runtime binding scope"
    );
}

#[test]
fn method_call_records_back_assign_for_ident() {
    // `arr.push(1)` is mutating; the compiler records the live binding as an
    // in-place target instead of deep-cloning it through `LoadVar`.
    assert_disasm(
        "let arr = []\narr.push(1)",
        r#"
        0: MakeArray 0
        1: DefineLocal arr
        2: PrepareMethodNamed .push/#0 (target arr)
        3: LoadConst 1
        4: CallPrepared #0
        5: Pop
        6: Halt
        "#,
    );
}

#[test]
fn method_call_on_literal_has_no_back_assign() {
    // `[1,2].len()` operates on a literal expression; no back-assign.
    let d = disasm("print([1, 2].len())");
    assert!(
        d.contains("PrepareMethodValue .len/#1\n") || d.contains("PrepareMethodValue .len/#1 "),
        "expected prepared temporary method call in:\n{d}"
    );
    assert!(
        !d.contains("(back to"),
        "literal method call shouldn't back-assign:\n{d}"
    );
}

#[test]
fn index_get_and_set() {
    assert_disasm(
        "let a = [1,2]\na[0] = 99\nprint(a[1])",
        r#"
        0: LoadConst 1
        1: LoadConst 2
        2: MakeArray 2
        3: DefineLocal a
        4: LoadConst 99
        5: LoadConst 0
        6: AssignPlace a[?]
        7: PrepareCall print/#0
        8: LoadVar a
        9: LoadConst 1
        10: GetIndex
        11: CallPrepared #0
        12: Pop
        13: Halt
        "#,
    );
}

#[test]
fn compound_assign_on_index_is_target_aware() {
    let d = disasm("let a = [1]\na[0] += 1");
    assert!(
        d.contains("AssignPlace += a[?]"),
        "should mutate the live binding:\n{d}"
    );
    assert!(!d.contains("LoadVar a"), "must not clone receiver:\n{d}");
    assert!(
        !d.contains("StoreVar a"),
        "must not clone-store receiver:\n{d}"
    );
}

#[test]
fn named_field_assign_uses_in_place_target() {
    let d = disasm(
        r#"struct Counter { n }
let c = Counter { n: 1 }
c.n += 2"#,
    );
    assert!(
        d.contains("AssignPlace += c.n"),
        "should mutate the live binding:\n{d}"
    );
    assert!(!d.contains("LoadVar c"), "must not clone receiver:\n{d}");
    assert!(
        !d.contains("StoreVar c"),
        "must not clone-store receiver:\n{d}"
    );
}

#[test]
fn function_assignment_targets_use_slots_in_place() {
    let d = disasm(
        r#"fn update(a) {
    a[0] = 4
    return a
}
print(update([1]))"#,
    );
    assert!(
        d.contains("AssignPlace @0[?]"),
        "function local should use direct slot target:\n{d}"
    );
}

#[test]
fn string_interpolation_uses_recipe() {
    let d = disasm(
        r#"let name = "nybl"
print("hi {name}!")"#,
    );
    assert!(
        d.contains(r#"StringInterp ["hi ", $name, "!"]"#),
        "interpolation recipe not rendered as expected:\n{d}"
    );
}

#[test]
fn string_interpolation_resolves_function_bindings_to_slots() {
    let d = disasm(
        r#"fn greet(name) {
    let punctuation = "!"
    return "hi {name}{punctuation}"
}
print(greet("nybl"))"#,
    );
    assert!(
        d.contains(r#"StringInterp ["hi ", $@0, $@1]"#),
        "function interpolation should reference parameter/local slots:\n{d}"
    );
}

#[test]
fn string_interpolation_only_reference_is_a_lambda_capture() {
    let ast = parse(
        r#"let outer = "captured"
let read = fn() { return "{outer}" }"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let lambda = &chunk.functions[0];

    assert_eq!(lambda.capture_names, ["outer"]);
    assert!(
        disassemble(&lambda.chunk).contains("StringInterp [$outer]"),
        "unresolved interpolation binding should use the named capture path"
    );
}

#[test]
fn dict_literal_layout() {
    // Keys go in as Str constants, values get compiled; MakeDict pops both.
    assert_disasm(
        r#"let d = {"a": 1, "b": 2}"#,
        r#"
        0: LoadConst "a"
        1: LoadConst 1
        2: LoadConst "b"
        3: LoadConst 2
        4: MakeDict 2
        5: DefineLocal d
        6: Halt
        "#,
    );
}

#[test]
fn if_expression_leaves_value_on_stack() {
    // Used as an expression rather than a statement: no `Pop` at the end
    // because the value is consumed by the enclosing `DefineLocal`.
    assert_disasm(
        "let x = if true { 1 } else { 2 }",
        r#"
        0: LoadTrue
        1: JumpIfFalse -> 4
        2: LoadConst 1
        3: Jump -> 5
        4: LoadConst 2
        5: DefineLocal x
        6: Halt
        "#,
    );
}

#[test]
fn peephole_fusion_stops_at_if_expression_jump_targets() {
    let source = r#"
fn add_const(c, a, b) { return (if c { a } else { b }) + 1 }
fn add_local(c, a, b, d) { return (if c { a } else { b }) + d }
fn sub_const(c, a, b) { return (if c { a } else { b }) - 1 }
fn lt_const(c, a, b) { return (if c { a } else { b }) < 15 }
fn lt_local(c, a, b, d) { return (if c { a } else { b }) < d }
"#;
    let ast = parse(source).expect("parse");
    let chunk = compile(&ast).expect("compile");

    for (name, operator) in [
        ("add_const", Instr::Add),
        ("add_local", Instr::Add),
        ("sub_const", Instr::Sub),
        ("lt_const", Instr::Lt),
        ("lt_local", Instr::Lt),
    ] {
        let function = chunk
            .functions
            .iter()
            .find(|function| function.name == name)
            .unwrap_or_else(|| panic!("missing function {name}"));
        let operator_offset = function
            .chunk
            .code
            .iter()
            .position(|instr| *instr == operator)
            .unwrap_or_else(|| panic!("{name} lost its generic operator"));
        let end_target = function
            .chunk
            .code
            .iter()
            .find_map(|instr| match instr {
                Instr::Jump(target) => Some(target.0 as usize),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{name} missing if-expression end jump"));

        assert_eq!(
            end_target + 1,
            operator_offset,
            "{name} must land on the right operand immediately before the operator"
        );
        assert!(
            matches!(
                function.chunk.code[end_target],
                Instr::LoadConst(_) | Instr::LoadLocal(_)
            ),
            "{name} jump target must remain an independently executable operand"
        );
    }
}

#[test]
fn peephole_fusion_resumes_after_a_jump_target_and_is_chunk_local() {
    let ast = parse(
        r#"
let module_value = if true { 1 } else { 2 }
fn select_then_add(c, a, b, d) {
    let selected = if c { a } else { b }
    return selected + d
}
fn plain_add(a, b) { return a + b }
"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");

    for name in ["select_then_add", "plain_add"] {
        let function = chunk
            .functions
            .iter()
            .find(|function| function.name == name)
            .unwrap_or_else(|| panic!("missing function {name}"));
        assert!(
            function
                .chunk
                .code
                .iter()
                .any(|instr| matches!(instr, Instr::AddLocals(_, _))),
            "{name} should retain a valid local-local fusion: {:?}",
            function.chunk.code
        );
    }
}

#[test]
fn store_local_fuses_small_integer_direct_and_compound_add_updates() {
    let ast = parse(
        r#"
fn update(value) {
    value = value + 1
    value += 2
    return value
}
"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let function = chunk
        .functions
        .iter()
        .find(|function| function.name == "update")
        .expect("update function");

    assert_eq!(
        &function.chunk.code[..2],
        &[
            Instr::IncLocalInt(SlotIdx(0), 1),
            Instr::IncLocalInt(SlotIdx(0), 2),
        ]
    );
    let output = disassemble(&chunk);
    for delta in [1, 2] {
        assert!(
            output.contains(&format!("IncLocalInt @0, {delta}")),
            "missing fused delta {delta}:\n{output}"
        );
    }
}

#[test]
fn subtraction_remains_on_the_generic_operator_path() {
    let ast = parse(
        r#"
fn update(value) {
    value = value - 3
    value -= 4
    return value
}
"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let function = chunk
        .functions
        .iter()
        .find(|function| function.name == "update")
        .expect("update function");

    assert!(matches!(
        function.chunk.code.as_slice(),
        [
            Instr::LoadLocal(SlotIdx(0)),
            Instr::LoadConst(_),
            Instr::Sub,
            Instr::StoreLocal(SlotIdx(0)),
            Instr::LoadLocal(SlotIdx(0)),
            Instr::LoadConst(_),
            Instr::Sub,
            Instr::StoreLocal(SlotIdx(0)),
            Instr::LoadLocal(SlotIdx(0)),
            Instr::Return,
            Instr::ReturnNone,
        ]
    ));
}

#[test]
fn store_local_fusion_requires_the_same_slot_and_an_i32_delta() {
    let ast = parse(
        r#"
fn update(source, target) {
    target = source + 1
    target = target + 2147483648
    return target
}
"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let function = chunk
        .functions
        .iter()
        .find(|function| function.name == "update")
        .expect("update function");

    assert!(matches!(
        function.chunk.code.as_slice(),
        [
            Instr::LoadLocalAddInt(SlotIdx(0), 1),
            Instr::StoreLocal(SlotIdx(1)),
            Instr::LoadLocal(SlotIdx(1)),
            Instr::LoadConst(_),
            Instr::Add,
            Instr::StoreLocal(SlotIdx(1)),
            Instr::LoadLocal(SlotIdx(1)),
            Instr::Return,
            Instr::ReturnNone,
        ]
    ));
}

#[test]
fn store_local_fusion_does_not_consume_an_if_expression_end_target() {
    let ast = parse(
        r#"
fn update(condition, value) {
    value = if condition { value + 1 } else { value + 2 }
    value += 3
    return value
}
"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let function = chunk
        .functions
        .iter()
        .find(|function| function.name == "update")
        .expect("update function");
    let code = &function.chunk.code;
    let end_target = code
        .iter()
        .find_map(|instr| match instr {
            Instr::Jump(target) => Some(target.0 as usize),
            _ => None,
        })
        .expect("if-expression end jump");

    assert_eq!(
        code[end_target],
        Instr::StoreLocal(SlotIdx(1)),
        "the end jump must still land on the standalone assignment store"
    );
    assert_eq!(
        code.iter()
            .filter(|instr| matches!(instr, Instr::IncLocalInt(SlotIdx(1), 3)))
            .count(),
        1,
        "fusion should resume immediately after the protected store: {code:?}"
    );
}

#[test]
fn nested_function_has_its_own_chunk() {
    let d = disasm(
        r#"fn outer() {
    fn inner() { return 1 }
    return inner()
}"#,
    );
    assert!(d.contains("DefineFn #0 (outer)"), "outer fn missing:\n{d}");
    assert!(d.contains("fn #0 outer"), "outer body missing:\n{d}");
    assert!(
        d.contains("DefineFn #0 (inner)"),
        "inner fn inside outer missing:\n{d}"
    );
    assert!(d.contains("fn #0 inner"), "inner body missing:\n{d}");
}

#[test]
fn unresolved_lambda_name_compiles_as_a_parent_scope_candidate() {
    let ast = parse(
        r#"fn build() {
    return fn() { return missing }
}"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let lambda = &chunk.functions[0].chunk.functions[0];

    assert_eq!(lambda.capture_names, ["missing"]);
    assert!(matches!(
        lambda.capture_sources.as_slice(),
        [nybl_vm::chunk::CaptureSource::ParentScope(name)] if name == "missing"
    ));
}

#[test]
fn free_variable_order_and_nested_propagation_are_deterministic() {
    let ast = parse(
        r#"fn build() {
    return fn() {
        let local = second
        return fn() { return first + missing + first }
    }
}
fn isolated() {
    return fn() { return other + other }
}"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");

    let build = &chunk.functions[0];
    let outer_lambda = &build.chunk.functions[0];
    let inner_lambda = &outer_lambda.chunk.functions[0];
    assert_eq!(inner_lambda.capture_names, ["first", "missing"]);
    assert_eq!(
        outer_lambda.capture_names,
        ["second", "first", "missing"],
        "nested ParentScope captures must propagate after earlier direct reads"
    );
    assert_eq!(build.capture_names, ["second", "first", "missing"]);

    let isolated = &chunk.functions[1];
    let isolated_lambda = &isolated.chunk.functions[0];
    assert_eq!(isolated_lambda.capture_names, ["other"]);
    assert_eq!(
        isolated.capture_names,
        ["other"],
        "a sibling function must start with a fresh free-variable collector"
    );
}

#[test]
fn runtime_bindings_do_not_enter_the_static_free_variable_index() {
    let ast = parse(
        r#"fn build(value) {
    return match value {
        bound => bound + outside,
    }
}"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let function = &chunk.functions[0];

    assert_eq!(function.capture_names, ["outside"]);
}

#[test]
fn large_unique_free_variable_compilation_keeps_first_seen_order() {
    const COUNT: usize = 8_192;
    let mut source = String::from("fn collect() {\n");
    for index in 0..COUNT {
        source.push_str(&format!("unresolved_{index:05}\n"));
    }
    source.push_str("}\n");

    let ast = parse(&source).expect("parse");
    let chunk = compile(&ast).expect("compile");
    let function = &chunk.functions[0];

    assert_eq!(function.capture_names.len(), COUNT);
    assert_eq!(
        function.capture_names.first().map(String::as_str),
        Some("unresolved_00000")
    );
    assert_eq!(
        function.capture_names.last().map(String::as_str),
        Some("unresolved_08191")
    );
}

#[test]
fn break_outside_loop_is_compile_error() {
    let ast = parse("break").expect("parse");
    let err = compile(&ast).expect_err("should reject break outside loop");
    assert!(
        err.message.contains("outside of a loop"),
        "wrong error: {}",
        err.message
    );
}

#[test]
fn continue_outside_loop_is_compile_error() {
    let ast = parse("continue").expect("parse");
    let err = compile(&ast).expect_err("should reject continue outside loop");
    assert!(
        err.message.contains("outside of a loop"),
        "wrong error: {}",
        err.message
    );
}

#[test]
fn constants_and_names_are_deduplicated() {
    let ast = parse(
        r#"let x = "hi"
let y = "hi"
print(x)
print(x)"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    // Only one "hi" constant: the string appears once in the pool.
    let hi_count = chunk
        .constants
        .iter()
        .filter(|c| matches!(c, nybl_vm::Constant::Str(s) if s == "hi"))
        .count();
    assert_eq!(hi_count, 1, "expected deduplicated string constant");
    // Only one `x` name entry.
    let x_count = chunk.names.iter().filter(|n| n.as_str() == "x").count();
    assert_eq!(x_count, 1, "expected deduplicated name entry");
    // But `y` is its own name.
    assert!(chunk.names.iter().any(|n| n == "y"));
}

#[test]
fn nested_function_pool_indexes_remain_chunk_local() {
    let ast = parse(
        r#"let outer_before = "shared"
fn nested() {
    let inner_first = "shared"
    let inner_second = "inner"
    print(inner_first, inner_second, external_inner, external_inner)
}
let outer_after = "outer"
print(outer_before, outer_after, external_outer, external_outer)"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let nested = &chunk.functions[0].chunk;

    assert!(matches!(
        chunk.constants.as_slice(),
        [Constant::Str(first), Constant::Str(second)]
            if first == "shared" && second == "outer"
    ));
    assert!(matches!(
        nested.constants.as_slice(),
        [Constant::Str(first), Constant::Str(second)]
            if first == "shared" && second == "inner"
    ));
    assert_eq!(
        chunk
            .names
            .iter()
            .filter(|name| name.as_str() == "external_outer")
            .count(),
        1
    );
    assert_eq!(
        nested
            .names
            .iter()
            .filter(|name| name.as_str() == "external_inner")
            .count(),
        1
    );
}

#[test]
fn instruction_and_line_tables_have_equal_length() {
    let sources = [
        "",
        "print(1)",
        "let x = 1\nx += 2",
        "if true { print(1) } else { print(2) }",
        "for x in [1, 2, 3] { print(x) }",
        "fn f() { return 1 }\nprint(f())",
    ];
    for src in sources {
        let ast = parse(src).unwrap();
        let chunk = compile(&ast).unwrap();
        assert_eq!(
            chunk.code.len(),
            chunk.lines.len(),
            "code/lines length mismatch for `{src}`"
        );
        for func in &chunk.functions {
            assert_eq!(
                func.chunk.code.len(),
                func.chunk.lines.len(),
                "fn `{}` code/lines length mismatch",
                func.name
            );
        }
    }
}

#[test]
fn cloned_chunks_share_immutable_function_pattern_and_interpolation_pools() {
    let ast = parse(
        r#"let add = fn(x) { return x + 1 }
let text = "value={add}"
let label = match 1 { 1 => "one", _ => "other" }"#,
    )
    .expect("parse");
    let chunk = compile(&ast).expect("compile");
    let cloned = chunk.clone();

    assert!(Arc::ptr_eq(
        &chunk.functions[0].chunk,
        &cloned.functions[0].chunk,
    ));
    assert!(Arc::ptr_eq(
        &chunk.patterns[0].pattern,
        &cloned.patterns[0].pattern,
    ));
    assert!(Arc::ptr_eq(
        &chunk.interps[0].parts,
        &cloned.interps[0].parts,
    ));

    let function_body = Arc::downgrade(&chunk.functions[0].chunk);
    let pattern = Arc::downgrade(&chunk.patterns[0].pattern);
    let interpolation = Arc::downgrade(&chunk.interps[0].parts);
    drop(cloned);
    drop(chunk);
    assert!(function_body.upgrade().is_none());
    assert!(pattern.upgrade().is_none());
    assert!(interpolation.upgrade().is_none());
}
