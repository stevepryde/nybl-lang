//! Bytecode chunk layout: instructions, constants, and nested function
//! bodies. See the crate root for the textual description of the
//! instruction set.

use core::num::{NonZeroU32, NonZeroU64};
use nybl::parser::ParamMode;

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{rc::Rc, string::String, vec::Vec};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::rc::Rc;

/// One bytecode operation.
///
/// Operand indices (`ConstIdx`, `NameIdx`, `FnIdx`, `InterpIdx`) are
/// indices into the owning chunk's pools. Jump targets are absolute
/// instruction indices inside the same chunk.
///
/// `Copy` is load-bearing for performance: the dispatch loop
/// reads one `Instr` per step by value out of the chunk's code
/// vec. Before `Copy` was added the read compiled to a full
/// `.clone()` call dispatching through the `Clone` impl's
/// match arm — surprisingly pricey in the hot path. With
/// `Copy`, the load is a trivial register-sized memcpy that
/// LLVM can fold into downstream dispatch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Instr {
    // ─── Literals ─────────────────────────────────────────────────
    LoadConst(ConstIdx),
    LoadNone,
    LoadTrue,
    LoadFalse,

    // ─── Variables ────────────────────────────────────────────────
    /// Push value of the named variable onto the stack. Slow path —
    /// walks the current frame's `scopes` BTreeMap stack, then falls
    /// through to module-level / function-registry lookup. Emitted
    /// for captures, imports, and anything the compiler's
    /// `LocalResolver` couldn't pin to a slot.
    LoadVar(NameIdx),
    /// Pop the top value and define it as a new block-scoped local
    /// in the current BTreeMap scope. Used for match bindings,
    /// for-in variables at module top-level, and module-top-level
    /// `let` statements — everywhere slot resolution isn't active.
    DefineLocal(NameIdx),
    /// Pop the top value and assign it to an existing BTreeMap-scoped
    /// variable (companion to the `LoadVar` / `DefineLocal` slow
    /// path).
    StoreVar(NameIdx),

    /// Push the value of the local at `slot`. Fast path: a single
    /// `Vec::get_unchecked` into the current frame's slot array.
    /// Emitted by the compiler for every identifier reference that
    /// resolves to a function-level local (parameter, `let`, or
    /// `for-in` variable).
    LoadLocal(SlotIdx),
    /// Pop the top value and assign it to the local at `slot`.
    /// Used for both `let x = ...` initialisation and `x = ...`
    /// assignment; the VM treats them identically once slots are
    /// pre-sized at call time.
    StoreLocal(SlotIdx),
    /// Pop an already-evaluated RHS, then read and compound-assign the live
    /// target binding. Keeping target resolution after RHS evaluation matches
    /// the walker/AOT error and side-effect order.
    CompoundAssign {
        target: AssignBack,
        op: InPlaceAssignOp,
    },

    // ─── Superinstructions ──────────────────────────────────────
    //
    // Fused opcodes that collapse a common 3-4 instruction
    // sequence into a single dispatch step. The compiler emits
    // them via peephole detection; the VM handles them with a
    // direct slot read + typed fast path, falling back to the
    // equivalent generic opcodes on type mismatch.
    /// Push `frame.slots[a] + frame.slots[b]` without touching
    /// the value stack for the operands — fuses `LoadLocal(a) +
    /// LoadLocal(b) + Add`. Fast path specialises on both
    /// operands being `Int`; the fallback delegates to
    /// `ops::add` with the slot values by reference.
    AddLocals(SlotIdx, SlotIdx),
    /// Push `frame.slots[a] < frame.slots[b]` — fuses
    /// `LoadLocal(a) + LoadLocal(b) + Lt`. Same
    /// Int-fast-path / generic-fallback split as `AddLocals`.
    LtLocals(SlotIdx, SlotIdx),
    /// `frame.slots[slot] += k` for a small `i32` `k`. The final store
    /// peephole fuses `LoadLocalAddInt(slot, k) + StoreLocal(slot)`, which
    /// represents `LoadLocal(slot) + LoadConst(k) + Add + StoreLocal(slot)`.
    /// Specialised for `Int` slots — the `i = i + 1` and
    /// `total = total + small_k` idioms in tight loops. If the
    /// slot isn't an `Int`, falls back to generic add via the
    /// runtime's `ops::add`.
    IncLocalInt(SlotIdx, i32),
    /// Push `frame.slots[slot] + k` (as `Int`), fuses
    /// `LoadLocal(slot) + LoadConst(k) + Add`. Covers the
    /// `array[i + 1]` pattern. Non-Int fallback delegates to
    /// `ops::add`, preserving the language's generic `+` semantics.
    LoadLocalAddInt(SlotIdx, i32),
    /// Push `frame.slots[slot] < k` (as `Bool`), fuses
    /// `LoadLocal(slot) + LoadConst(k:Int) + Lt`. The `n < 2`
    /// base-case test in recursive functions.
    LtLocalInt(SlotIdx, i32),

    // ─── Scope ────────────────────────────────────────────────────
    /// Push a fresh BTreeMap onto the current frame's scopes stack.
    /// Only relevant when the slow-path `LoadVar` / `DefineLocal`
    /// machinery is in play; compiler omits these around blocks
    /// whose locals live in slots.
    PushScope,
    PopScope,

    // ─── Stack ────────────────────────────────────────────────────
    /// Discard the top of stack.
    Pop,
    /// Duplicate the top of stack.
    Dup,
    /// Duplicate the top two items: `[.., a, b]` → `[.., a, b, a, b]`.
    Dup2,

    // ─── Binary ops ───────────────────────────────────────────────
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,

    // ─── Unary ops ────────────────────────────────────────────────
    Neg,
    Not,

    /// Replace top with `Value::Bool(top.is_truthy())`. Used for
    /// short-circuit `&&` / `||`, which must return a Bool.
    TruthyToBool,

    // ─── Indexing ─────────────────────────────────────────────────
    /// `[.., obj, idx]` → `[.., obj[idx]]`.
    GetIndex,
    /// `[.., obj, idx, val]` → `[.., obj']` with `obj'[idx] == val`.
    SetIndex,
    /// `[.., rhs, idx]` → `[..]`, applying `op` through the live named
    /// binding. Keeping the receiver off-stack preserves refcount-1 CoW
    /// mutation; carrying `op` lets compound assignment read the current
    /// element only after RHS and index evaluation, matching the walker.
    SetIndexInPlace {
        target: AssignBack,
        op: InPlaceAssignOp,
    },
    /// Apply an assignment through an arbitrary identifier-rooted place. The
    /// RHS and each dynamic index projection are evaluated before this
    /// instruction; the place recipe supplies the root and field projections.
    AssignPlace {
        place: PlaceIdx,
        op: InPlaceAssignOp,
    },

    // ─── String interpolation ─────────────────────────────────────
    /// Interpolate using a recipe from the chunk's interp pool. Each
    /// variable part is already resolved to either a frame slot or a
    /// name-pool entry; the resulting string is pushed.
    StringInterp(InterpIdx),

    // ─── Collections ──────────────────────────────────────────────
    /// Pop `n` items, push an array.
    MakeArray(u32),
    /// Pop `n` (key, value) pairs (value on top), push a dict.
    /// Keys are pushed as string values on the stack, interleaved with
    /// values.
    MakeDict(u32),

    // ─── Calls ────────────────────────────────────────────────────
    /// Call `name` with `argc` arguments popped from the stack.
    /// Resolution order: locally-bound closure → builtin → host →
    /// named user fn → error.
    Call {
        name: NameIdx,
        argc: u32,
    },
    /// Call whatever sits on top of the `argc` args on the stack. The
    /// callee must be a `Value::Fn`; anything else is a runtime
    /// error. Emitted when the call's callee expression isn't a
    /// bare ident (e.g. `funcs[0](x)` or `make_adder(5)(3)`). Arguments
    /// are evaluated before the callee, matching the tree walker.
    CallValue {
        argc: u32,
    },
    /// Resolve a named callable exactly once and validate the call-site shape
    /// before any ordinary argument expression is evaluated.
    PrepareCall {
        name: NameIdx,
        site: CallSiteIdx,
    },
    /// Pop and preflight an already-evaluated callable value.
    PrepareCallValue {
        site: CallSiteIdx,
    },
    /// Pop an already-evaluated method receiver and preflight its positional
    /// non-receiver arguments. Used when any explicit `ref` marker is present.
    PrepareMethodValue {
        method: NameIdx,
        site: CallSiteIdx,
        nested_place: bool,
    },
    /// Preflight a named method receiver without snapshotting a mutable
    /// receiver. The VM snapshots value receivers immediately, but defers a
    /// built-in mutator or user-defined `ref self` receiver until ordinary
    /// arguments have run.
    PrepareMethodNamed {
        target: NamespaceRef,
        method: NameIdx,
        site: CallSiteIdx,
    },
    /// Resolve an identifier-rooted field/index receiver before ordinary
    /// arguments execute, retaining its exact place for mutating/ref-self
    /// write-back.
    PrepareMethodPlace {
        place: PlaceIdx,
        method: NameIdx,
        site: CallSiteIdx,
    },
    /// Snapshot the prepared call's `ref` targets, interleave them with the
    /// ordinary argument values, and enter the callable.
    CallPrepared {
        site: CallSiteIdx,
    },
    /// Method call: `[.., args..., obj]` → `[.., ret]`, and if the
    /// method is mutating and `obj` came from a variable, the VM
    /// writes the mutated value back to the original binding. The
    /// back-write target is the binding that produced `obj` —
    /// `Slot(idx)` for compile-time-resolved locals, `Name(idx)`
    /// for the scope-map fallback, or `None` when the receiver is
    /// a transient (e.g. `[1,2].push(3)` — nothing to update).
    /// `nested_place` distinguishes an index/field receiver from a
    /// genuine temporary so built-in array mutation can fail rather
    /// than silently discarding the detached mutation.
    CallMethod {
        method: NameIdx,
        argc: u32,
        assign_back_to: Option<AssignBack>,
        nested_place: bool,
    },
    /// Mutating method call on a named receiver. Unlike `CallMethod`, the
    /// receiver is not copied onto the value stack: after evaluating the
    /// arguments, the VM resolves `target` and mutates an array binding's
    /// owned buffer directly. Non-array bindings fall back to ordinary method
    /// dispatch so user-defined methods named `push`, `pop`, etc. retain their
    /// existing behaviour. `NamespaceRef` packs both the source spelling and
    /// optional local slot so constant checks retain the exact receiver name
    /// without growing this hot instruction.
    CallMethodInPlace {
        target: NamespaceRef,
        method: NameIdx,
        argc: u32,
    },

    // ─── Functions ────────────────────────────────────────────────
    /// Register the function at `FnIdx` in the current scope.
    DefineFn(FnIdx),
    /// Build a `Value::Fn` for the lambda at `FnIdx`, capturing
    /// every variable currently visible in the frame's scope
    /// stack. Pushes the resulting closure onto the value stack.
    MakeLambda(FnIdx),
    /// Pop the top value and return from the current call frame.
    Return,
    /// Return with `Value::None`.
    ReturnNone,

    // ─── Iteration / repeat ───────────────────────────────────────
    /// Pop iterable, push an internal iterator value. The VM owns
    /// the representation.
    MakeIter,
    /// If the iterator at the top of stack has another item, push
    /// it. Otherwise pop the iterator and jump to `target`.
    IterNext {
        target: CodeOffset,
    },
    /// Pop a number, validate it, push an internal repeat counter.
    MakeRepeatCount,
    /// If the repeat counter at the top is non-zero, decrement it
    /// and fall through. Otherwise pop it and jump to `target`.
    RepeatNext {
        target: CodeOffset,
    },
    /// Discard the sidecar owned by a loop exited through `break`.
    /// The VM checks the kind so broken compiler bookkeeping cannot
    /// silently consume an enclosing loop's state.
    PopLoopState(LoopStateKind),

    // ─── Control flow ─────────────────────────────────────────────
    Jump(CodeOffset),
    /// Pop top; jump if falsy.
    JumpIfFalse(CodeOffset),
    /// Peek top; jump if falsy (don't pop). For `&&` short-circuit.
    JumpIfFalsePeek(CodeOffset),
    /// Peek top; jump if truthy (don't pop). For `||` short-circuit.
    JumpIfTruePeek(CodeOffset),

    // ─── Modules ─────────────────────────────────────────────────
    /// Resolve, parse, compile, and run the module at the path
    /// stored in `chunk.use_specs[idx]`, then inject its exports
    /// per the spec — glob (default), selective (`.{a, b}`),
    /// aliased (`as m`), or both. The VM caches by module path
    /// so re-uses are cheap. Instruction stays `Copy` by keeping
    /// the variable-length `items` / `alias` data in a side pool.
    Use(UseIdx),

    // ─── User-defined types ─────────────────────────────────────
    /// Register the struct type at `StructIdx` (declared fields
    /// live in the chunk's `struct_defs` pool). Subsequent
    /// `ConstructStruct` / `FieldGet` / `FieldSet` opcodes
    /// reference it by type name.
    DefineStruct(StructIdx),
    /// Register the enum type at `EnumIdx` (variants + their
    /// payload shapes live in the chunk's `enum_defs` pool).
    DefineEnum(EnumIdx),
    /// Register a user method. Receiver type is looked up by
    /// `type_name`; the body lives in `chunk.functions[fn_idx]`
    /// (same pool as user fns / lambdas).
    DefineMethod {
        type_name: NameIdx,
        method_name: NameIdx,
        fn_idx: FnIdx,
    },
    /// Validate a complete struct-construction recipe before any field payload
    /// expression is evaluated. The field-name list lives in a side pool so
    /// this instruction remains `Copy`.
    ValidateStructConstruct {
        namespace: Option<NamespaceIdx>,
        type_name: NameIdx,
        fields: ConstructFieldsIdx,
    },
    /// Validate enum type/variant/payload shape before evaluating payloads.
    ValidateEnumConstruct {
        namespace: Option<NamespaceIdx>,
        type_name: NameIdx,
        variant: NameIdx,
        shape: EnumConstructShape,
        fields: ConstructFieldsIdx,
    },
    /// Struct literal: pop `2 * count` stack entries (field-name
    /// string + value, alternating — same layout as `MakeDict`),
    /// validate against the struct declaration, push a
    /// `Value::Struct`. `namespace` (when present) is the alias
    /// for a `m.Type { ... }` access — the VM checks that the
    /// name resolves to a `Value::Module` whose exports include
    /// this type before falling through to the normal registry.
    ConstructStruct {
        namespace: Option<NamespaceIdx>,
        type_name: NameIdx,
        count: u32,
    },
    /// Enum variant construction. The `shape` tells the VM how
    /// many stack entries the payload consumes:
    ///
    /// - `Unit` — no pops
    /// - `Tuple(argc)` — pop `argc` values
    /// - `Struct(count)` — pop `2 * count` (name, value) pairs
    ///
    /// `namespace` mirrors the `ConstructStruct` field — set for
    /// `m.Result::Ok(v)` forms.
    ConstructEnum {
        namespace: Option<NamespaceIdx>,
        type_name: NameIdx,
        variant: NameIdx,
        shape: EnumConstructShape,
    },
    /// Pop the object, push the value of the named field. Works
    /// on `Value::Struct` and on `Value::EnumVariant` with
    /// struct-shaped payloads.
    FieldGet(NameIdx),
    /// `[.., obj, val]` → `[.., obj']` with `obj'.field = val`.
    /// Generic owned-value building block; named field assignment uses
    /// [`Self::FieldSetInPlace`] to preserve unique CoW ownership.
    FieldSet(NameIdx),
    /// `[.., rhs]` → `[..]`, applying `op` to `field` through the live
    /// named binding without putting a receiver clone on the value stack.
    FieldSetInPlace {
        target: AssignBack,
        field: NameIdx,
        op: InPlaceAssignOp,
    },

    // ─── Pattern matching ───────────────────────────────────────
    /// Pattern-match the top-of-stack value against `pattern`.
    /// The value is popped unconditionally. On success, every
    /// binding introduced by the pattern is installed in the
    /// current scope and execution falls through. On failure,
    /// nothing is bound and execution jumps to `on_fail`.
    ///
    /// The compiler pairs each `match` arm with its own
    /// `PushScope` / `PopScope` bracket so bindings from a
    /// failed arm (e.g. when a guard rejects the candidate) are
    /// discarded before the next arm runs.
    MatchFail {
        pattern: PatternIdx,
        on_fail: CodeOffset,
    },
    /// Raise "No match arm matched the scrutinee". Emitted after
    /// the last arm's failure path so exhausting a `match` with
    /// no arm that applies is observable as a runtime error.
    MatchExhausted,

    /// `try <expr>` handler. Pops the top value and inspects it:
    /// - `Ok(v)` (single tuple payload)  → push `v`, fall through
    /// - `Ok`    (unit variant)          → push `none`, fall through
    /// - `Err(...)`                       → act like `Return` from
    ///   the current frame, carrying the whole `Err` variant as
    ///   the returned value. Raises at the engine boundary if
    ///   the current frame is the top-level program (no fn to
    ///   return from).
    /// - any other shape → raise a runtime error.
    TryUnwrap,

    // ─── Termination ──────────────────────────────────────────────
    /// End the current chunk (top-level program only).
    Halt,
}

