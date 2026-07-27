//! Structural validation for public, hand-built bytecode chunks.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    collections::{BTreeMap, BTreeSet},
    format,
    string::String,
    vec,
    vec::Vec,
};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::collections::{BTreeMap, BTreeSet};

use nybl::error::NyblError;

use crate::chunk::{
    AssignBack, CaptureSource, Chunk, CodeOffset, Instr, InterpPart, NamespaceIdx, NamespaceRef,
    RefArgTarget, SlotIdx,
};
use nybl::parser::ParamMode;

/// Validate a bytecode chunk before executing it.
///
/// Compiler-produced chunks satisfy these invariants by construction. This
/// function is primarily useful for embedders that construct or deserialize
/// [`Chunk`] values themselves.
pub fn validate_chunk(chunk: &Chunk) -> Result<(), NyblError> {
    let mut pending = vec![Validator::new(chunk, 0, String::from("top-level chunk"))];
    let mut visited = BTreeSet::new();
    while let Some(validator) = pending.pop() {
        // Function bodies are Rc-backed and may form a heavily shared DAG.
        // Validate each allocation once so a hand-built diamond cannot turn
        // verification into exponential work. The traversal is iterative so
        // arbitrary nesting cannot overflow the Rust call stack.
        let identity = validator.chunk as *const Chunk as usize;
        if visited.insert(identity) {
            validator.validate(&mut pending)?;
        }
    }
    Ok(())
}

struct Validator<'a> {
    chunk: &'a Chunk,
    available_slots: u32,
    context: String,
}

impl<'a> Validator<'a> {
    fn new(chunk: &'a Chunk, available_slots: u32, context: String) -> Self {
        Self {
            chunk,
            available_slots,
            context,
        }
    }

