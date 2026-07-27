//! Human-readable rendering of a [`Chunk`] for debugging and tests.
//!
//! The output is stable-ish — assertions in tests match against it
//! directly. Each instruction renders to a single line with its
//! operand resolved inline (constants, names, jump targets). Nested
//! functions are recursively rendered after the main body.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

use crate::chunk::{Chunk, Constant, Instr, InterpPart, LoopStateKind, NamespaceIdx};

/// Render a chunk as a string. One line per instruction; nested
/// function bodies are indented and follow the main body.
pub fn disassemble(chunk: &Chunk) -> String {
    let mut out = String::new();
    render_chunk(chunk, &mut out, 0);
    out
}

fn namespace_prefix(chunk: &Chunk, namespace: Option<NamespaceIdx>) -> String {
    let Some(namespace_idx) = namespace else {
        return String::new();
    };
    let namespace = chunk.namespace_ref(namespace_idx);
    let name = chunk.name(namespace.name_idx());
    match namespace.slot_idx() {
        Some(slot) => format!("{}.(@{}).", name, slot.0),
        None => format!("{name}."),
    }
}

fn render_chunk(chunk: &Chunk, out: &mut String, indent: usize) {
    let pad = "  ".repeat(indent);
    let width = chunk.code.len().saturating_sub(1).to_string().len().max(1);
    for (i, instr) in chunk.code.iter().enumerate() {
        out.push_str(&pad);
        out.push_str(&format!(
            "{:>width$}: {}\n",
            i,
            render_instr(chunk, instr),
            width = width
        ));
    }

    for (i, f) in chunk.functions.iter().enumerate() {
        out.push_str(&pad);
        out.push_str(&format!(
            "\n{}fn #{} {}({}):\n",
            pad,
            i,
            f.name,
            f.params
                .iter()
                .zip(f.param_modes.iter())
                .map(|(name, mode)| match mode {
                    nybl::parser::ParamMode::Value => name.clone(),
                    nybl::parser::ParamMode::Ref => format!("ref {name}"),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
        render_chunk(&f.chunk, out, indent + 1);
    }
}

fn render_instr(chunk: &Chunk, instr: &Instr) -> String {
    match instr {
        Instr::LoadConst(idx) => {
            format!("LoadConst {}", render_constant(chunk.constant(*idx)))
        }
        Instr::LoadNone => "LoadNone".to_string(),
        Instr::LoadTrue => "LoadTrue".to_string(),
        Instr::LoadFalse => "LoadFalse".to_string(),

        Instr::LoadVar(n) => format!("LoadVar {}", chunk.name(*n)),
        Instr::DefineLocal(n) => format!("DefineLocal {}", chunk.name(*n)),
        Instr::StoreVar(n) => format!("StoreVar {}", chunk.name(*n)),

        Instr::LoadLocal(s) => format!("LoadLocal @{}", s.0),
        Instr::StoreLocal(s) => format!("StoreLocal @{}", s.0),
        Instr::CompoundAssign { target, op } => match target {
            crate::chunk::AssignBack::Name(name) => format!(
                "CompoundAssign{} (target {})",
                assign_op_suffix(*op),
                chunk.name(*name)
            ),
            crate::chunk::AssignBack::Slot(slot) => format!(
                "CompoundAssign{} (target @{})",
                assign_op_suffix(*op),
                slot.0
            ),
        },

        Instr::AddLocals(a, b) => format!("AddLocals @{}, @{}", a.0, b.0),
        Instr::LtLocals(a, b) => format!("LtLocals @{}, @{}", a.0, b.0),
        Instr::IncLocalInt(s, k) => format!("IncLocalInt @{}, {}", s.0, k),
        Instr::LoadLocalAddInt(s, k) => format!("LoadLocalAddInt @{}, {}", s.0, k),
        Instr::LtLocalInt(s, k) => format!("LtLocalInt @{}, {}", s.0, k),

        Instr::PushScope => "PushScope".to_string(),
        Instr::PopScope => "PopScope".to_string(),

        Instr::Pop => "Pop".to_string(),
        Instr::Dup => "Dup".to_string(),
        Instr::Dup2 => "Dup2".to_string(),

        Instr::Add => "Add".to_string(),
        Instr::Sub => "Sub".to_string(),
        Instr::Mul => "Mul".to_string(),
        Instr::Div => "Div".to_string(),
        Instr::Rem => "Rem".to_string(),
        Instr::Eq => "Eq".to_string(),
        Instr::NotEq => "NotEq".to_string(),
        Instr::Lt => "Lt".to_string(),
        Instr::Gt => "Gt".to_string(),
        Instr::LtEq => "LtEq".to_string(),
        Instr::GtEq => "GtEq".to_string(),

        Instr::Neg => "Neg".to_string(),
        Instr::Not => "Not".to_string(),

        Instr::TruthyToBool => "TruthyToBool".to_string(),

        Instr::GetIndex => "GetIndex".to_string(),
        Instr::SetIndex => "SetIndex".to_string(),
        Instr::SetIndexInPlace { target, op } => match target {
            crate::chunk::AssignBack::Name(var) => {
                format!(
                    "SetIndexInPlace{} (target {})",
                    assign_op_suffix(*op),
                    chunk.name(*var)
                )
            }
            crate::chunk::AssignBack::Slot(slot) => {
                format!(
                    "SetIndexInPlace{} (target @{})",
                    assign_op_suffix(*op),
                    slot.0
                )
            }
        },

        Instr::StringInterp(idx) => {
            let recipe = chunk.interp(*idx);
            let parts: Vec<String> = recipe
                .parts
                .iter()
                .map(|p| match p {
                    InterpPart::Literal(s) => format!("{s:?}"),
                    InterpPart::Local(slot) => format!("$@{}", slot.0),
                    InterpPart::Name(name) => format!("${}", chunk.name(*name)),
                })
                .collect();
            format!("StringInterp [{}]", parts.join(", "))
        }

        Instr::MakeArray(n) => format!("MakeArray {n}"),
        Instr::MakeDict(n) => format!("MakeDict {n}"),

        Instr::Call { name, argc } => {
            format!("Call {}/{}", chunk.name(*name), argc)
        }
        Instr::CallValue { argc } => format!("CallValue /{argc}"),
        Instr::PrepareCall { name, site } => {
            format!("PrepareCall {}/#{}", chunk.name(*name), site.0)
        }
        Instr::PrepareCallValue { site } => format!("PrepareCallValue #{}", site.0),
        Instr::PrepareMethodValue {
            method,
            site,
            nested_place,
        } => format!(
            "PrepareMethodValue .{}/#{}{}",
            chunk.name(*method),
            site.0,
            if *nested_place { " (nested place)" } else { "" }
        ),
        Instr::PrepareMethodNamed {
            target,
            method,
            site,
        } => match target.slot_idx() {
            Some(slot) => format!(
                "PrepareMethodNamed .{}/#{} (target @{})",
                chunk.name(*method),
                site.0,
                slot.0
            ),
            None => format!(
                "PrepareMethodNamed .{}/#{} (target {})",
                chunk.name(*method),
                site.0,
                chunk.name(target.name_idx())
            ),
        },
        Instr::CallPrepared { site } => format!("CallPrepared #{}", site.0),
        Instr::CallMethod {
            method,
            argc,
            assign_back_to,
            nested_place,
        } => {
            let name = chunk.name(*method);
            if *nested_place {
                return format!("CallMethod .{name}/{argc} (nested place)");
            }
            match assign_back_to {
                Some(crate::chunk::AssignBack::Name(var)) => format!(
                    "CallMethod .{}/{} (back to {})",
                    name,
                    argc,
                    chunk.name(*var)
                ),
                Some(crate::chunk::AssignBack::Slot(slot)) => {
                    format!("CallMethod .{}/{} (back to @{})", name, argc, slot.0)
                }
                None => format!("CallMethod .{name}/{argc}"),
            }
        }
        Instr::CallMethodInPlace {
            target,
            method,
            argc,
        } => {
            let name = chunk.name(*method);
            let receiver = chunk.name(target.name_idx());
            match target.slot_idx() {
                None => format!("CallMethodInPlace .{name}/{argc} (target {receiver})"),
                Some(slot) => format!("CallMethodInPlace .{}/{} (target @{})", name, argc, slot.0),
            }
        }

        Instr::DefineFn(idx) => {
            format!("DefineFn #{} ({})", idx.0, chunk.function(*idx).name)
        }
        Instr::MakeLambda(idx) => {
            format!("MakeLambda #{} ({})", idx.0, chunk.function(*idx).name)
        }
        Instr::Return => "Return".to_string(),
        Instr::ReturnNone => "ReturnNone".to_string(),

        Instr::MakeIter => "MakeIter".to_string(),
        Instr::IterNext { target } => format!("IterNext -> {}", target.0),
        Instr::MakeRepeatCount => "MakeRepeatCount".to_string(),
        Instr::RepeatNext { target } => format!("RepeatNext -> {}", target.0),
        Instr::PopLoopState(LoopStateKind::Iterator) => "PopLoopState iterator".to_string(),
        Instr::PopLoopState(LoopStateKind::Repeat) => "PopLoopState repeat".to_string(),

        Instr::Jump(t) => format!("Jump -> {}", t.0),
        Instr::JumpIfFalse(t) => format!("JumpIfFalse -> {}", t.0),
        Instr::JumpIfFalsePeek(t) => format!("JumpIfFalsePeek -> {}", t.0),
        Instr::JumpIfTruePeek(t) => format!("JumpIfTruePeek -> {}", t.0),

        Instr::Use(idx) => {
            let spec = chunk.use_spec(*idx);
            let items = match &spec.items {
                Some(list) => format!(".{{{}}}", list.join(", ")),
                None => String::new(),
            };
            let alias = match &spec.alias {
                Some(a) => format!(" as {a}"),
                None => String::new(),
            };
            format!("Use {}{}{}", spec.path, items, alias)
        }

        Instr::DefineStruct(idx) => {
            let def = chunk.struct_def(*idx);
            format!("DefineStruct {} {{ {} }}", def.name, def.fields.join(", "))
        }
        Instr::DefineEnum(idx) => {
            let def = chunk.enum_def(*idx);
            format!("DefineEnum {} [{} variants]", def.name, def.variants.len())
        }
        Instr::DefineMethod {
            type_name,
            method_name,
            fn_idx,
        } => {
            format!(
                "DefineMethod {}::{} (#{})",
                chunk.name(*type_name),
                chunk.name(*method_name),
                fn_idx.0,
            )
        }
        Instr::ValidateStructConstruct {
            namespace,
            type_name,
            fields,
        } => {
            let namespace = namespace_prefix(chunk, *namespace);
            format!(
                "ValidateStructConstruct {}{} [{}]",
                namespace,
                chunk.name(*type_name),
                chunk.construct_fields(*fields).join(", ")
            )
        }
        Instr::ValidateEnumConstruct {
            namespace,
            type_name,
            variant,
            shape,
            fields,
        } => {
            let namespace = namespace_prefix(chunk, *namespace);
            format!(
                "ValidateEnumConstruct {}{}::{} {:?} [{}]",
                namespace,
                chunk.name(*type_name),
                chunk.name(*variant),
                shape,
                chunk.construct_fields(*fields).join(", ")
            )
        }
        Instr::ConstructStruct {
            namespace,
            type_name,
            count,
        } => {
            let ns_prefix = namespace_prefix(chunk, *namespace);
            format!(
                "ConstructStruct {}{}/{}",
                ns_prefix,
                chunk.name(*type_name),
                count
            )
        }
        Instr::ConstructEnum {
            namespace,
            type_name,
            variant,
            shape,
        } => {
            use crate::chunk::EnumConstructShape as S;
            let shape_str = match shape {
                S::Unit => "Unit".to_string(),
                S::Tuple(n) => format!("Tuple({n})"),
                S::Struct(n) => format!("Struct({n})"),
            };
            let ns_prefix = namespace_prefix(chunk, *namespace);
            format!(
                "ConstructEnum {}{}::{} {}",
                ns_prefix,
                chunk.name(*type_name),
                chunk.name(*variant),
                shape_str,
            )
        }
        Instr::FieldGet(n) => format!("FieldGet .{}", chunk.name(*n)),
        Instr::FieldSet(n) => format!("FieldSet .{}", chunk.name(*n)),
        Instr::FieldSetInPlace { target, field, op } => match target {
            crate::chunk::AssignBack::Name(var) => format!(
                "FieldSetInPlace{} .{} (target {})",
                assign_op_suffix(*op),
                chunk.name(*field),
                chunk.name(*var)
            ),
            crate::chunk::AssignBack::Slot(slot) => format!(
                "FieldSetInPlace{} .{} (target @{})",
                assign_op_suffix(*op),
                chunk.name(*field),
                slot.0
            ),
        },

        Instr::MatchFail { pattern, on_fail } => {
            format!("MatchFail pat#{} -> {}", pattern.0, on_fail.0)
        }
        Instr::MatchExhausted => "MatchExhausted".to_string(),

        Instr::TryUnwrap => "TryUnwrap".to_string(),

        Instr::Halt => "Halt".to_string(),
    }
}

fn assign_op_suffix(op: crate::chunk::InPlaceAssignOp) -> &'static str {
    match op {
        crate::chunk::InPlaceAssignOp::Eq => "",
        crate::chunk::InPlaceAssignOp::Add => " +=",
        crate::chunk::InPlaceAssignOp::Sub => " -=",
        crate::chunk::InPlaceAssignOp::Mul => " *=",
        crate::chunk::InPlaceAssignOp::Div => " /=",
        crate::chunk::InPlaceAssignOp::Rem => " %=",
    }
}

fn render_constant(c: &Constant) -> String {
    match c {
        Constant::Int(n) => format!("{n}"),
        Constant::Number(n) => {
            if *n == (*n as i64 as f64) && n.is_finite() {
                format!("{}", *n as i64)
            } else {
                format!("{n}")
            }
        }
        Constant::Str(s) => format!("{s:?}"),
    }
}