// ─── Index newtypes ────────────────────────────────────────────────

/// Index into a chunk's constant pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConstIdx(pub u32);

/// Index into a chunk's name pool (used for variable and function names).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NameIdx(pub u32);

/// Index into a chunk's nested-function table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FnIdx(pub u32);

/// Slot index inside a function frame's flat `slots: Vec<Value>`
/// array. Assigned at compile time by the scope resolver so the
/// dispatch loop can read/write locals via direct `Vec` indexing
/// instead of name-keyed BTreeMap lookups. Slot numbers are
/// unique within a function body — blocks don't reuse them, so
/// `FnDef::slot_count` is the maximum index ever emitted plus
/// one, which pre-sizes the `slots` vec at call time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotIdx(pub u32);

/// Index into a chunk's string-interpolation recipe table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InterpIdx(pub u32);

/// Absolute instruction index within the same chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CodeOffset(pub u32);

/// Internal stack state owned by a `for` or `repeat` loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopStateKind {
    Iterator,
    Repeat,
}

/// Instruction span of one compiled source loop (`while` / `repeat`
/// / `for`), recorded so a step-limit error can be attributed to the
/// loop's header line no matter which instruction the budget happens
/// to trip on. `start` covers the per-iteration entry point (the
/// `while` condition, `RepeatNext`, or `IterNext`); one-time setup
/// like a `repeat` count or `for` iterable stays outside the span,
/// matching where the tree-walker enters its loop context. `end` is
/// exclusive: the first instruction after the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopRange {
    pub start: CodeOffset,
    pub end: CodeOffset,
    /// Source line of the loop header.
    pub line: u32,
}