    fn validate(self, pending: &mut Vec<Validator<'a>>) -> Result<(), NyblError> {
        if self.chunk.code.len() != self.chunk.lines.len() {
            return Err(self.invalid(
                0,
                format!(
                    "code has {} instructions but {} source lines",
                    self.chunk.code.len(),
                    self.chunk.lines.len()
                ),
            ));
        }
        if self.chunk.slot_count != self.available_slots {
            return Err(self.invalid(
                0,
                format!(
                    "chunk records {} local slots but its containing function records {}",
                    self.chunk.slot_count, self.available_slots
                ),
            ));
        }

        // Compiler slot allocation is dense: parameters occupy the prefix and
        // every additional slot is introduced by a StoreLocal. Requiring that
        // shape prevents a tiny hand-built chunk from claiming billions of
        // unused slots and forcing an effectively unbounded frame allocation.
        let mut declared_slots = BTreeSet::new();
        for slot in &self.chunk.parameter_slots {
            self.slot(*slot, 0, "parameter metadata")?;
            declared_slots.insert(slot.0);
        }
        for instr in &self.chunk.code {
            if let Instr::StoreLocal(slot) = instr {
                declared_slots.insert(slot.0);
            }
        }
        if declared_slots.len() != self.available_slots as usize {
            return Err(self.invalid(
                0,
                format!(
                    "chunk records {} local slots but only {} are densely declared",
                    self.available_slots,
                    declared_slots.len()
                ),
            ));
        }

        for (index, recipe) in self.chunk.interps.iter().enumerate() {
            for part in recipe.parts.iter() {
                match part {
                    InterpPart::Literal(_) => {}
                    InterpPart::Local(slot) => {
                        self.slot(*slot, 0, &format!("interpolation {index}"))?
                    }
                    InterpPart::Name(name) => self.pool(
                        name.0,
                        self.chunk.names.len(),
                        0,
                        "name",
                        &format!("interpolation {index}"),
                    )?,
                }
            }
        }

        for (index, scope) in self.chunk.local_scopes.iter().enumerate() {
            let detail = format!("local scope {index}");
            let mut previous_name: Option<&str> = None;
            let mut declaration_positions = BTreeSet::new();
            for entry in &scope.entries {
                self.pool(entry.name.0, self.chunk.names.len(), 0, "name", &detail)?;
                if entry.first_binding >= scope.binding_count {
                    return Err(self.invalid(
                        0,
                        format!(
                            "{detail} introduces `{}` at binding {} but records only {} bindings",
                            self.chunk.name(entry.name),
                            entry.first_binding,
                            scope.binding_count
                        ),
                    ));
                }
                if !declaration_positions.insert(entry.first_binding) {
                    return Err(self.invalid(
                        0,
                        format!(
                            "{detail} assigns binding {} to more than one name",
                            entry.first_binding
                        ),
                    ));
                }
                let name = self.chunk.name(entry.name);
                if previous_name.is_some_and(|previous| previous >= name) {
                    return Err(self.invalid(0, format!("{detail} names are not strictly sorted")));
                }
                previous_name = Some(name);
            }
        }

        for (index, spec) in self.chunk.use_specs.iter().enumerate() {
            let Some(snapshot) = spec.local_scope else {
                continue;
            };
            let detail = format!("use specification {index}");
            self.pool(
                snapshot.scope.0,
                self.chunk.local_scopes.len(),
                0,
                "local scope",
                &detail,
            )?;
            let scope = &self.chunk.local_scopes[snapshot.scope.0 as usize];
            if snapshot.binding_count > scope.binding_count {
                return Err(self.invalid(
                    0,
                    format!(
                        "{detail} sees {} local bindings but local scope {} records only {}",
                        snapshot.binding_count, snapshot.scope.0, scope.binding_count
                    ),
                ));
            }
        }

        for (index, function) in self.chunk.functions.iter().enumerate() {
            let detail = format!("function {index} `{}`", function.name);
            if function.capture_names.len() != function.capture_sources.len() {
                return Err(self.invalid(
                    0,
                    format!(
                        "{detail} has {} capture names but {} capture sources",
                        function.capture_names.len(),
                        function.capture_sources.len()
                    ),
                ));
            }
            if function.params.len() != function.chunk.parameter_slots.len() {
                return Err(self.invalid(
                    0,
                    format!(
                        "{detail} has {} parameters but {} parameter-slot entries",
                        function.params.len(),
                        function.chunk.parameter_slots.len()
                    ),
                ));
            }
            if function.param_modes.len() != function.params.len() {
                return Err(self.invalid(
                    0,
                    format!(
                        "{detail} has {} parameters but {} parameter modes",
                        function.params.len(),
                        function.param_modes.len()
                    ),
                ));
            }
            if function.chunk.slot_count != function.slot_count {
                return Err(self.invalid(
                    0,
                    format!(
                        "{detail} records {} slots but its chunk records {}",
                        function.slot_count, function.chunk.slot_count
                    ),
                ));
            }
            let mut names_to_slots = BTreeMap::new();
            let mut slots_to_names = BTreeMap::new();
            for (name, slot) in function
                .params
                .iter()
                .zip(function.chunk.parameter_slots.iter())
            {
                if slot.0 >= function.slot_count {
                    return Err(self.invalid(
                        0,
                        format!(
                            "{detail} maps parameter `{name}` to out-of-range local slot {}",
                            slot.0
                        ),
                    ));
                }
                if let Some(previous) = names_to_slots.insert(name.as_str(), slot.0)
                    && previous != slot.0
                {
                    return Err(self.invalid(
                            0,
                            format!(
                                "{detail} maps repeated parameter `{name}` to both local slots {previous} and {}",
                                slot.0
                            ),
                        ));
                }
                if let Some(previous) = slots_to_names.insert(slot.0, name.as_str())
                    && previous != name
                {
                    return Err(self.invalid(
                            0,
                            format!(
                                "{detail} maps distinct parameters `{previous}` and `{name}` to local slot {}",
                                slot.0
                            ),
                        ));
                }
            }
            for source in &function.capture_sources {
                if let CaptureSource::ParentSlot(slot) = source {
                    self.slot(*slot, 0, &format!("{detail} capture"))?;
                }
            }
            pending.push(Validator::new(
                &function.chunk,
                function.slot_count,
                format!("nested {detail}"),
            ));
        }

        for (index, recipe) in self.chunk.patterns.iter().enumerate() {
            for (_, namespace) in &recipe.namespaces {
                self.namespace_ref(*namespace, 0, &format!("pattern {index}"))?;
            }
        }

        for (index, site) in self.chunk.call_sites.iter().enumerate() {
            let detail = format!("call site {index}");
            if site.arg_modes.len() != site.ref_targets.len() {
                return Err(self.invalid(
                    0,
                    format!(
                        "{detail} has {} argument modes but {} ref-target entries",
                        site.arg_modes.len(),
                        site.ref_targets.len()
                    ),
                ));
            }
            let mut ref_slots = BTreeSet::new();
            for target in site.ref_targets.iter().flatten() {
                if let RefArgTarget::Binding(namespace) = target
                    && let Some(slot) = namespace.slot_idx()
                    && !ref_slots.insert(slot.0)
                {
                    return Err(self.invalid(
                        0,
                        format!(
                            "{detail} uses local slot {} for more than one ref target",
                            slot.0
                        ),
                    ));
                }
            }
            for (position, (mode, target)) in site
                .arg_modes
                .iter()
                .zip(site.ref_targets.iter())
                .enumerate()
            {
                match (mode, target) {
                    (ParamMode::Value, None) => {}
                    (ParamMode::Ref, Some(RefArgTarget::Invalid)) => {}
                    (ParamMode::Ref, Some(RefArgTarget::Binding(namespace))) => {
                        self.namespace_ref(
                            *namespace,
                            0,
                            &format!("{detail} argument {}", position + 1),
                        )?;
                    }
                    _ => {
                        return Err(self.invalid(
                            0,
                            format!(
                                "{detail} argument {} has inconsistent mode and ref target",
                                position + 1
                            ),
                        ));
                    }
                }
            }
        }

        for (offset, instr) in self.chunk.code.iter().copied().enumerate() {
            self.instruction(instr, offset)?;
        }
        Ok(())
    }

