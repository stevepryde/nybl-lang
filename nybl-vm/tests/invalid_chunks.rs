use std::sync::Arc;

use nybl::parser::ParamMode;
use nybl::{NyblHost, NyblLimits, Value};
use nybl_vm::chunk::{
    CallSite, CallSiteIdx, Chunk, CodeOffset, ConstIdx, ConstructFieldsIdx, EnumConstructShape,
    EnumIdx, FnDef, FnIdx, Instr, InterpIdx, InterpPart, InterpRecipe, LocalScopeIdx,
    LocalScopeName, LocalScopeNames, LocalScopeSnapshot, NameIdx, NamespaceIdx, NamespaceRef,
    PatternIdx, PatternRecipe, RefArgTarget, SlotIdx, StructIdx, UseIdx, UseSpec,
};
use nybl_vm::{Vm, execute};

#[derive(Default)]
struct SilentHost;

impl NyblHost for SilentHost {
    fn call(
        &mut self,
        _name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, nybl::NyblError>> {
        None
    }
}

#[test]
fn execute_rejects_invalid_call_site_metadata_and_indexes() {
    let missing_site = execution_error(chunk_with(Instr::PrepareCallValue {
        site: CallSiteIdx(0),
    }));
    assert!(missing_site.message.contains("call site 0"));

    let mut mismatched = chunk_with(Instr::Halt);
    mismatched.call_sites.push(CallSite {
        arg_modes: vec![ParamMode::Ref],
        ref_targets: vec![],
    });
    let mismatch = execution_error(mismatched);
    assert!(
        mismatch
            .message
            .contains("1 argument modes but 0 ref-target entries")
    );

    let mut invalid_target = chunk_with(Instr::Halt);
    invalid_target.names.push("value".into());
    invalid_target.call_sites.push(CallSite {
        arg_modes: vec![ParamMode::Ref],
        ref_targets: vec![Some(RefArgTarget::Binding(NamespaceRef::from_slot(
            NameIdx(0),
            SlotIdx(0),
        )))],
    });
    let target = execution_error(invalid_target);
    assert!(target.message.contains("local slot 0"));

    let mut duplicate_slot = chunk_with(Instr::Halt);
    duplicate_slot.names = vec!["left".into(), "right".into()];
    duplicate_slot.call_sites.push(CallSite {
        arg_modes: vec![ParamMode::Ref, ParamMode::Ref],
        ref_targets: vec![
            Some(RefArgTarget::Binding(NamespaceRef::from_slot(
                NameIdx(0),
                SlotIdx(0),
            ))),
            Some(RefArgTarget::Binding(NamespaceRef::from_slot(
                NameIdx(1),
                SlotIdx(0),
            ))),
        ],
    });
    let duplicate = execution_error(duplicate_slot);
    assert!(
        duplicate
            .message
            .contains("uses local slot 0 for more than one ref target")
    );
}

fn chunk_with(instr: Instr) -> Chunk {
    Chunk {
        code: vec![instr, Instr::Halt],
        lines: vec![7, 0],
        ..Chunk::new()
    }
}

fn execution_error(chunk: Chunk) -> nybl::NyblError {
    execute(chunk, &mut SilentHost, &NyblLimits::standard())
        .expect_err("malformed bytecode must be rejected")
}

#[test]
fn execute_accepts_a_valid_hand_built_chunk() {
    let chunk = Chunk {
        code: vec![Instr::Halt],
        lines: vec![1],
        ..Chunk::new()
    };

    execute(chunk, &mut SilentHost, &NyblLimits::standard()).unwrap();
}

#[test]
fn execute_rejects_mismatched_code_and_line_tables() {
    let error = execution_error(Chunk {
        code: vec![Instr::Halt],
        lines: vec![],
        ..Chunk::new()
    });

    assert_eq!(error.line, Some(0));
    assert!(error.message.contains("1 instructions but 0 source lines"));
}

#[test]
fn execute_rejects_every_index_pool_without_panicking() {
    let cases = [
        (Instr::LoadConst(ConstIdx(0)), "constant 0"),
        (Instr::LoadVar(NameIdx(0)), "name 0"),
        (Instr::StringInterp(InterpIdx(0)), "interpolation 0"),
        (Instr::DefineFn(FnIdx(0)), "function 0"),
        (Instr::DefineStruct(StructIdx(0)), "struct definition 0"),
        (Instr::DefineEnum(EnumIdx(0)), "enum definition 0"),
        (Instr::Use(UseIdx(0)), "use specification 0"),
        (
            Instr::ConstructStruct {
                namespace: Some(NamespaceIdx::new(0)),
                type_name: NameIdx(0),
                count: 0,
            },
            "namespace reference 0",
        ),
        (
            Instr::MatchFail {
                pattern: PatternIdx(0),
                on_fail: CodeOffset(1),
            },
            "pattern 0",
        ),
    ];

    for (instr, expected) in cases {
        let error = execution_error(chunk_with(instr));
        assert_eq!(error.line, Some(7));
        assert!(
            error.message.contains(expected),
            "expected `{expected}` in `{}`",
            error.message
        );
    }
}

#[test]
fn execute_rejects_invalid_interpolation_parts() {
    let mut chunk = chunk_with(Instr::StringInterp(InterpIdx(0)));
    chunk.interps.push(InterpRecipe {
        parts: Arc::from([InterpPart::Name(NameIdx(0))]),
    });

    let error = execution_error(chunk);
    assert!(error.message.contains("interpolation 0 references name 0"));
}

fn chunk_with_local_scope(
    names: Vec<&str>,
    scope: LocalScopeNames,
    snapshot: LocalScopeSnapshot,
) -> Chunk {
    Chunk {
        code: vec![Instr::Use(UseIdx(0)), Instr::Halt],
        lines: vec![7, 0],
        names: names.into_iter().map(str::to_string).collect(),
        local_scopes: vec![scope],
        use_specs: vec![UseSpec {
            path: "dep".into(),
            items: None,
            alias: None,
            local_scope: Some(snapshot),
        }],
        ..Chunk::new()
    }
}

#[test]
fn execute_rejects_malformed_local_scope_snapshots() {
    let missing_scope = execution_error(Chunk {
        code: vec![Instr::Use(UseIdx(0)), Instr::Halt],
        lines: vec![7, 0],
        use_specs: vec![UseSpec {
            path: "dep".into(),
            items: None,
            alias: None,
            local_scope: Some(LocalScopeSnapshot {
                scope: LocalScopeIdx(0),
                binding_count: 0,
            }),
        }],
        ..Chunk::new()
    });
    assert!(
        missing_scope
            .message
            .contains("use specification 0 references local scope 0")
    );

    let excessive_frontier = execution_error(chunk_with_local_scope(
        vec![],
        LocalScopeNames::default(),
        LocalScopeSnapshot {
            scope: LocalScopeIdx(0),
            binding_count: 1,
        },
    ));
    assert!(
        excessive_frontier
            .message
            .contains("sees 1 local bindings but local scope 0 records only 0")
    );
}

#[test]
fn execute_rejects_malformed_local_scope_name_indexes() {
    let invalid_name = execution_error(chunk_with_local_scope(
        vec![],
        LocalScopeNames {
            binding_count: 1,
            entries: vec![LocalScopeName {
                name: NameIdx(0),
                first_binding: 0,
            }],
        },
        LocalScopeSnapshot {
            scope: LocalScopeIdx(0),
            binding_count: 1,
        },
    ));
    assert!(
        invalid_name
            .message
            .contains("local scope 0 references name 0")
    );

    let invalid_binding = execution_error(chunk_with_local_scope(
        vec!["name"],
        LocalScopeNames {
            binding_count: 1,
            entries: vec![LocalScopeName {
                name: NameIdx(0),
                first_binding: 1,
            }],
        },
        LocalScopeSnapshot {
            scope: LocalScopeIdx(0),
            binding_count: 1,
        },
    ));
    assert!(
        invalid_binding
            .message
            .contains("introduces `name` at binding 1 but records only 1 bindings")
    );
}

#[test]
fn execute_rejects_unsorted_or_duplicate_local_scope_names() {
    for entries in [
        vec![
            LocalScopeName {
                name: NameIdx(1),
                first_binding: 0,
            },
            LocalScopeName {
                name: NameIdx(0),
                first_binding: 1,
            },
        ],
        vec![
            LocalScopeName {
                name: NameIdx(0),
                first_binding: 0,
            },
            LocalScopeName {
                name: NameIdx(0),
                first_binding: 1,
            },
        ],
    ] {
        let error = execution_error(chunk_with_local_scope(
            vec!["alpha", "beta"],
            LocalScopeNames {
                binding_count: 2,
                entries,
            },
            LocalScopeSnapshot {
                scope: LocalScopeIdx(0),
                binding_count: 2,
            },
        ));
        assert!(
            error.message.contains("names are not strictly sorted"),
            "{}",
            error.message
        );
    }
}

#[test]
fn execute_rejects_duplicate_local_scope_binding_positions() {
    let error = execution_error(chunk_with_local_scope(
        vec!["alpha", "beta"],
        LocalScopeNames {
            binding_count: 2,
            entries: vec![
                LocalScopeName {
                    name: NameIdx(0),
                    first_binding: 0,
                },
                LocalScopeName {
                    name: NameIdx(1),
                    first_binding: 0,
                },
            ],
        },
        LocalScopeSnapshot {
            scope: LocalScopeIdx(0),
            binding_count: 2,
        },
    ));
    assert!(
        error
            .message
            .contains("assigns binding 0 to more than one name")
    );
}

#[test]
fn execute_rejects_invalid_construction_field_recipe_pool() {
    let mut chunk = chunk_with(Instr::ValidateStructConstruct {
        namespace: None,
        type_name: NameIdx(0),
        fields: ConstructFieldsIdx(0),
    });
    chunk.names.push("Point".into());

    let error = execution_error(chunk);
    assert!(error.message.contains("construction field recipe 0"));
}

#[test]
fn execute_rejects_top_level_local_slots_and_out_of_stream_jumps() {
    let slot_error = execution_error(chunk_with(Instr::LoadLocal(SlotIdx(0))));
    assert!(slot_error.message.contains("local slot 0"));

    let jump_error = execution_error(chunk_with(Instr::Jump(CodeOffset(2))));
    assert!(jump_error.message.contains("jump target 2"));
}

#[test]
fn execute_rejects_invalid_namespace_references() {
    let mut construct = chunk_with(Instr::ConstructStruct {
        namespace: Some(NamespaceIdx::new(0)),
        type_name: NameIdx(1),
        count: 0,
    });
    construct.names = vec!["module".into(), "Point".into()];
    construct
        .namespace_refs
        .push(NamespaceRef::from_slot(NameIdx(0), SlotIdx(0)));
    let slot_error = execution_error(construct);
    assert!(slot_error.message.contains("local slot 0"));

    let mut struct_preflight = chunk_with(Instr::ValidateStructConstruct {
        namespace: Some(NamespaceIdx::new(0)),
        type_name: NameIdx(0),
        fields: ConstructFieldsIdx(0),
    });
    struct_preflight.names.push("Point".into());
    struct_preflight
        .namespace_refs
        .push(NamespaceRef::from_name(NameIdx(1)));
    struct_preflight.construct_fields.push(vec![]);
    let preflight_name_error = execution_error(struct_preflight);
    assert!(preflight_name_error.message.contains("name 1"));

    let mut enum_preflight = chunk_with(Instr::ValidateEnumConstruct {
        namespace: Some(NamespaceIdx::new(0)),
        type_name: NameIdx(1),
        variant: NameIdx(2),
        shape: EnumConstructShape::Unit,
        fields: ConstructFieldsIdx(0),
    });
    enum_preflight.names = vec!["module".into(), "Maybe".into(), "Some".into()];
    enum_preflight
        .namespace_refs
        .push(NamespaceRef::from_slot(NameIdx(0), SlotIdx(0)));
    enum_preflight.construct_fields.push(vec![]);
    let preflight_slot_error = execution_error(enum_preflight);
    assert!(preflight_slot_error.message.contains("local slot 0"));

    let mut pattern = chunk_with(Instr::MatchFail {
        pattern: PatternIdx(0),
        on_fail: CodeOffset(1),
    });
    pattern.patterns.push(PatternRecipe {
        pattern: Arc::new(nybl::parser::Pattern::Wildcard),
        namespaces: vec![("module".into(), NamespaceRef::from_name(NameIdx(0)))],
    });
    let name_error = execution_error(pattern);
    assert!(name_error.message.contains("pattern 0 references name 0"));
}

#[test]
fn execute_recursively_validates_function_chunks_and_capture_metadata() {
    let child = Chunk {
        code: vec![Instr::LoadConst(ConstIdx(0)), Instr::ReturnNone],
        lines: vec![11, 0],
        slot_count: 1,
        parameter_slots: vec![SlotIdx(0)],
        ..Chunk::new()
    };
    let function = FnDef {
        name: "broken".into(),
        params: vec!["x".into()],
        param_modes: vec![ParamMode::Value],
        chunk: Arc::new(child),
        slot_count: 1,
        capture_names: vec![],
        capture_sources: vec![],
    };
    let mut outer = chunk_with(Instr::DefineFn(FnIdx(0)));
    outer.functions.push(function);

    let error = execution_error(outer);
    assert_eq!(error.line, Some(11));
    assert!(error.message.contains("nested function 0 `broken`"));
    assert!(error.message.contains("constant 0"));
}

#[test]
fn shared_function_chunk_dags_are_validated_once_without_recursive_traversal() {
    let leaf = Arc::new(Chunk {
        code: vec![Instr::ReturnNone],
        lines: vec![0],
        ..Chunk::new()
    });
    let mut shared = leaf;
    for depth in 0..24 {
        let function = |name: &str| FnDef {
            name: format!("{name}_{depth}"),
            params: vec![],
            param_modes: vec![],
            chunk: Arc::clone(&shared),
            slot_count: 0,
            capture_names: vec![],
            capture_sources: vec![],
        };
        shared = Arc::new(Chunk {
            code: vec![Instr::ReturnNone],
            lines: vec![0],
            functions: vec![function("left"), function("right")],
            ..Chunk::new()
        });
    }

    let top = Chunk {
        code: vec![Instr::Halt],
        lines: vec![0],
        functions: vec![FnDef {
            name: "root".into(),
            params: vec![],
            param_modes: vec![],
            chunk: shared,
            slot_count: 0,
            capture_names: vec![],
            capture_sources: vec![],
        }],
        ..Chunk::new()
    };

    execute(top, &mut SilentHost, &NyblLimits::standard()).unwrap();
}

#[test]
fn execute_rejects_sparse_or_absurd_function_slot_metadata() {
    let child = Chunk {
        code: vec![Instr::ReturnNone],
        lines: vec![0],
        slot_count: u32::MAX,
        ..Chunk::new()
    };
    let function = FnDef {
        name: "oversized".into(),
        params: vec![],
        param_modes: vec![],
        chunk: Arc::new(child),
        slot_count: u32::MAX,
        capture_names: vec![],
        capture_sources: vec![],
    };
    let mut outer = chunk_with(Instr::DefineFn(FnIdx(0)));
    outer.functions.push(function);

    let error = execution_error(outer);
    assert!(error.message.contains("only 0 are densely declared"));
}

#[test]
fn shared_chunks_are_validated_for_each_distinct_parameter_layout() {
    let child = Arc::new(Chunk {
        code: vec![Instr::ReturnNone],
        lines: vec![0],
        slot_count: 1,
        parameter_slots: vec![SlotIdx(0)],
        ..Chunk::new()
    });
    let function = |name: &str, params: Vec<String>| {
        let param_modes = vec![ParamMode::Value; params.len()];
        FnDef {
            name: name.into(),
            params,
            param_modes,
            chunk: Arc::clone(&child),
            slot_count: 1,
            capture_names: vec![],
            capture_sources: vec![],
        }
    };
    let mut outer = chunk_with(Instr::Halt);
    // LIFO traversal sees `valid` first. Pointer-only memoization would then
    // skip the invalid zero-parameter layout for the same Arc allocation.
    outer.functions = vec![
        function("invalid", vec![]),
        function("valid", vec!["value".into()]),
    ];

    let error = execution_error(outer);
    assert!(
        error
            .message
            .contains("0 parameters but 1 parameter-slot entries")
    );
}

#[test]
fn parameter_binding_metadata_must_match_names_and_slots() {
    let function = |params: &[&str], parameter_slots: &[u32], slot_count: u32| FnDef {
        name: "binding".into(),
        params: params.iter().map(|name| (*name).into()).collect(),
        param_modes: vec![ParamMode::Value; params.len()],
        chunk: Arc::new(Chunk {
            code: vec![Instr::ReturnNone],
            lines: vec![0],
            slot_count,
            parameter_slots: parameter_slots.iter().copied().map(SlotIdx).collect(),
            ..Chunk::new()
        }),
        slot_count,
        capture_names: vec![],
        capture_sources: vec![],
    };
    let outer = |function| Chunk {
        code: vec![Instr::DefineFn(FnIdx(0)), Instr::Halt],
        lines: vec![1, 0],
        functions: vec![function],
        ..Chunk::new()
    };

    let missing = execution_error(outer(function(&["x"], &[], 1)));
    assert!(
        missing
            .message
            .contains("1 parameters but 0 parameter-slot entries")
    );

    let out_of_range = execution_error(outer(function(&["x"], &[1], 1)));
    assert!(out_of_range.message.contains("out-of-range local slot 1"));

    let split_duplicate = execution_error(outer(function(&["x", "x"], &[0, 1], 2)));
    assert!(
        split_duplicate
            .message
            .contains("repeated parameter `x` to both local slots 0 and 1")
    );

    let aliased_distinct = execution_error(outer(function(&["x", "y"], &[0, 0], 1)));
    assert!(
        aliased_distinct
            .message
            .contains("distinct parameters `x` and `y` to local slot 0")
    );

    execute(
        outer(function(&["x", "x"], &[0, 0], 1)),
        &mut SilentHost,
        &NyblLimits::standard(),
    )
    .expect("duplicate spellings may intentionally share a binding slot");
}

#[test]
fn rest_parameter_metadata_must_be_unique_and_final() {
    let function = |modes: Vec<ParamMode>| FnDef {
        name: "rest".into(),
        params: (0..modes.len()).map(|index| format!("p{index}")).collect(),
        param_modes: modes.clone(),
        chunk: Arc::new(Chunk {
            code: vec![Instr::ReturnNone],
            lines: vec![0],
            slot_count: modes.len() as u32,
            parameter_slots: (0..modes.len() as u32).map(SlotIdx).collect(),
            ..Chunk::new()
        }),
        slot_count: modes.len() as u32,
        capture_names: vec![],
        capture_sources: vec![],
    };
    let outer = |function| Chunk {
        code: vec![Instr::DefineFn(FnIdx(0)), Instr::Halt],
        lines: vec![1, 0],
        functions: vec![function],
        ..Chunk::new()
    };

    let non_final = execution_error(outer(function(vec![ParamMode::Rest, ParamMode::Value])));
    assert!(
        non_final
            .message
            .contains("invalid rest-parameter metadata")
    );
    let duplicate = execution_error(outer(function(vec![ParamMode::Rest, ParamMode::Rest])));
    assert!(
        duplicate
            .message
            .contains("invalid rest-parameter metadata")
    );
}

#[test]
fn public_surface_metadata_must_not_repeat_names() {
    let chunk = Chunk {
        code: vec![Instr::Halt],
        lines: vec![0],
        public_surface: Some(vec!["same".into(), "same".into()]),
        ..Chunk::new()
    };
    let error = execution_error(chunk);
    assert!(error.message.contains("duplicate name `same`"));
}

#[test]
fn trusted_vm_api_checks_every_fused_local_slot_instruction() {
    let instructions = [
        Instr::AddLocals(SlotIdx(0), SlotIdx(0)),
        Instr::LtLocals(SlotIdx(0), SlotIdx(0)),
        Instr::IncLocalInt(SlotIdx(0), 1),
        Instr::LoadLocalAddInt(SlotIdx(0), 1),
        Instr::LtLocalInt(SlotIdx(0), 1),
    ];

    for instr in instructions {
        let mut host = SilentHost;
        let error = Vm::new(chunk_with(instr), &mut host, NyblLimits::standard())
            .run()
            .expect_err("invalid fused slot must return an error");
        assert_eq!(error.line, Some(7));
        assert_eq!(error.message, "VM: local slot out of range");
    }
}