/// Index into a chunk's struct-definition pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StructIdx(pub u32);

/// Index into a chunk's enum-definition pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EnumIdx(pub u32);

/// Index into a chunk's pattern pool. Each `match` arm points at
/// one `Pattern` here; `MatchFail` consults it at runtime via the
/// shared `nybl::pattern_matches` helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PatternIdx(pub u32);

/// Compact lexically resolved namespace used by a namespaced type reference.
///
/// Function locals live in slots, while captures and module bindings retain
/// name-based lookup. The non-zero packed representation keeps side-table
/// entries at eight bytes. Name index `u32::MAX` and slot index `u32::MAX` are
/// reserved; neither can address a materializable bytecode pool or frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NamespaceRef(NonZeroU64);

impl NamespaceRef {
    pub fn from_name(name: NameIdx) -> Self {
        assert!(name.0 != u32::MAX, "namespace name index is reserved");
        Self(NonZeroU64::new(u64::from(name.0) + 1).expect("non-zero name encoding"))
    }

    pub fn from_slot(name: NameIdx, slot: SlotIdx) -> Self {
        assert!(name.0 != u32::MAX, "namespace name index is reserved");
        assert!(slot.0 != u32::MAX, "namespace slot index is reserved");
        let encoded = ((u64::from(slot.0) + 1) << 32) | u64::from(name.0);
        Self(NonZeroU64::new(encoded).expect("non-zero slot encoding"))
    }