    fn instruction(&self, instr: Instr, offset: usize) -> Result<(), NyblError> {
        let line = self.chunk.lines[offset];
        let at = format!("instruction {offset}");
        match instr {
            Instr::LoadConst(index) => {
                self.pool(index.0, self.chunk.constants.len(), line, "constant", &at)?
            }
            Instr::LoadVar(index) | Instr::DefineLocal(index) | Instr::StoreVar(index) => {
                self.pool(index.0, self.chunk.names.len(), line, "name", &at)?
            }
            Instr::LoadLocal(slot)
            | Instr::StoreLocal(slot)
            | Instr::IncLocalInt(slot, _)
            | Instr::LoadLocalAddInt(slot, _)
            | Instr::LtLocalInt(slot, _) => self.slot(slot, line, &at)?,
            Instr::AddLocals(a, b) | Instr::LtLocals(a, b) => {
                self.slot(a, line, &at)?;
                self.slot(b, line, &at)?;
            }
            Instr::CompoundAssign { target, .. } => self.assign_back(target, line, &at)?,
            Instr::SetIndexInPlace { target, .. } => self.assign_back(target, line, &at)?,
            Instr::StringInterp(index) => self.pool(
                index.0,
                self.chunk.interps.len(),
                line,
                "interpolation",
                &at,
            )?,
            Instr::MakeDict(count) => self.pair_count(count, line, &at)?,
            Instr::Call { name, .. } => {
                self.pool(name.0, self.chunk.names.len(), line, "name", &at)?
            }
            Instr::PrepareCall { name, site } => {
                self.pool(name.0, self.chunk.names.len(), line, "name", &at)?;
                self.pool(site.0, self.chunk.call_sites.len(), line, "call site", &at)?;
            }
            Instr::PrepareCallValue { site } | Instr::CallPrepared { site } => {
                self.pool(site.0, self.chunk.call_sites.len(), line, "call site", &at)?;
            }
            Instr::PrepareMethodValue { method, site, .. } => {
                self.pool(method.0, self.chunk.names.len(), line, "name", &at)?;
                self.pool(site.0, self.chunk.call_sites.len(), line, "call site", &at)?;
            }
            Instr::PrepareMethodNamed {
                target,
                method,
                site,
            } => {
                self.namespace_ref(target, line, &at)?;
                self.pool(method.0, self.chunk.names.len(), line, "name", &at)?;
                self.pool(site.0, self.chunk.call_sites.len(), line, "call site", &at)?;
            }
            Instr::CallMethod {
                method,
                assign_back_to,
                ..
            } => {
                self.pool(method.0, self.chunk.names.len(), line, "name", &at)?;
                if let Some(target) = assign_back_to {
                    self.assign_back(target, line, &at)?;
                }
            }
            Instr::CallMethodInPlace { target, method, .. } => {
                self.namespace_ref(target, line, &at)?;
                self.pool(method.0, self.chunk.names.len(), line, "name", &at)?;
            }
            Instr::DefineFn(index) | Instr::MakeLambda(index) => {
                self.pool(index.0, self.chunk.functions.len(), line, "function", &at)?
            }
            Instr::IterNext { target }
            | Instr::RepeatNext { target }
            | Instr::Jump(target)
            | Instr::JumpIfFalse(target)
            | Instr::JumpIfFalsePeek(target)
            | Instr::JumpIfTruePeek(target) => self.jump(target, line, &at)?,
            Instr::Use(index) => self.pool(
                index.0,
                self.chunk.use_specs.len(),
                line,
                "use specification",
                &at,
            )?,
            Instr::DefineStruct(index) => self.pool(
                index.0,
                self.chunk.struct_defs.len(),
                line,
                "struct definition",
                &at,
            )?,
            Instr::DefineEnum(index) => self.pool(
                index.0,
                self.chunk.enum_defs.len(),
                line,
                "enum definition",
                &at,
            )?,
            Instr::DefineMethod {
                type_name,
                method_name,
                fn_idx,
            } => {
                self.pool(type_name.0, self.chunk.names.len(), line, "name", &at)?;
                self.pool(method_name.0, self.chunk.names.len(), line, "name", &at)?;
                self.pool(fn_idx.0, self.chunk.functions.len(), line, "function", &at)?;
            }
            Instr::ValidateStructConstruct {
                namespace,
                type_name,
                fields,
            } => {
                if let Some(namespace) = namespace {
                    self.namespace_idx(namespace, line, &at)?;
                }
                self.pool(type_name.0, self.chunk.names.len(), line, "name", &at)?;
                self.pool(
                    fields.0,
                    self.chunk.construct_fields.len(),
                    line,
                    "construction field recipe",
                    &at,
                )?;
            }
            Instr::ValidateEnumConstruct {
                namespace,
                type_name,
                variant,
                fields,
                ..
            } => {
                if let Some(namespace) = namespace {
                    self.namespace_idx(namespace, line, &at)?;
                }
                self.pool(type_name.0, self.chunk.names.len(), line, "name", &at)?;
                self.pool(variant.0, self.chunk.names.len(), line, "name", &at)?;
                self.pool(
                    fields.0,
                    self.chunk.construct_fields.len(),
                    line,
                    "construction field recipe",
                    &at,
                )?;
            }
            Instr::ConstructStruct {
                namespace,
                type_name,
                count,
            } => {
                self.pair_count(count, line, &at)?;
                if let Some(namespace) = namespace {
                    self.namespace_idx(namespace, line, &at)?;
                }
                self.pool(type_name.0, self.chunk.names.len(), line, "name", &at)?;
            }
            Instr::ConstructEnum {
                namespace,
                type_name,
                variant,
                shape,
            } => {
                if let crate::chunk::EnumConstructShape::Struct(count) = shape {
                    self.pair_count(count, line, &at)?;
                }
                if let Some(namespace) = namespace {
                    self.namespace_idx(namespace, line, &at)?;
                }
                self.pool(type_name.0, self.chunk.names.len(), line, "name", &at)?;
                self.pool(variant.0, self.chunk.names.len(), line, "name", &at)?;
            }
            Instr::FieldGet(name) | Instr::FieldSet(name) => {
                self.pool(name.0, self.chunk.names.len(), line, "name", &at)?
            }
            Instr::FieldSetInPlace { target, field, .. } => {
                self.assign_back(target, line, &at)?;
                self.pool(field.0, self.chunk.names.len(), line, "name", &at)?;
            }
            Instr::MatchFail { pattern, on_fail } => {
                self.pool(pattern.0, self.chunk.patterns.len(), line, "pattern", &at)?;
                self.jump(on_fail, line, &at)?;
            }
            Instr::LoadNone
            | Instr::LoadTrue
            | Instr::LoadFalse
            | Instr::PushScope
            | Instr::PopScope
            | Instr::Pop
            | Instr::Dup
            | Instr::Dup2
            | Instr::Add
            | Instr::Sub
            | Instr::Mul
            | Instr::Div
            | Instr::Rem
            | Instr::Eq
            | Instr::NotEq
            | Instr::Lt
            | Instr::Gt
            | Instr::LtEq
            | Instr::GtEq
            | Instr::Neg
            | Instr::Not
            | Instr::TruthyToBool
            | Instr::GetIndex
            | Instr::SetIndex
            | Instr::MakeArray(_)
            | Instr::CallValue { .. }
            | Instr::Return
            | Instr::ReturnNone
            | Instr::MakeIter
            | Instr::MakeRepeatCount
            | Instr::PopLoopState(_)
            | Instr::MatchExhausted
            | Instr::TryUnwrap
            | Instr::Halt => {}
        }
        Ok(())
    }