    pub fn name_idx(self) -> NameIdx {
        let encoded = self.0.get();
        if encoded <= u64::from(u32::MAX) {
            NameIdx((encoded - 1) as u32)
        } else {
            NameIdx(encoded as u32)
        }
    }

    pub fn slot_idx(self) -> Option<SlotIdx> {
        let encoded = self.0.get();
        if encoded <= u64::from(u32::MAX) {
            None
        } else {
            Some(SlotIdx(((encoded >> 32) - 1) as u32))
        }
    }
}

/// Index into a chunk's namespace-reference pool. Stored as one-based
/// `NonZeroU32` so `Option<NamespaceIdx>` remains four bytes inside hot
/// instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NamespaceIdx(NonZeroU32);

impl NamespaceIdx {
    pub fn new(index: u32) -> Self {
        Self(
            NonZeroU32::new(index.checked_add(1).expect("namespace pool is too large"))
                .expect("one-based namespace index"),
        )
    }

    pub fn index(self) -> u32 {
        self.0.get() - 1
    }
}

#[cfg(test)]
mod instruction_layout_tests {
    use super::{Instr, NameIdx, NamespaceIdx, NamespaceRef, SlotIdx};

    #[test]
    fn instruction_remains_three_machine_words() {
        assert_eq!(core::mem::size_of::<Instr>(), 24);
        assert_eq!(core::mem::size_of::<Option<NamespaceIdx>>(), 4);
    }

    #[test]
    fn packed_namespace_references_round_trip() {
        for name in [0, u32::MAX - 1] {
            let namespace = NamespaceRef::from_name(NameIdx(name));
            assert_eq!(namespace.name_idx(), NameIdx(name));
            assert_eq!(namespace.slot_idx(), None);

            for slot in [0, u32::MAX - 1] {
                let namespace = NamespaceRef::from_slot(NameIdx(name), SlotIdx(slot));
                assert_eq!(namespace.name_idx(), NameIdx(name));
                assert_eq!(namespace.slot_idx(), Some(SlotIdx(slot)));
            }
        }
    }
}

/// Index into a chunk's `use_specs` pool. Each `use` statement
/// emits one [`UseSpec`] describing the shape of the import
/// (path + optional `items` + optional `alias`); `Instr::Use`
/// stays `Copy` by holding this side-table index rather than the
/// variable-length spec inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UseIdx(pub u32);

/// Index into a chunk's function-local lexical-scope metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalScopeIdx(pub u32);

/// Index into a chunk's construction field-name recipe pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConstructFieldsIdx(pub u32);

/// Index into a chunk's call-site metadata pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallSiteIdx(pub u32);

/// Index into a chunk's identifier-rooted place recipe pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlaceIdx(pub u32);

/// Shape of an enum variant's payload at the construction site —
/// tells the VM how many stack entries to pop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EnumConstructShape {
    Unit,
    Tuple(u32),
    Struct(u32),
}