    fn pair_count(&self, count: u32, line: u32, detail: &str) -> Result<(), NyblError> {
        let valid = usize::try_from(count)
            .ok()
            .and_then(|count| count.checked_mul(2))
            .is_some();
        if valid {
            Ok(())
        } else {
            Err(self.invalid(
                line,
                format!("{detail} has a field-pair count that overflows this target"),
            ))
        }
    }

    fn namespace_ref(
        &self,
        namespace: NamespaceRef,
        line: u32,
        detail: &str,
    ) -> Result<(), NyblError> {
        self.pool(
            namespace.name_idx().0,
            self.chunk.names.len(),
            line,
            "name",
            detail,
        )?;
        if let Some(slot) = namespace.slot_idx() {
            self.slot(slot, line, detail)?;
        }
        Ok(())
    }

    fn namespace_idx(
        &self,
        namespace: NamespaceIdx,
        line: u32,
        detail: &str,
    ) -> Result<(), NyblError> {
        self.pool(
            namespace.index(),
            self.chunk.namespace_refs.len(),
            line,
            "namespace reference",
            detail,
        )?;
        self.namespace_ref(self.chunk.namespace_ref(namespace), line, detail)
    }

    fn assign_back(&self, target: AssignBack, line: u32, detail: &str) -> Result<(), NyblError> {
        match target {
            AssignBack::Slot(slot) => self.slot(slot, line, detail),
            AssignBack::Name(name) => {
                self.pool(name.0, self.chunk.names.len(), line, "name", detail)
            }
        }
    }

    fn slot(&self, slot: SlotIdx, line: u32, detail: &str) -> Result<(), NyblError> {
        self.pool(
            slot.0,
            self.available_slots as usize,
            line,
            "local slot",
            detail,
        )
    }

    fn jump(&self, target: CodeOffset, line: u32, detail: &str) -> Result<(), NyblError> {
        self.pool(target.0, self.chunk.code.len(), line, "jump target", detail)
    }

    fn pool(
        &self,
        index: u32,
        len: usize,
        line: u32,
        kind: &str,
        detail: &str,
    ) -> Result<(), NyblError> {
        if (index as usize) < len {
            Ok(())
        } else {
            Err(self.invalid(
                line,
                format!("{detail} references {kind} {index}, but the valid range is 0..{len}"),
            ))
        }
    }

    fn invalid(&self, line: u32, detail: String) -> NyblError {
        NyblError::runtime(format!("VM: invalid {}: {detail}", self.context), line)
    }
}