/// Target for a mutating-method write-back. A call like
/// `arr.push(v)` needs the VM to re-bind `arr` to the mutated
/// array, but *where* that binding lives depends on whether the
/// receiver was a slot-resolved local or a name-scoped variable
/// (captures, module top-level, match bindings). The compiler
/// picks the right form at emit time so the runtime doesn't have
/// to probe both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssignBack {
    /// Write the mutated value back into `slots[slot]` on the
    /// current frame.
    Slot(SlotIdx),
    /// Walk the current frame's BTreeMap scope stack, find the
    /// first entry named `name`, overwrite it. Matches the
    /// pre-slot `CallMethod` behaviour.
    Name(NameIdx),
}

/// Compile-time classification of one explicit `ref` argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefArgTarget {
    /// A mutable-binding candidate. Runtime preflight resolves the name form
    /// to an exact scope entry and rejects captures/constants before arguments.
    Binding(NamespaceRef),
    /// An identifier root followed by zero or more field/index projections.
    Place(PlaceIdx),
    /// Any expression other than a transparent-grouped plain identifier.
    Invalid,
}

/// One projection in a compiled mutable place. Index expressions are emitted
/// as ordinary bytecode and supplied on the value stack; field names remain in
/// the recipe side table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceProjectionRecipe {
    Index,
    Field(NameIdx),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceRecipe {
    pub root: NamespaceRef,
    pub projections: Vec<PlaceProjectionRecipe>,
}

/// Passive metadata for one source call. `arg_modes` retains positional
/// markers even when the callee is reached dynamically. `ref_targets` is
/// positionally parallel and must contain a target exactly at `Ref` positions.
#[derive(Debug, Clone)]
pub struct CallSite {
    pub arg_modes: Vec<ParamMode>,
    pub ref_targets: Vec<Option<RefArgTarget>>,
}

/// Assignment operation carried by direct named index/field stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InPlaceAssignOp {
    Eq,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

// ─── Constants and recipes ─────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Constant {
    /// Exact integer constant (phase 6). Lowered from
    /// `ExprKind::Int` and materialised as `Value::Int` at
    /// `LoadConst` time.
    Int(i64),
    Number(f64),
    Str(String),
}

/// One part of a compiled string-interpolation recipe.
///
/// Binding resolution happens in the compiler, not the VM: function
/// parameters and locals become [`Self::Local`] while captures and
/// module-level bindings remain [`Self::Name`]. This is the same split
/// used by `LoadLocal` / `LoadVar` and avoids losing slot-only locals at
/// runtime.
#[derive(Debug, Clone, PartialEq)]
pub enum InterpPart {
    Literal(String),
    Local(SlotIdx),
    Name(NameIdx),
}

/// A compiled string-interpolation recipe. Parts are formatted in
/// source order using their compile-time-resolved bindings. The immutable
/// slice is shared so repeated interpolation only bumps a refcount.
#[derive(Debug, Clone, PartialEq)]
pub struct InterpRecipe {
    pub parts: Rc<[InterpPart]>,
}

/// One unique slot-backed name declared in a lexical scope.
///
/// `first_binding` is the zero-based declaration position at which the name
/// first became visible. Repeated declarations do not need another entry:
/// every later snapshot includes the name once the first declaration is in
/// its binding frontier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalScopeName {
    pub name: NameIdx,
    pub first_binding: u32,
}

/// Function-local names declared in one lexical scope.
///
/// Entries are sorted strictly by their referenced name strings so import
/// clash checks can use binary search. The table is retained once per scope;
/// individual `use` statements only retain a [`LocalScopeSnapshot`].
#[derive(Debug, Clone, Default)]
pub struct LocalScopeNames {
    pub binding_count: u32,
    pub entries: Vec<LocalScopeName>,
}

/// Prefix of a lexical scope that was visible at one `use` statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalScopeSnapshot {
    pub scope: LocalScopeIdx,
    pub binding_count: u32,
}

/// Compiled descriptor for a `use` statement. Parallel to the
/// parser's `StmtKind::Use` fields — the VM's `Use` opcode picks
/// this up by [`UseIdx`] at runtime so the hot-path instruction
/// stays `Copy`.
#[derive(Debug, Clone)]
pub struct UseSpec {
    pub path: String,
    pub items: Option<Vec<String>>,
    pub alias: Option<String>,
    /// Slot-allocated locals / parameters visible in the innermost lexical
    /// scope at this site. The referenced scope table is shared by every use
    /// in that scope; the frontier makes source-order visibility exact.
    pub local_scope: Option<LocalScopeSnapshot>,
}

// ─── Chunk ─────────────────────────────────────────────────────────

/// A single compiled unit: either the top-level program or one
/// user-defined function body.
#[derive(Debug, Clone, Default)]
pub struct Chunk {
    pub code: Vec<Instr>,
    /// Source line for each instruction; parallel to `code`.
    pub lines: Vec<u32>,
    pub constants: Vec<Constant>,
    pub names: Vec<String>,
    pub interps: Vec<InterpRecipe>,
    pub functions: Vec<FnDef>,
    pub struct_defs: Vec<StructDef>,
    pub enum_defs: Vec<EnumDef>,
    /// Namespace references shared by construction preflight and construction
    /// instructions. Indirection keeps namespace-aware instructions compact.
    pub namespace_refs: Vec<NamespaceRef>,
    /// Source-order field names for struct and struct-variant construction
    /// preflight instructions.
    pub construct_fields: Vec<Vec<String>>,
    /// Positional modes and reference-place recipes for source calls.
    pub call_sites: Vec<CallSite>,
    /// Identifier-rooted mutable places used by deep assignments, explicit
    /// ref arguments, and method receivers.
    pub places: Vec<PlaceRecipe>,
    /// Match patterns referenced by `MatchFail` instructions. Pool entries
    /// are shared because matching may execute the same arm in a hot loop.
    pub patterns: Vec<PatternRecipe>,
    /// Side-table of `use` descriptors referenced by
    /// `Instr::Use`. Holds variable-length fields (path, items,
    /// alias) so the instruction payload stays `Copy`.
    pub use_specs: Vec<UseSpec>,
    /// `Some` when this module declares one or more `pub { ... }` surfaces.
    /// The names are unioned in source order. `Some([])` deliberately means
    /// an explicit empty public surface; `None` preserves legacy exports.
    pub public_surface: Option<Vec<String>>,
    /// Shared function-local name indexes referenced by `UseSpec` snapshots.
    /// Empty for top-level chunks, whose bindings already live in runtime
    /// scope maps.
    pub local_scopes: Vec<LocalScopeNames>,
    /// Instruction spans of every source loop in this chunk, used to
    /// attribute step-limit errors to the innermost enclosing loop
    /// header. Passive metadata — never consulted on the hot path.
    pub loop_ranges: Vec<LoopRange>,
    /// Slot count for this chunk when it serves as a function /
    /// lambda body. Zero at the top-level program chunk (where
    /// bindings live in the BTreeMap scope). The VM uses this
    /// to pre-size each call frame's `slots` vec exactly once.
    pub slot_count: u32,
    /// Positional ABI parameter -> canonical language binding slot.
    ///
    /// Repeated parameter spellings map to the same slot. Calls assign these
    /// slots in source order, so the last positional argument for a spelling
    /// becomes the binding visible to the body. Ref write-backs use the same
    /// map rather than assuming every positional parameter owns a slot.
    pub parameter_slots: Vec<SlotIdx>,
}

/// Compiled struct type record.
#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    pub fields: Vec<String>,
}

/// Compiled enum type record.
#[derive(Debug, Clone)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<EnumVariantDef>,
}

#[derive(Debug, Clone)]
pub struct EnumVariantDef {
    pub name: String,
    pub shape: EnumVariantShape,
}

/// Payload shape of a declared enum variant.
#[derive(Debug, Clone, PartialEq)]
pub enum EnumVariantShape {
    Unit,
    Tuple(Vec<String>),
    Struct(Vec<String>),
}

impl Chunk {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    pub fn constant(&self, idx: ConstIdx) -> &Constant {
        &self.constants[idx.0 as usize]
    }

    pub fn name(&self, idx: NameIdx) -> &str {
        &self.names[idx.0 as usize]
    }

    pub fn interp(&self, idx: InterpIdx) -> &InterpRecipe {
        &self.interps[idx.0 as usize]
    }

    pub fn function(&self, idx: FnIdx) -> &FnDef {
        &self.functions[idx.0 as usize]
    }

    pub fn struct_def(&self, idx: StructIdx) -> &StructDef {
        &self.struct_defs[idx.0 as usize]
    }

    pub fn enum_def(&self, idx: EnumIdx) -> &EnumDef {
        &self.enum_defs[idx.0 as usize]
    }

    pub fn construct_fields(&self, idx: ConstructFieldsIdx) -> &[String] {
        &self.construct_fields[idx.0 as usize]
    }

    pub fn call_site(&self, idx: CallSiteIdx) -> &CallSite {
        &self.call_sites[idx.0 as usize]
    }

    pub fn place(&self, idx: PlaceIdx) -> &PlaceRecipe {
        &self.places[idx.0 as usize]
    }

    pub fn namespace_ref(&self, idx: NamespaceIdx) -> NamespaceRef {
        self.namespace_refs[idx.index() as usize]
    }

    pub fn pattern(&self, idx: PatternIdx) -> &PatternRecipe {
        &self.patterns[idx.0 as usize]
    }

    pub fn use_spec(&self, idx: UseIdx) -> &UseSpec {
        &self.use_specs[idx.0 as usize]
    }

    /// Header line of the outermost source loop whose instruction
    /// span contains `offset`, if any. Loop spans nest properly, so
    /// the outermost match is the one that starts earliest.
    ///
    /// Outermost (not innermost) keeps step-limit error lines stable
    /// across engines: an infinite outer loop with a finite inner
    /// loop trips the budget at an engine-dependent phase, sometimes
    /// inside the inner loop and sometimes between passes, but the
    /// outermost active loop is the same either way.
    pub fn outermost_loop_line(&self, offset: CodeOffset) -> Option<u32> {
        self.loop_ranges
            .iter()
            .filter(|range| range.start.0 <= offset.0 && offset.0 < range.end.0)
            .min_by_key(|range| range.start.0)
            .map(|range| range.line)
    }
}

/// A pattern plus the lexical resolution selected for every namespace it
/// contains. Namespace names remain alongside their bytecode reference because
/// one recursive pattern can mention several distinct aliases.
#[derive(Debug, Clone)]
pub struct PatternRecipe {
    pub pattern: Rc<nybl::parser::Pattern>,
    pub namespaces: Vec<(String, NamespaceRef)>,
}

/// A compiled user-defined function.
#[derive(Debug, Clone)]
pub struct FnDef {
    pub name: String,
    pub params: Vec<String>,
    /// Positional parameter modes retained by every callable representation.
    pub param_modes: Vec<ParamMode>,
    /// Immutable compiled body shared by the defining chunk, function
    /// registry, and every closure materialised from this definition.
    pub chunk: Rc<Chunk>,
    /// Total slot count for this function's frame (params + every
    /// `let` / `for-in` variable assigned a slot by the compiler).
    /// The VM resizes the frame's `slots` vec to this length
    /// exactly once at call time, so `LoadLocal` / `StoreLocal`
    /// can index it without bounds-check surprises.
    pub slot_count: u32,
    /// Names this function body references that don't resolve to
    /// its own locals. Filled in by the compiler for lambdas and
    /// nested fn bodies; empty for named fn declarations (which
    /// don't capture — their bodies see only params + the global
    /// function registry, matching the walker's FnDecl semantics).
    ///
    /// Paired with `capture_sources` positionally: `captures[i]`
    /// tells the VM *where* in the enclosing frame to read
    /// `capture_names[i]`'s value at `MakeLambda` time.
    pub capture_names: Vec<String>,
    pub capture_sources: Vec<CaptureSource>,
}

/// Where a captured name's value comes from when a lambda is
/// materialised. Resolved at compile time by walking the enclosing
/// function's slot table, falling back to the BTreeMap scope stack
/// for captures-of-captures and module-top-level bindings.
#[derive(Debug, Clone)]
pub enum CaptureSource {
    /// Read `enclosing_frame.slots[SlotIdx]`. Covers the common
    /// case: a lambda referencing a local of its immediate
    /// enclosing function.
    ParentSlot(SlotIdx),
    /// Look the name up in `enclosing_frame.scopes` (walked inner
    /// -> outer). Covers nested-lambda captures-of-captures and
    /// module-top-level references. Carries the name again
    /// because the runtime needs it for the BTreeMap lookup.
    ParentScope(String),
}
