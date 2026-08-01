//! Bytecode VM execution.
//!
//! The stack-based dispatch loop executes a validated [`Chunk`] produced by
//! [`crate::compile`] and preserves the observable behavior of the
//! tree-walking evaluator in `nybl-lang`. The public surface includes
//! one-shot [`run`] / [`execute`] calls, the lower-level [`Vm`], and
//! persistent [`NyblInstance`] programs.
//!
//! # Stack model
//!
//! All runtime values live on a single internal slot stack. A slot is
//! either a [`Value`], an in-progress `for`-loop iterator, or an
//! in-progress `repeat` counter. Iterators and counters are sidecar
//! items pushed by [`Instr::MakeIter`] / [`Instr::MakeRepeatCount`]
//! and consumed by [`Instr::IterNext`] / [`Instr::RepeatNext`] on
//! exhaustion or [`Instr::PopLoopState`] on `break`; only bytecode that
//! participates in iteration ever sees them, so the rest of the dispatch
//! loop treats the stack as a stack of `Value`.
//!
//! # Frames
//!
//! Each function call — including the top-level program — runs in its
//! own internal frame. A frame owns its chunk (wrapped in `Rc` so repeated
//! calls share the compiled code), its instruction pointer, and its
//! lexical scope stack. Returning from a function pops the frame and
//! truncates the value stack back to the frame's base in case the body
//! left anything behind.
//!
//! # Resource limits
//!
//! [`NyblLimits`] is enforced exactly as in the tree-walker:
//!
//! - A tick fires at every bytecode dispatch. It bumps `steps`,
//!   checks `max_steps`, checks its explicit
//!   [`nybl::memory::MemoryContext`],
//!   and invokes [`NyblHost::on_tick`]. `max_memory` is shared with the
//!   tree-walker via the per-value allocation tracking in
//!   [`nybl::memory`], so no VM-specific bookkeeping is needed.
//! - The VM scales `max_steps` internally so a single
//!   source-level step, which typically maps to several bytecode ops,
//!   still fits under the tree-walker's calibration of
//!   `NyblLimits::standard()` / `NyblLimits::demo()`.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    format,
    rc::Rc,
    string::{String, ToString},
    vec,
    vec::Vec,
};

#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::rc::Rc;

use core::cell::{Cell, RefCell};

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::collections::{BTreeMap, BTreeSet};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::collections::{BTreeMap, BTreeSet};

use nybl::builtins::{self, error, error_fatal_with_hint, error_with_hint};
use nybl::error::{NyblError, NyblWarning};
use nybl::methods;
use nybl::ops;
use nybl::parser::{ParamMode, Visibility};
use nybl::value::{FnBody, NyblFn, NyblFnOrigin, Value};
use nybl::{EntryPoint, NyblHost, NyblLimits};

use crate::chunk::{
    CallSiteIdx, CaptureSource, Chunk, CodeOffset, Constant, EnumConstructShape, EnumIdx,
    EnumVariantShape, FnDef, FnIdx, Instr, InterpPart, LoopStateKind, NameIdx, NamespaceRef,
    PatternIdx, PlaceIdx, PlaceProjectionRecipe, RefArgTarget, SlotIdx, StructIdx,
};

/// Hard cap on call depth; matches the tree-walker.
const MAX_CALL_DEPTH: usize = 64;

/// `max_steps` is a source-level budget. One source-level statement
/// typically maps to several bytecode instructions, so the dispatch
/// loop gets a proportionally larger internal budget. Calibrated so
/// that `while true { }` still halts under `NyblLimits::demo()` and
/// small programs like fizzbuzz don't exhaust `standard()`.
const STEP_SCALE: u64 = 8;

/// Memory-limit check cadence. Checked every `1 << N` ticks; a
/// power-of-two window lets us AND with a mask instead of
/// dividing. 256 is a sweet spot — detection still lands within
/// microseconds of the limit being breached, and a tight
/// arithmetic loop reads the explicit account once per 256 instructions
/// instead of every instruction.
const TICK_MEMCHECK_MASK: u64 = 0xFF;

/// Maximum slot vecs kept on the freelist. Deep recursion with
/// varying slot counts could otherwise grow this unboundedly.
/// `MAX_CALL_DEPTH` (64) plus a little headroom is plenty — any
/// extra allocations just go back through the global allocator
/// like before.
const SLOTS_FREELIST_CAP: usize = 128;

// ─── Stack slot ────────────────────────────────────────────────────

enum Slot {
    Value(Value),
    /// Remaining items in reverse order — `pop()` yields the next one.
    /// Eager fast path for `for x in <array | string | dict>`.
    Iter(Vec<Value>),
    /// Iterator protocol state: holds a value whose `.next()`
    /// method returns `Iter::Next(v)` / `Iter::Done`. Produced
    /// when the for-loop's iterable is a `Value::Iter` or a
    /// user type; the `IterNext` opcode dispatches `.next()`
    /// through the regular method path.
    IterObject(Value),
    /// Remaining iterations for a `repeat` loop.
    Repeat(i64),
}

// ─── Frame ─────────────────────────────────────────────────────────

struct FrameScopeBases {
    types: usize,
    aliases: usize,
}

struct FunctionFrameContext {
    scope_bases: FrameScopeBases,
    function_module: Option<String>,
    lexical_context: Rc<ModuleLexicalContext>,
}

struct Frame {
    chunk: Rc<Chunk>,
    ip: usize,
    /// Flat slot array for this function's compile-time-resolved
    /// locals (params + every `let` / `for-in` variable assigned
    /// a slot). `LoadLocal(slot)` / `StoreLocal(slot)` read and
    /// write directly into this vec — the VM's hot path for
    /// variable access. Empty for module-top-level frames and
    /// match-body sub-scopes where slot resolution doesn't apply.
    slots: Vec<Value>,
    /// Slow-path BTreeMap scope stack. Used for module-top-level
    /// bindings, match-pattern bindings, captures snapshotted
    /// into lambdas, and anything else that reaches `LoadVar` /
    /// `DefineLocal` / `StoreVar`. Function frames that stay on
    /// the fast path never push into this at runtime.
    scopes: Vec<BTreeMap<String, Value>>,
    /// Number of value scopes owned by the frame at entry. `PopScope`
    /// may only remove scopes above this floor: the top-level frame and
    /// closures retain their initial scope, while named-function and method
    /// frames start at zero.
    scope_base: usize,
    /// Lazily inserted protected value scope for assignments that shadow an
    /// inherited declaration alias. It lives for the whole call even when
    /// the assignment occurs inside a nested runtime scope.
    has_declaration_overlay: bool,
    /// Protected depth of the VM-wide `type_bindings` stack at frame entry.
    /// Runtime scopes live above this depth and are paired with `scopes`.
    /// Function exit truncates back to the caller depth so an early return
    /// cannot leak type scopes whose `PopScope` was skipped.
    type_scope_base: usize,
    /// Protected depth of the VM-wide module-alias/imported-callable stacks at
    /// frame entry. Function lookup sees frame zero plus frames at/above it.
    alias_scope_base: usize,
    stack_base: usize,
    is_function: bool,
    /// Defining module for named-function and module-exported closure bodies.
    /// Used to resolve private sibling function bindings from the module cache.
    function_module: Option<String>,
    /// Immutable defining-module namespace shared by every call to this
    /// function. Mutable block/function-local declarations remain in the VM
    /// stacks above `alias_scope_base`.
    lexical_context: Rc<ModuleLexicalContext>,
    /// How this frame's return value should be transformed
    /// before being pushed for the caller. See [`FrameWrap`].
    wrap: FrameWrap,
    defining_environment_module: Option<String>,
    /// Names copied into this closure from an enclosing frame. They remain
    /// readable values but are never legal explicit `ref` targets.
    captured_names: BTreeSet<String>,
    /// Calls whose callee and static fences have passed preflight while their
    /// ordinary argument expressions are still being evaluated.
    prepared_calls: Vec<PreparedCall>,
    /// Copy-out records owned by this invocation. They are committed only by
    /// the normal-return path and are otherwise dropped during unwind.
    pending_write_backs: Vec<PendingWriteBack>,
    /// Exact declaration invoked for a direct named-function frame. Bare
    /// self-recursion resolves here before host/name dispatch.
    current_function_entry: Option<Rc<FnEntry>>,
}

impl Frame {
    fn top(chunk: Chunk, lexical_context: Rc<ModuleLexicalContext>) -> Self {
        let scopes = vec![BTreeMap::new()];
        Self {
            chunk: Rc::new(chunk),
            ip: 0,
            slots: Vec::new(),
            scope_base: scopes.len(),
            has_declaration_overlay: false,
            scopes,
            // The builtin type-binding map is the top frame's protected base.
            type_scope_base: 1,
            alias_scope_base: 1,
            stack_base: 0,
            is_function: false,
            function_module: None,
            lexical_context,
            wrap: FrameWrap::None,
            defining_environment_module: None,
            captured_names: BTreeSet::new(),
            prepared_calls: Vec::new(),
            pending_write_backs: Vec::new(),
            current_function_entry: None,
        }
    }

    fn function(
        chunk: Rc<Chunk>,
        slots: Vec<Value>,
        scopes: Vec<BTreeMap<String, Value>>,
        stack_base: usize,
        context: FunctionFrameContext,
        wrap: FrameWrap,
    ) -> Self {
        Self {
            chunk,
            ip: 0,
            slots,
            scope_base: scopes.len(),
            has_declaration_overlay: false,
            scopes,
            type_scope_base: context.scope_bases.types,
            alias_scope_base: context.scope_bases.aliases,
            stack_base,
            is_function: true,
            function_module: context.function_module,
            lexical_context: context.lexical_context,
            wrap,
            defining_environment_module: None,
            captured_names: BTreeSet::new(),
            prepared_calls: Vec::new(),
            pending_write_backs: Vec::new(),
            current_function_entry: None,
        }
    }

    fn caller_type_scope_depth(&self) -> usize {
        if self.is_function {
            self.type_scope_base.saturating_sub(1)
        } else {
            self.type_scope_base
        }
    }
}

/// Post-processing applied to a frame's return value / error at
/// the moment it resumes the caller. Most frames don't need
/// any — only callable-dispatch helpers (`try_call`, Result's
/// `map` / `map_err`) set this so the engine can wrap the
/// closure's bare return value without the user having to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameWrap {
    /// Plain return — push the value as-is.
    None,
    /// `try_call(f)` landing pad: wrap a clean return in
    /// `Result::Ok(v)`; a non-fatal error unwinds to this frame
    /// and pushes `Result::Err(RuntimeError { … })` instead.
    /// Fatal errors still bypass the trap — see
    /// `NyblError::is_fatal`.
    TryCall { line: u32 },
    /// `r.map(f)` landing pad: wrap the clean return in
    /// `Result::Ok(v)`. Errors in `f` propagate unchanged.
    ResultOk { line: u32 },
    /// `r.map_err(f)` landing pad: wrap the clean return in
    /// `Result::Err(v)`. Errors in `f` propagate unchanged.
    ResultErr { line: u32 },
    /// `MakeIter` landing pad for user-typed iterables: the
    /// frame ran the user's `.iter()` method; its return value
    /// should become a `Slot::IterObject` on the caller's
    /// stack instead of the usual `Slot::Value`.
    IterStart,
    /// `IterNext` landing pad for user-typed iterators: the
    /// frame ran the user's `.next()` method. Inspect the
    /// return (expected `Iter::Next(x)` / `Iter::Done`) and
    /// either push `x` (so the subsequent slot store picks it
    /// up) or pop the iterator and jump to the exit target.
    IterAdvance(crate::chunk::CodeOffset),
}

/// Destructured view of a value returned from an iterator's
/// `.next()` method — either `Iter::Next(v)`, `Iter::Done`, or
/// anything else (user bug, surfaced as a runtime error).
enum IterStep {
    Next(Value),
    Done,
    Malformed,
}

fn unwrap_iter_step(v: &Value) -> IterStep {
    let e = match v {
        Value::EnumVariant(e) => e,
        _ => return IterStep::Malformed,
    };
    if e.type_name() != "Iter" {
        return IterStep::Malformed;
    }
    match (e.variant(), e.payload()) {
        ("Next", nybl::value::EnumPayload::Tuple(items)) if items.len() == 1 => {
            IterStep::Next(items[0].clone())
        }
        ("Done", nybl::value::EnumPayload::Unit) => IterStep::Done,
        _ => IterStep::Malformed,
    }
}

struct FnEntry {
    exact_self_name: Option<String>,
    params: Vec<String>,
    param_modes: Vec<ParamMode>,
    chunk: Rc<Chunk>,
    module_path: String,
    declaration_alias_names: BTreeSet<String>,
}

enum PreparedCallable {
    User(Rc<FnEntry>),
    /// Value-only named declarations retain the language's established
    /// host-before-user-function dispatch. Ref-aware declarations stay
    /// `User` so their mode metadata can drive transactional staging.
    HostThenUser {
        name: String,
        entry: Rc<FnEntry>,
    },
    Closure(Rc<NyblFn>),
    UserMethod {
        entry: Rc<FnEntry>,
        receiver: Value,
        receiver_target: Option<NamespaceRef>,
        receiver_place: Option<ResolvedPlace>,
    },
    DeferredMethod {
        receiver: Value,
        method: String,
        nested_place: bool,
    },
    NamedMethodInPlace {
        target: NamespaceRef,
        method: String,
    },
    PlaceMethodInPlace {
        place: ResolvedPlace,
        method: String,
    },
    /// Builtins and host callables are value-only. Host arity is intentionally
    /// deferred because the embedding trait does not expose signature metadata.
    NamedFallback(String),
}

impl PreparedCallable {
    fn named_user(name: &str, entry: Rc<FnEntry>) -> Self {
        if entry.param_modes.iter().all(|mode| *mode != ParamMode::Ref) {
            Self::HostThenUser {
                name: name.to_string(),
                entry,
            }
        } else {
            Self::User(entry)
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum ResolvedRefTarget {
    Slot {
        frame: usize,
        slot: SlotIdx,
    },
    Scope {
        frame: usize,
        scope: usize,
        name: String,
    },
}

#[derive(Clone)]
enum ResolvedPlaceProjection {
    Index(Value),
    Field(String),
}

#[derive(Clone)]
struct ResolvedPlace {
    target: ResolvedRefTarget,
    root_recipe: NamespaceRef,
    root_name: String,
    root: Value,
    projections: Vec<ResolvedPlaceProjection>,
}

struct PreparedRef {
    param: usize,
    recipe: RefArgTarget,
    index_count: usize,
}

enum PreparedReceiverRef {
    Binding(NamespaceRef),
    Place(ResolvedPlace),
}

struct PreparedCall {
    site: CallSiteIdx,
    callable: PreparedCallable,
    display_name: String,
    refs: Vec<PreparedRef>,
    receiver_ref: Option<PreparedReceiverRef>,
    value_count: usize,
    ref_index_count: usize,
}

struct PendingWriteBack {
    parameter: usize,
    place: ResolvedPlace,
}

/// Structural equality check for two enum-variant lists.
/// Used when the same module is resolved via two paths so a
/// re-import is idempotent rather than an error.
///
/// Matches the walker's `variants_equivalent` rule: variant
/// order + names must match, tuple variants compare by arity
/// only (payload names are positional stubs with no runtime
/// meaning), struct variants still require matching field
/// names.
fn shapes_match(a: &[(String, EnumVariantShape)], b: &[(String, EnumVariantShape)]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|((an, av), (bn, bv))| {
        if an != bn {
            return false;
        }
        match (av, bv) {
            (EnumVariantShape::Unit, EnumVariantShape::Unit) => true,
            (EnumVariantShape::Tuple(fa), EnumVariantShape::Tuple(fb)) => fa.len() == fb.len(),
            (EnumVariantShape::Struct(fa), EnumVariantShape::Struct(fb)) => fa == fb,
            _ => false,
        }
    })
}

type TypeKey = (String, String);
type RuntimeEnumVariants = Vec<(String, EnumVariantShape)>;
type StructTypeTable = BTreeMap<TypeKey, Vec<String>>;
type EnumTypeTable = BTreeMap<TypeKey, RuntimeEnumVariants>;
type BuiltinTypeTables = (StructTypeTable, EnumTypeTable, BTreeMap<String, String>);
type ModuleStructDef = (TypeKey, Vec<String>);
type ModuleEnumDef = (TypeKey, RuntimeEnumVariants);
type ModuleMethodDef = (TypeKey, String, Rc<FnEntry>);

/// Seed a VM's type tables with the engine-wide builtin shapes
/// (`Result`, `RuntimeError`). Same source of truth as the walker
/// — both engines read `nybl::builtins::builtin_*` so they can
/// never drift out of sync.
///
/// Returns:
/// - `struct_defs` keyed by `(module_path, type_name)`, with the
///   builtin entry under `<builtin>`;
/// - `enum_defs` keyed the same way;
/// - an outer-scope type_bindings map pointing each builtin's
///   bare name at `<builtin>` so user code resolves `Result::Ok`
///   without an explicit `use`.
fn seed_builtin_types() -> BuiltinTypeTables {
    use nybl::parser::VariantKind;
    let builtin_mp = nybl::value::BUILTIN_MODULE_PATH.to_string();
    let mut struct_defs: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut enum_defs: BTreeMap<(String, String), Vec<(String, EnumVariantShape)>> =
        BTreeMap::new();
    let mut bindings: BTreeMap<String, String> = BTreeMap::new();
    struct_defs.insert(
        (builtin_mp.clone(), String::from("RuntimeError")),
        nybl::builtins::builtin_runtime_error_fields(),
    );
    bindings.insert(String::from("RuntimeError"), builtin_mp.clone());
    let variants: Vec<(String, EnumVariantShape)> = nybl::builtins::builtin_result_variants()
        .into_iter()
        .map(|v| {
            let shape = match v.kind {
                VariantKind::Unit => EnumVariantShape::Unit,
                VariantKind::Tuple(fs) => EnumVariantShape::Tuple(fs),
                VariantKind::Struct(fs) => EnumVariantShape::Struct(fs),
            };
            (v.name, shape)
        })
        .collect();
    enum_defs.insert((builtin_mp.clone(), String::from("Result")), variants);
    bindings.insert(String::from("Result"), builtin_mp.clone());
    let iter_variants: Vec<(String, EnumVariantShape)> = nybl::builtins::builtin_iter_variants()
        .into_iter()
        .map(|v| {
            let shape = match v.kind {
                VariantKind::Unit => EnumVariantShape::Unit,
                VariantKind::Tuple(fs) => EnumVariantShape::Tuple(fs),
                VariantKind::Struct(fs) => EnumVariantShape::Struct(fs),
            };
            (v.name, shape)
        })
        .collect();
    enum_defs.insert((builtin_mp.clone(), String::from("Iter")), iter_variants);
    bindings.insert(String::from("Iter"), builtin_mp);
    (struct_defs, enum_defs, bindings)
}

/// Cached result of loading a module. `Loading` is the
/// in-progress sentinel for circular-import detection; `Loaded`
/// carries every top-level export — bindings, struct/enum
/// decls, and methods — so the importer can rebuild the
/// module's visible surface without re-evaluating it.
#[allow(clippy::large_enum_variant)]
enum ImportSlot {
    Loading,
    Loaded(ModuleArtifacts),
}

#[derive(Clone)]
struct ModuleArtifacts {
    /// Whether the module opted into an explicit `pub { ... }` allow-list.
    /// Legacy modules retain underscore/selective compatibility rules.
    explicit_surface: bool,
    /// Top-level `let` bindings, reified as `Value`s. Fns live
    /// in `fn_decls` instead so the importer can register them
    /// in `self.functions` for cross-fn call resolution.
    bindings: Vec<(String, Value)>,
    /// Declaring module and binding for every exported value. Facades retain
    /// this provenance so their live execution environment can borrow the
    /// authoritative handle rather than cloning a stale snapshot.
    binding_origins: BTreeMap<String, BindingOrigin>,
    /// Top-level `fn` declarations. The importer copies each
    /// into its own `self.functions` table AND pushes a
    /// `Value::Fn` into the current scope so first-class use
    /// (`let g = some_imported_fn`) still works.
    fn_decls: Vec<(String, Rc<FnEntry>)>,
    /// Struct-type declarations introduced by the module,
    /// keyed by their full identity `(module_path, type_name)`.
    struct_defs: Vec<ModuleStructDef>,
    /// Enum-type declarations introduced by the module, same
    /// keying as `struct_defs`.
    enum_defs: Vec<ModuleEnumDef>,
    /// `((module_path, type_name), method_name, FnEntry)` for
    /// every method the module declared on its own types.
    methods: Vec<ModuleMethodDef>,
    /// Public type names exposed by this module and their declaration origins.
    type_exports: BTreeMap<String, String>,
    /// Exported values that remain module namespaces after value-level winner
    /// resolution. Values retain their original path and selected surface.
    module_exports: BTreeMap<String, Rc<nybl::value::NyblModule>>,
    /// Private module-owned context restored for calls declared in this
    /// module. This is intentionally separate from the exported surface.
    lexical_context: Rc<ModuleLexicalContext>,
}

#[derive(Clone, Default)]
struct ModuleLexicalContext {
    type_bindings: Rc<BTreeMap<String, String>>,
    module_aliases: Rc<BTreeMap<String, Rc<nybl::value::NyblModule>>>,
    imported_functions: Rc<BTreeMap<String, Rc<FnEntry>>>,
}

/// Import cache shared across nested VMs so recursive imports
/// resolve exactly once per top-level run.
type ImportCache = Rc<RefCell<BTreeMap<String, ImportSlot>>>;
type LiveValueEnvironments = Rc<RefCell<BTreeMap<String, BTreeMap<String, Value>>>>;
type BindingOrigin = (String, String);

fn snapshot_module_bindings(
    top_scope: &BTreeMap<String, Value>,
    binding_origins: &BTreeMap<String, BindingOrigin>,
    module_path: &str,
    imports: &ImportCache,
    line: u32,
) -> Result<Vec<(String, Value)>, NyblError> {
    // Re-exported value bindings can reuse their origin's already-external
    // snapshot. Named functions are intentionally absent from `bindings`
    // (their callable metadata lives in `fn_decls`), so those are snapshot
    // locally with the module's own values instead.
    let forwarded: BTreeMap<String, Value> = {
        let imports = imports.borrow();
        top_scope
            .keys()
            .filter_map(|name| {
                let origin = binding_origins.get(name)?;
                if origin.0 == module_path && origin.1 == *name {
                    return None;
                }
                let ImportSlot::Loaded(origin_module) = imports.get(&origin.0)? else {
                    return None;
                };
                origin_module
                    .bindings
                    .iter()
                    .find(|(origin_name, _)| origin_name == &origin.1)
                    .map(|(_, value)| (name.clone(), value.clone()))
            })
            .collect()
    };
    let local: BTreeMap<String, Value> = top_scope
        .iter()
        .filter(|(name, _)| !forwarded.contains_key(*name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let mut local_snapshots: BTreeMap<String, Value> =
        Value::__compatibility_snapshot_bindings(&local, line)?
            .into_iter()
            .collect();

    top_scope
        .keys()
        .map(|name| {
            let value = forwarded
                .get(name)
                .cloned()
                .or_else(|| local_snapshots.remove(name))
                .ok_or_else(|| {
                    error(line, format!("Missing compatibility snapshot for `{name}`"))
                })?;
            Ok((name.clone(), value))
        })
        .collect()
}
type LiveBindingOrigins = Rc<RefCell<BTreeMap<String, BTreeMap<String, BindingOrigin>>>>;

#[derive(Clone)]
struct ModuleRuntime {
    imports: ImportCache,
    environments: LiveValueEnvironments,
    origins: LiveBindingOrigins,
}

impl ModuleRuntime {
    fn empty() -> Self {
        Self {
            imports: Rc::new(RefCell::new(BTreeMap::new())),
            environments: Rc::new(RefCell::new(BTreeMap::new())),
            origins: Rc::new(RefCell::new(BTreeMap::new())),
        }
    }
}

fn take_live_environment(
    environments: &LiveValueEnvironments,
    module_path: &str,
    origins: &BTreeMap<String, BindingOrigin>,
) -> BTreeMap<String, Value> {
    let mut environments = environments.borrow_mut();
    let mut environment = environments.remove(module_path).unwrap_or_default();
    for (binding, (origin_module, origin_binding)) in origins {
        if origin_module == module_path && origin_binding == binding {
            continue;
        }
        if let Some(value) = environments
            .get_mut(origin_module)
            .and_then(|origin| origin.remove(origin_binding))
        {
            environment.insert(binding.clone(), value);
        }
    }
    environment
}

fn put_live_environment(
    environments: &LiveValueEnvironments,
    module_path: &str,
    mut environment: BTreeMap<String, Value>,
    origins: &BTreeMap<String, BindingOrigin>,
) {
    let mut environments = environments.borrow_mut();
    for (binding, (origin_module, origin_binding)) in origins {
        if origin_module == module_path && origin_binding == binding {
            continue;
        }
        if let Some(value) = environment.remove(binding) {
            environments
                .entry(origin_module.clone())
                .or_default()
                .insert(origin_binding.clone(), value);
        }
    }
    environments.insert(module_path.to_string(), environment);
}

// ─── Next action ───────────────────────────────────────────────────

enum Next {
    /// Keep fetching the next instruction.
    Continue,
    /// End the program (top-level `Halt`).
    Halt,
}

// ─── VM ────────────────────────────────────────────────────────────

/// Stack machine that executes a compiled [`Chunk`].
pub struct Vm<'h, H: NyblHost + ?Sized> {
    frames: Vec<Frame>,
    stack: Vec<Slot>,
    /// User-declared functions, keyed by name. Wrapped in `Rc`
    /// so the call path can take a cheap handle (one refcount
    /// bump) instead of cloning the params `Vec<String>` and
    /// re-taking the chunk `Rc` on every invocation — a hot
    /// path that ran ~500 000 times in `fib(28)`.
    functions: BTreeMap<String, Rc<FnEntry>>,
    host: &'h mut H,
    steps: u64,
    step_budget: u64,
    rand_state: u64,
    imports: ImportCache,
    imported_here: Vec<BTreeSet<String>>,
    limits: NyblLimits,
    /// Module this VM is running — tags newly declared types
    /// with their full identity so two modules declaring the
    /// same name remain distinct. `<root>` at the top level,
    /// the dot-joined module path inside a `use`'d module's
    /// sub-VM.
    current_module: String,
    /// Declared struct types, keyed by full identity
    /// `(module_path, type_name)`. Populated by
    /// `DefineStruct` (which tags with `current_module`) and
    /// merged from imported modules via `exec_use`.
    struct_defs: BTreeMap<(String, String), Vec<String>>,
    /// Declared enum types, same `(module_path, type_name)`
    /// keying as `struct_defs`.
    enum_defs: BTreeMap<(String, String), Vec<(String, EnumVariantShape)>>,
    /// User-defined methods. Outer key is the receiver type's
    /// full identity `(module_path, type_name)`; inner is the
    /// method name. A method declared for `paint.Color` doesn't
    /// fire on `other.Color` — identity is strict.
    user_methods: BTreeMap<(String, String), BTreeMap<String, Rc<FnEntry>>>,
    /// Public type projection for the module currently executing.
    type_exports: BTreeMap<String, String>,
    /// Parallel scope-stack for bare-name type and module-alias
    /// resolution. Pushed / popped in lockstep with frame scopes,
    /// plus a fresh frame at fn-call entry so a fn's own type
    /// decls are isolated from the caller's scope. `<builtin>`
    /// types seed scope 0.
    type_bindings: Vec<BTreeMap<String, String>>,
    /// Module-level aliases. Unlike value scope, these persist
    /// across function boundaries so namespaced references
    /// inside fn bodies (`p.Color::Red` in a pattern) can still
    /// resolve to the aliased module.
    module_aliases: Vec<BTreeMap<String, Rc<nybl::value::NyblModule>>>,
    /// Non-aliased imported callables, paired with lexical scope frames.
    imported_functions: Vec<BTreeMap<String, Rc<FnEntry>>>,
    /// Current module namespace. Rebuilt only when top-level declarations or
    /// imports change; calls clone this single Rc handle.
    root_lexical_context: Rc<ModuleLexicalContext>,
    /// Freelist of cleared slot vecs from popped frames. Every
    /// fn call needs a fresh `Vec<Value>` sized to `slot_count`
    /// — allocating a new one per call was ~500k small heap
    /// allocations under `fib(28)`. On return we truncate + park;
    /// next call grabs one, resizes in place.
    slots_freelist: Vec<Vec<Value>>,
    root_function_visibility: BTreeMap<u32, Visibility>,
    abi_declarations: Vec<(String, Rc<FnEntry>)>,
    function_origin: NyblFnOrigin,
    memory: nybl::memory::MemoryContext,
    live_value_environments: LiveValueEnvironments,
    binding_origins: BTreeMap<String, BindingOrigin>,
    live_binding_origins: LiveBindingOrigins,
    /// Non-fatal runtime diagnostics accumulated across the root VM and any
    /// nested module VMs. Only a public operation boundary writes them.
    runtime_warnings: Vec<NyblWarning>,
}

struct VmState {
    frames: Vec<Frame>,
    stack: Vec<Slot>,
    functions: BTreeMap<String, Rc<FnEntry>>,
    rand_state: u64,
    imports: ImportCache,
    imported_here: Vec<BTreeSet<String>>,
    current_module: String,
    struct_defs: BTreeMap<(String, String), Vec<String>>,
    enum_defs: BTreeMap<(String, String), Vec<(String, EnumVariantShape)>>,
    user_methods: BTreeMap<(String, String), BTreeMap<String, Rc<FnEntry>>>,
    type_exports: BTreeMap<String, String>,
    type_bindings: Vec<BTreeMap<String, String>>,
    module_aliases: Vec<BTreeMap<String, Rc<nybl::value::NyblModule>>>,
    imported_functions: Vec<BTreeMap<String, Rc<FnEntry>>>,
    root_lexical_context: Rc<ModuleLexicalContext>,
    slots_freelist: Vec<Vec<Value>>,
    root_function_visibility: BTreeMap<u32, Visibility>,
    abi_declarations: Vec<(String, Rc<FnEntry>)>,
    function_origin: NyblFnOrigin,
    live_value_environments: LiveValueEnvironments,
    binding_origins: BTreeMap<String, BindingOrigin>,
    live_binding_origins: LiveBindingOrigins,
    runtime_warnings: Vec<NyblWarning>,
}

/// A loaded bytecode program whose globals and module state survive calls.
pub struct NyblInstance {
    state: Option<VmState>,
    entries: Vec<EntryPoint>,
    limits: NyblLimits,
    in_operation: Cell<bool>,
    memory: nybl::memory::MemoryContext,
}

impl<'h, H: NyblHost + ?Sized> Vm<'h, H> {
    /// Construct a VM from trusted bytecode.
    ///
    /// Embedders executing a hand-built or deserialized [`Chunk`] should use
    /// [`execute`], which validates structural bytecode invariants first.
    pub fn new(chunk: Chunk, host: &'h mut H, limits: NyblLimits) -> Self {
        let memory = nybl::memory::MemoryContext::__new(limits.max_memory);
        Self::new_internal(
            chunk,
            host,
            limits,
            ModuleRuntime::empty(),
            String::from(nybl::value::ROOT_MODULE_PATH),
            BTreeMap::new(),
            NyblFnOrigin::__instance("vm"),
            memory,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_internal(
        chunk: Chunk,
        host: &'h mut H,
        limits: NyblLimits,
        module_runtime: ModuleRuntime,
        current_module: String,
        root_function_visibility: BTreeMap<u32, Visibility>,
        function_origin: NyblFnOrigin,
        memory: nybl::memory::MemoryContext,
    ) -> Self {
        let step_budget = limits.max_steps.saturating_mul(STEP_SCALE);
        let (struct_defs, enum_defs, builtin_bindings) = seed_builtin_types();
        let root_lexical_context = Rc::new(ModuleLexicalContext {
            type_bindings: Rc::new(builtin_bindings.clone()),
            module_aliases: Rc::new(BTreeMap::new()),
            imported_functions: Rc::new(BTreeMap::new()),
        });
        // The top frame resolves directly through the VM's live scope maps.
        // Keeping another strong reference to the root lexical context would
        // force copy-on-write publication to clone on every declaration.
        let top = Frame::top(chunk, Rc::new(ModuleLexicalContext::default()));
        Self {
            frames: vec![top],
            stack: Vec::new(),
            functions: BTreeMap::new(),
            host,
            steps: 0,
            step_budget,
            rand_state: 0,
            imports: module_runtime.imports,
            imported_here: vec![BTreeSet::new()],
            limits,
            current_module,
            struct_defs,
            enum_defs,
            user_methods: BTreeMap::new(),
            type_exports: BTreeMap::new(),
            type_bindings: vec![builtin_bindings],
            module_aliases: vec![BTreeMap::new()],
            imported_functions: vec![BTreeMap::new()],
            root_lexical_context,
            slots_freelist: Vec::new(),
            root_function_visibility,
            abi_declarations: Vec::new(),
            function_origin,
            memory,
            live_value_environments: module_runtime.environments,
            binding_origins: BTreeMap::new(),
            live_binding_origins: module_runtime.origins,
            runtime_warnings: Vec::new(),
        }
    }

    fn into_state(self) -> VmState {
        VmState {
            frames: self.frames,
            stack: self.stack,
            functions: self.functions,
            rand_state: self.rand_state,
            imports: self.imports,
            imported_here: self.imported_here,
            current_module: self.current_module,
            struct_defs: self.struct_defs,
            enum_defs: self.enum_defs,
            user_methods: self.user_methods,
            type_exports: self.type_exports,
            type_bindings: self.type_bindings,
            module_aliases: self.module_aliases,
            imported_functions: self.imported_functions,
            root_lexical_context: self.root_lexical_context,
            slots_freelist: self.slots_freelist,
            root_function_visibility: self.root_function_visibility,
            abi_declarations: self.abi_declarations,
            function_origin: self.function_origin,
            live_value_environments: self.live_value_environments,
            binding_origins: self.binding_origins,
            live_binding_origins: self.live_binding_origins,
            runtime_warnings: self.runtime_warnings,
        }
    }

    fn from_state(
        state: VmState,
        host: &'h mut H,
        limits: NyblLimits,
        memory: nybl::memory::MemoryContext,
    ) -> Self {
        Self {
            frames: state.frames,
            stack: state.stack,
            functions: state.functions,
            host,
            steps: 0,
            step_budget: limits.max_steps.saturating_mul(STEP_SCALE),
            rand_state: state.rand_state,
            imports: state.imports,
            imported_here: state.imported_here,
            limits,
            current_module: state.current_module,
            struct_defs: state.struct_defs,
            enum_defs: state.enum_defs,
            user_methods: state.user_methods,
            type_exports: state.type_exports,
            type_bindings: state.type_bindings,
            module_aliases: state.module_aliases,
            imported_functions: state.imported_functions,
            root_lexical_context: state.root_lexical_context,
            slots_freelist: state.slots_freelist,
            root_function_visibility: state.root_function_visibility,
            abi_declarations: state.abi_declarations,
            function_origin: state.function_origin,
            memory,
            live_value_environments: state.live_value_environments,
            binding_origins: state.binding_origins,
            live_binding_origins: state.live_binding_origins,
            runtime_warnings: state.runtime_warnings,
        }
    }

    fn restore_instance_baseline(&mut self) {
        while self.frames.len() > 1 {
            let mut frame = self.frames.pop().expect("frame present");
            self.store_frame_defining_environment(&mut frame);
            if !frame.slots.is_empty() {
                self.return_slots(frame.slots);
            }
        }
        self.restore_active_defining_environment();
        self.stack.clear();
        if let Some(root) = self.frames.first_mut() {
            root.scopes.truncate(root.scope_base);
        }
        self.type_bindings.truncate(1);
        self.module_aliases.truncate(1);
        self.imported_functions.truncate(1);
        self.imported_here.truncate(1);
    }

    /// Grab a slot vec from the freelist or allocate a fresh one,
    /// then size it to `len` with `Value::None` placeholders.
    /// Callers write their args into the first few entries after
    /// calling this.
    fn take_slots(&mut self, len: usize) -> Vec<Value> {
        let mut v = self.slots_freelist.pop().unwrap_or_default();
        if v.capacity() < len {
            v.reserve(len - v.capacity());
        }
        v.resize(len, Value::None);
        v
    }

    /// Return a slot vec to the freelist after a frame pops. We
    /// clear (not deallocate) so the next call reuses the backing
    /// allocation. Capped so a pathological call graph doesn't
    /// grow unbounded memory.
    fn return_slots(&mut self, mut slots: Vec<Value>) {
        if self.slots_freelist.len() >= SLOTS_FREELIST_CAP {
            return;
        }
        slots.clear();
        self.slots_freelist.push(slots);
    }

    pub fn run(mut self) -> Result<(), NyblError> {
        let result = (|| {
            let mut last_line: u32 = 0;
            while let Some((instr, line)) = self.fetch() {
                last_line = line;
                // Tick errors (resource-limit violations) are always
                // fatal, so `unwind_to_try_call` will short-circuit
                // them. The path still goes through the helper so
                // the two error paths behave identically.
                if let Err(err) = self.tick(line) {
                    let err = self.attach_frame_module_context(err);
                    self.unwind_to_try_call(err)?;
                    continue;
                }
                match self.dispatch(instr, line) {
                    Ok(Next::Continue) => {}
                    Ok(Next::Halt) => break,
                    Err(err) => {
                        let err = self.attach_frame_module_context(err);
                        self.unwind_to_try_call(err)?;
                    }
                }
            }
            // Programs that allocate past the memory limit and then
            // finish in fewer ticks than the periodic memory-check
            // cadence (see `TICK_MEMCHECK_MASK`) would otherwise slip
            // through silently. Catch them here — cheap, and a
            // program ending always runs this exactly once.
            if self.memory.__exceeded() {
                return Err(error_fatal_with_hint(
                    last_line,
                    "Memory limit exceeded",
                    "Your code is using too much memory. Check for large strings or arrays growing in loops.",
                ));
            }
            Ok(())
        })();
        self.write_runtime_warnings();
        result
    }

    /// Deliver accumulated warnings once at an externally visible execution
    /// boundary. The Vec exists in no_std builds too; only stderr delivery is
    /// unavailable there.
    fn write_runtime_warnings(&mut self) {
        #[cfg(any(feature = "std", not(feature = "no_std")))]
        for warning in self.runtime_warnings.drain(..) {
            eprintln!("warning: {}", warning.message);
        }
        #[cfg(all(feature = "no_std", not(feature = "std")))]
        self.runtime_warnings.clear();
    }

    // ─── Fetch / ip ──────────────────────────────────────────────

    #[inline]
    fn fetch(&mut self) -> Option<(Instr, u32)> {
        let frame = self.frames.last_mut()?;
        if frame.ip >= frame.chunk.code.len() {
            return None;
        }
        let instr = frame.chunk.code[frame.ip];
        let line = frame.chunk.lines[frame.ip];
        frame.ip += 1;
        Some((instr, line))
    }

    fn jump(&mut self, target: CodeOffset) {
        if let Some(frame) = self.frames.last_mut() {
            frame.ip = target.0 as usize;
        }
    }

    // ─── Tick ────────────────────────────────────────────────────

    #[inline]
    fn tick(&mut self, line: u32) -> Result<(), NyblError> {
        self.steps += 1;
        if self.steps > self.step_budget {
            // Fatal — `try_call` can't catch this or the
            // step-limit sandbox invariant would break.
            //
            // Which instruction the budget trips on is an accident
            // of step-count phase (and differs from the walker's
            // per-statement accounting), so report the outermost
            // active source loop's header instead — the walker does
            // the same, keeping the two engines' error lines
            // aligned.
            return Err(error_fatal_with_hint(
                self.active_loop_line().unwrap_or(line),
                "Your code took too many steps (possible infinite loop)",
                "Check your loops — make sure they have a condition that eventually stops them.",
            ));
        }
        // Reading the explicit memory account is cheap in absolute
        // terms, but done every instruction it dominates a tight
        // arithmetic loop.
        // Allocations enter through `Value::Clone` /
        // constructors, so we only need to notice when the
        // counter crosses the limit. Checking every 256 ticks
        // caps detection latency at ~256 instructions worth of
        // allocations — negligible for a hard cap that's already
        // elastic by a few MB.
        if self.steps & TICK_MEMCHECK_MASK == 0 && self.memory.__exceeded() {
            return Err(error_fatal_with_hint(
                line,
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            ));
        }
        self.host.on_tick()?;
        Ok(())
    }

    /// Header line of the outermost source loop the VM is currently
    /// executing, scanning call frames outermost-first. For the
    /// active frame `ip - 1` is the instruction being ticked; for a
    /// caller frame it is the `Call` that entered the callee, so a
    /// budget trip inside a loop-free callee still resolves to the
    /// calling loop. `None` when no frame is inside a loop (e.g.
    /// branching recursion exhausting the budget). See
    /// [`Chunk::outermost_loop_line`] for why outermost.
    fn active_loop_line(&self) -> Option<u32> {
        self.frames.iter().find_map(|frame| {
            let offset = CodeOffset(frame.ip.saturating_sub(1) as u32);
            frame.chunk.outermost_loop_line(offset)
        })
    }

    // ─── Stack helpers ───────────────────────────────────────────

    #[inline]
    fn push_value(&mut self, v: Value) {
        self.stack.push(Slot::Value(v));
    }

    #[inline]
    fn pop_value(&mut self, line: u32) -> Result<Value, NyblError> {
        match self.stack.pop() {
            Some(Slot::Value(v)) => Ok(v),
            Some(_) => Err(error(line, "VM: expected value on stack")),
            None => Err(error(line, "VM: stack underflow")),
        }
    }

    #[inline]
    fn peek_value(&self, line: u32) -> Result<&Value, NyblError> {
        match self.stack.last() {
            Some(Slot::Value(v)) => Ok(v),
            Some(_) => Err(error(line, "VM: expected value on stack")),
            None => Err(error(line, "VM: stack underflow")),
        }
    }

    fn pop_n_values(&mut self, n: usize, line: u32) -> Result<Vec<Value>, NyblError> {
        if self.stack.len() < n {
            return Err(error(line, "VM: stack underflow"));
        }
        let start = self.stack.len() - n;
        let mut out = Vec::with_capacity(n);
        for slot in self.stack.drain(start..) {
            match slot {
                Slot::Value(v) => out.push(v),
                _ => return Err(error(line, "VM: expected value on stack")),
            }
        }
        Ok(out)
    }

    // ─── Scope ───────────────────────────────────────────────────

    fn current_scopes_mut(&mut self) -> &mut Vec<BTreeMap<String, Value>> {
        &mut self.frames.last_mut().expect("frame present").scopes
    }

    fn current_scopes(&self) -> &Vec<BTreeMap<String, Value>> {
        &self.frames.last().expect("frame present").scopes
    }

    fn push_scope(&mut self) {
        self.current_scopes_mut().push(BTreeMap::new());
        // Type bindings parallel value scopes so inline type
        // decls inside a block vanish on block exit, same rule
        // the walker applies.
        self.type_bindings.push(BTreeMap::new());
        self.module_aliases.push(BTreeMap::new());
        self.imported_functions.push(BTreeMap::new());
        self.imported_here.push(BTreeSet::new());
    }

    fn pop_scope(&mut self) {
        let popped = {
            let frame = self.frames.last_mut().expect("frame present");
            if frame.scopes.len() > frame.scope_base {
                frame.scopes.pop();
                true
            } else {
                false
            }
        };
        if popped {
            let type_scope_base = self.frames.last().expect("frame present").type_scope_base;
            debug_assert!(
                self.type_bindings.len() > type_scope_base,
                "value and type scope stacks must stay paired"
            );
            if self.type_bindings.len() > type_scope_base {
                self.type_bindings.pop();
                self.module_aliases.pop();
                self.imported_functions.pop();
                self.imported_here.pop();
            }
        }
    }

    /// Push a function-like frame together with its protected type-binding
    /// scope. Keeping this transition in one helper prevents constructors for
    /// named functions, closures, methods, and iterator methods from drifting.
    fn push_function_frame(
        &mut self,
        chunk: Rc<Chunk>,
        slots: Vec<Value>,
        scopes: Vec<BTreeMap<String, Value>>,
        stack_base: usize,
        function_module: Option<String>,
        wrap: FrameWrap,
    ) {
        self.park_active_defining_environment();
        let defining_environment = function_module.as_ref().map(|module| {
            let origins = self.binding_origins_for(module);
            take_live_environment(&self.live_value_environments, module, &origins)
        });
        let mut scopes = scopes;
        if let Some(environment) = defining_environment {
            scopes.insert(0, environment);
            // Runtime declarations/imports belong to the call, never to the
            // defining module environment parked below it.
            scopes.push(BTreeMap::new());
        }
        let lexical_context = function_module
            .as_deref()
            .map(|module| self.module_lexical_context(module))
            .unwrap_or_else(|| Rc::clone(&self.root_lexical_context));
        let type_scope_base = self.type_bindings.len() + 1;
        let alias_scope_base = self.module_aliases.len();
        self.module_aliases.push(BTreeMap::new());
        self.imported_functions.push(BTreeMap::new());
        self.imported_here.push(BTreeSet::new());
        let mut frame = Frame::function(
            chunk,
            slots,
            scopes,
            stack_base,
            FunctionFrameContext {
                scope_bases: FrameScopeBases {
                    types: type_scope_base,
                    aliases: alias_scope_base,
                },
                function_module,
                lexical_context,
            },
            wrap,
        );
        frame.defining_environment_module = frame.function_module.clone();
        self.frames.push(frame);
        self.type_bindings.push(BTreeMap::new());
    }

    fn apply_entry_alias_context(&mut self, entry: &FnEntry) {
        let frame = self.frames.last_mut().expect("callee frame");
        let mut context = (*frame.lexical_context).clone();
        Rc::make_mut(&mut context.module_aliases)
            .retain(|name, _| entry.declaration_alias_names.contains(name));
        frame.lexical_context = Rc::new(context);
    }

    fn park_active_defining_environment(&mut self) {
        let active_module = self.frames.last().and_then(|frame| {
            frame
                .defining_environment_module
                .clone()
                .or_else(|| (!frame.is_function).then(|| self.current_module.clone()))
        });
        let Some(module) = active_module else {
            return;
        };
        let environment = self
            .frames
            .last_mut()
            .and_then(|frame| frame.scopes.first_mut())
            .map(core::mem::take)
            .unwrap_or_default();
        let origins = self.binding_origins_for(&module);
        put_live_environment(
            &self.live_value_environments,
            &module,
            environment,
            &origins,
        );
        self.live_binding_origins
            .borrow_mut()
            .insert(module, origins);
    }

    fn store_frame_defining_environment(&mut self, frame: &mut Frame) {
        let Some(module) = frame.defining_environment_module.as_ref() else {
            return;
        };
        let origins = self.binding_origins_for(module);
        if !self.live_value_environments.borrow().contains_key(module) {
            let environment = frame
                .scopes
                .first_mut()
                .map(core::mem::take)
                .unwrap_or_default();
            put_live_environment(&self.live_value_environments, module, environment, &origins);
        }
        self.live_binding_origins
            .borrow_mut()
            .insert(module.clone(), origins);
    }

    fn restore_active_defining_environment(&mut self) {
        let active_module = self.frames.last().and_then(|frame| {
            frame
                .defining_environment_module
                .clone()
                .or_else(|| (!frame.is_function).then(|| self.current_module.clone()))
        });
        let Some(module) = active_module else {
            return;
        };
        if !self.live_value_environments.borrow().contains_key(&module) {
            return;
        }
        let origins = self.binding_origins_for(&module);
        let environment = take_live_environment(&self.live_value_environments, &module, &origins);
        if let Some(scope) = self
            .frames
            .last_mut()
            .and_then(|frame| frame.scopes.first_mut())
        {
            *scope = environment;
        }
    }

    fn module_alias(&self, name: &str) -> Option<&Rc<nybl::value::NyblModule>> {
        let floor = self
            .frames
            .last()
            .filter(|frame| frame.is_function)
            .map(|frame| frame.alias_scope_base);
        self.module_aliases
            .iter()
            .enumerate()
            .rev()
            .filter(|(index, _)| floor.is_none_or(|floor| *index >= floor))
            .find_map(|(_, frame)| frame.get(name))
            .or_else(|| {
                self.frames
                    .last()
                    .and_then(|frame| frame.lexical_context.module_aliases.get(name))
            })
    }

    fn module_binding(&self, module: &nybl::value::NyblModule, name: &str) -> Option<Value> {
        // The import-time snapshot is the module object's capability surface.
        // Live-environment lookup refreshes values within that surface; it
        // must never turn an unselected or private binding into a new member.
        if !module.bindings.iter().any(|(binding, _)| binding == name) {
            return None;
        }
        if let Some((active_module, environment)) = self.frames.last().and_then(|frame| {
            let active_module = frame
                .defining_environment_module
                .as_deref()
                .or_else(|| (!frame.is_function).then_some(self.current_module.as_str()))?;
            Some((active_module, frame.scopes.first()?))
        }) {
            if module.path == active_module
                && let Some(value) = environment.get(name)
            {
                return Some(value.clone());
            }
            let (origin_module, origin_binding) = module.__binding_origin(name);
            if origin_module == active_module
                && let Some(value) = environment.get(&origin_binding)
            {
                return Some(value.clone());
            }
            let active_origins = self.binding_origins_for(active_module);
            if let Some(active_binding) =
                active_origins
                    .iter()
                    .find_map(|(active_binding, active_origin)| {
                        (active_origin.0 == origin_module && active_origin.1 == origin_binding)
                            .then_some(active_binding)
                    })
                && let Some(value) = environment.get(active_binding)
            {
                return Some(value.clone());
            }
        }
        module.__binding(name)
    }

    fn binding_origins_for(&self, module: &str) -> BTreeMap<String, BindingOrigin> {
        if module == self.current_module {
            return self.binding_origins.clone();
        }
        self.live_binding_origins
            .borrow()
            .get(module)
            .cloned()
            .unwrap_or_default()
    }

    fn live_origin_value(&self, origin: &BindingOrigin) -> Option<Value> {
        if let Some((active_module, environment)) = self.frames.last().and_then(|frame| {
            let active_module = frame
                .defining_environment_module
                .as_deref()
                .or_else(|| (!frame.is_function).then_some(self.current_module.as_str()))?;
            Some((active_module, frame.scopes.first()?))
        }) {
            if active_module == origin.0
                && let Some(value) = environment.get(&origin.1)
            {
                return Some(value.clone());
            }
            let active_origins = self.binding_origins_for(active_module);
            if let Some(binding) = active_origins
                .iter()
                .find_map(|(binding, candidate)| (candidate == origin).then_some(binding))
                && let Some(value) = environment.get(binding)
            {
                return Some(value.clone());
            }
        }
        self.live_value_environments
            .borrow()
            .get(&origin.0)
            .and_then(|environment| environment.get(&origin.1))
            .cloned()
    }

    fn imported_function(&self, name: &str) -> Option<&Rc<FnEntry>> {
        let floor = self
            .frames
            .last()
            .filter(|frame| frame.is_function)
            .map(|frame| frame.alias_scope_base);
        self.imported_functions
            .iter()
            .enumerate()
            .rev()
            .filter(|(index, _)| floor.is_none_or(|floor| *index >= floor))
            .find_map(|(_, frame)| frame.get(name))
            .or_else(|| {
                self.frames
                    .last()
                    .and_then(|frame| frame.lexical_context.imported_functions.get(name))
            })
    }

    fn is_module_top_scope(&self) -> bool {
        self.frames
            .last()
            .is_some_and(|frame| !frame.is_function && frame.scopes.len() == frame.scope_base)
    }

    fn define_local(&mut self, name: String, value: Value) {
        if self.current_scopes().is_empty() {
            self.current_scopes_mut().push(BTreeMap::new());
        }
        if let Some(scope) = self.current_scopes_mut().last_mut() {
            scope.insert(name, value);
        }
    }

    fn sync_top_level_module_alias(
        &mut self,
        name: String,
        module: Option<Rc<nybl::value::NyblModule>>,
    ) {
        if !self.is_module_top_scope() {
            return;
        }
        let aliases = self.module_aliases.last_mut().expect("module alias scope");
        if let Some(module) = module {
            aliases.insert(name.clone(), Rc::clone(&module));
            self.publish_root_module_alias(name, Some(module));
        } else {
            aliases.remove(&name);
            self.publish_root_module_alias(name, None);
        }
    }

    /// `lookup_var`, but pull the name from the current chunk by
    /// index. Avoids an `Rc::clone` + separate borrow at the call
    /// site — the dispatch loop's hottest read path.
    fn lookup_var_by_idx(&self, idx: NameIdx) -> Option<&Value> {
        let frame = self.frames.last()?;
        let name = frame.chunk.name(idx);
        for scope in frame.scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v);
            }
        }
        if frame.is_function
            && frame.function_module.as_deref() == Some(nybl::value::ROOT_MODULE_PATH)
        {
            return self
                .frames
                .first()
                .and_then(|root| root.scopes.iter().rev().find_map(|scope| scope.get(name)));
        }
        None
    }

    fn lookup_var_mut_by_idx(&mut self, idx: NameIdx) -> Option<&mut Value> {
        let (name, uses_root) = {
            let frame = self.frames.last()?;
            (
                frame.chunk.name(idx).to_string(),
                frame.is_function
                    && frame.function_module.as_deref() == Some(nybl::value::ROOT_MODULE_PATH),
            )
        };
        let current_index = self.frames.len().checked_sub(1)?;
        if current_index == 0 {
            return self.frames[0]
                .scopes
                .iter_mut()
                .rev()
                .find_map(|scope| scope.get_mut(&name));
        }
        let (earlier, current) = self.frames.split_at_mut(current_index);
        for scope in current[0].scopes.iter_mut().rev() {
            if let Some(value) = scope.get_mut(&name) {
                return Some(value);
            }
        }
        if uses_root {
            return earlier.first_mut().and_then(|root| {
                root.scopes
                    .iter_mut()
                    .rev()
                    .find_map(|scope| scope.get_mut(&name))
            });
        }
        None
    }

    fn set_existing(&mut self, name: &str, value: Value) -> bool {
        for scope in self.current_scopes_mut().iter_mut().rev() {
            // Writing through `get_mut` keeps the existing key
            // allocation in place — we'd otherwise pay a fresh
            // `String` alloc per loop iteration for every
            // `StoreVar` in a tight loop (e.g. `i = i + 1`).
            if let Some(slot) = scope.get_mut(name) {
                *slot = value;
                return true;
            }
        }
        if self.frames.last().is_some_and(|frame| {
            frame.is_function
                && frame.function_module.as_deref() == Some(nybl::value::ROOT_MODULE_PATH)
        }) && let Some(slot) = self.frames.first_mut().and_then(|root| {
            root.scopes
                .iter_mut()
                .rev()
                .find_map(|scope| scope.get_mut(name))
        }) {
            *slot = value;
            return true;
        }
        false
    }

    /// `set_existing`, but pull the name from the current chunk
    /// by index. Splits the frame's `&mut` into a `&Rc<Chunk>`
    /// (for the name slice) and `&mut scopes` (for the walk)
    /// using field-level borrow splitting — no `Rc::clone`.
    fn set_existing_by_idx(&mut self, idx: NameIdx, value: Value) -> bool {
        let current_index = self.frames.len() - 1;
        let name = self.frames[current_index].chunk.name(idx).to_string();
        for scope in self.frames[current_index].scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(&name) {
                *slot = value;
                return true;
            }
        }
        let uses_root = self.frames[current_index].is_function
            && self.frames[current_index].function_module.as_deref()
                == Some(nybl::value::ROOT_MODULE_PATH);
        if self.frames[current_index].is_function
            && self.frames[current_index]
                .lexical_context
                .module_aliases
                .contains_key(&name)
        {
            let frame = &mut self.frames[current_index];
            let overlay_scope = if frame.has_declaration_overlay {
                frame.scope_base - 1
            } else {
                let scope = frame.scope_base;
                frame.scopes.insert(scope, BTreeMap::new());
                frame.scope_base += 1;
                frame.has_declaration_overlay = true;
                scope
            };
            frame
                .scopes
                .get_mut(overlay_scope)
                .expect("alias overlay scope")
                .insert(name, value);
            return true;
        }
        if uses_root
            && let Some(slot) = self.frames[0]
                .scopes
                .iter_mut()
                .rev()
                .find_map(|scope| scope.get_mut(&name))
        {
            *slot = value;
            return true;
        }
        false
    }

    // ─── "Did you mean?" candidate collectors ─────────────────
    //
    // Same pattern as the walker: gather every name the user
    // could plausibly have meant so `nybl::suggest::did_you_mean`
    // picks the closest one. VM-specific quirk: the scope stack
    // is per-frame, so we only scan the current frame's scopes
    // rather than the whole interpreter's.

    /// Names reachable when an identifier is used as a value —
    /// locals in the enclosing scopes plus user fn declarations.
    fn value_candidates_hint(&self, target: &str) -> Option<String> {
        let mut candidates: Vec<String> = Vec::new();
        for scope in self.current_scopes() {
            for k in scope.keys() {
                candidates.push(k.clone());
            }
        }
        for name in self.functions.keys() {
            candidates.push(name.clone());
        }
        nybl::suggest::did_you_mean(target, candidates)
    }

    /// Names reachable in a call position: user fns plus core
    /// builtins. Host builtins stay with the host's own
    /// `function_hint()` (the call site falls back to that path
    /// when no user-level suggestion fits).
    fn callable_candidates_hint(&self, target: &str) -> Option<String> {
        let mut candidates: Vec<String> = self.functions.keys().cloned().collect();
        for b in nybl::suggest::CORE_CALLABLE_BUILTINS {
            candidates.push((*b).to_string());
        }
        nybl::suggest::did_you_mean(target, candidates)
    }

    fn active_module_path(&self) -> &str {
        self.frames
            .last()
            .and_then(|frame| frame.function_module.as_deref())
            .unwrap_or(&self.current_module)
    }

    fn module_lexical_context(&self, module_path: &str) -> Rc<ModuleLexicalContext> {
        if module_path == self.current_module {
            return Rc::clone(&self.root_lexical_context);
        }
        let cache = self.imports.borrow();
        match cache.get(module_path) {
            Some(ImportSlot::Loaded(artifacts)) => Rc::clone(&artifacts.lexical_context),
            _ => Rc::new(ModuleLexicalContext::default()),
        }
    }

    fn publish_root_type_binding(&mut self, name: String, origin: String) {
        let context = Rc::make_mut(&mut self.root_lexical_context);
        Rc::make_mut(&mut context.type_bindings).insert(name, origin);
    }

    fn publish_root_module_alias(
        &mut self,
        name: String,
        module: Option<Rc<nybl::value::NyblModule>>,
    ) {
        let context = Rc::make_mut(&mut self.root_lexical_context);
        let aliases = Rc::make_mut(&mut context.module_aliases);
        if let Some(module) = module {
            aliases.insert(name, module);
        } else {
            aliases.remove(&name);
        }
    }

    fn publish_root_imported_function(&mut self, name: String, function: Rc<FnEntry>) {
        let context = Rc::make_mut(&mut self.root_lexical_context);
        Rc::make_mut(&mut context.imported_functions).insert(name, function);
    }

    /// Resolve module-owned sibling functions from cached module artifacts
    /// before falling back to caller-visible functions. This preserves module
    /// function environments without making alias imports publish bare names.
    #[inline]
    fn lookup_function_entry(&self, name: &str) -> Option<Rc<FnEntry>> {
        let frame = self.frames.last()?;
        let defining_module = frame.function_module.as_deref();
        if let Some(module_path) = defining_module
            && module_path != nybl::value::ROOT_MODULE_PATH
        {
            let cache = self.imports.borrow();
            if let Some(ImportSlot::Loaded(artifacts)) = cache.get(module_path)
                && let Some((_, entry)) = artifacts
                    .fn_decls
                    .iter()
                    .find(|(candidate, _)| candidate == name)
            {
                return Some(entry.clone());
            }
        }

        // Alias-free named calls dominate real programs. Avoid probing every
        // empty runtime import scope and then the empty defining-module map
        // before reaching `self.functions`. The moment any active import map
        // contains a callable, keep the full lookup below so a block/function
        // import can still shadow a same-named declared function.
        let import_floor = if frame.is_function {
            frame.alias_scope_base
        } else {
            0
        };
        let runtime_imports_empty = self.imported_functions[import_floor..]
            .iter()
            .all(BTreeMap::is_empty);
        if runtime_imports_empty && frame.lexical_context.imported_functions.is_empty() {
            if let Some(module_path) = defining_module {
                return self
                    .functions
                    .get(name)
                    .filter(|entry| entry.module_path == module_path)
                    .cloned();
            }
            return self.functions.get(name).cloned();
        }

        if let Some(entry) = self.imported_function(name) {
            return Some(entry.clone());
        }
        if let Some(module_path) = defining_module
            && let Some(entry) = self
                .functions
                .get(name)
                .filter(|entry| entry.module_path == module_path)
        {
            return Some(entry.clone());
        }
        if defining_module.is_none_or(|path| path == self.current_module) {
            self.functions.get(name).cloned()
        } else {
            None
        }
    }

    // ─── Dispatch ────────────────────────────────────────────────

    #[inline]
    fn dispatch(&mut self, instr: Instr, line: u32) -> Result<Next, NyblError> {
        match instr {
            // ─── Literals ─────────────────────────────────────────
            Instr::LoadConst(idx) => {
                let value = match self.current_chunk().constant(idx) {
                    Constant::Int(n) => Value::Int(*n),
                    Constant::Number(n) => Value::Number(*n),
                    Constant::Str(s) => Value::__new_str_in(s.clone(), &self.memory),
                };
                self.push_value(value);
            }
            Instr::LoadNone => self.push_value(Value::None),
            Instr::LoadTrue => self.push_value(Value::Bool(true)),
            Instr::LoadFalse => self.push_value(Value::Bool(false)),

            // ─── Variables ────────────────────────────────────────
            Instr::LoadVar(n) => {
                // Fast path: look the name up by index so both
                // the chunk borrow and the scope walk happen in
                // one helper — no `Rc::clone` and no intermediate
                // owned `String`. A tight loop with 3 `LoadVar`s
                // per iteration previously paid one `Rc::clone`
                // per access here.
                if let Some(v) = self.lookup_var_by_idx(n).cloned() {
                    self.push_value(v);
                } else {
                    // Slow path: fall back to the named-fn
                    // registry so `fn fib(...) {...}; let g = fib`
                    // yields a real `Value::Fn`. Only then do we
                    // need the name as a `&str` / `String`.
                    let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
                    let name = chunk.name(n);
                    if let Some(module) = self.module_alias(name).cloned() {
                        self.push_value(Value::Module(module));
                        return Ok(Next::Continue);
                    }
                    let fn_entry = self.lookup_function_entry(name);
                    if let Some(entry) = fn_entry {
                        let params = entry.params.clone();
                        let chunk_rc = entry.chunk.clone();
                        let body: Rc<dyn core::any::Any + 'static> = chunk_rc;
                        let v = NyblFn::try_new_compiled_in_module_with_origin_and_modes(
                            params,
                            entry.param_modes.clone(),
                            Vec::new(),
                            body,
                            Some(name.to_string()),
                            Some(entry.module_path.clone()),
                            self.function_origin.clone(),
                            0,
                            line,
                        )
                        .map(Value::Fn)?;
                        self.push_value(v);
                    } else {
                        // "did you mean?" first, else the generic
                        // "use `let`" nudge. Mirrors the walker's
                        // behaviour for consistency across engines.
                        let hint = self.value_candidates_hint(name).unwrap_or_else(|| {
                            "Did you forget to create it with `let`?".to_string()
                        });
                        return Err(error_with_hint(
                            line,
                            nybl::error_messages::variable_not_found(name),
                            hint,
                        ));
                    }
                }
            }
            Instr::DefineLocal(n) => {
                // `define_local` takes an owned `String` because
                // it inserts into the scope map — can't skip the
                // allocation here.
                let name = self.current_chunk().name(n).to_string();
                let v = self.pop_value(line)?;
                let module = match &v {
                    Value::Module(module) => Some(Rc::clone(module)),
                    _ => None,
                };
                if self.is_module_top_scope()
                    && let Some(origin) = self.binding_origins.get(&name).cloned()
                    && (origin.0 != self.current_module || origin.1 != name)
                    && let Some(previous) = self
                        .current_scopes_mut()
                        .last_mut()
                        .and_then(|scope| scope.remove(&name))
                {
                    self.live_value_environments
                        .borrow_mut()
                        .entry(origin.0)
                        .or_default()
                        .insert(origin.1, previous);
                }
                self.define_local(name.clone(), v);
                if self.is_module_top_scope() {
                    self.binding_origins
                        .insert(name.clone(), (self.current_module.clone(), name.clone()));
                }
                self.sync_top_level_module_alias(name, module);
            }
            Instr::StoreVar(n) => {
                // Fast path: look the target up by index so we
                // neither `Rc::clone` nor allocate the name.
                let v = self.pop_value(line)?;
                let module = match &v {
                    Value::Module(module) => Some(Rc::clone(module)),
                    _ => None,
                };
                let top_level_name = self
                    .is_module_top_scope()
                    .then(|| self.current_chunk().name(n).to_string());
                let runtime_alias_without_value_binding = {
                    let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
                    let name = chunk.name(n);
                    self.lookup_var_by_idx(n).is_none() && self.module_alias(name).is_some()
                };
                if runtime_alias_without_value_binding {
                    let name = self.current_chunk().name(n).to_string();
                    self.define_local(name, v);
                    return Ok(Next::Continue);
                }
                if !self.set_existing_by_idx(n, v) {
                    // Cold path: synthesise the error with the
                    // name — allocation is fine here.
                    let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
                    let name = chunk.name(n);
                    return Err(error_with_hint(
                        line,
                        format!("Variable `{name}` doesn't exist yet"),
                        format!("Use `let` to create a new variable: let {name} = ..."),
                    ));
                }
                if let Some(name) = top_level_name {
                    self.sync_top_level_module_alias(name, module);
                }
            }
            Instr::CompoundAssign { target, op } => {
                let rhs = self.pop_value(line)?;
                let runtime_alias_without_value_binding = match target {
                    crate::chunk::AssignBack::Name(name_idx) => {
                        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
                        self.lookup_var_by_idx(name_idx).is_none()
                            && self.module_alias(chunk.name(name_idx)).is_some()
                    }
                    crate::chunk::AssignBack::Slot(_) => false,
                };
                let current = match target {
                    crate::chunk::AssignBack::Slot(slot) => self
                        .frames
                        .last()
                        .and_then(|frame| frame.slots.get(slot.0 as usize))
                        .cloned()
                        .ok_or_else(|| error(line, "VM: local slot out of range"))?,
                    crate::chunk::AssignBack::Name(name_idx) => {
                        if let Some(value) = self.lookup_var_by_idx(name_idx).cloned() {
                            value
                        } else {
                            let chunk =
                                Rc::clone(&self.frames.last().expect("frame present").chunk);
                            let name = chunk.name(name_idx);
                            if let Some(module) = self.module_alias(name).cloned() {
                                Value::Module(module)
                            } else {
                                return Err(error(
                                    line,
                                    nybl::error_messages::variable_not_found(name),
                                ));
                            }
                        }
                    }
                };
                let value = apply_in_place_assign(op, rhs, line, &self.memory, || Ok(current))?;
                match target {
                    crate::chunk::AssignBack::Slot(slot) => {
                        let target = self
                            .frames
                            .last_mut()
                            .expect("frame present")
                            .slots
                            .get_mut(slot.0 as usize)
                            .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                        *target = value;
                    }
                    crate::chunk::AssignBack::Name(name) => {
                        if runtime_alias_without_value_binding {
                            let name = self.current_chunk().name(name).to_string();
                            self.define_local(name, value);
                            return Ok(Next::Continue);
                        }
                        if !self.set_existing_by_idx(name, value) {
                            let chunk =
                                Rc::clone(&self.frames.last().expect("frame present").chunk);
                            return Err(error(
                                line,
                                nybl::error_messages::variable_not_found(chunk.name(name)),
                            ));
                        }
                    }
                }
            }

            // ─── Slot-based locals (fast path) ───────────────────
            Instr::LoadLocal(slot) => {
                // Direct vec index — no hashing, no string compare.
                // Slots are pre-sized at call time from the
                // enclosing `FnDef::slot_count`, so an out-of-range
                // read can only happen on miscompiled bytecode.
                let frame = self.frames.last().expect("frame present");
                let v = frame
                    .slots
                    .get(slot.0 as usize)
                    .cloned()
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                self.push_value(v);
            }
            Instr::StoreLocal(slot) => {
                let v = self.pop_value(line)?;
                let frame = self.frames.last_mut().expect("frame present");
                let i = slot.0 as usize;
                if i < frame.slots.len() {
                    frame.slots[i] = v;
                } else {
                    return Err(error(line, "VM: local slot out of range"));
                }
            }

            // ─── Superinstructions ───────────────────────────────
            Instr::AddLocals(a, b) => {
                // Typed fast path: Int + Int → Int, no Value
                // variant match and no stack push/pop for the
                // operands. `fib`'s recursive body and every
                // `total = total + i` loop go through here.
                let frame = self.frames.last().expect("frame present");
                let av = frame
                    .slots
                    .get(a.0 as usize)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                let bv = frame
                    .slots
                    .get(b.0 as usize)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                let result = match (av, bv) {
                    (Value::Int(x), Value::Int(y)) => x
                        .checked_add(*y)
                        .map(Value::Int)
                        .map_or_else(|| ops::add_in(av, bv, line, &self.memory), Ok)?,
                    // Cold path: delegate to the generic Value
                    // adder. Covers Number, String concat, array
                    // concat, etc.
                    _ => ops::add_in(av, bv, line, &self.memory)?,
                };
                self.push_value(result);
            }
            Instr::LtLocals(a, b) => {
                let frame = self.frames.last().expect("frame present");
                let av = frame
                    .slots
                    .get(a.0 as usize)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                let bv = frame
                    .slots
                    .get(b.0 as usize)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                let result = match (av, bv) {
                    (Value::Int(x), Value::Int(y)) => Value::Bool(x < y),
                    _ => ops::lt_in(av, bv, line, &self.memory)?,
                };
                self.push_value(result);
            }
            Instr::IncLocalInt(slot, k) => {
                // `slot += k` for small-int `k`, the `i = i + 1`
                // idiom. Fast path: Int → Int with overflow
                // check. On non-Int, build an Int value and
                // dispatch through generic add so `x = x + 1`
                // still works when `x` is a Number.
                let memory = &self.memory;
                let frame = self.frames.last_mut().expect("frame present");
                let i = slot.0 as usize;
                let current = frame
                    .slots
                    .get(i)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                let new = match current {
                    Value::Int(x) => x.checked_add(k as i64).map(Value::Int).map_or_else(
                        || ops::add_in(current, &Value::Int(k as i64), line, memory),
                        Ok,
                    )?,
                    _ => ops::add_in(current, &Value::Int(k as i64), line, memory)?,
                };
                frame.slots[i] = new;
            }
            Instr::LoadLocalAddInt(slot, k) => {
                // Push `slots[slot] + k`. Covers `array[i + 1]` after the
                // compiler folds a small-int literal into the op.
                let frame = self.frames.last().expect("frame present");
                let v = frame
                    .slots
                    .get(slot.0 as usize)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                let result = match v {
                    Value::Int(x) => x.checked_add(k as i64).map(Value::Int).map_or_else(
                        || ops::add_in(v, &Value::Int(k as i64), line, &self.memory),
                        Ok,
                    )?,
                    _ => ops::add_in(v, &Value::Int(k as i64), line, &self.memory)?,
                };
                self.push_value(result);
            }
            Instr::LtLocalInt(slot, k) => {
                // Push `slots[slot] < k` — the `n < 2` base-case
                // test in `fib` and every bounded `while i < K`
                // loop.
                let frame = self.frames.last().expect("frame present");
                let v = frame
                    .slots
                    .get(slot.0 as usize)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                let result = match v {
                    Value::Int(x) => Value::Bool(*x < k as i64),
                    _ => ops::lt_in(v, &Value::Int(k as i64), line, &self.memory)?,
                };
                self.push_value(result);
            }

            // ─── Scope ────────────────────────────────────────────
            Instr::PushScope => self.push_scope(),
            Instr::PopScope => self.pop_scope(),

            // ─── Stack ────────────────────────────────────────────
            Instr::Pop => {
                if self.stack.pop().is_none() {
                    return Err(error(line, "VM: stack underflow"));
                }
            }
            Instr::Dup => {
                let v = self.peek_value(line)?.clone();
                self.push_value(v);
            }
            Instr::Dup2 => {
                let len = self.stack.len();
                if len < 2 {
                    return Err(error(line, "VM: stack underflow"));
                }
                let b = match &self.stack[len - 1] {
                    Slot::Value(v) => v.clone(),
                    _ => return Err(error(line, "VM: expected value on stack")),
                };
                let a = match &self.stack[len - 2] {
                    Slot::Value(v) => v.clone(),
                    _ => return Err(error(line, "VM: expected value on stack")),
                };
                self.push_value(a);
                self.push_value(b);
            }

            // ─── Binary ops ───────────────────────────────────────
            Instr::Add => self.binary_tracked(line, ops::add_in)?,
            Instr::Sub => self.binary(line, ops::sub)?,
            Instr::Mul => self.binary_tracked(line, ops::mul_in)?,
            Instr::Div => self.binary(line, ops::div)?,
            Instr::Rem => self.binary(line, ops::rem)?,
            Instr::Eq => self.binary_infallible(line, |a, b, _| Ok(ops::eq(a, b)))?,
            Instr::NotEq => self.binary_infallible(line, |a, b, _| Ok(ops::not_eq(a, b)))?,
            Instr::Lt => self.binary(line, ops::lt)?,
            Instr::Gt => self.binary(line, ops::gt)?,
            Instr::LtEq => self.binary(line, ops::lt_eq)?,
            Instr::GtEq => self.binary(line, ops::gt_eq)?,

            // ─── Unary ops ────────────────────────────────────────
            Instr::Neg => {
                let v = self.pop_value(line)?;
                self.push_value(ops::neg(&v, line)?);
            }
            Instr::Not => {
                let v = self.pop_value(line)?;
                self.push_value(ops::not(&v));
            }

            Instr::TruthyToBool => {
                let v = self.pop_value(line)?;
                self.push_value(Value::Bool(v.is_truthy()));
            }

            // ─── Indexing ─────────────────────────────────────────
            Instr::GetIndex => {
                let idx = self.pop_value(line)?;
                let obj = self.pop_value(line)?;
                self.push_value(ops::index_get_in(&obj, &idx, line, &self.memory)?);
            }
            Instr::SetIndex => {
                let val = self.pop_value(line)?;
                let idx = self.pop_value(line)?;
                let mut obj = self.pop_value(line)?;
                ops::index_set_in(&mut obj, &idx, val, line, &self.memory)?;
                self.push_value(obj);
            }
            Instr::SetIndexInPlace { target, op } => {
                let idx = self.pop_value(line)?;
                let rhs = self.pop_value(line)?;
                let memory = self.memory.clone();
                match target {
                    crate::chunk::AssignBack::Slot(slot) => {
                        let value = self
                            .frames
                            .last_mut()
                            .expect("frame present")
                            .slots
                            .get_mut(slot.0 as usize)
                            .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                        let val = apply_in_place_assign(op, rhs, line, &memory, || {
                            ops::index_get_in(value, &idx, line, &memory)
                        })?;
                        ops::index_set_in(value, &idx, val, line, &memory)?;
                    }
                    crate::chunk::AssignBack::Name(name_idx) => {
                        let name = self.current_chunk().name(name_idx).to_string();
                        if self.lookup_var_mut_by_idx(name_idx).is_none() {
                            if let Some(module) = self.module_alias(&name).cloned() {
                                let mut value = Value::Module(module);
                                let val = apply_in_place_assign(op, rhs, line, &memory, || {
                                    ops::index_get_in(&value, &idx, line, &memory)
                                })?;
                                ops::index_set_in(&mut value, &idx, val, line, &memory)?;
                                return Ok(Next::Continue);
                            }
                            let hint = self.value_candidates_hint(&name).unwrap_or_else(|| {
                                "Did you forget to create it with `let`?".to_string()
                            });
                            return Err(error_with_hint(
                                line,
                                nybl::error_messages::variable_not_found(&name),
                                hint,
                            ));
                        }
                        let value = self
                            .lookup_var_mut_by_idx(name_idx)
                            .expect("binding checked above");
                        let val = apply_in_place_assign(op, rhs, line, &memory, || {
                            ops::index_get_in(value, &idx, line, &memory)
                        })?;
                        ops::index_set_in(value, &idx, val, line, &memory)?;
                    }
                }
            }
            Instr::AssignPlace { place, op } => {
                let index_count = self
                    .current_chunk()
                    .place(place)
                    .projections
                    .iter()
                    .filter(|projection| matches!(projection, PlaceProjectionRecipe::Index))
                    .count();
                let indices = self.pop_n_values(index_count, line)?;
                let rhs = self.pop_value(line)?;
                let resolved =
                    self.resolve_place_internal(place, indices, 0, line, false, false)?;
                if nybl::naming::is_constant_name(&resolved.root_name) {
                    return Err(nybl::error_messages::constant_mutation_error(
                        &resolved.root_name,
                        line,
                    ));
                }
                let value = apply_in_place_assign(op, rhs, line, &self.memory, || {
                    self.place_value_from(&resolved.root, &resolved.projections, line)
                })?;
                let mut root = resolved.root.clone();
                self.write_place_value(&mut root, &resolved.projections, value, line)?;
                self.store_resolved_target(&resolved.target, root, line)?;
            }

            // ─── String interpolation ────────────────────────────
            Instr::StringInterp(idx) => {
                let recipe_parts = Rc::clone(&self.current_chunk().interp(idx).parts);
                self.push_value(self.build_interp(&recipe_parts, line)?);
            }

            // ─── Collections ──────────────────────────────────────
            Instr::MakeArray(n) => {
                let items = self.pop_n_values(n as usize, line)?;
                self.push_value(Value::__try_new_array_in(items, line, &self.memory)?);
            }
            Instr::MakeDict(n) => {
                let flat = self.pop_n_values((n as usize) * 2, line)?;
                let mut entries: Vec<(String, Value)> = Vec::with_capacity(n as usize);
                let mut iter = flat.into_iter();
                while let (Some(key), Some(val)) = (iter.next(), iter.next()) {
                    let key_str = match &key {
                        Value::Str(s) => s.as_str().to_string(),
                        other => {
                            return Err(error(
                                line,
                                format!("Dict keys must be strings, got {}", other.type_name()),
                            ));
                        }
                    };
                    drop(key);
                    entries.push((key_str, val));
                }
                self.push_value(Value::__try_new_dict_in(entries, line, &self.memory)?);
            }

            // ─── Calls ────────────────────────────────────────────
            Instr::Call { name, argc } => {
                return self.call(name, argc as usize, line);
            }
            Instr::CallValue { argc } => {
                return self.call_value(argc as usize, line);
            }
            Instr::PrepareCall { name, site } => {
                self.prepare_named_call(name, site, line)?;
            }
            Instr::PrepareCallValue { site } => {
                let callee = self.pop_value(line)?;
                self.prepare_value_call(callee, site, line)?;
            }
            Instr::PrepareMethodValue {
                method,
                site,
                nested_place,
            } => {
                let receiver = self.pop_value(line)?;
                self.prepare_method_call(receiver, None, None, method, site, nested_place, line)?;
            }
            Instr::PrepareMethodNamed {
                target,
                method,
                site,
            } => {
                self.prepare_named_method_call(target, method, site, line)?;
            }
            Instr::PrepareMethodPlace {
                place,
                method,
                site,
            } => {
                let index_count = self
                    .current_chunk()
                    .place(place)
                    .projections
                    .iter()
                    .filter(|projection| matches!(projection, PlaceProjectionRecipe::Index))
                    .count();
                let indices = self.pop_n_values(index_count, line)?;
                let resolved = self.resolve_place(place, indices, 0, line)?;
                let receiver =
                    self.place_value_from(&resolved.root, &resolved.projections, line)?;
                self.prepare_method_call(receiver, None, Some(resolved), method, site, true, line)?;
            }
            Instr::CallPrepared { site } => {
                return self.call_prepared(site, line);
            }
            Instr::CallMethod {
                method,
                argc,
                assign_back_to,
                nested_place,
            } => {
                return self.call_method(method, argc as usize, assign_back_to, nested_place, line);
            }
            Instr::CallMethodInPlace {
                target,
                method,
                argc,
            } => {
                return self.call_method_in_place(target, method, argc as usize, line);
            }

            // ─── Functions ────────────────────────────────────────
            Instr::DefineFn(idx) => {
                self.define_fn(idx);
            }
            Instr::MakeLambda(idx) => {
                self.make_lambda(idx, line)?;
            }
            Instr::Return => {
                let v = self.pop_value(line)?;
                return self.do_return(v, line);
            }
            Instr::ReturnNone => {
                return self.do_return(Value::None, line);
            }

            // ─── Iteration / repeat ──────────────────────────────
            Instr::MakeIter => {
                let mut v = self.pop_value(line)?;
                // Fast path: Array / Str / Dict materialise eagerly
                // into a `Slot::Iter(Vec<_>)`. Every other iterable
                // goes through the protocol — either wraps a
                // `Value::Iter` directly, or dispatches the user's
                // `.iter()` method and lands the result via
                // `FrameWrap::IterStart`.
                match &mut v {
                    Value::Array(arr) => {
                        let mut items = arr.__take_in(&self.memory);
                        drop(v);
                        items.reverse();
                        self.stack.push(Slot::Iter(items));
                    }
                    Value::Str(s) => {
                        let mut items: Vec<Value> = s
                            .chars()
                            .map(|c| Value::__new_str_in(c.to_string(), &self.memory))
                            .collect();
                        drop(v);
                        items.reverse();
                        self.stack.push(Slot::Iter(items));
                    }
                    Value::Dict(d) => {
                        let mut items: Vec<Value> = d
                            .iter()
                            .map(|(k, _)| Value::__new_str_in(k.clone(), &self.memory))
                            .collect();
                        drop(v);
                        items.reverse();
                        self.stack.push(Slot::Iter(items));
                    }
                    Value::Iter(_) => {
                        // Already an iterator — use as-is.
                        self.stack.push(Slot::IterObject(v));
                    }
                    Value::Struct(_) | Value::EnumVariant(_) => {
                        // Dispatch user `.iter()`. The result
                        // lands on the stack as a Slot::IterObject
                        // via `FrameWrap::IterStart`.
                        let iterable = v;
                        self.dispatch_iter_method(
                            iterable,
                            "iter",
                            Vec::new(),
                            FrameWrap::IterStart,
                            line,
                        )?;
                    }
                    _ => {
                        return Err(error(
                            line,
                            nybl::error_messages::cant_iterate_over(v.type_name()),
                        ));
                    }
                }
            }
            Instr::IterNext { target } => {
                match self.stack.last_mut() {
                    Some(Slot::Iter(items)) => {
                        let next = items.pop();
                        match next {
                            Some(item) => self.push_value(item),
                            None => {
                                self.stack.pop();
                                self.jump(target);
                            }
                        }
                    }
                    Some(Slot::IterObject(iter_val)) => {
                        // Synchronous fast-path for the built-in
                        // iterator: `iter_method::next` runs in
                        // Rust, no bytecode frame push required.
                        if matches!(iter_val, Value::Iter(_)) {
                            let iter_clone = iter_val.clone();
                            let (result, _) = methods::iter_method_in(
                                &iter_clone,
                                "next",
                                &[],
                                line,
                                &self.memory,
                            )?;
                            match unwrap_iter_step(&result) {
                                IterStep::Next(v) => self.push_value(v),
                                IterStep::Done => {
                                    self.stack.pop();
                                    self.jump(target);
                                }
                                IterStep::Malformed => {
                                    return Err(error(
                                        line,
                                        format!(
                                            "`.next()` on a `for` iterator must return `Iter::Next(v)` or `Iter::Done`, got {}",
                                            result.inspect()
                                        ),
                                    ));
                                }
                            }
                        } else {
                            // User-typed iterator — dispatch
                            // `.next()` through the normal method
                            // path. The return value is handled
                            // in `do_return` under `IterAdvance`.
                            let iter_clone = iter_val.clone();
                            self.dispatch_iter_method(
                                iter_clone,
                                "next",
                                Vec::new(),
                                FrameWrap::IterAdvance(target),
                                line,
                            )?;
                        }
                    }
                    _ => return Err(error(line, "VM: expected iterator on stack")),
                }
            }
            Instr::MakeRepeatCount => {
                let v = self.pop_value(line)?;
                let n = match v {
                    Value::Int(n) => n,
                    Value::Number(n) => n as i64,
                    other => {
                        return Err(error(
                            line,
                            format!("repeat needs a number, but got {}", other.type_name()),
                        ));
                    }
                };
                self.stack.push(Slot::Repeat(n.max(0)));
            }
            Instr::RepeatNext { target } => {
                let done = match self.stack.last_mut() {
                    Some(Slot::Repeat(n)) => {
                        if *n > 0 {
                            *n -= 1;
                            false
                        } else {
                            true
                        }
                    }
                    _ => return Err(error(line, "VM: expected repeat counter on stack")),
                };
                if done {
                    self.stack.pop();
                    self.jump(target);
                }
            }
            Instr::PopLoopState(kind) => {
                let matches_kind = matches!(
                    (kind, self.stack.last()),
                    (
                        LoopStateKind::Iterator,
                        Some(Slot::Iter(_) | Slot::IterObject(_)),
                    ) | (LoopStateKind::Repeat, Some(Slot::Repeat(_)))
                );
                if !matches_kind {
                    let expected = match kind {
                        LoopStateKind::Iterator => "iterator",
                        LoopStateKind::Repeat => "repeat counter",
                    };
                    return Err(error(
                        line,
                        format!("VM: expected {expected} loop state on stack"),
                    ));
                }
                self.stack.pop();
            }

            // ─── Control flow ─────────────────────────────────────
            Instr::Jump(t) => self.jump(t),
            Instr::JumpIfFalse(t) => {
                let v = self.pop_value(line)?;
                if !v.is_truthy() {
                    self.jump(t);
                }
            }
            Instr::JumpIfFalsePeek(t) => {
                let truthy = self.peek_value(line)?.is_truthy();
                if !truthy {
                    self.jump(t);
                }
            }
            Instr::JumpIfTruePeek(t) => {
                let truthy = self.peek_value(line)?.is_truthy();
                if truthy {
                    self.jump(t);
                }
            }

            // ─── Modules ─────────────────────────────────────────
            Instr::Use(idx) => {
                let spec = self.current_chunk().use_spec(idx).clone();
                self.exec_use(&spec, line)?;
            }

            // ─── User-defined types ──────────────────────────────
            Instr::DefineStruct(idx) => self.define_struct(idx, line)?,
            Instr::DefineEnum(idx) => self.define_enum(idx, line)?,
            Instr::DefineMethod {
                type_name,
                method_name,
                fn_idx,
            } => self.define_method(type_name, method_name, fn_idx),
            Instr::ValidateStructConstruct {
                namespace,
                type_name,
                fields,
            } => {
                let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
                self.validate_struct_construct(
                    namespace.map(|namespace| chunk.namespace_ref(namespace)),
                    chunk.name(type_name),
                    chunk.construct_fields(fields),
                    line,
                )?;
            }
            Instr::ValidateEnumConstruct {
                namespace,
                type_name,
                variant,
                shape,
                fields,
            } => {
                let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
                self.validate_enum_construct(
                    namespace.map(|namespace| chunk.namespace_ref(namespace)),
                    chunk.name(type_name),
                    chunk.name(variant),
                    shape,
                    chunk.construct_fields(fields),
                    line,
                )?;
            }
            Instr::ConstructStruct {
                namespace,
                type_name,
                count,
            } => {
                let namespace =
                    namespace.map(|namespace| self.current_chunk().namespace_ref(namespace));
                self.construct_struct(namespace, type_name, count as usize, line)?;
            }
            Instr::ConstructEnum {
                namespace,
                type_name,
                variant,
                shape,
            } => {
                let namespace =
                    namespace.map(|namespace| self.current_chunk().namespace_ref(namespace));
                self.construct_enum(namespace, type_name, variant, shape, line)?;
            }
            Instr::FieldGet(n) => {
                let field = self.current_chunk().name(n).to_string();
                let obj = self.pop_value(line)?;
                self.push_value(self.field_get(&obj, &field, line)?);
            }
            Instr::FieldSet(n) => {
                let field = self.current_chunk().name(n).to_string();
                let val = self.pop_value(line)?;
                let obj = self.pop_value(line)?;
                self.push_value(self.field_set(obj, &field, val, line)?);
            }
            Instr::FieldSetInPlace { target, field, op } => {
                let field = self.current_chunk().name(field).to_string();
                let rhs = self.pop_value(line)?;
                let memory = self.memory.clone();

                let value = match target {
                    crate::chunk::AssignBack::Slot(slot) => self
                        .frames
                        .last_mut()
                        .expect("frame present")
                        .slots
                        .get_mut(slot.0 as usize)
                        .ok_or_else(|| error(line, "VM: local slot out of range"))?,
                    crate::chunk::AssignBack::Name(name_idx) => {
                        let name = self.current_chunk().name(name_idx).to_string();
                        if self.lookup_var_mut_by_idx(name_idx).is_none() {
                            if let Some(module) = self.module_alias(&name).cloned() {
                                let value = Value::Module(module);
                                let val = apply_in_place_assign(op, rhs, line, &memory, || {
                                    self.field_get(&value, &field, line)
                                })?;
                                self.field_set(value, &field, val, line)?;
                                return Ok(Next::Continue);
                            }
                            let hint = self.value_candidates_hint(&name).unwrap_or_else(|| {
                                "Did you forget to create it with `let`?".to_string()
                            });
                            return Err(error_with_hint(
                                line,
                                nybl::error_messages::variable_not_found(&name),
                                hint,
                            ));
                        }
                        self.lookup_var_mut_by_idx(name_idx)
                            .expect("binding checked above")
                    }
                };
                match value {
                    Value::Struct(structure) => {
                        let type_name = structure.type_name().to_string();
                        let val = apply_in_place_assign(op, rhs, line, &memory, || {
                            structure.field(&field).cloned().ok_or_else(|| {
                                error(
                                    line,
                                    nybl::error_messages::struct_has_no_field(&type_name, &field),
                                )
                            })
                        })?;
                        if !structure.__try_set_field_in(&field, val, line, &memory)? {
                            return Err(error(
                                line,
                                nybl::error_messages::struct_has_no_field(&type_name, &field),
                            ));
                        }
                    }
                    other => {
                        return Err(error(
                            line,
                            nybl::error_messages::cant_assign_field(&field, other.type_name()),
                        ));
                    }
                }
            }

            // ─── Pattern matching ───────────────────────────────
            Instr::MatchFail { pattern, on_fail } => {
                self.match_fail(pattern, on_fail, line)?;
            }
            Instr::MatchExhausted => {
                return Err(error(line, "No match arm matched the scrutinee"));
            }

            // ─── try ─────────────────────────────────────────────
            Instr::TryUnwrap => {
                return self.try_unwrap(line);
            }

            // ─── Termination ──────────────────────────────────────
            Instr::Halt => return Ok(Next::Halt),
        }

        Ok(Next::Continue)
    }

    // ─── Binary helpers ──────────────────────────────────────────

    fn binary(
        &mut self,
        line: u32,
        op: fn(&Value, &Value, u32) -> Result<Value, NyblError>,
    ) -> Result<(), NyblError> {
        let b = self.pop_value(line)?;
        let a = self.pop_value(line)?;
        self.push_value(op(&a, &b, line)?);
        Ok(())
    }

    fn binary_tracked(
        &mut self,
        line: u32,
        op: fn(&Value, &Value, u32, &nybl::memory::MemoryContext) -> Result<Value, NyblError>,
    ) -> Result<(), NyblError> {
        let b = self.pop_value(line)?;
        let a = self.pop_value(line)?;
        self.push_value(op(&a, &b, line, &self.memory)?);
        Ok(())
    }

    fn binary_infallible(
        &mut self,
        line: u32,
        op: fn(&Value, &Value, u32) -> Result<Value, NyblError>,
    ) -> Result<(), NyblError> {
        let b = self.pop_value(line)?;
        let a = self.pop_value(line)?;
        self.push_value(op(&a, &b, line)?);
        Ok(())
    }

    // ─── String interpolation ────────────────────────────────────

    fn build_interp(&self, parts: &[InterpPart], line: u32) -> Result<Value, NyblError> {
        let mut result = String::new();
        for part in parts {
            match part {
                InterpPart::Literal(s) => result.push_str(s),
                InterpPart::Local(slot) => {
                    let v = self
                        .frames
                        .last()
                        .and_then(|frame| frame.slots.get(slot.0 as usize))
                        .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                    result.push_str(&format!("{v}"));
                }
                InterpPart::Name(name_idx) => {
                    let name = self.current_chunk().name(*name_idx);
                    if let Some(value) = self.lookup_var_by_idx(*name_idx) {
                        result.push_str(&format!("{value}"));
                    } else if let Some(module) = self.module_alias(name) {
                        result.push_str(&format!("{}", Value::Module(Rc::clone(module))));
                    } else {
                        return Err(error(line, nybl::error_messages::variable_not_found(name)));
                    }
                }
            }
        }
        Ok(Value::__new_str_in(result, &self.memory))
    }

    // ─── Chunk accessor ──────────────────────────────────────────

    fn current_chunk(&self) -> &Chunk {
        &self.frames.last().expect("frame present").chunk
    }

    // ─── Calls ───────────────────────────────────────────────────

    fn prepare_named_call(
        &mut self,
        name_idx: NameIdx,
        site: CallSiteIdx,
        line: u32,
    ) -> Result<(), NyblError> {
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let name = chunk.name(name_idx).to_string();

        let scoped = self
            .current_scopes()
            .iter()
            .rev()
            .find_map(|scope| scope.get(&name))
            .cloned();
        let callable = if let Some(value) = scoped {
            match value {
                Value::Fn(function) => PreparedCallable::Closure(function),
                other => {
                    return Err(error(
                        line,
                        format!("`{name}` is a {}, not a function", other.type_name()),
                    ));
                }
            }
        } else if let Some(entry) = self
            .frames
            .last()
            .and_then(|frame| frame.current_function_entry.as_ref())
            .filter(|entry| entry.exact_self_name.as_deref() == Some(name.as_str()))
            .cloned()
        {
            PreparedCallable::User(entry)
        } else if let Some(module) = self
            .frames
            .last()
            .and_then(|frame| frame.lexical_context.module_aliases.get(&name).cloned())
        {
            return Err(error(
                line,
                format!(
                    "`{name}` is a {}, not a function",
                    Value::Module(module).type_name()
                ),
            ));
        } else if let Some(entry) = self.imported_function(&name).cloned() {
            PreparedCallable::User(entry)
        } else if self.frames.last().is_some_and(|frame| {
            frame.is_function
                && frame.function_module.as_deref() == Some(nybl::value::ROOT_MODULE_PATH)
        }) {
            match self
                .frames
                .first()
                .and_then(|root| root.scopes.iter().rev().find_map(|scope| scope.get(&name)))
                .cloned()
            {
                Some(Value::Fn(function)) => PreparedCallable::Closure(function),
                Some(other) => {
                    return Err(error(
                        line,
                        format!("`{name}` is a {}, not a function", other.type_name()),
                    ));
                }
                None => self
                    .lookup_function_entry(&name)
                    .map(|entry| PreparedCallable::named_user(&name, entry))
                    .unwrap_or_else(|| PreparedCallable::NamedFallback(name.clone())),
            }
        } else {
            self.lookup_function_entry(&name)
                .map(|entry| PreparedCallable::named_user(&name, entry))
                .unwrap_or_else(|| PreparedCallable::NamedFallback(name.clone()))
        };
        self.preflight_call(callable, name, site, line)
    }

    fn prepare_value_call(
        &mut self,
        callee: Value,
        site: CallSiteIdx,
        line: u32,
    ) -> Result<(), NyblError> {
        match callee {
            Value::Fn(function) => {
                let display = function.self_name.as_deref().unwrap_or("fn").to_string();
                self.preflight_call(PreparedCallable::Closure(function), display, site, line)
            }
            other => Err(error(
                line,
                nybl::error_messages::cant_call_a(other.type_name()),
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_method_call(
        &mut self,
        receiver: Value,
        receiver_target: Option<NamespaceRef>,
        receiver_place: Option<ResolvedPlace>,
        method_idx: NameIdx,
        site: CallSiteIdx,
        nested_place: bool,
        line: u32,
    ) -> Result<(), NyblError> {
        let method = self.current_chunk().name(method_idx).to_string();
        if let Value::Module(module) = &receiver {
            // Universal common methods win over same-named exports.
            if matches!(method.as_str(), "type" | "to_str" | "inspect") {
                return self.preflight_call(
                    PreparedCallable::DeferredMethod {
                        receiver,
                        method: method.clone(),
                        nested_place,
                    },
                    method,
                    site,
                    line,
                );
            }
            let callee = self.module_binding(module, &method).ok_or_else(|| {
                error(
                    line,
                    format!("`{method}` isn't exported from `{}`", module.path),
                )
            })?;
            return self.prepare_value_call(callee, site, line);
        }

        let type_key = match &receiver {
            Value::Struct(value) => Some((
                value.module_path().to_string(),
                value.type_name().to_string(),
            )),
            Value::EnumVariant(value) => Some((
                value.module_path().to_string(),
                value.type_name().to_string(),
            )),
            _ => None,
        };
        if let Some(type_key) = type_key
            && let Some(entry) = self
                .user_methods
                .get(&type_key)
                .and_then(|methods| methods.get(&method))
                .cloned()
        {
            return self.preflight_call(
                PreparedCallable::UserMethod {
                    entry,
                    receiver,
                    receiver_target,
                    receiver_place,
                },
                format!("{}.{}", type_key.1, method),
                site,
                line,
            );
        }
        if matches!(receiver, Value::Array(_))
            && methods::is_mutating_method(&method)
            && let Some(place) = receiver_place
        {
            return self.preflight_call(
                PreparedCallable::PlaceMethodInPlace {
                    place,
                    method: method.clone(),
                },
                method,
                site,
                line,
            );
        }
        if nested_place && receiver_place.is_none() {
            methods::reject_nested_array_mutation(&receiver, &method, line)?;
        }
        self.preflight_call(
            PreparedCallable::DeferredMethod {
                receiver,
                method: method.clone(),
                nested_place,
            },
            method,
            site,
            line,
        )
    }

    fn prepare_named_method_call(
        &mut self,
        target: NamespaceRef,
        method_idx: NameIdx,
        site: CallSiteIdx,
        line: u32,
    ) -> Result<(), NyblError> {
        let method = self.current_chunk().name(method_idx).to_string();
        let receiver_name = self.current_chunk().name(target.name_idx()).to_string();
        let receiver = match target.slot_idx() {
            Some(slot) => self
                .frames
                .last()
                .and_then(|frame| frame.slots.get(slot.0 as usize))
                .cloned()
                .ok_or_else(|| error(line, "VM: local slot out of range"))?,
            None => {
                let value = self
                    .current_scopes()
                    .iter()
                    .rev()
                    .find_map(|scope| scope.get(&receiver_name))
                    .cloned();
                match value {
                    Some(value) => value,
                    None => {
                        if let Some(entry) = self.lookup_function_entry(&receiver_name) {
                            let body: Rc<dyn core::any::Any + 'static> = entry.chunk.clone();
                            let function =
                                NyblFn::try_new_compiled_in_module_with_origin_and_modes(
                                    entry.params.clone(),
                                    entry.param_modes.clone(),
                                    Vec::new(),
                                    body,
                                    Some(receiver_name.clone()),
                                    Some(entry.module_path.clone()),
                                    self.function_origin.clone(),
                                    0,
                                    line,
                                )?;
                            return self.prepare_method_call(
                                Value::Fn(function),
                                None,
                                None,
                                method_idx,
                                site,
                                false,
                                line,
                            );
                        }
                        if let Some(module) = self.module_alias(&receiver_name).cloned() {
                            return self.prepare_method_call(
                                Value::Module(module),
                                None,
                                None,
                                method_idx,
                                site,
                                false,
                                line,
                            );
                        }
                        let hint =
                            self.value_candidates_hint(&receiver_name)
                                .unwrap_or_else(|| {
                                    "Did you forget to create it with `let`?".to_string()
                                });
                        return Err(error_with_hint(
                            line,
                            nybl::error_messages::variable_not_found(&receiver_name),
                            hint,
                        ));
                    }
                }
            }
        };

        if matches!(receiver, Value::Array(_)) && methods::is_mutating_method(&method) {
            return self.preflight_call(
                PreparedCallable::NamedMethodInPlace {
                    target,
                    method: method.clone(),
                },
                method,
                site,
                line,
            );
        }
        self.prepare_method_call(receiver, Some(target), None, method_idx, site, false, line)
    }

    fn preflight_call(
        &mut self,
        callable: PreparedCallable,
        display_name: String,
        site_idx: CallSiteIdx,
        line: u32,
    ) -> Result<(), NyblError> {
        let site = self.current_chunk().call_site(site_idx).clone();
        if let PreparedCallable::NamedFallback(name) = &callable {
            validate_named_builtin_arity(name, site.arg_modes.len(), line)?;
        }
        let expected_modes: Option<&[ParamMode]> = match &callable {
            PreparedCallable::User(entry) | PreparedCallable::HostThenUser { entry, .. } => {
                Some(&entry.param_modes)
            }
            PreparedCallable::Closure(function) => Some(&function.param_modes),
            PreparedCallable::UserMethod {
                entry,
                receiver_target,
                receiver_place,
                ..
            } => {
                validate_user_arity(
                    &display_name,
                    &entry.param_modes,
                    site.arg_modes.len() + 1,
                    true,
                    line,
                )?;
                if entry.param_modes.first() == Some(&ParamMode::Ref)
                    && receiver_target.is_none()
                    && receiver_place.is_none()
                {
                    let mut error =
                        NyblError::runtime("a `ref` method receiver must be a mutable place", line);
                    error.friendly_hint = Some(
                        "Store the value in a `let` binding, or call through one of its fields or indexes."
                            .to_string(),
                    );
                    return Err(error);
                }
                Some(&entry.param_modes[1..])
            }
            PreparedCallable::NamedFallback(_)
            | PreparedCallable::DeferredMethod { .. }
            | PreparedCallable::NamedMethodInPlace { .. }
            | PreparedCallable::PlaceMethodInPlace { .. } => None,
        };
        if let Some(expected) = expected_modes {
            validate_user_call_modes(&display_name, expected, &site.arg_modes, line)?;
        } else if let Some((index, _)) = site
            .arg_modes
            .iter()
            .enumerate()
            .find(|(_, mode)| **mode == ParamMode::Ref)
        {
            return Err(call_mode_error(
                line,
                &display_name,
                index,
                ParamMode::Value,
            ));
        }

        if matches!(
            &callable,
            PreparedCallable::NamedMethodInPlace { .. }
                | PreparedCallable::PlaceMethodInPlace { .. }
        ) || matches!(
            &callable,
            PreparedCallable::DeferredMethod { receiver, .. }
                if !matches!(receiver, Value::Host(_))
        ) {
            validate_builtin_method_arity(&display_name, site.arg_modes.len(), line)?;
        }
        if let PreparedCallable::NamedMethodInPlace { target, method } = &callable {
            let receiver_name = self.current_chunk().name(target.name_idx());
            methods::reject_constant_array_mutation(receiver_name, method, line)?;
            if target.slot_idx().is_none()
                && self
                    .frames
                    .last()
                    .is_some_and(|frame| frame.captured_names.contains(receiver_name))
            {
                return Err(nybl::ref_params::captured_ref_target(1, line));
            }
        }
        if let PreparedCallable::PlaceMethodInPlace { place, method } = &callable {
            methods::reject_constant_array_mutation(&place.root_name, method, line)?;
            self.validate_mutable_resolved_target(&place.target, &place.root_name, 0, line)?;
        }

        let receiver_ref = match &callable {
            PreparedCallable::UserMethod {
                entry,
                receiver_target,
                receiver_place,
                ..
            } if entry.param_modes.first() == Some(&ParamMode::Ref) => {
                if let Some(place) = receiver_place {
                    self.validate_mutable_resolved_target(
                        &place.target,
                        &place.root_name,
                        0,
                        line,
                    )?;
                    Some(PreparedReceiverRef::Place(place.clone()))
                } else {
                    let recipe = receiver_target.expect("mutable receiver target was preflighted");
                    let name = self.current_chunk().name(recipe.name_idx());
                    if nybl::naming::is_constant_name(name) {
                        return Err(nybl::error_messages::constant_mutation_error(name, line));
                    }
                    if recipe.slot_idx().is_none()
                        && self
                            .frames
                            .last()
                            .is_some_and(|frame| frame.captured_names.contains(name))
                    {
                        return Err(nybl::ref_params::captured_ref_target(1, line));
                    }
                    Some(PreparedReceiverRef::Binding(recipe))
                }
            }
            _ => None,
        };
        let mut seen = BTreeSet::new();
        if let Some(receiver) = &receiver_ref {
            let identity = match receiver {
                PreparedReceiverRef::Binding(recipe) => match recipe.slot_idx() {
                    Some(slot) => (0_u8, slot.0),
                    None => (1_u8, recipe.name_idx().0),
                },
                PreparedReceiverRef::Place(place) => match place.root_recipe.slot_idx() {
                    Some(slot) => (0_u8, slot.0),
                    None => (1_u8, place.root_recipe.name_idx().0),
                },
            };
            seen.insert(identity);
        }
        let mut refs = Vec::new();
        for (index, (mode, target)) in site
            .arg_modes
            .iter()
            .zip(site.ref_targets.iter())
            .enumerate()
        {
            if *mode != ParamMode::Ref {
                continue;
            }
            let recipe = match target {
                Some(RefArgTarget::Binding(recipe)) => RefArgTarget::Binding(*recipe),
                Some(RefArgTarget::Place(place)) => RefArgTarget::Place(*place),
                Some(RefArgTarget::Invalid) | None => {
                    return Err(invalid_ref_target_error(line, index));
                }
            };
            let root = match recipe {
                RefArgTarget::Binding(root) => root,
                RefArgTarget::Place(place) => self.current_chunk().place(place).root,
                RefArgTarget::Invalid => unreachable!(),
            };
            let name = self.current_chunk().name(root.name_idx());
            if nybl::naming::is_constant_name(name) {
                return Err(nybl::error_messages::constant_mutation_error(name, line));
            }
            if root.slot_idx().is_none()
                && self
                    .frames
                    .last()
                    .is_some_and(|frame| frame.captured_names.contains(name))
            {
                return Err(nybl::ref_params::captured_ref_target(index + 1, line));
            }
            let root_exists = match root.slot_idx() {
                Some(slot) => self
                    .frames
                    .last()
                    .is_some_and(|frame| frame.slots.get(slot.0 as usize).is_some()),
                None => self
                    .current_scopes()
                    .iter()
                    .rev()
                    .any(|scope| scope.contains_key(name)),
            };
            if !root_exists {
                return Err(invalid_ref_target_error(line, index));
            }
            let identity = match root.slot_idx() {
                Some(slot) => (0_u8, slot.0),
                None => (1_u8, root.name_idx().0),
            };
            if !seen.insert(identity) {
                return Err(duplicate_ref_target_error(line));
            }
            refs.push(PreparedRef {
                param: index,
                recipe,
                index_count: match recipe {
                    RefArgTarget::Place(place) => self
                        .current_chunk()
                        .place(place)
                        .projections
                        .iter()
                        .filter(|projection| matches!(projection, PlaceProjectionRecipe::Index))
                        .count(),
                    RefArgTarget::Binding(_) | RefArgTarget::Invalid => 0,
                },
            });
        }
        let value_count = site
            .arg_modes
            .iter()
            .filter(|mode| **mode == ParamMode::Value)
            .count();
        let ref_index_count = refs.iter().map(|prepared| prepared.index_count).sum();
        self.frames
            .last_mut()
            .expect("frame present")
            .prepared_calls
            .push(PreparedCall {
                site: site_idx,
                callable,
                display_name,
                refs,
                receiver_ref,
                value_count,
                ref_index_count,
            });
        Ok(())
    }

    fn call_prepared(&mut self, site: CallSiteIdx, line: u32) -> Result<Next, NyblError> {
        let prepared = self
            .frames
            .last_mut()
            .expect("frame present")
            .prepared_calls
            .pop()
            .ok_or_else(|| error(line, "VM: missing prepared call"))?;
        if prepared.site != site {
            return Err(error(line, "VM: prepared call-site mismatch"));
        }
        let stacked = self.pop_n_values(prepared.value_count + prepared.ref_index_count, line)?;
        let (ordinary, ref_indices) = stacked.split_at(prepared.value_count);
        let modes = self.current_chunk().call_site(site).arg_modes.clone();
        let param_offset = usize::from(matches!(
            &prepared.callable,
            PreparedCallable::UserMethod { .. }
        ));
        let mut ordinary = ordinary.iter().cloned();
        let mut full_args: Vec<Option<Value>> =
            core::iter::repeat_with(|| None).take(modes.len()).collect();
        for (index, mode) in modes.iter().enumerate() {
            if *mode == ParamMode::Value {
                full_args[index] = ordinary.next();
            }
        }

        let mut pending =
            Vec::with_capacity(prepared.refs.len() + usize::from(prepared.receiver_ref.is_some()));
        let receiver_snapshot = if let Some(recipe) = prepared.receiver_ref {
            let place = match recipe {
                PreparedReceiverRef::Binding(recipe) => {
                    let (target, root) = self.resolve_ref_target(recipe, 0, line)?;
                    ResolvedPlace {
                        target,
                        root_recipe: recipe,
                        root_name: self.current_chunk().name(recipe.name_idx()).to_string(),
                        root,
                        projections: Vec::new(),
                    }
                }
                PreparedReceiverRef::Place(place) => self.refresh_resolved_place(place, line)?,
            };
            let snapshot = self.place_value_from(&place.root, &place.projections, line)?;
            pending.push(PendingWriteBack {
                parameter: 0,
                place,
            });
            Some(snapshot)
        } else {
            None
        };
        let mut ref_index_offset = 0;
        for prepared_ref in prepared.refs {
            let next_offset = ref_index_offset + prepared_ref.index_count;
            let indices = ref_indices[ref_index_offset..next_offset].to_vec();
            ref_index_offset = next_offset;
            let place =
                self.resolve_ref_recipe(prepared_ref.recipe, indices, prepared_ref.param, line)?;
            let snapshot = self.place_value_from(&place.root, &place.projections, line)?;
            if pending
                .iter()
                .any(|write_back: &PendingWriteBack| write_back.place.target == place.target)
            {
                return Err(duplicate_ref_target_error(line));
            }
            full_args[prepared_ref.param] = Some(snapshot);
            pending.push(PendingWriteBack {
                parameter: prepared_ref.param + param_offset,
                place,
            });
        }
        let args = full_args
            .into_iter()
            .map(|value| value.ok_or_else(|| error(line, "VM: incomplete prepared arguments")))
            .collect::<Result<Vec<_>, _>>()?;

        let before = self.frames.len();
        let next = match prepared.callable {
            PreparedCallable::User(entry) => self.enter_user_fn_args(entry, args, line)?,
            PreparedCallable::HostThenUser { name, entry } => {
                if let Some(result) = self.host.call(&name, &args, line) {
                    self.push_value(result?);
                    return Ok(Next::Continue);
                }
                self.enter_user_fn_args(entry, args, line)?
            }
            PreparedCallable::Closure(function) => self.call_closure(&function, args, line)?,
            PreparedCallable::UserMethod {
                entry, receiver, ..
            } => {
                let mut method_args = Vec::with_capacity(args.len() + 1);
                method_args.push(receiver_snapshot.unwrap_or(receiver));
                method_args.extend(args);
                self.enter_user_fn_args(entry, method_args, line)?
            }
            PreparedCallable::DeferredMethod {
                receiver,
                method,
                nested_place,
            } => {
                debug_assert!(pending.is_empty());
                return self.dispatch_method(receiver, &method, args, None, nested_place, line);
            }
            PreparedCallable::NamedMethodInPlace { target, method } => {
                debug_assert!(pending.is_empty());
                return self.call_method_in_place_args(target, &method, args, line);
            }
            PreparedCallable::PlaceMethodInPlace { place, method } => {
                debug_assert!(pending.is_empty());
                return self.call_method_at_place(place, &method, args, line);
            }
            PreparedCallable::NamedFallback(name) => {
                debug_assert!(pending.is_empty());
                return self.invoke_named_fallback(&name, args, line);
            }
        };
        if !pending.is_empty() {
            if self.frames.len() != before + 1 {
                return Err(error(
                    line,
                    format!(
                        "VM: `{}` did not enter a function frame",
                        prepared.display_name
                    ),
                ));
            }
            self.frames
                .last_mut()
                .expect("callee frame")
                .pending_write_backs = pending;
        }
        Ok(next)
    }

    fn resolve_ref_target(
        &self,
        recipe: NamespaceRef,
        position: usize,
        line: u32,
    ) -> Result<(ResolvedRefTarget, Value), NyblError> {
        self.resolve_target(recipe, position, line, true)
    }

    fn resolve_target(
        &self,
        recipe: NamespaceRef,
        position: usize,
        line: u32,
        reject_capture: bool,
    ) -> Result<(ResolvedRefTarget, Value), NyblError> {
        let frame_index = self.frames.len().saturating_sub(1);
        let frame = self.frames.last().expect("frame present");
        if let Some(slot) = recipe.slot_idx() {
            let value = frame
                .slots
                .get(slot.0 as usize)
                .cloned()
                .ok_or_else(|| error(line, "VM: local slot out of range"))?;
            return Ok((
                ResolvedRefTarget::Slot {
                    frame: frame_index,
                    slot,
                },
                value,
            ));
        }
        let name = frame.chunk.name(recipe.name_idx()).to_string();
        if reject_capture && frame.captured_names.contains(&name) {
            return Err(nybl::ref_params::captured_ref_target(position + 1, line));
        }
        for (scope_index, scope) in frame.scopes.iter().enumerate().rev() {
            if let Some(value) = scope.get(&name) {
                return Ok((
                    ResolvedRefTarget::Scope {
                        frame: frame_index,
                        scope: scope_index,
                        name,
                    },
                    value.clone(),
                ));
            }
        }
        Err(error_with_hint(
            line,
            nybl::error_messages::variable_not_found(&name),
            "Did you forget to create it with `let`?",
        ))
    }

    fn validate_mutable_resolved_target(
        &self,
        target: &ResolvedRefTarget,
        root_name: &str,
        position: usize,
        line: u32,
    ) -> Result<(), NyblError> {
        if nybl::naming::is_constant_name(root_name) {
            return Err(nybl::error_messages::constant_mutation_error(
                root_name, line,
            ));
        }
        if let ResolvedRefTarget::Scope { frame, name, .. } = target
            && self
                .frames
                .get(*frame)
                .is_some_and(|frame| frame.captured_names.contains(name))
        {
            return Err(nybl::ref_params::captured_ref_target(position + 1, line));
        }
        Ok(())
    }

    fn resolved_target_value(
        &self,
        target: &ResolvedRefTarget,
        line: u32,
    ) -> Result<Value, NyblError> {
        match target {
            ResolvedRefTarget::Slot { frame, slot } => self
                .frames
                .get(*frame)
                .and_then(|frame| frame.slots.get(slot.0 as usize))
                .cloned()
                .ok_or_else(|| error(line, "VM: mutable place slot is no longer live")),
            ResolvedRefTarget::Scope { frame, scope, name } => self
                .frames
                .get(*frame)
                .and_then(|frame| frame.scopes.get(*scope))
                .and_then(|scope| scope.get(name))
                .cloned()
                .ok_or_else(|| error(line, "VM: mutable place binding is no longer live")),
        }
    }

    fn store_resolved_target(
        &mut self,
        target: &ResolvedRefTarget,
        value: Value,
        line: u32,
    ) -> Result<(), NyblError> {
        match target {
            ResolvedRefTarget::Slot { frame, slot } => {
                let target = self
                    .frames
                    .get_mut(*frame)
                    .and_then(|frame| frame.slots.get_mut(slot.0 as usize))
                    .ok_or_else(|| error(line, "VM: mutable place slot is no longer live"))?;
                *target = value;
            }
            ResolvedRefTarget::Scope { frame, scope, name } => {
                let target = self
                    .frames
                    .get_mut(*frame)
                    .and_then(|frame| frame.scopes.get_mut(*scope))
                    .and_then(|scope| scope.get_mut(name))
                    .ok_or_else(|| error(line, "VM: mutable place binding is no longer live"))?;
                *target = value;
            }
        }
        Ok(())
    }

    fn resolve_ref_recipe(
        &self,
        recipe: RefArgTarget,
        indices: Vec<Value>,
        position: usize,
        line: u32,
    ) -> Result<ResolvedPlace, NyblError> {
        match recipe {
            RefArgTarget::Binding(root) => {
                let (target, value) = self.resolve_ref_target(root, position, line)?;
                Ok(ResolvedPlace {
                    target,
                    root_recipe: root,
                    root_name: self.current_chunk().name(root.name_idx()).to_string(),
                    root: value,
                    projections: Vec::new(),
                })
            }
            RefArgTarget::Place(place) => self.resolve_place(place, indices, position, line),
            RefArgTarget::Invalid => Err(invalid_ref_target_error(line, position)),
        }
    }

    fn resolve_place(
        &self,
        place: PlaceIdx,
        indices: Vec<Value>,
        position: usize,
        line: u32,
    ) -> Result<ResolvedPlace, NyblError> {
        self.resolve_place_internal(place, indices, position, line, true, true)
    }

    fn resolve_place_internal(
        &self,
        place: PlaceIdx,
        indices: Vec<Value>,
        position: usize,
        line: u32,
        reject_capture: bool,
        validate_path: bool,
    ) -> Result<ResolvedPlace, NyblError> {
        let recipe = self.current_chunk().place(place);
        let root_recipe = recipe.root;
        let root_name = self
            .current_chunk()
            .name(root_recipe.name_idx())
            .to_string();
        let (target, root) = self.resolve_target(root_recipe, position, line, reject_capture)?;
        let mut indices = indices.into_iter();
        let mut projections = Vec::with_capacity(recipe.projections.len());
        for projection in &recipe.projections {
            projections.push(match projection {
                PlaceProjectionRecipe::Index => ResolvedPlaceProjection::Index(
                    indices
                        .next()
                        .ok_or_else(|| error(line, "VM: missing mutable-place index"))?,
                ),
                PlaceProjectionRecipe::Field(field) => {
                    ResolvedPlaceProjection::Field(self.current_chunk().name(*field).to_string())
                }
            });
        }
        if indices.next().is_some() {
            return Err(error(line, "VM: too many mutable-place indices"));
        }
        let resolved = ResolvedPlace {
            target,
            root_recipe,
            root_name,
            root,
            projections,
        };
        if validate_path {
            let _ = self.place_value_from(&resolved.root, &resolved.projections, line)?;
        }
        Ok(resolved)
    }

    fn refresh_resolved_place(
        &self,
        mut place: ResolvedPlace,
        line: u32,
    ) -> Result<ResolvedPlace, NyblError> {
        place.root = self.resolved_target_value(&place.target, line)?;
        let _ = self.place_value_from(&place.root, &place.projections, line)?;
        Ok(place)
    }

    fn place_value_from(
        &self,
        root: &Value,
        projections: &[ResolvedPlaceProjection],
        line: u32,
    ) -> Result<Value, NyblError> {
        let mut value = root.clone();
        for projection in projections {
            value = match projection {
                ResolvedPlaceProjection::Index(index) => {
                    ops::index_get_in(&value, index, line, &self.memory)?
                }
                ResolvedPlaceProjection::Field(field) => match &value {
                    Value::Struct(structure) => {
                        structure.field(field).cloned().ok_or_else(|| {
                            error(
                                line,
                                nybl::error_messages::struct_has_no_field(
                                    structure.type_name(),
                                    field,
                                ),
                            )
                        })?
                    }
                    other => {
                        return Err(error(
                            line,
                            nybl::error_messages::cant_assign_field(field, other.type_name()),
                        ));
                    }
                },
            };
        }
        Ok(value)
    }

    fn write_place_value(
        &self,
        root: &mut Value,
        projections: &[ResolvedPlaceProjection],
        value: Value,
        line: u32,
    ) -> Result<(), NyblError> {
        let Some((projection, tail)) = projections.split_first() else {
            *root = value;
            return Ok(());
        };
        if tail.is_empty() {
            return match projection {
                ResolvedPlaceProjection::Index(index) => {
                    ops::index_set_in(root, index, value, line, &self.memory)
                }
                ResolvedPlaceProjection::Field(field) => {
                    self.set_place_field(root, field, value, line)
                }
            };
        }
        let mut child = match projection {
            ResolvedPlaceProjection::Index(index) => {
                ops::index_get_in(root, index, line, &self.memory)?
            }
            ResolvedPlaceProjection::Field(_) => {
                self.place_value_from(root, core::slice::from_ref(projection), line)?
            }
        };
        self.write_place_value(&mut child, tail, value, line)?;
        match projection {
            ResolvedPlaceProjection::Index(index) => {
                ops::index_set_in(root, index, child, line, &self.memory)
            }
            ResolvedPlaceProjection::Field(field) => self.set_place_field(root, field, child, line),
        }
    }

    fn set_place_field(
        &self,
        root: &mut Value,
        field: &str,
        value: Value,
        line: u32,
    ) -> Result<(), NyblError> {
        match root {
            Value::Struct(structure) => {
                let type_name = structure.type_name().to_string();
                if structure.__try_set_field_in(field, value, line, &self.memory)? {
                    Ok(())
                } else {
                    Err(error(
                        line,
                        nybl::error_messages::struct_has_no_field(&type_name, field),
                    ))
                }
            }
            other => Err(error(
                line,
                nybl::error_messages::cant_assign_field(field, other.type_name()),
            )),
        }
    }

    fn enter_user_fn_args(
        &mut self,
        entry: Rc<FnEntry>,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Next, NyblError> {
        if self.frames.len().saturating_sub(1) >= MAX_CALL_DEPTH {
            return Err(error_with_hint(
                line,
                "Too many nested function calls (possible infinite recursion)",
                "Check that your recursive function has a base case that stops calling itself.",
            ));
        }
        let args = self.pack_rest_arguments(args, &entry.param_modes, line)?;
        let slot_count = entry.chunk.slot_count as usize;
        let mut slots = self.take_slots(slot_count);
        for (index, arg) in args.into_iter().enumerate() {
            let slot = entry
                .chunk
                .parameter_slots
                .get(index)
                .ok_or_else(|| error(line, "VM: parameter slot metadata is incomplete"))?;
            slots[slot.0 as usize] = arg;
        }
        self.push_function_frame(
            entry.chunk.clone(),
            slots,
            Vec::new(),
            self.stack.len(),
            Some(entry.module_path.clone()),
            FrameWrap::None,
        );
        self.frames
            .last_mut()
            .expect("callee frame")
            .current_function_entry = Some(Rc::clone(&entry));
        self.apply_entry_alias_context(&entry);
        Ok(Next::Continue)
    }

    fn pack_rest_arguments(
        &self,
        mut args: Vec<Value>,
        modes: &[ParamMode],
        line: u32,
    ) -> Result<Vec<Value>, NyblError> {
        if modes.last() != Some(&ParamMode::Rest) {
            return Ok(args);
        }
        let fixed = modes.len().saturating_sub(1);
        let extras = args.split_off(fixed);
        let rest = Value::__try_new_array_in(extras, line, &self.memory)?;
        if self.memory.__exceeded() {
            return Err(error_fatal_with_hint(
                line,
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            ));
        }
        args.push(rest);
        Ok(args)
    }

    fn invoke_named_fallback(
        &mut self,
        name: &str,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Next, NyblError> {
        match name {
            "range" => {
                let value =
                    builtins::builtin_range_in(&args, line, &mut self.rand_state, &self.memory)?;
                self.push_value(value);
                return Ok(Next::Continue);
            }
            "rand" => {
                let value = builtins::builtin_rand(&args, line, &mut self.rand_state)?;
                self.push_value(value);
                return Ok(Next::Continue);
            }
            "print" => {
                let message = nybl::formatting::__format_values_in(&args, " ", line, &self.memory)?;
                self.host.on_print(&message);
                if let Some(error) = self.host.print_error(line) {
                    return Err(error);
                }
                self.push_value(Value::None);
                return Ok(Next::Continue);
            }
            "try_call" => return self.builtin_try_call(args, line),
            "panic" => {
                let value = builtins::builtin_panic(&args, line)?;
                self.push_value(value);
                return Ok(Next::Continue);
            }
            _ => {}
        }
        let host_result = self.host.call(name, &args, line);
        if let Some(result) = host_result {
            self.push_value(result?);
            return Ok(Next::Continue);
        }
        if let Some(hint) = self.callable_candidates_hint(name) {
            return Err(error_with_hint(
                line,
                nybl::error_messages::function_not_found(name),
                hint,
            ));
        }
        let host_hint = self.host.function_hint().to_string();
        Err(if host_hint.is_empty() {
            error(line, nybl::error_messages::function_not_found(name))
        } else {
            error_with_hint(
                line,
                nybl::error_messages::function_not_found(name),
                host_hint,
            )
        })
    }

    fn call(&mut self, name_idx: NameIdx, argc: usize, line: u32) -> Result<Next, NyblError> {
        // Borrow the name out of the chunk without allocating a
        // fresh `String` per call — `fib(25)` dispatches this
        // path ~75 000 times and even one small allocation per
        // call shows up in profiles. Holding a local `Rc<Chunk>`
        // keeps the name slice valid for the whole body.
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let name: &str = chunk.name(name_idx);

        // Check lexical shadowing first (a `let f = fn() {...}`
        // must win over a same-named builtin / user fn). We peek
        // rather than clone so the common case — no shadow —
        // pays nothing.
        let current_value = self
            .current_scopes()
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
            .cloned();
        if let Some(value) = current_value {
            let args = self.pop_n_values(argc, line)?;
            return match &value {
                Value::Fn(f) => {
                    let f = Rc::clone(f);
                    drop(value);
                    self.call_closure(&f, args, line)
                }
                other => Err(error(
                    line,
                    format!("`{}` is a {}, not a function", name, other.type_name()),
                )),
            };
        }

        // A defining-module declaration alias is a protected value binding.
        // Keep the borrowed local peek above as the hot path, then consult the
        // lexical context only when no local/parameter shadows the name. Clone
        // the module handle solely on this rare alias-call error path.
        let declaration_alias = self.frames.last().and_then(|frame| {
            if frame.lexical_context.module_aliases.is_empty() {
                None
            } else {
                frame.lexical_context.module_aliases.get(name).cloned()
            }
        });
        if let Some(module) = declaration_alias {
            let _args = self.pop_n_values(argc, line)?;
            return Err(error(
                line,
                format!(
                    "`{}` is a {}, not a function",
                    name,
                    Value::Module(module).type_name()
                ),
            ));
        }

        if let Some(entry) = self.imported_function(name).cloned() {
            validate_user_arity(name, &entry.param_modes, argc, false, line)?;
            drop(chunk);
            return self.enter_user_fn(entry, argc, line);
        }

        if self.frames.last().is_some_and(|frame| {
            frame.is_function
                && frame.function_module.as_deref() == Some(nybl::value::ROOT_MODULE_PATH)
        }) {
            let root_value = self
                .frames
                .first()
                .and_then(|root| root.scopes.iter().rev().find_map(|scope| scope.get(name)))
                .cloned();
            if let Some(value) = root_value {
                let args = self.pop_n_values(argc, line)?;
                return match &value {
                    Value::Fn(function) => {
                        let function = Rc::clone(function);
                        self.call_closure(&function, args, line)
                    }
                    other => Err(error(
                        line,
                        format!("`{}` is a {}, not a function", name, other.type_name()),
                    )),
                };
            }
        }

        // User-fn hot path: if `name` is a declared fn and no
        // local shadow exists, pop args straight into the new
        // frame's scope — no intermediate `Vec<Value>` allocation
        // per call. Callsite profile on `fib(28)` showed this
        // path taking ~90 % of all dispatched calls; eliminating
        // the per-call `Vec` cost takes ~500 000 small heap
        // allocations off the table.
        //
        // SAFETY-of-reorder: we've already ruled out lexical
        // shadowing above, so moving the user-fn check in front
        // of builtins / host matches the "let wins over fn; fn
        // wins over builtin" ordering the walker uses. (Walker:
        // locals checked at Call site, then call_function
        // matches builtins — a user-defined `fn print` never
        // reaches this path because `print` isn't in the
        // walker's `functions` table before `fn print` runs.
        // Same here: DefineFn populates `self.functions` only
        // for user-declared names, so builtins like `range`,
        // `print` don't collide.)
        if let Some(entry) = self.lookup_function_entry(name) {
            // Argc check before we touch the stack so error
            // messages match the walker's `name` wording.
            validate_user_arity(name, &entry.param_modes, argc, false, line)?;
            drop(chunk);
            return self.enter_user_fn(entry, argc, line);
        }

        // Everything from here down needs the args as a
        // contiguous slice, so pay the `Vec` cost once.
        let args = self.pop_n_values(argc, line)?;

        // 1. Global builtins — the narrow set that can't be
        // expressed as methods on a receiver (variadic,
        // constructor-shape, session-stateful, or takes a
        // callable). Everything that used to live here is now
        // a method; see `methods::common_method` and
        // `methods::numeric_method`.
        match name {
            "range" => {
                let v =
                    builtins::builtin_range_in(&args, line, &mut self.rand_state, &self.memory)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "rand" => {
                let v = builtins::builtin_rand(&args, line, &mut self.rand_state)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "print" => {
                let message = nybl::formatting::__format_values_in(&args, " ", line, &self.memory)?;
                self.host.on_print(&message);
                if let Some(error) = self.host.print_error(line) {
                    return Err(error);
                }
                self.push_value(Value::None);
                return Ok(Next::Continue);
            }
            "try_call" => return self.builtin_try_call(args, line),
            "panic" => {
                // `builtin_panic` always returns `Err`; the `Ok`
                // arm is unreachable, but matching keeps the
                // compiler happy and keeps this branch symmetric
                // with the other builtins.
                let v = builtins::builtin_panic(&args, line)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            _ => {}
        }

        // 2. Host-provided builtins.
        let host_result = self.host.call(name, &args, line);
        if let Some(result) = host_result {
            let v = result?;
            self.push_value(v);
            return Ok(Next::Continue);
        }

        // Nothing left to try — the name is unresolved. The
        // user-fn fast path above already handled the success
        // case; we only reach here when no declared fn matches
        // either, so we can go straight to the "did you mean?"
        // / host-hint path.
        if let Some(hint) = self.callable_candidates_hint(name) {
            return Err(error_with_hint(
                line,
                nybl::error_messages::function_not_found(name),
                hint,
            ));
        }
        let host_hint = self.host.function_hint().to_string();
        Err(if host_hint.is_empty() {
            error(line, nybl::error_messages::function_not_found(name))
        } else {
            error_with_hint(
                line,
                nybl::error_messages::function_not_found(name),
                host_hint,
            )
        })
    }

    /// Fast path for user-defined function calls: pop `argc`
    /// args straight off the value stack into the new frame's
    /// parameter scope, and push the frame. No intermediate
    /// `Vec<Value>`. Assumes the caller has already validated
    /// `argc == entry.params.len()` so the per-call error
    /// message can include the target name.
    fn enter_user_fn(
        &mut self,
        entry: Rc<FnEntry>,
        argc: usize,
        line: u32,
    ) -> Result<Next, NyblError> {
        if entry.param_modes.last() == Some(&ParamMode::Rest) {
            let args = self.pop_n_values(argc, line)?;
            return self.enter_user_fn_args(entry, args, line);
        }
        if self.frames.len().saturating_sub(1) >= MAX_CALL_DEPTH {
            return Err(error_with_hint(
                line,
                "Too many nested function calls (possible infinite recursion)",
                "Check that your recursive function has a base case that stops calling itself.",
            ));
        }

        if self.stack.len() < argc {
            return Err(error(line, "VM: stack underflow"));
        }

        // Build the frame's flat slot array. Positional arguments rebind their
        // canonical language slots in source order; the rest of `slot_count`
        // is pre-seeded with `None` so later `StoreLocal`s land in-bounds.
        // Backing allocation comes from the freelist when possible — ~500k
        // per-call heap allocs under `fib(28)` fold into a steady-state set of
        // ~MAX_CALL_DEPTH reused vecs.
        let slot_count = entry.chunk.slot_count as usize;
        let mut slots = self.take_slots(slot_count);
        let start = self.stack.len() - argc;
        for i in 0..argc {
            let slot = core::mem::replace(&mut self.stack[start + i], Slot::Value(Value::None));
            match slot {
                Slot::Value(v) => {
                    let parameter_slot =
                        entry.chunk.parameter_slots.get(i).ok_or_else(|| {
                            error(line, "VM: parameter slot metadata is incomplete")
                        })?;
                    slots[parameter_slot.0 as usize] = v;
                }
                _ => {
                    self.stack.truncate(start);
                    self.return_slots(slots);
                    return Err(error(line, "VM: expected value on stack"));
                }
            }
        }
        self.stack.truncate(start);

        self.push_function_frame(
            entry.chunk.clone(),
            slots,
            // Function frames don't use the BTreeMap scope stack
            // on the fast path — captures are on the `Value::Fn`
            // itself (handled in `call_closure`), and all locals
            // live in `slots`. An empty `scopes` vec is cheap to
            // allocate and keeps `LoadVar` / `DefineLocal` from
            // panicking if they still get emitted (they shouldn't
            // inside a fn body, but the fallback is safe).
            Vec::new(),
            self.stack.len(),
            Some(entry.module_path.clone()),
            FrameWrap::None,
        );
        self.frames
            .last_mut()
            .expect("callee frame")
            .current_function_entry = Some(Rc::clone(&entry));
        self.apply_entry_alias_context(&entry);
        // A fresh type_bindings frame scopes any type decl
        // inside this fn to the fn body itself — same rule
        // as push_scope / pop_scope for block scoping. On
        // `do_return` we pop this frame.
        Ok(Next::Continue)
    }

    /// Dispatch a value-based call: the callee sits on top of the
    /// `argc` args. Pops all `argc + 1` slots,
    /// expects the callee to be a `Value::Fn`, and delegates to
    /// `call_closure`.
    fn call_value(&mut self, argc: usize, line: u32) -> Result<Next, NyblError> {
        let callee = self.pop_value(line)?;
        let args = self.pop_n_values(argc, line)?;
        self.invoke_value(callee, args, line)
    }

    /// Invoke an already-evaluated callable with already-popped
    /// args. Shared between `Instr::CallValue` and the
    /// `Value::Module` fast path in `call_method` (where
    /// `m.foo(...)` resolves the fn through a module field
    /// lookup rather than going through the stack callee slot).
    fn invoke_value(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Next, NyblError> {
        match &callee {
            Value::Fn(f) => {
                let f = Rc::clone(f);
                drop(callee);
                self.call_closure(&f, args, line)
            }
            other => Err(error(
                line,
                nybl::error_messages::cant_call_a(other.type_name()),
            )),
        }
    }

    fn call_method(
        &mut self,
        method_idx: NameIdx,
        argc: usize,
        assign_back_to: Option<crate::chunk::AssignBack>,
        nested_place: bool,
        line: u32,
    ) -> Result<Next, NyblError> {
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let method = chunk.name(method_idx);

        let obj = self.pop_value(line)?;
        let args = self.pop_n_values(argc, line)?;

        self.dispatch_method(obj, method, args, assign_back_to, nested_place, line)
    }

    fn call_method_in_place(
        &mut self,
        target: crate::chunk::NamespaceRef,
        method_idx: NameIdx,
        argc: usize,
        line: u32,
    ) -> Result<Next, NyblError> {
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let method = chunk.name(method_idx);
        let args = self.pop_n_values(argc, line)?;
        self.call_method_in_place_args(target, method, args, line)
    }

    fn call_method_in_place_args(
        &mut self,
        target: crate::chunk::NamespaceRef,
        method: &str,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Next, NyblError> {
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let memory = self.memory.clone();
        let receiver_idx = target.name_idx();
        let receiver_name = chunk.name(receiver_idx);
        // The compiler only emits this instruction for a bare identifier and
        // a built-in mutating method name. Resolve the binding after argument
        // evaluation, matching the walker/AOT order. Arrays take the direct
        // path; every other value is cloned once for ordinary dispatch so a
        // user type is still free to define a method named `push`, `pop`, etc.
        let (fallback, assign_back) = match target.slot_idx() {
            Some(slot) => {
                let value = self
                    .frames
                    .last_mut()
                    .expect("frame present")
                    .slots
                    .get_mut(slot.0 as usize)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                if matches!(value, Value::Array(_)) {
                    methods::reject_constant_array_mutation(receiver_name, method, line)?;
                    let result =
                        methods::transactional_array_method_in(value, method, args, line, &memory)?;
                    self.push_value(result);
                    return Ok(Next::Continue);
                }
                (value.clone(), crate::chunk::AssignBack::Slot(slot))
            }
            None => {
                if self.lookup_var_mut_by_idx(receiver_idx).is_none() {
                    if let Some(module) = self.module_alias(receiver_name).cloned() {
                        return self.dispatch_method(
                            Value::Module(module),
                            method,
                            args,
                            None,
                            false,
                            line,
                        );
                    }
                    let hint = self
                        .value_candidates_hint(receiver_name)
                        .unwrap_or_else(|| "Did you forget to create it with `let`?".to_string());
                    return Err(error_with_hint(
                        line,
                        nybl::error_messages::variable_not_found(receiver_name),
                        hint,
                    ));
                }
                let value = self
                    .lookup_var_mut_by_idx(receiver_idx)
                    .expect("binding checked above");
                if matches!(value, Value::Array(_)) {
                    methods::reject_constant_array_mutation(receiver_name, method, line)?;
                    let result =
                        methods::transactional_array_method_in(value, method, args, line, &memory)?;
                    self.push_value(result);
                    return Ok(Next::Continue);
                }
                (value.clone(), crate::chunk::AssignBack::Name(receiver_idx))
            }
        };

        self.dispatch_method(fallback, method, args, Some(assign_back), false, line)
    }

    fn call_method_at_place(
        &mut self,
        place: ResolvedPlace,
        method: &str,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Next, NyblError> {
        let place = self.refresh_resolved_place(place, line)?;
        let mut receiver = self.place_value_from(&place.root, &place.projections, line)?;
        methods::reject_constant_array_mutation(&place.root_name, method, line)?;
        let result = methods::transactional_array_method_in(
            &mut receiver,
            method,
            args,
            line,
            &self.memory,
        )?;
        let mut root = place.root.clone();
        self.write_place_value(&mut root, &place.projections, receiver, line)?;
        self.store_resolved_target(&place.target, root, line)?;
        self.push_value(result);
        Ok(Next::Continue)
    }

    fn dispatch_method(
        &mut self,
        obj: Value,
        method: &str,
        args: Vec<Value>,
        assign_back_to: Option<crate::chunk::AssignBack>,
        nested_place: bool,
        line: u32,
    ) -> Result<Next, NyblError> {
        // `m.foo(args)` on a module alias: there's no struct /
        // enum receiver — `m` is a `Value::Module` whose `foo`
        // export is a callable. Look it up and dispatch through
        // the regular call-by-value path.
        //
        // Before falling through to the binding lookup, check
        // whether the method name is one of the common methods
        // (`type`, `to_str`, `inspect`). Those work on every
        // value, including `Value::Module`, and shouldn't be
        // gated by whether the module happens to export that
        // name.
        if let Value::Module(ref m) = obj {
            if let Some(result) =
                methods::common_method_in(&obj, method, &args, line, &self.memory)?
            {
                self.push_value(result.0);
                return Ok(Next::Continue);
            }
            if let Some(callee) = self.module_binding(m, method) {
                drop(obj);
                return self.invoke_value(callee, args, line);
            }
            return Err(error(
                line,
                format!("`{}` isn't exported from `{}`", method, m.path),
            ));
        }

        // User-method dispatch comes first — any method declared
        // on the receiver's full type identity wins over the
        // built-in method of the same name, matching the walker.
        // Dispatch is strict: `fn paint.Color.shade(self)` never
        // fires on `other.Color` even though the bare name is
        // the same.
        let user_type_key: Option<(String, String)> = match &obj {
            Value::Struct(s) => Some((s.module_path().to_string(), s.type_name().to_string())),
            Value::EnumVariant(e) => Some((e.module_path().to_string(), e.type_name().to_string())),
            _ => None,
        };
        if let Some(type_key) = user_type_key {
            let entry = self
                .user_methods
                .get(&type_key)
                .and_then(|m| m.get(method))
                .cloned();
            if let Some(entry) = entry {
                validate_user_arity(
                    &format!("{}.{}", type_key.1, method),
                    &entry.param_modes,
                    args.len() + 1,
                    true,
                    line,
                )?;
                let mut method_args = Vec::with_capacity(args.len() + 1);
                method_args.push(obj);
                method_args.extend(args);
                self.enter_user_fn_args(entry, method_args, line)?;
                // Legacy `CallMethod` only reaches value-only methods. Calls
                // with a `ref` receiver or explicit ref arguments use the
                // prepared-call path above, which owns their atomic
                // write-backs. Keep the legacy assignment recipe unused here.
                let _ = assign_back_to;
                return Ok(Next::Continue);
            }
        }

        if nested_place {
            methods::reject_nested_array_mutation(&obj, method, line)?;
        }

        // `type` / `to_str` / `inspect` work on every value —
        // dispatch them ahead of the type-specific tables so
        // walker / VM / AOT agree on the common method surface.
        if let Some((ret, _)) = methods::common_method_in(&obj, method, &args, line, &self.memory)?
        {
            self.push_value(ret);
            return Ok(Next::Continue);
        }

        // Built-in `Result` combinators. Pure methods (`is_ok`,
        // `unwrap`, …) return through the regular path; callable-
        // taking ones (`map`, `map_err`, `and_then`) push a
        // closure frame with a `FrameWrap` that wraps the return
        // in `Ok`/`Err` or passes it through.
        if methods::is_builtin_result(&obj) {
            if let Some(v) = methods::result_method(&obj, method, &args, line)? {
                self.push_value(v);
                return Ok(Next::Continue);
            }
            if let Some(kind) = methods::is_result_callable_method(method) {
                return self.call_result_callable_method(obj, kind, method, args, line);
            }
        }

        let (ret, mutated) = match &obj {
            Value::Array(arr) => methods::array_method_in(arr, method, &args, line, &self.memory)?,
            Value::Str(s) => {
                methods::string_method_in(s.as_str(), method, &args, line, &self.memory)?
            }
            Value::Dict(entries) => {
                methods::dict_method_in(entries, method, &args, line, &self.memory)?
            }
            Value::Int(_) | Value::Number(_) => methods::numeric_method(&obj, method, &args, line)?,
            Value::Bool(_) => methods::bool_method(&obj, method, &args, line)?,
            Value::Iter(_) => methods::iter_method_in(&obj, method, &args, line, &self.memory)?,
            Value::Host(value) => {
                let result = self.host.call_method(value, method, &args, line);
                match result {
                    Some(result) => (result?, None),
                    None => {
                        return Err(error(
                            line,
                            nybl::error_messages::no_such_method(obj.type_name(), method),
                        ));
                    }
                }
            }
            _ => {
                return Err(error(
                    line,
                    nybl::error_messages::no_such_method(obj.type_name(), method),
                ));
            }
        };

        if methods::is_mutating_method(method)
            && let (Some(target), Some(new_obj)) = (assign_back_to, mutated)
        {
            match target {
                crate::chunk::AssignBack::Slot(slot) => {
                    let frame = self.frames.last_mut().expect("frame present");
                    let i = slot.0 as usize;
                    if i < frame.slots.len() {
                        frame.slots[i] = new_obj;
                    }
                    // Out-of-range slot: silently drop the
                    // mutation. The compiler only emits
                    // `Slot(i)` for a slot it knows exists,
                    // so this branch is effectively a
                    // miscompile guard.
                }
                crate::chunk::AssignBack::Name(var_idx) => {
                    let var_name = self.current_chunk().name(var_idx).to_string();
                    self.set_existing(&var_name, new_obj);
                }
            }
        }
        self.push_value(ret);
        Ok(Next::Continue)
    }

    fn define_struct(&mut self, idx: StructIdx, line: u32) -> Result<(), NyblError> {
        let def = self.current_chunk().struct_def(idx).clone();
        let mut seen = BTreeSet::new();
        for field in &def.fields {
            if !seen.insert(field.clone()) {
                return Err(error(
                    line,
                    format!("Struct `{}` has duplicate field `{}`", def.name, field),
                ));
            }
        }
        // Type identity is `(current_module, def.name)`. Two
        // different modules declaring the same name coexist at
        // distinct registry keys; same-module redecl is a no-op
        // on matching shape and an error otherwise.
        let key = (self.current_module.clone(), def.name.clone());
        if let Some(existing) = self.struct_defs.get(&key) {
            if existing == &def.fields {
                self.bind_local_type(&def.name);
                return Ok(());
            }
            return Err(error(
                line,
                format!("Struct `{}` is already declared", def.name),
            ));
        }
        self.struct_defs.insert(key, def.fields);
        self.bind_local_type(&def.name);
        Ok(())
    }

    fn define_enum(&mut self, idx: EnumIdx, line: u32) -> Result<(), NyblError> {
        let def = self.current_chunk().enum_def(idx).clone();
        let variants: Vec<(String, EnumVariantShape)> = def
            .variants
            .into_iter()
            .map(|v| (v.name, v.shape))
            .collect();
        let key = (self.current_module.clone(), def.name.clone());
        if let Some(existing) = self.enum_defs.get(&key) {
            if shapes_match(existing, &variants) {
                self.bind_local_type(&def.name);
                return Ok(());
            }
            return Err(error(
                line,
                format!("Enum `{}` is already declared", def.name),
            ));
        }
        let mut seen_variants = BTreeSet::new();
        for (variant, shape) in &variants {
            if !seen_variants.insert(variant.clone()) {
                return Err(error(
                    line,
                    format!("Enum `{}` has duplicate variant `{}`", def.name, variant),
                ));
            }
            let fields = match shape {
                EnumVariantShape::Unit => continue,
                EnumVariantShape::Tuple(fields) | EnumVariantShape::Struct(fields) => fields,
            };
            let mut seen_fields = BTreeSet::new();
            for field in fields {
                if !seen_fields.insert(field.clone()) {
                    return Err(error(
                        line,
                        format!(
                            "Enum variant `{}::{}` has duplicate field `{}`",
                            def.name, variant, field
                        ),
                    ));
                }
            }
        }
        self.enum_defs.insert(key, variants);
        self.bind_local_type(&def.name);
        Ok(())
    }

    /// Bind a declared type's bare name in the current
    /// `type_bindings` frame so subsequent references resolve
    /// to *this* module's version.
    fn bind_local_type(&mut self, name: &str) {
        if let Some(scope) = self.type_bindings.last_mut() {
            scope.insert(name.to_string(), self.current_module.clone());
        }
        if self.is_module_top_scope() {
            self.type_exports
                .insert(name.to_string(), self.current_module.clone());
            self.publish_root_type_binding(name.to_string(), self.current_module.clone());
        }
    }

    fn define_method(&mut self, type_name: NameIdx, method_name: NameIdx, fn_idx: FnIdx) {
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let type_name_s = chunk.name(type_name).to_string();
        let method_name_s = chunk.name(method_name).to_string();
        let fn_def = chunk.function(fn_idx);
        let entry = Rc::new(FnEntry {
            exact_self_name: None,
            params: fn_def.params.clone(),
            param_modes: fn_def.param_modes.clone(),
            chunk: Rc::clone(&fn_def.chunk),
            module_path: self.current_module.clone(),
            declaration_alias_names: self
                .frames
                .last()
                .expect("defining frame")
                .lexical_context
                .module_aliases
                .keys()
                .cloned()
                .collect(),
        });
        // Methods attach to the *full* receiver-type identity.
        // A method declared inside `paint` for `Color` only
        // fires on `paint.Color` values, not on same-named
        // types from other modules.
        let key = (self.current_module.clone(), type_name_s);
        self.user_methods
            .entry(key)
            .or_default()
            .insert(method_name_s, entry);
    }

    fn construct_struct(
        &mut self,
        namespace: Option<crate::chunk::NamespaceRef>,
        type_name: NameIdx,
        count: usize,
        line: u32,
    ) -> Result<(), NyblError> {
        let type_name_s = self.current_chunk().name(type_name).to_string();
        // Resolve the source-level reference to its full type
        // identity before validating the shape. `namespace`
        // means `ns.Type { ... }`; bare means the scope walker
        // looks up `Type` in `type_bindings`.
        let module_path = match namespace {
            Some(namespace) => self.resolve_namespaced_type(namespace, &type_name_s, line)?,
            None => self.resolve_type_ref(None, &type_name_s).ok_or_else(|| {
                error(
                    line,
                    nybl::error_messages::struct_not_declared(&type_name_s),
                )
            })?,
        };
        let key = (module_path.clone(), type_name_s.clone());
        let decl = self
            .struct_defs
            .get(&key)
            .ok_or_else(|| {
                error(
                    line,
                    nybl::error_messages::struct_not_declared(&type_name_s),
                )
            })?
            .clone();
        let flat = self.pop_n_values(count * 2, line)?;
        let mut provided: BTreeMap<String, Value> = BTreeMap::new();
        let mut iter = flat.into_iter();
        while let (Some(key), Some(val)) = (iter.next(), iter.next()) {
            let key_str = match &key {
                Value::Str(s) => s.as_str().to_string(),
                other => {
                    return Err(error(
                        line,
                        format!(
                            "Struct field names must be strings, got {}",
                            other.type_name()
                        ),
                    ));
                }
            };
            drop(key);
            if provided.contains_key(&key_str) {
                return Err(error(
                    line,
                    format!("Field `{key_str}` specified twice in `{type_name_s}` construction"),
                ));
            }
            if !decl.iter().any(|d| d == &key_str) {
                let msg = nybl::error_messages::struct_has_no_field(&type_name_s, &key_str);
                let err =
                    match nybl::suggest::did_you_mean(&key_str, decl.iter().map(|s| s.as_str())) {
                        Some(hint) => error_with_hint(line, msg, hint),
                        None => error(line, msg),
                    };
                return Err(err);
            }
            provided.insert(key_str, val);
        }
        let mut fields: Vec<(String, Value)> = Vec::with_capacity(decl.len());
        for d in &decl {
            match provided.remove(d) {
                Some(v) => fields.push((d.clone(), v)),
                None => {
                    return Err(error(
                        line,
                        format!("Missing field `{d}` in `{type_name_s}` construction"),
                    ));
                }
            }
        }
        self.push_value(Value::__try_new_struct_in(
            module_path,
            type_name_s,
            fields,
            line,
            &self.memory,
        )?);
        Ok(())
    }

    fn validate_struct_construct(
        &self,
        namespace: Option<crate::chunk::NamespaceRef>,
        type_name: &str,
        provided: &[String],
        line: u32,
    ) -> Result<(), NyblError> {
        let module_path = match namespace {
            Some(namespace) => self.resolve_namespaced_type(namespace, type_name, line)?,
            None => self
                .resolve_type_ref(None, type_name)
                .ok_or_else(|| error(line, nybl::error_messages::struct_not_declared(type_name)))?,
        };
        let declared = self
            .struct_defs
            .get(&(module_path, type_name.to_string()))
            .ok_or_else(|| error(line, nybl::error_messages::struct_not_declared(type_name)))?;
        for (index, field) in provided.iter().enumerate() {
            if provided[..index].contains(field) {
                return Err(error(
                    line,
                    format!("Field `{field}` specified twice in `{type_name}` construction"),
                ));
            }
            if !declared.contains(field) {
                let message = nybl::error_messages::struct_has_no_field(type_name, field);
                return match nybl::suggest::did_you_mean(
                    field,
                    declared.iter().map(|name| name.as_str()),
                ) {
                    Some(hint) => Err(error_with_hint(line, message, hint)),
                    None => Err(error(line, message)),
                };
            }
        }
        for field in declared {
            if !provided.contains(field) {
                return Err(error(
                    line,
                    format!("Missing field `{field}` in `{type_name}` construction"),
                ));
            }
        }
        Ok(())
    }

    fn validate_enum_construct(
        &self,
        namespace: Option<crate::chunk::NamespaceRef>,
        type_name: &str,
        variant: &str,
        shape: EnumConstructShape,
        provided: &[String],
        line: u32,
    ) -> Result<(), NyblError> {
        let module_path = match namespace {
            Some(namespace) => self.resolve_namespaced_type(namespace, type_name, line)?,
            None => self
                .resolve_type_ref(None, type_name)
                .ok_or_else(|| error(line, nybl::error_messages::enum_not_declared(type_name)))?,
        };
        let variants = self
            .enum_defs
            .get(&(module_path, type_name.to_string()))
            .ok_or_else(|| error(line, nybl::error_messages::enum_not_declared(type_name)))?;
        let declared = variants
            .iter()
            .find(|(name, _)| name == variant)
            .map(|(_, shape)| shape)
            .ok_or_else(|| {
                let message = nybl::error_messages::enum_has_no_variant(type_name, variant);
                match nybl::suggest::did_you_mean(
                    variant,
                    variants.iter().map(|(name, _)| name.as_str()),
                ) {
                    Some(hint) => error_with_hint(line, message, hint),
                    None => error(line, message),
                }
            })?;
        match (declared, shape) {
            (EnumVariantShape::Unit, EnumConstructShape::Unit) => Ok(()),
            (EnumVariantShape::Tuple(fields), EnumConstructShape::Tuple(argc)) => {
                if fields.len() == argc as usize {
                    Ok(())
                } else {
                    Err(error(
                        line,
                        format!(
                            "`{}::{}` expects {} argument{}, but got {}",
                            type_name,
                            variant,
                            fields.len(),
                            if fields.len() == 1 { "" } else { "s" },
                            argc
                        ),
                    ))
                }
            }
            (EnumVariantShape::Struct(fields), EnumConstructShape::Struct(_)) => {
                for (index, field) in provided.iter().enumerate() {
                    if provided[..index].contains(field) {
                        return Err(error(
                            line,
                            format!("Field `{field}` specified twice in `{type_name}::{variant}`"),
                        ));
                    }
                    if !fields.contains(field) {
                        return Err(error(
                            line,
                            nybl::error_messages::variant_has_no_field(type_name, variant, field),
                        ));
                    }
                }
                for field in fields {
                    if !provided.contains(field) {
                        return Err(error(
                            line,
                            format!(
                                "Missing field `{field}` in `{type_name}::{variant}` construction"
                            ),
                        ));
                    }
                }
                Ok(())
            }
            (EnumVariantShape::Unit, _) => Err(error(
                line,
                format!("Variant `{type_name}::{variant}` takes no payload"),
            )),
            (EnumVariantShape::Tuple(_), _) => Err(error(
                line,
                format!("Variant `{type_name}::{variant}` expects positional arguments `(…)`"),
            )),
            (EnumVariantShape::Struct(_), _) => Err(error(
                line,
                format!("Variant `{type_name}::{variant}` expects named fields `{{ … }}`"),
            )),
        }
    }

    fn construct_enum(
        &mut self,
        namespace: Option<crate::chunk::NamespaceRef>,
        type_name: NameIdx,
        variant: NameIdx,
        shape: EnumConstructShape,
        line: u32,
    ) -> Result<(), NyblError> {
        let type_name_s = self.current_chunk().name(type_name).to_string();
        let variant_s = self.current_chunk().name(variant).to_string();
        let module_path = match namespace {
            Some(namespace) => self.resolve_namespaced_type(namespace, &type_name_s, line)?,
            None => self.resolve_type_ref(None, &type_name_s).ok_or_else(|| {
                error(line, nybl::error_messages::enum_not_declared(&type_name_s))
            })?,
        };
        let key = (module_path.clone(), type_name_s.clone());
        let decl = self
            .enum_defs
            .get(&key)
            .ok_or_else(|| error(line, nybl::error_messages::enum_not_declared(&type_name_s)))?
            .clone();
        let variant_decl = decl
            .iter()
            .find(|(n, _)| n == &variant_s)
            .cloned()
            .ok_or_else(|| {
                let msg = nybl::error_messages::enum_has_no_variant(&type_name_s, &variant_s);
                match nybl::suggest::did_you_mean(&variant_s, decl.iter().map(|(n, _)| n.as_str()))
                {
                    Some(hint) => error_with_hint(line, msg, hint),
                    None => error(line, msg),
                }
            })?;
        match (&variant_decl.1, shape) {
            (EnumVariantShape::Unit, EnumConstructShape::Unit) => {
                self.push_value(Value::__new_enum_unit_in(
                    module_path,
                    type_name_s,
                    variant_s,
                    &self.memory,
                ));
            }
            (EnumVariantShape::Tuple(fields), EnumConstructShape::Tuple(argc)) => {
                if fields.len() as u32 != argc {
                    return Err(error(
                        line,
                        format!(
                            "`{}::{}` expects {} argument{}, but got {}",
                            type_name_s,
                            variant_s,
                            fields.len(),
                            if fields.len() == 1 { "" } else { "s" },
                            argc
                        ),
                    ));
                }
                let items = self.pop_n_values(argc as usize, line)?;
                self.push_value(Value::__try_new_enum_tuple_in(
                    module_path,
                    type_name_s,
                    variant_s,
                    items,
                    line,
                    &self.memory,
                )?);
            }
            (EnumVariantShape::Struct(decl_fields), EnumConstructShape::Struct(count)) => {
                let flat = self.pop_n_values(count as usize * 2, line)?;
                let mut provided: BTreeMap<String, Value> = BTreeMap::new();
                let mut iter = flat.into_iter();
                while let (Some(key), Some(val)) = (iter.next(), iter.next()) {
                    let key_str = match &key {
                        Value::Str(s) => s.as_str().to_string(),
                        _ => {
                            return Err(error(
                                line,
                                "Enum struct-variant field names must be strings",
                            ));
                        }
                    };
                    drop(key);
                    if provided.contains_key(&key_str) {
                        return Err(error(
                            line,
                            format!(
                                "Field `{key_str}` specified twice in `{type_name_s}::{variant_s}`"
                            ),
                        ));
                    }
                    if !decl_fields.iter().any(|d| d == &key_str) {
                        return Err(error(
                            line,
                            nybl::error_messages::variant_has_no_field(
                                &type_name_s,
                                &variant_s,
                                &key_str,
                            ),
                        ));
                    }
                    provided.insert(key_str, val);
                }
                let mut fields: Vec<(String, Value)> = Vec::with_capacity(decl_fields.len());
                for d in decl_fields {
                    match provided.remove(d) {
                        Some(v) => fields.push((d.clone(), v)),
                        None => {
                            return Err(error(
                                line,
                                format!(
                                    "Missing field `{d}` in `{type_name_s}::{variant_s}` construction"
                                ),
                            ));
                        }
                    }
                }
                self.push_value(Value::__try_new_enum_struct_in(
                    module_path,
                    type_name_s,
                    variant_s,
                    fields,
                    line,
                    &self.memory,
                )?);
            }
            (EnumVariantShape::Unit, _) => {
                return Err(error(
                    line,
                    format!("Variant `{type_name_s}::{variant_s}` takes no payload"),
                ));
            }
            (EnumVariantShape::Tuple(_), _) => {
                return Err(error(
                    line,
                    format!(
                        "Variant `{type_name_s}::{variant_s}` expects positional arguments `(…)`"
                    ),
                ));
            }
            (EnumVariantShape::Struct(_), _) => {
                return Err(error(
                    line,
                    format!("Variant `{type_name_s}::{variant_s}` expects named fields `{{ … }}`"),
                ));
            }
        }
        Ok(())
    }

    fn field_get(&self, obj: &Value, field: &str, line: u32) -> Result<Value, NyblError> {
        match obj {
            Value::Struct(s) => s.field(field).cloned().ok_or_else(|| {
                let msg = nybl::error_messages::struct_has_no_field(s.type_name(), field);
                let names = s.fields().iter().map(|(k, _)| k.as_str());
                match nybl::suggest::did_you_mean(field, names) {
                    Some(hint) => error_with_hint(line, msg, hint),
                    None => error(line, msg),
                }
            }),
            Value::EnumVariant(e) => e.field(field).cloned().ok_or_else(|| {
                error(
                    line,
                    nybl::error_messages::variant_has_no_field(e.type_name(), e.variant(), field),
                )
            }),
            Value::Module(m) => {
                if let Some(v) = self.module_binding(m, field) {
                    return Ok(v);
                }
                if m.has_type(field) {
                    return Err(error(
                        line,
                        format!("`{}` in `{}` is a type, not a value", field, m.path),
                    ));
                }
                Err(error(
                    line,
                    format!("`{}` isn't exported from `{}`", field, m.path),
                ))
            }
            other => Err(error(
                line,
                nybl::error_messages::cant_read_field(field, other.type_name()),
            )),
        }
    }

    fn field_set(
        &self,
        mut obj: Value,
        field: &str,
        value: Value,
        line: u32,
    ) -> Result<Value, NyblError> {
        // Mutate the owned value and return it to the generic opcode caller.
        // Named-field assignment uses `FieldSetInPlace` instead so its
        // receiver stays off the stack and cannot spuriously trigger a detach.
        match &mut obj {
            Value::Struct(boxed) => {
                let type_name = boxed.type_name().to_string();
                if !boxed.__try_set_field_in(field, value, line, &self.memory)? {
                    return Err(error(
                        line,
                        nybl::error_messages::struct_has_no_field(&type_name, field),
                    ));
                }
                Ok(obj)
            }
            other => Err(error(
                line,
                nybl::error_messages::cant_assign_field(field, other.type_name()),
            )),
        }
    }

    /// Handle a `MatchFail` instruction: pop the scrutinee and
    /// attempt to match it against the pattern at `pattern`. On
    /// success, install the captured bindings into the current
    /// scope and fall through. On failure, jump to `on_fail`.
    ///
    /// Delegates to `nybl::pattern_matches_in` so the VM behaves
    /// exactly like the tree-walker on every pattern shape.
    fn match_fail(
        &mut self,
        pattern: PatternIdx,
        on_fail: CodeOffset,
        line: u32,
    ) -> Result<(), NyblError> {
        let value = self.pop_value(line)?;
        // `pattern` refers to a slot in the *currently executing*
        // chunk's pattern pool; clone the shared handle rather than hold a
        // frame borrow while mutating `self` to install bindings.
        let recipe = self.current_chunk().pattern(pattern).clone();
        let mut bindings: Vec<(String, Value)> = Vec::new();
        // Build a resolver snapshot from the current frame's
        // value scopes plus the VM-level type_bindings and
        // alias map. Patterns referring to `Color::Red` or
        // `m.Color::Red` thread through this so the matcher can
        // compare the value's full identity.
        let frame = self.frames.last().expect("frame present");
        let frame_scopes = &frame.scopes;
        let type_bindings = &self.type_bindings;
        let module_aliases = &self.module_aliases;
        let resolver = |ns: Option<&str>, tn: &str| -> Option<String> {
            if let Some(ns) = ns
                && let Some(slot) = recipe
                    .namespaces
                    .iter()
                    .find(|(name, _)| name == ns)
                    .map(|(_, namespace)| namespace)
                    .and_then(|namespace| namespace.slot_idx())
            {
                return match frame.slots.get(slot.0 as usize) {
                    Some(Value::Module(module)) => module.type_origin(tn).map(str::to_string),
                    _ => None,
                };
            }
            resolve_type_in_frame(frame, frame_scopes, type_bindings, module_aliases, ns, tn)
        };
        let matched = nybl::pattern_matches_in(
            &recipe.pattern,
            &value,
            &mut bindings,
            &resolver,
            &self.memory,
        );
        if matched {
            for (name, v) in bindings {
                self.define_local(name, v);
            }
        } else {
            self.jump(on_fail);
        }
        Ok(())
    }

    /// Implement `try`: pop the top value and inspect the
    /// `Ok` / `Err` shape.
    ///
    /// - `Ok(v)` (single tuple payload) / `Ok` (unit) → push the
    ///   unwrapped value (`v` or `Value::None`) and continue.
    /// - `Err(...)` → act like `Return` from the current frame,
    ///   carrying the whole `Err` variant as the returned value.
    ///   If the current frame is the top-level program, raise a
    ///   runtime error instead (there's no fn to return from).
    /// - Anything else → runtime error.
    ///
    /// Mirrors the walker's `eval_try` so all three engines agree
    /// on the same shape recognition rules.
    fn try_unwrap(&mut self, line: u32) -> Result<Next, NyblError> {
        let value = self.pop_value(line)?;
        match &value {
            Value::EnumVariant(ev) if ev.variant() == "Ok" => {
                use nybl::value::EnumPayload;
                let payload = match ev.payload() {
                    EnumPayload::Tuple(items) if items.len() == 1 => items[0].clone(),
                    EnumPayload::Unit => Value::None,
                    EnumPayload::Tuple(items) => {
                        return Err(error(
                            line,
                            format!(
                                "try: Ok variant must carry exactly one value, got {}",
                                items.len()
                            ),
                        ));
                    }
                    EnumPayload::Struct(_) => {
                        return Err(error(
                            line,
                            "try: Ok variant must carry a single positional value, not named fields",
                        ));
                    }
                };
                self.push_value(payload);
                Ok(Next::Continue)
            }
            Value::EnumVariant(ev) if ev.variant() == "Err" => {
                let current_is_fn = self.frames.last().map(|f| f.is_function).unwrap_or(false);
                if !current_is_fn {
                    return Err(nybl::error_messages::top_level_try_error(line));
                }
                // Fast-return with the Err value: identical path
                // to an ordinary `return err`.
                self.do_return(value, line)
            }
            other => Err(error(
                line,
                format!(
                    "try expected a Result-shaped value (Ok/Err variant), got {}",
                    other.type_name()
                ),
            )),
        }
    }

    fn define_fn(&mut self, idx: FnIdx) {
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let fn_def = chunk.function(idx);
        let name = fn_def.name.clone();
        let entry = Rc::new(FnEntry {
            exact_self_name: Some(name.clone()),
            params: fn_def.params.clone(),
            param_modes: fn_def.param_modes.clone(),
            chunk: Rc::clone(&fn_def.chunk),
            module_path: self.active_module_path().to_string(),
            declaration_alias_names: self
                .frames
                .last()
                .expect("defining frame")
                .lexical_context
                .module_aliases
                .keys()
                .cloned()
                .collect(),
        });
        self.functions.insert(name.clone(), Rc::clone(&entry));
        if self.is_module_top_scope()
            && self.current_module == nybl::value::ROOT_MODULE_PATH
            && let Some(visibility) = self.root_function_visibility.get(&idx.0).copied()
        {
            self.abi_declarations
                .retain(|(existing, _)| existing != &name);
            if visibility == Visibility::Public {
                self.abi_declarations.push((name, entry));
            }
        }
    }

    /// Materialise a lambda expression as a `Value::Fn`. Each
    /// `CaptureSource` in the lambda's `FnDef` tells us exactly
    /// where to read the captured value from the enclosing frame
    /// — no "flatten every binding in sight" pass, and no
    /// over-capture of out-of-scope slots.
    fn make_lambda(&mut self, idx: FnIdx, line: u32) -> Result<(), NyblError> {
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let fn_def = chunk.function(idx);
        let captures = self.snapshot_captures_for(fn_def);
        let compiled_chunk = Rc::clone(&fn_def.chunk);
        let body: Rc<dyn core::any::Any + 'static> = compiled_chunk;
        let value = NyblFn::try_new_compiled_in_module_with_origin_and_modes(
            fn_def.params.clone(),
            fn_def.param_modes.clone(),
            captures,
            body,
            None,
            Some(self.active_module_path().to_string()),
            self.function_origin.clone(),
            0,
            line,
        )
        .map(Value::Fn)?;
        self.push_value(value);
        Ok(())
    }

    /// Package the captures for a lambda according to its
    /// compile-time `capture_sources`. `ParentSlot(n)` reads slot
    /// `n` from the enclosing frame; `ParentScope(name)` walks
    /// the enclosing frame's BTreeMap scope stack to find the
    /// binding.
    ///
    /// A missing `ParentScope` binding is skipped rather than
    /// represented by `Value::None`. Absence and a binding whose
    /// value is genuinely `none` are observably different: leaving
    /// an absent name uncaptured lets the lambda body's normal
    /// `LoadVar` / call path resolve a named function or report
    /// `Variable not found`.
    fn snapshot_captures_for(&self, fn_def: &FnDef) -> Vec<(String, Value)> {
        let frame = self.frames.last().expect("frame present");
        let mut out = Vec::with_capacity(fn_def.capture_names.len());
        for (name, source) in fn_def
            .capture_names
            .iter()
            .zip(fn_def.capture_sources.iter())
        {
            match source {
                CaptureSource::ParentSlot(slot) => {
                    let v = frame
                        .slots
                        .get(slot.0 as usize)
                        .cloned()
                        .unwrap_or(Value::None);
                    out.push((name.clone(), v));
                }
                CaptureSource::ParentScope(look_name) => {
                    let mut found = None;
                    let defining_environment_floor =
                        usize::from(frame.defining_environment_module.is_some());
                    for scope in frame.scopes.iter().skip(defining_environment_floor).rev() {
                        if let Some(v) = scope.get(look_name.as_str()) {
                            found = Some(v.clone());
                            break;
                        }
                    }
                    if let Some(v) = found {
                        out.push((name.clone(), v));
                    }
                }
            };
        }
        out
    }

    /// Call a `Value::Fn` by pushing a new frame whose scope holds
    /// the closure's captures plus its parameters (plus the
    /// closure itself under `self_name`, when present, so
    /// self-reference works without a separate pathway).
    fn call_closure(
        &mut self,
        func: &Rc<NyblFn>,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Next, NyblError> {
        if !func.__is_allowed_by(&self.function_origin, "vm") {
            return Err(error(
                line,
                "This function belongs to a different Nybl engine instance",
            ));
        }
        // The body must be a VM-compiled chunk. Walker-created
        // `Value::Fn`s would carry `FnBody::Ast` and don't belong
        // in the VM.
        let chunk: Rc<Chunk> = match &func.body {
            FnBody::Compiled(any) => match Rc::clone(any).downcast::<Chunk>() {
                Ok(c) => c,
                Err(_) => {
                    return Err(error(
                        line,
                        "Closure body wasn't compiled by the bytecode VM",
                    ));
                }
            },
            FnBody::Ast(_) => {
                return Err(error(
                    line,
                    "Closure body wasn't compiled for the VM — use `nybl::run` to execute tree-walker closures",
                ));
            }
        };

        let display_name = func.self_name.as_deref().unwrap_or("fn");
        validate_user_arity(display_name, &func.param_modes, args.len(), false, line)?;
        let args = self.pack_rest_arguments(args, &func.param_modes, line)?;

        if self.frames.len().saturating_sub(1) >= MAX_CALL_DEPTH {
            return Err(error_with_hint(
                line,
                "Too many nested function calls (possible infinite recursion)",
                "Check that your recursive function has a base case that stops calling itself.",
            ));
        }

        // Lambda params + locals go into the flat `slots` array
        // (same fast path as named fns). Captures and the
        // `self_name` self-reference stay on the BTreeMap scope
        // stack — they're looked up by name at the compile-time
        // fallback path (`LoadVar`) because the compiler can't
        // resolve them to slots from inside the lambda body.
        let slot_count = chunk.slot_count as usize;
        let mut slots = self.take_slots(slot_count);
        for (i, arg) in args.into_iter().enumerate() {
            let slot = chunk
                .parameter_slots
                .get(i)
                .ok_or_else(|| error(line, "VM: parameter slot metadata is incomplete"))?;
            slots[slot.0 as usize] = arg;
        }

        let mut scope = BTreeMap::new();
        for (name, value) in &func.captures {
            scope.insert(name.clone(), value.clone());
        }
        if let Some(self_name) = &func.self_name {
            scope.insert(self_name.clone(), Value::Fn(Rc::clone(func)));
        }

        self.push_function_frame(
            chunk,
            slots,
            vec![scope],
            self.stack.len(),
            func.module_path.clone(),
            FrameWrap::None,
        );
        if let Some(frame) = self.frames.last_mut() {
            frame.captured_names = func.captures.iter().map(|(name, _)| name.clone()).collect();
        }
        Ok(Next::Continue)
    }

    /// Execute a `use` statement. Dispatches the four shapes
    /// (glob / selective / aliased / selective + aliased) against
    /// the module's exported bindings. Types register globally
    /// on every form; the distinction is in how the caller sees
    /// the module's *values and fns* (flat in their scope vs.
    /// behind a namespace binding).
    fn exec_use(&mut self, spec: &crate::chunk::UseSpec, line: u32) -> Result<(), NyblError> {
        let refresh_root_context = self.is_module_top_scope();
        let path = spec.path.as_str();
        let items = spec.items.as_deref();
        let alias = spec.alias.as_deref();

        let is_plain_glob = items.is_none() && alias.is_none();
        if is_plain_glob
            && self
                .imported_here
                .last()
                .is_some_and(|imports| imports.contains(path))
        {
            return Ok(());
        }
        // A lazily evaluated dependency may import the module that is
        // currently executing. Move that active environment into the shared
        // registry for the duration of loading so the nested VM observes and
        // updates the one authoritative handle rather than a cached snapshot.
        self.park_active_defining_environment();
        let loaded = self.load_module(path, line);
        self.restore_active_defining_environment();
        let artifacts = loaded?;

        // Types always register under their *full identity*
        // `(module_path, type_name)`. Two modules declaring the
        // same name now coexist at distinct keys; same-key
        // reinsertion is idempotent on matching shape and a
        // hard error otherwise (means the module was re-loaded
        // with different source).
        for (key, fields) in &artifacts.struct_defs {
            if let Some(existing) = self.struct_defs.get(key) {
                if existing == fields {
                    continue;
                }
                return Err(error(
                    line,
                    format!(
                        "Type `{}` from `{}` reloaded with different fields",
                        key.1, key.0
                    ),
                ));
            }
            self.struct_defs.insert(key.clone(), fields.clone());
        }
        for (key, variants) in &artifacts.enum_defs {
            if let Some(existing) = self.enum_defs.get(key) {
                if shapes_match(existing, variants) {
                    continue;
                }
                return Err(error(
                    line,
                    format!(
                        "Type `{}` from `{}` reloaded with different variants",
                        key.1, key.0
                    ),
                ));
            }
            self.enum_defs.insert(key.clone(), variants.clone());
        }
        for (type_key, method_name, entry) in &artifacts.methods {
            self.user_methods
                .entry(type_key.clone())
                .or_default()
                .insert(method_name.clone(), entry.clone());
        }

        // Gather the module's value-level exports as a single
        // name-keyed list. Fn declarations produce both a
        // scope-visible `Value::Fn` and a `self.functions` entry;
        // plain let bindings contribute just the value.
        let mut exports: Vec<(String, Value)> =
            Vec::with_capacity(artifacts.fn_decls.len() + artifacts.bindings.len());
        let mut fn_entries: BTreeMap<String, Rc<FnEntry>> = BTreeMap::new();
        for (name, entry) in &artifacts.fn_decls {
            if artifacts
                .bindings
                .iter()
                .any(|(binding, _)| binding == name)
            {
                continue;
            }
            let chunk_rc: Rc<Chunk> = entry.chunk.clone();
            let body: Rc<dyn core::any::Any + 'static> = chunk_rc;
            let value = NyblFn::try_new_compiled_in_module_with_origin_and_modes(
                entry.params.clone(),
                entry.param_modes.clone(),
                Vec::new(),
                body,
                Some(name.clone()),
                Some(entry.module_path.clone()),
                self.function_origin.clone(),
                0,
                line,
            )
            .map(Value::Fn)?;
            exports.push((name.clone(), value));
            fn_entries.insert(name.clone(), entry.clone());
        }
        for (name, value) in &artifacts.bindings {
            exports.push((name.clone(), value.clone()));
        }
        // Functions and values are stored separately inside module artifacts,
        // but a flat import exposes one namespace. Sorting the combined
        // projection makes warning order match the walker and generated AOT.
        exports.sort_by(|(left, _), (right, _)| left.cmp(right));

        // Selective filter: ensure every listed name exists, then
        // retain only the listed exports.
        if let Some(list) = items {
            let available: BTreeSet<&str> = exports.iter().map(|(k, _)| k.as_str()).collect();
            for wanted in list {
                if !available.contains(wanted.as_str())
                    && !artifacts.type_exports.contains_key(wanted)
                {
                    return Err(error(
                        line,
                        format!("`{wanted}` isn't exported from `{path}` (selective import)"),
                    ));
                }
            }
            let listed: BTreeSet<String> = list.iter().cloned().collect();
            exports.retain(|(k, _)| listed.contains(k));
            fn_entries.retain(|name, _| listed.contains(name));
        }

        // Decide which of the module's declared type names the
        // caller sees by bare name. Selective = exactly the
        // listed items that are types; glob = all public
        // (non-`_`-prefixed) types; aliased = none at bare name
        // (only reachable through the alias).
        let exposed_types: BTreeMap<String, String> = match items {
            Some(list) => artifacts
                .type_exports
                .iter()
                .filter(|(name, _)| list.iter().any(|item| item == *name))
                .map(|(name, origin)| (name.clone(), origin.clone()))
                .collect(),
            None => artifacts
                .type_exports
                .iter()
                .filter(|(name, _)| {
                    artifacts.explicit_surface || alias.is_some() || !nybl::naming::is_private(name)
                })
                .map(|(name, origin)| (name.clone(), origin.clone()))
                .collect(),
        };

        if let Some(alias_name) = alias {
            // Aliased form: pack the exports into a Value::Module
            // and bind it under the alias. The alias lives in the
            // current frame's top scope. Function values retain
            // their defining module, so sibling calls resolve
            // through cached module artifacts without publishing
            // bare names in this caller.
            let frame_has = self
                .frames
                .last()
                .and_then(|f| f.scopes.last())
                .map(|s| s.contains_key(alias_name))
                .unwrap_or(false);
            let imported_function_has = self
                .imported_functions
                .last()
                .is_some_and(|functions| functions.contains_key(alias_name));
            if frame_has || self.functions.contains_key(alias_name) || imported_function_has {
                return Err(error(
                    line,
                    format!("`{alias_name}` is already bound — can't use it as a module alias"),
                ));
            }
            let live_bindings = exports
                .iter()
                .map(|(name, _)| {
                    let origin = artifacts
                        .binding_origins
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| (path.to_string(), name.clone()));
                    (name.clone(), origin)
                })
                .collect();
            let module_rc = nybl::value::NyblModule::__try_new_live_with_type_exports(
                path.to_string(),
                exports,
                nybl::value::NyblTypeExports::from_origins(exposed_types),
                live_bindings,
                Rc::clone(&self.live_value_environments),
                line,
            )?;
            // Bind the alias three ways:
            //   1. as Value::Module in the current value scope
            //      (immediate `m.helper(x)` at the use site);
            //   2. in `module_aliases` so it survives the
            //      fresh-scope reset at fn call boundaries;
            //   3. in `type_bindings` so patterns + construction
            //      resolve `m.Type` inside fn bodies.
            let module_value = Value::Module(Rc::clone(&module_rc));
            if let Some(frame) = self.frames.last_mut()
                && let Some(scope) = frame.scopes.last_mut()
            {
                scope.insert(alias_name.to_string(), module_value);
            }
            self.module_aliases
                .last_mut()
                .expect("module alias scope")
                .insert(alias_name.to_string(), Rc::clone(&module_rc));
            if let Some(scope) = self.type_bindings.last_mut() {
                scope.insert(alias_name.to_string(), path.to_string());
            }
            if refresh_root_context {
                self.publish_root_module_alias(alias_name.to_string(), Some(Rc::clone(&module_rc)));
                self.publish_root_type_binding(alias_name.to_string(), path.to_string());
            }
        } else {
            // Flat form (glob / selective). Glob skips
            // `_`-prefixed names (privacy); selective doesn't
            // (the user explicitly asked).
            let skip_private = items.is_none() && !artifacts.explicit_surface;
            let module_top_scope = self.is_module_top_scope();
            for (name, mut value) in exports {
                if skip_private && nybl::naming::is_private(&name) {
                    continue;
                }
                let clashes = self
                    .frames
                    .last()
                    .and_then(|f| f.scopes.last())
                    .map(|s| s.contains_key(&name))
                    .unwrap_or(false)
                    || (module_top_scope && self.functions.contains_key(&name))
                    // Slot-allocated locals / params never appear in
                    // dynamic scope maps. Their shared lexical-scope
                    // index plus this use site's binding frontier keeps
                    // the same first-definition-wins check logarithmic.
                    || self.local_scope_shadows(spec.local_scope, &name);
                if clashes {
                    if is_plain_glob {
                        self.runtime_warnings.push(NyblWarning::at(
                            nybl::error_messages::glob_shadow_warning(&name, path),
                            line,
                        ));
                    }
                    continue;
                }
                if let Some(entry) = fn_entries.remove(&name) {
                    self.imported_functions
                        .last_mut()
                        .expect("imported function scope")
                        .insert(name.clone(), Rc::clone(&entry));
                    if module_top_scope {
                        self.publish_root_imported_function(name.clone(), entry);
                    }
                }
                let module = artifacts.module_exports.get(&name).cloned();
                let origin = artifacts
                    .binding_origins
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| (path.to_string(), name.clone()));
                if module_top_scope {
                    self.binding_origins.insert(name.clone(), origin.clone());
                    if (origin.0 != self.current_module || origin.1 != name)
                        && let Some(authoritative) = self
                            .live_value_environments
                            .borrow_mut()
                            .get_mut(&origin.0)
                            .and_then(|environment| environment.remove(&origin.1))
                    {
                        value = authoritative;
                    }
                } else if let Some(current) = self.live_origin_value(&origin) {
                    value = current;
                }
                if let Some(frame) = self.frames.last_mut()
                    && let Some(scope) = frame.scopes.last_mut()
                {
                    scope.insert(name.clone(), value);
                }
                if let Some(module) = module {
                    self.module_aliases
                        .last_mut()
                        .expect("module alias scope")
                        .insert(name.clone(), Rc::clone(&module));
                    if module_top_scope {
                        self.publish_root_module_alias(name, Some(module));
                    }
                }
            }
            // Bring the module's exposed types in by bare name
            // too — `Color::Red` now resolves to *this*
            // module's Color. First-win on conflict matches the
            // value-binding rule.
            for (type_name, origin) in &exposed_types {
                let already_bound = self
                    .type_bindings
                    .last()
                    .map(|s| s.contains_key(type_name))
                    .unwrap_or(false);
                if already_bound {
                    continue;
                }
                if let Some(scope) = self.type_bindings.last_mut() {
                    scope.insert(type_name.clone(), origin.clone());
                }
                if module_top_scope {
                    self.type_exports
                        .entry(type_name.clone())
                        .or_insert_with(|| origin.clone());
                    self.publish_root_type_binding(type_name.clone(), origin.clone());
                }
            }
        }

        if is_plain_glob {
            self.imported_here
                .last_mut()
                .expect("import scope")
                .insert(path.to_string());
        }
        Ok(())
    }

    fn local_scope_shadows(
        &self,
        snapshot: Option<crate::chunk::LocalScopeSnapshot>,
        name: &str,
    ) -> bool {
        let Some(snapshot) = snapshot else {
            return false;
        };
        let chunk = self.current_chunk();
        let scope = &chunk.local_scopes[snapshot.scope.0 as usize];
        scope
            .entries
            .binary_search_by(|entry| chunk.name(entry.name).cmp(name))
            .is_ok_and(|index| scope.entries[index].first_binding < snapshot.binding_count)
    }

    /// Validate `ns.Type` — confirm `ns` binds a `Value::Module`
    /// whose type exports include `type_name`. Used by
    /// namespaced struct-literal / variant-ctor dispatch.
    fn resolve_namespaced_type(
        &self,
        namespace: crate::chunk::NamespaceRef,
        type_name: &str,
        line: u32,
    ) -> Result<String, NyblError> {
        let name = self.current_chunk().name(namespace.name_idx());
        let Some(slot) = namespace.slot_idx() else {
            self.validate_namespaced_type(name, type_name, line)?;
            return self.resolve_type_ref(Some(name), type_name).ok_or_else(|| {
                error(
                    line,
                    format!("`{type_name}` isn't a type exported from `{name}`"),
                )
            });
        };
        let value = self
            .frames
            .last()
            .and_then(|frame| frame.slots.get(slot.0 as usize))
            .ok_or_else(|| error(line, "VM: local slot out of range"))?;
        let module = match value {
            Value::Module(module) => module,
            other => {
                return Err(error(
                    line,
                    format!(
                        "`{name}` is a {}, not a module alias — can't reach `{type_name}` through it",
                        other.type_name()
                    ),
                ));
            }
        };
        module
            .type_origin(type_name)
            .map(str::to_string)
            .ok_or_else(|| {
                error(
                    line,
                    format!("`{type_name}` isn't a type exported from `{}`", module.path),
                )
            })
    }

    fn validate_namespaced_type(
        &self,
        ns: &str,
        type_name: &str,
        line: u32,
    ) -> Result<(), NyblError> {
        // Prefer a local value-scope binding (catches
        // shadowing), but fall back to the VM-level module
        // alias map so namespaced references inside fn bodies
        // still resolve — the fn frame's value scopes don't
        // carry the caller's aliases.
        let frame = self
            .frames
            .last()
            .ok_or_else(|| error(line, "VM: no frame for namespaced type access"))?;
        for scope in frame.scopes.iter().rev() {
            if let Some(v) = scope.get(ns) {
                let module = match v {
                    Value::Module(m) => m,
                    other => {
                        return Err(error(
                            line,
                            format!(
                                "`{}` is a {}, not a module alias — can't reach `{}` through it",
                                ns,
                                other.type_name(),
                                type_name
                            ),
                        ));
                    }
                };
                if !module.has_type(type_name) {
                    return Err(error(
                        line,
                        format!(
                            "`{}` isn't a type exported from `{}`",
                            type_name, module.path
                        ),
                    ));
                }
                return Ok(());
            }
        }
        if let Some(module) = self.module_alias(ns) {
            if !module.has_type(type_name) {
                return Err(error(
                    line,
                    format!(
                        "`{}` isn't a type exported from `{}`",
                        type_name, module.path
                    ),
                ));
            }
            return Ok(());
        }
        Err(error(line, format!("`{ns}` isn't a module alias in scope")))
    }

    /// Resolve a source-level type reference to its declaring
    /// module. Shared scope walker between construction,
    /// pattern matching, and namespace validation: keeps
    /// resolution rules centralised.
    fn resolve_type_ref(&self, namespace: Option<&str>, type_name: &str) -> Option<String> {
        let frame = self.frames.last()?;
        resolve_type_in_frame(
            frame,
            &frame.scopes,
            &self.type_bindings,
            &self.module_aliases,
            namespace,
            type_name,
        )
    }

    fn load_module(&mut self, path: &str, line: u32) -> Result<ModuleArtifacts, NyblError> {
        {
            let cache = self.imports.borrow();
            if let Some(ImportSlot::Loaded(bindings)) = cache.get(path) {
                return Ok(bindings.clone());
            }
            if let Some(ImportSlot::Loading) = cache.get(path) {
                return Err(error(
                    line,
                    format!("Circular import: module `{path}` is still loading"),
                ));
            }
        }

        let resolved = self.host.resolve_module(path);
        let source = match resolved {
            Some(Ok(s)) => s,
            Some(Err(e)) => return Err(e),
            None => {
                return Err(error(line, format!("Module `{path}` not found")));
            }
        };

        self.imports
            .borrow_mut()
            .insert(path.to_string(), ImportSlot::Loading);

        let result = self
            .evaluate_module(path, &source)
            .map_err(|error| error.with_module_source(path, source.as_str()));

        match result {
            Ok(bindings) => {
                self.imports
                    .borrow_mut()
                    .insert(path.to_string(), ImportSlot::Loaded(bindings.clone()));
                Ok(bindings)
            }
            Err(e) => {
                self.imports.borrow_mut().remove(path);
                Err(e)
            }
        }
    }

    /// Parse, compile, and execute a module in a nested VM that
    /// shares the import cache and limits. Returns the module's
    /// top-level `(name, Value)` bindings, with named fns reified
    /// as `Value::Fn` carrying VM-compiled chunks so the caller's
    /// `Call` / `CallValue` paths can dispatch them directly.
    fn evaluate_module(
        &mut self,
        module_path: &str,
        source: &str,
    ) -> Result<ModuleArtifacts, NyblError> {
        let stmts = nybl::parse(source)?;
        let chunk = crate::compile(&stmts)?;
        let public_surface = chunk.public_surface.clone();
        let module_runtime = ModuleRuntime {
            imports: Rc::clone(&self.imports),
            environments: Rc::clone(&self.live_value_environments),
            origins: Rc::clone(&self.live_binding_origins),
        };
        let limits = self.limits.clone();
        let mut sub = Vm::new_internal(
            chunk,
            self.host,
            limits,
            module_runtime,
            module_path.to_string(),
            BTreeMap::new(),
            self.function_origin.clone(),
            self.memory.clone(),
        );
        let module_result = sub.run_internal();
        // Nested module VMs are an implementation detail of the root
        // operation. Preserve their diagnostics (including transitive ones)
        // and defer stderr delivery to that operation's public boundary.
        self.runtime_warnings.append(&mut sub.runtime_warnings);
        if let Err(module_error) = module_result {
            sub.restore_instance_baseline();
            let failed_environment = sub
                .frames
                .first_mut()
                .and_then(|frame| frame.scopes.first_mut())
                .map(core::mem::take)
                .unwrap_or_default();
            put_live_environment(
                &sub.live_value_environments,
                module_path,
                failed_environment,
                &sub.binding_origins,
            );
            // Forwarded bindings have been returned to their declaration
            // modules; the failed facade's partial state must not survive.
            sub.live_value_environments.borrow_mut().remove(module_path);
            sub.live_binding_origins.borrow_mut().remove(module_path);
            return Err(module_error);
        }
        // Collect top-level lets from the module frame's one
        // remaining scope…
        let mut bindings = match sub
            .frames
            .first()
            .and_then(|frame| frame.scopes.first())
            .map(|scope| {
                snapshot_module_bindings(scope, &sub.binding_origins, module_path, &sub.imports, 0)
            })
            .transpose()
        {
            Ok(bindings) => bindings.unwrap_or_default(),
            Err(snapshot_error) => {
                let failed_environment = sub
                    .frames
                    .first_mut()
                    .and_then(|frame| frame.scopes.first_mut())
                    .map(core::mem::take)
                    .unwrap_or_default();
                put_live_environment(
                    &sub.live_value_environments,
                    module_path,
                    failed_environment,
                    &sub.binding_origins,
                );
                sub.live_value_environments.borrow_mut().remove(module_path);
                sub.live_binding_origins.borrow_mut().remove(module_path);
                return Err(snapshot_error);
            }
        };
        let live_environment = sub
            .frames
            .first_mut()
            .and_then(|frame| frame.scopes.first_mut())
            .map(core::mem::take)
            .unwrap_or_default();
        let binding_origins = sub.binding_origins.clone();
        put_live_environment(
            &sub.live_value_environments,
            module_path,
            live_environment,
            &binding_origins,
        );
        sub.live_binding_origins
            .borrow_mut()
            .insert(module_path.to_string(), binding_origins.clone());
        // Named fn entries go out separately so the importer
        // can register them in `self.functions` for bare-ident
        // call resolution. Reified `Value::Fn`s for the same
        // fns are synthesised at the import site (see
        // `exec_import`).
        let mut fn_decls: Vec<(String, Rc<FnEntry>)> = sub.functions.into_iter().collect();
        // Type decls & methods come along for the ride — the
        // importer needs them so pattern-matching and
        // construction against the module's types works
        // without re-declaration. Engine builtins (<builtin>
        // module path) are already seeded in the importer, so
        // we filter them out to avoid duplicating the entries.
        let builtin_mp = nybl::value::BUILTIN_MODULE_PATH;
        let struct_defs: Vec<ModuleStructDef> = sub
            .struct_defs
            .into_iter()
            .filter(|((mp, _), _)| mp != builtin_mp)
            .collect();
        let enum_defs: Vec<ModuleEnumDef> = sub
            .enum_defs
            .into_iter()
            .filter(|((mp, _), _)| mp != builtin_mp)
            .collect();
        let mut methods: Vec<ModuleMethodDef> = Vec::new();
        for (type_key, by_method) in sub.user_methods {
            for (method_name, entry) in by_method {
                methods.push((type_key.clone(), method_name, entry));
            }
        }
        let all_module_exports: BTreeMap<String, Rc<nybl::value::NyblModule>> = bindings
            .iter()
            .filter_map(|(name, value)| match value {
                Value::Module(module) => Some((name.clone(), Rc::clone(module))),
                _ => None,
            })
            .collect();
        let mut lexical_context = Rc::try_unwrap(core::mem::take(&mut sub.root_lexical_context))
            .unwrap_or_else(|context| (*context).clone());
        lexical_context.module_aliases = Rc::new(all_module_exports.clone());
        let lexical_context = Rc::new(lexical_context);
        let mut type_exports = sub.type_exports;
        let explicit_surface = public_surface.is_some();
        if let Some(names) = public_surface {
            let names: BTreeSet<String> = names.into_iter().collect();
            bindings.retain(|(name, _)| names.contains(name));
            fn_decls.retain(|(name, _)| names.contains(name));
            type_exports.retain(|name, _| names.contains(name));
        }
        let module_exports: BTreeMap<String, Rc<nybl::value::NyblModule>> = bindings
            .iter()
            .filter_map(|(name, value)| match value {
                Value::Module(module) => Some((name.clone(), Rc::clone(module))),
                _ => None,
            })
            .collect();
        Ok(ModuleArtifacts {
            explicit_surface,
            bindings,
            binding_origins,
            fn_decls,
            struct_defs,
            enum_defs,
            methods,
            type_exports,
            module_exports,
            lexical_context,
        })
    }

    /// Like `run` but keeps `self` around afterwards so the
    /// caller can inspect the module's final state. Used by
    /// `evaluate_module`.
    fn run_internal(&mut self) -> Result<(), NyblError> {
        while let Some((instr, line)) = self.fetch() {
            if let Err(err) = self.tick(line) {
                let err = self.attach_frame_module_context(err);
                self.unwind_to_try_call(err)?;
                continue;
            }
            match self.dispatch(instr, line) {
                Ok(Next::Continue) => {}
                Ok(Next::Halt) => break,
                Err(err) => {
                    let err = self.attach_frame_module_context(err);
                    self.unwind_to_try_call(err)?;
                }
            }
        }
        Ok(())
    }

    fn do_return(&mut self, value: Value, line: u32) -> Result<Next, NyblError> {
        if self.memory.__exceeded() {
            return Err(error_fatal_with_hint(
                line,
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            ));
        }
        if self.frames.last().is_some_and(|frame| !frame.is_function) {
            drop(value);
            let frame = self.frames.last_mut().expect("frame present");
            frame.ip = frame.chunk.code.len();
            frame.scopes.truncate(frame.scope_base);
            self.stack.clear();
            self.type_bindings.truncate(frame.type_scope_base);
            self.module_aliases.truncate(frame.alias_scope_base);
            self.imported_functions.truncate(frame.alias_scope_base);
            self.imported_here.truncate(frame.alias_scope_base);
            return Ok(Next::Halt);
        }
        // Pop the current frame, truncate any frame-local stack
        // residue, and push the return value for the caller.
        let mut frame = self.frames.pop().expect("frame present");
        self.store_frame_defining_environment(&mut frame);
        self.restore_active_defining_environment();
        self.stack.truncate(frame.stack_base);
        // Drop the function's protected type-binding scope plus any runtime
        // scopes skipped by an early return. Top-level return preserves the
        // builtin map while discarding any open runtime scopes.
        self.type_bindings.truncate(frame.caller_type_scope_depth());
        self.module_aliases.truncate(frame.alias_scope_base);
        self.imported_functions.truncate(frame.alias_scope_base);
        self.imported_here.truncate(frame.alias_scope_base);
        // Iterator-protocol landing pads get their own return
        // handling — they don't just push the value; they
        // manipulate the caller's stack (and maybe jump) to
        // continue the for-loop.
        match frame.wrap {
            FrameWrap::IterStart => {
                // User `.iter()` returned `value`. Stash it on
                // the stack as the iterator object for the
                // matching `IterNext` instruction to consume.
                self.stack.push(Slot::IterObject(value));
                return Ok(Next::Continue);
            }
            FrameWrap::IterAdvance(target) => {
                // User `.next()` returned `value`; dispatch:
                //   Iter::Next(x) → push x so the following
                //     StoreLocal/DefineLocal binds the loop var.
                //     Iterator object stays on the stack under x.
                //   Iter::Done    → pop the iterator, jump exit.
                //   malformed     → raise.
                match unwrap_iter_step(&value) {
                    IterStep::Next(x) => {
                        drop(value);
                        self.push_value(x);
                        return Ok(Next::Continue);
                    }
                    IterStep::Done => {
                        drop(value);
                        self.stack.pop();
                        self.jump(target);
                        return Ok(Next::Continue);
                    }
                    IterStep::Malformed => {
                        let msg = format!(
                            "`.next()` on a `for` iterator must return `Iter::Next(v)` or `Iter::Done`, got {}",
                            value.inspect()
                        );
                        // The returning frame no longer has a
                        // meaningful "current line" — use the
                        // top frame's instruction line as the
                        // best approximation. 0 is fine if we're
                        // returning to the top-level frame.
                        let caller_line = self
                            .frames
                            .last()
                            .and_then(|f| {
                                let idx = f.ip.saturating_sub(1);
                                f.chunk.lines.get(idx).copied()
                            })
                            .unwrap_or(0);
                        return Err(error(caller_line, msg));
                    }
                }
            }
            _ => {}
        }
        // Apply the frame-level return wrapper: `try_call` wraps
        // in `Result::Ok`, `r.map` / `r.map_err` wrap the closure
        // result in Ok / Err respectively. Plain frames pass the
        // value through untouched.
        let final_value = match frame.wrap {
            FrameWrap::None => value,
            FrameWrap::TryCall { line } => {
                builtins::make_try_call_ok_in(value, line, &self.memory)?
            }
            FrameWrap::ResultOk { line } => methods::make_result_ok_in(value, line, &self.memory)?,
            FrameWrap::ResultErr { line } => {
                methods::make_result_err_in(value, line, &self.memory)?
            }
            FrameWrap::IterStart | FrameWrap::IterAdvance(_) => {
                unreachable!("handled above")
            }
        };
        if self.memory.__exceeded() {
            return Err(error_fatal_with_hint(
                line,
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            ));
        }
        // All fallible return processing is complete. Validate every resolved
        // target before the first store, then perform the infallible copy-out
        // as one user-observable commit.
        self.commit_frame_write_backs(&frame, line)?;

        // Recycle the slot vec only after copy-out has read the staged ref
        // parameters.
        if !frame.slots.is_empty() {
            self.return_slots(core::mem::take(&mut frame.slots));
        }
        if frame.is_function {
            self.push_value(final_value);
            Ok(Next::Continue)
        } else {
            // Return at top level: behave like Halt (matches tree-walker,
            // which silently accepts Signal::Return at program scope).
            drop(final_value);
            Ok(Next::Halt)
        }
    }

    fn commit_frame_write_backs(&mut self, frame: &Frame, line: u32) -> Result<(), NyblError> {
        if frame.pending_write_backs.is_empty() {
            return Ok(());
        }
        let mut values = Vec::with_capacity(frame.pending_write_backs.len());
        for pending in &frame.pending_write_backs {
            let parameter_slot = frame
                .chunk
                .parameter_slots
                .get(pending.parameter)
                .ok_or_else(|| error(line, "VM: ref parameter metadata is incomplete"))?;
            let value = frame
                .slots
                .get(parameter_slot.0 as usize)
                .cloned()
                .ok_or_else(|| error(line, "VM: ref parameter slot out of range"))?;
            let mut root = self.resolved_target_value(&pending.place.target, line)?;
            self.write_place_value(&mut root, &pending.place.projections, value, line)?;
            values.push(root);
        }
        for (pending, value) in frame.pending_write_backs.iter().zip(values) {
            self.store_resolved_target(&pending.place.target, value, line)?;
        }
        Ok(())
    }

    /// Implement the `try_call(f)` builtin. Validates the arg
    /// shape, dispatches to `call_closure` to push a frame for
    /// `f`, and marks that frame with `FrameWrap::TryCall` so
    /// the outcome (whether a normal return or a non-fatal
    /// error) gets wrapped in a `Result::Ok`/`Result::Err`
    /// before returning to the original `try_call` caller.
    fn builtin_try_call(&mut self, args: Vec<Value>, line: u32) -> Result<Next, NyblError> {
        if args.len() != 1 {
            return Err(error(
                line,
                format!("`try_call` expects 1 argument, but got {}", args.len()),
            ));
        }
        let mut iter = args.into_iter();
        let callable = iter.next().unwrap();
        let func = match &callable {
            Value::Fn(f) => Rc::clone(f),
            other => {
                return Err(error(
                    line,
                    format!("`try_call` expects a function, got {}", other.type_name()),
                ));
            }
        };
        drop(callable);
        if let Err(err) = self.call_closure(&func, Vec::new(), line) {
            if err.is_fatal {
                return Err(err);
            }
            self.push_value(builtins::make_try_call_err_in(&err, &self.memory));
            if self.memory.__exceeded() {
                return Err(error_fatal_with_hint(
                    line,
                    "Memory limit exceeded",
                    "Your code is using too much memory. Check for large strings or arrays growing in loops.",
                ));
            }
            return Ok(Next::Continue);
        }
        // The frame we just pushed is the one that should
        // participate in the try_call wrap/catch dance.
        if let Some(frame) = self.frames.last_mut() {
            frame.wrap = FrameWrap::TryCall { line };
        }
        Ok(Next::Continue)
    }

    /// Dispatch `receiver.method(args)` as a user-method call
    /// for the iterator protocol. Pushes a bytecode frame with
    /// `wrap` attached so the return value lands on the caller's
    /// stack with the right post-processing. Returns a clean
    /// "iterable doesn't have a .iter() method" error when the
    /// user type doesn't implement the protocol.
    fn dispatch_iter_method(
        &mut self,
        receiver: Value,
        method: &str,
        extra_args: Vec<Value>,
        wrap: FrameWrap,
        line: u32,
    ) -> Result<(), NyblError> {
        let type_key: Option<(String, String)> = match &receiver {
            Value::Struct(s) => Some((s.module_path().to_string(), s.type_name().to_string())),
            Value::EnumVariant(e) => Some((e.module_path().to_string(), e.type_name().to_string())),
            _ => None,
        };
        let key = type_key.ok_or_else(|| {
            error(
                line,
                nybl::error_messages::cant_iterate_over(receiver.type_name()),
            )
        })?;
        let entry = self
            .user_methods
            .get(&key)
            .and_then(|m| m.get(method))
            .cloned()
            .ok_or_else(|| {
                error(
                    line,
                    format!(
                        "`{}` doesn't have a `.{}()` method — can't iterate",
                        key.1, method
                    ),
                )
            })?;
        validate_user_arity(
            &format!("{}.{}", key.1, method),
            &entry.param_modes,
            extra_args.len() + 1,
            true,
            line,
        )?;
        let mut args = Vec::with_capacity(extra_args.len() + 1);
        args.push(receiver);
        args.extend(extra_args);
        self.enter_user_fn_args(Rc::clone(&entry), args, line)?;
        self.frames.last_mut().expect("callee frame").wrap = wrap;
        self.apply_entry_alias_context(&entry);
        Ok(())
    }

    /// Handle `r.map(f)`, `r.map_err(f)`, `r.and_then(f)` for a
    /// built-in `Result`. `map` / `map_err` push a closure frame
    /// marked with a `FrameWrap` that wraps the closure's return
    /// value in `Ok` / `Err`; the short-circuit branch (map on
    /// Err, map_err on Ok) skips the call and pushes the passed-
    /// through Result directly. `and_then` trusts the closure to
    /// return a Result and pushes the call with no wrap.
    fn call_result_callable_method(
        &mut self,
        obj: Value,
        kind: methods::ResultCallableKind,
        method: &str,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Next, NyblError> {
        use methods::{ResultCallableKind, make_result_err_in, make_result_ok_in};
        if args.len() != 1 {
            return Err(error(
                line,
                format!("`{}` expects 1 argument, but got {}", method, args.len()),
            ));
        }
        let mut args_iter = args.into_iter();
        let callable = args_iter.next().unwrap();

        let (is_ok, payload) = match &obj {
            Value::EnumVariant(e) => {
                let payload = match e.payload() {
                    nybl::value::EnumPayload::Tuple(items) if items.len() == 1 => items[0].clone(),
                    _ => {
                        return Err(error(
                            line,
                            format!("malformed Result::{} payload", e.variant()),
                        ));
                    }
                };
                (e.variant() == "Ok", payload)
            }
            _ => return Err(error(line, "Result method called on non-Result")),
        };
        drop(obj);

        match kind {
            ResultCallableKind::Map => {
                if !is_ok {
                    // Err passes through unchanged — no closure call.
                    let value = make_result_err_in(payload, line, &self.memory)?;
                    if self.memory.__exceeded() {
                        return Err(error_fatal_with_hint(
                            line,
                            "Memory limit exceeded",
                            "Your code is using too much memory. Check for large strings or arrays growing in loops.",
                        ));
                    }
                    self.push_value(value);
                    return Ok(Next::Continue);
                }
                let func = match &callable {
                    Value::Fn(f) => Rc::clone(f),
                    other => {
                        return Err(error(
                            line,
                            format!("`map` expects a function, got {}", other.type_name()),
                        ));
                    }
                };
                drop(callable);
                self.call_closure(&func, vec![payload], line)?;
                if let Some(frame) = self.frames.last_mut() {
                    frame.wrap = FrameWrap::ResultOk { line };
                }
                Ok(Next::Continue)
            }
            ResultCallableKind::MapErr => {
                if is_ok {
                    let value = make_result_ok_in(payload, line, &self.memory)?;
                    if self.memory.__exceeded() {
                        return Err(error_fatal_with_hint(
                            line,
                            "Memory limit exceeded",
                            "Your code is using too much memory. Check for large strings or arrays growing in loops.",
                        ));
                    }
                    self.push_value(value);
                    return Ok(Next::Continue);
                }
                let func = match &callable {
                    Value::Fn(f) => Rc::clone(f),
                    other => {
                        return Err(error(
                            line,
                            format!("`map_err` expects a function, got {}", other.type_name()),
                        ));
                    }
                };
                drop(callable);
                self.call_closure(&func, vec![payload], line)?;
                if let Some(frame) = self.frames.last_mut() {
                    frame.wrap = FrameWrap::ResultErr { line };
                }
                Ok(Next::Continue)
            }
            ResultCallableKind::AndThen => {
                if !is_ok {
                    let value = make_result_err_in(payload, line, &self.memory)?;
                    if self.memory.__exceeded() {
                        return Err(error_fatal_with_hint(
                            line,
                            "Memory limit exceeded",
                            "Your code is using too much memory. Check for large strings or arrays growing in loops.",
                        ));
                    }
                    self.push_value(value);
                    return Ok(Next::Continue);
                }
                let func = match &callable {
                    Value::Fn(f) => Rc::clone(f),
                    other => {
                        return Err(error(
                            line,
                            format!("`and_then` expects a function, got {}", other.type_name()),
                        ));
                    }
                };
                drop(callable);
                // Closure is expected to return a Result — no
                // wrapping. The result flows back via the normal
                // `do_return` path (`FrameWrap::None`).
                self.call_closure(&func, vec![payload], line)
            }
        }
    }

    /// Attach the raising frame's defining module to an escaping
    /// runtime error. The error's line numbers refer to the chunk
    /// of the frame that raised it, so the frame's
    /// `function_module` — the fn's *defining* module — is the
    /// source the error must render against, no matter which file
    /// the call chain started in. Root-declared fns attach the
    /// root sentinel ([`nybl::value::ROOT_MODULE_PATH`]), which
    /// renders as plain root source but keeps an enclosing module
    /// fn (e.g. one invoking a root callback) from claiming the
    /// error later. Deepest context wins: an error that already
    /// carries a context (a nested module VM's load boundary, or
    /// an earlier pass through this helper) is left untouched.
    ///
    /// Top-of-program frames have no `function_module`; their
    /// errors stay uncontexted here and, for a module's top-level
    /// code, pick up their context (with source) at the
    /// `load_module` boundary instead.
    fn attach_frame_module_context(&self, err: NyblError) -> NyblError {
        if err.source_context.is_some() || err.is_try_return {
            return err;
        }
        match self
            .frames
            .last()
            .and_then(|frame| frame.function_module.as_deref())
        {
            Some(module) => err.with_module(module),
            None => err,
        }
    }

    /// Propagate a non-fatal error up through any number of fn
    /// frames until we find a `FrameWrap::TryCall` landing pad.
    /// On success, truncates the frame stack and value stack
    /// back to the wrapper's base, pushes a
    /// `Result::Err(RuntimeError { … })` for the outer caller,
    /// and returns `Ok(())` so the dispatch loop keeps going.
    ///
    /// Returns `Err(err)` (untouched) when:
    /// - the error is fatal (resource-limit violation), or
    /// - no enclosing `try_call` frame exists.
    fn unwind_to_try_call(&mut self, err: NyblError) -> Result<(), NyblError> {
        if err.is_fatal {
            return Err(err);
        }
        let wrap_idx = match self
            .frames
            .iter()
            .rposition(|f| matches!(f.wrap, FrameWrap::TryCall { .. }))
        {
            Some(i) => i,
            None => return Err(err),
        };
        let wrapper_stack_base = self.frames[wrap_idx].stack_base;
        let wrapper_line = match self.frames[wrap_idx].wrap {
            FrameWrap::TryCall { line } => line,
            _ => unreachable!("try_call frame selected by wrapper kind"),
        };
        // Drain the unwound frames through the freelist instead
        // of `truncate`, so their slot vecs get recycled.
        while self.frames.len() > wrap_idx {
            let mut frame = self.frames.pop().expect("frame present");
            self.store_frame_defining_environment(&mut frame);
            self.type_bindings.truncate(frame.caller_type_scope_depth());
            self.module_aliases.truncate(frame.alias_scope_base);
            self.imported_functions.truncate(frame.alias_scope_base);
            self.imported_here.truncate(frame.alias_scope_base);
            if !frame.slots.is_empty() {
                self.return_slots(frame.slots);
            }
        }
        self.restore_active_defining_environment();
        self.stack.truncate(wrapper_stack_base);
        self.push_value(builtins::make_try_call_err_in(&err, &self.memory));
        if self.memory.__exceeded() {
            Err(error_fatal_with_hint(
                wrapper_line,
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            ))
        } else {
            Ok(())
        }
    }
}

fn resolve_type_in_frame(
    frame: &Frame,
    value_scopes: &[BTreeMap<String, Value>],
    type_scopes: &[BTreeMap<String, String>],
    module_aliases: &[BTreeMap<String, Rc<nybl::value::NyblModule>>],
    namespace: Option<&str>,
    type_name: &str,
) -> Option<String> {
    if let Some(namespace) = namespace {
        for scope in value_scopes.iter().rev() {
            if let Some(value) = scope.get(namespace) {
                return match value {
                    Value::Module(module) => module.type_origin(type_name).map(str::to_string),
                    _ => None,
                };
            }
        }
        let floor = frame.is_function.then_some(frame.alias_scope_base);
        for (index, aliases) in module_aliases.iter().enumerate().rev() {
            if floor.is_some_and(|floor| index < floor) {
                continue;
            }
            if let Some(module) = aliases.get(namespace) {
                return module.type_origin(type_name).map(str::to_string);
            }
        }
        return frame
            .lexical_context
            .module_aliases
            .get(namespace)
            .and_then(|module| module.type_origin(type_name).map(str::to_string));
    }

    let floor = frame
        .is_function
        .then_some(frame.type_scope_base.saturating_sub(1));
    for (index, bindings) in type_scopes.iter().enumerate().rev() {
        if floor.is_some_and(|floor| index < floor) {
            continue;
        }
        if let Some(module_path) = bindings.get(type_name) {
            return Some(module_path.clone());
        }
    }
    frame.lexical_context.type_bindings.get(type_name).cloned()
}

fn apply_in_place_assign<F>(
    op: crate::chunk::InPlaceAssignOp,
    rhs: Value,
    line: u32,
    memory: &nybl::memory::MemoryContext,
    current: F,
) -> Result<Value, NyblError>
where
    F: FnOnce() -> Result<Value, NyblError>,
{
    use crate::chunk::InPlaceAssignOp;

    if op == InPlaceAssignOp::Eq {
        return Ok(rhs);
    }
    let left = current()?;
    match op {
        InPlaceAssignOp::Eq => unreachable!(),
        InPlaceAssignOp::Add => ops::add_in(&left, &rhs, line, memory),
        InPlaceAssignOp::Sub => ops::sub_in(&left, &rhs, line, memory),
        InPlaceAssignOp::Mul => ops::mul_in(&left, &rhs, line, memory),
        InPlaceAssignOp::Div => ops::div_in(&left, &rhs, line, memory),
        InPlaceAssignOp::Rem => ops::rem_in(&left, &rhs, line, memory),
    }
}

impl NyblInstance {
    /// Compile and execute a program once, retaining its VM state for calls.
    pub fn load(
        source: &str,
        host: &mut dyn NyblHost,
        limits: &NyblLimits,
    ) -> Result<Self, NyblError> {
        let statements = nybl::parse(source)?;
        let compiled = crate::compiler::compile_program(&statements)?;
        crate::validate_chunk(&compiled.chunk)?;
        let memory = nybl::memory::MemoryContext::__new(limits.max_memory);
        let mut vm = Vm::new_internal(
            compiled.chunk,
            host,
            limits.clone(),
            ModuleRuntime::empty(),
            nybl::value::ROOT_MODULE_PATH.to_string(),
            compiled.root_function_visibility,
            NyblFnOrigin::__instance("vm"),
            memory.clone(),
        );
        let execution = vm.run_internal();
        vm.write_runtime_warnings();
        execution?;
        if memory.__exceeded() {
            return Err(instance_memory_error());
        }
        let entries = vm
            .abi_declarations
            .iter()
            .map(|(name, target)| {
                let required = nybl::ref_params::required_arity(&target.param_modes);
                if target.param_modes.last() == Some(&ParamMode::Rest) {
                    EntryPoint::__new_variadic(name.clone(), required)
                } else {
                    EntryPoint::__new(name.clone(), required)
                }
            })
            .collect();
        Ok(Self {
            state: Some(vm.into_state()),
            entries,
            limits: limits.clone(),
            in_operation: Cell::new(false),
            memory,
        })
    }

    pub fn entry_points(&self) -> &[EntryPoint] {
        &self.entries
    }

    pub fn call(
        &mut self,
        name: &str,
        args: &[Value],
        host: &mut dyn NyblHost,
    ) -> Result<Value, NyblError> {
        let _operation = VmOperationGuard::begin(&self.in_operation)?;
        let state = self.state.as_ref().expect("instance state present");
        let target = state
            .abi_declarations
            .iter()
            .find(|(entry_name, _)| entry_name == name)
            .map(|(_, target)| Rc::clone(target))
            .ok_or_else(|| error(0, format!("Public entry point `{name}` was not found")))?;
        validate_user_arity(name, &target.param_modes, args.len(), false, 0)?;
        nybl::ref_params::validate_call_modes(
            name,
            &target.param_modes,
            &vec![ParamMode::Value; args.len()],
            0,
        )?;
        if self.memory.__exceeded() {
            return Err(instance_memory_error());
        }
        let state = self.state.take().expect("instance state present");
        let mut vm = Vm::from_state(state, host, self.limits.clone(), self.memory.clone());
        let result = {
            for argument in args {
                vm.push_value(argument.clone());
            }
            let execution = vm
                .enter_user_fn(target, args.len(), 0)
                .and_then(|_| vm.run_internal());
            let value = execution.and_then(|_| vm.pop_value(0));
            vm.restore_instance_baseline();
            if self.memory.__exceeded() {
                Err(instance_memory_error())
            } else {
                value
            }
        };
        vm.write_runtime_warnings();
        self.state = Some(vm.into_state());
        result
    }

    pub fn call_value(
        &mut self,
        callable: &Value,
        args: &[Value],
        host: &mut dyn NyblHost,
    ) -> Result<Value, NyblError> {
        let _operation = VmOperationGuard::begin(&self.in_operation)?;
        let function = match callable {
            Value::Fn(function) => Rc::clone(function),
            other => {
                return Err(error(
                    0,
                    format!("expected function, got {}", other.type_name()),
                ));
            }
        };
        let state = self.state.as_ref().expect("instance state present");
        if !function.__is_allowed_by(&state.function_origin, "vm") {
            return Err(error(
                0,
                "This function belongs to a different Nybl engine instance",
            ));
        }
        let display_name = function.self_name.as_deref().unwrap_or("fn");
        validate_user_arity(display_name, &function.param_modes, args.len(), false, 0)?;
        nybl::ref_params::validate_call_modes(
            display_name,
            &function.param_modes,
            &vec![ParamMode::Value; args.len()],
            0,
        )?;
        if self.memory.__exceeded() {
            return Err(instance_memory_error());
        }
        let state = self.state.take().expect("instance state present");
        let mut vm = Vm::from_state(state, host, self.limits.clone(), self.memory.clone());
        let result = {
            let execution = vm
                .call_closure(&function, args.to_vec(), 0)
                .and_then(|_| vm.run_internal());
            let value = execution.and_then(|_| vm.pop_value(0));
            vm.restore_instance_baseline();
            if self.memory.__exceeded() {
                Err(instance_memory_error())
            } else {
                value
            }
        };
        vm.write_runtime_warnings();
        self.state = Some(vm.into_state());
        result
    }
}

fn instance_memory_error() -> NyblError {
    NyblError::fatal("Memory limit exceeded", 0)
}

fn call_mode_error(
    line: u32,
    callable: &str,
    zero_based_position: usize,
    expected: ParamMode,
) -> NyblError {
    let position = zero_based_position + 1;
    match expected {
        ParamMode::Ref => error_with_hint(
            line,
            format!("argument {position} to `{callable}` must be passed with `ref`"),
            format!("Write `ref` before argument {position}."),
        ),
        ParamMode::Value => error_with_hint(
            line,
            format!("argument {position} to `{callable}` is a value parameter and can't use `ref`"),
            format!("Remove `ref` from argument {position}."),
        ),
        ParamMode::Rest => error_with_hint(
            line,
            format!("argument {position} to `{callable}` is a value parameter and can't use `ref`"),
            format!("Remove `ref` from argument {position}."),
        ),
    }
}

fn validate_user_arity(
    callable: &str,
    expected: &[ParamMode],
    actual: usize,
    including_self: bool,
    line: u32,
) -> Result<(), NyblError> {
    let has_rest = expected.last() == Some(&ParamMode::Rest);
    let minimum = expected.len().saturating_sub(usize::from(has_rest));
    if (has_rest && actual >= minimum) || (!has_rest && actual == minimum) {
        return Ok(());
    }
    let self_suffix = if including_self {
        " (including `self`)"
    } else {
        ""
    };
    let expectation = if has_rest {
        format!(
            "at least {minimum} argument{}",
            if minimum == 1 { "" } else { "s" }
        )
    } else {
        format!("{minimum} argument{}", if minimum == 1 { "" } else { "s" })
    };
    Err(error(
        line,
        format!("`{callable}` expects {expectation}{self_suffix}, but got {actual}"),
    ))
}

fn validate_user_call_modes(
    callable: &str,
    expected: &[ParamMode],
    actual: &[ParamMode],
    line: u32,
) -> Result<(), NyblError> {
    validate_user_arity(callable, expected, actual.len(), false, line)?;
    let fixed = expected
        .len()
        .saturating_sub(usize::from(expected.last() == Some(&ParamMode::Rest)));
    for (index, mode) in actual.iter().enumerate() {
        let expected_mode = expected.get(index).copied().unwrap_or(ParamMode::Value);
        let expected_mode = if index >= fixed || expected_mode == ParamMode::Rest {
            ParamMode::Value
        } else {
            expected_mode
        };
        if *mode != expected_mode {
            return Err(call_mode_error(line, callable, index, expected_mode));
        }
    }
    Ok(())
}

fn invalid_ref_target_error(line: u32, zero_based_position: usize) -> NyblError {
    error_with_hint(
        line,
        format!(
            "`ref` argument {} must name a mutable variable",
            zero_based_position + 1
        ),
        "Assign the value to a `let` variable, then pass that variable with `ref`.",
    )
}

fn duplicate_ref_target_error(line: u32) -> NyblError {
    error_with_hint(
        line,
        "the same variable can't be passed to more than one `ref` parameter",
        "Use a distinct variable for each `ref` argument.",
    )
}

fn validate_named_builtin_arity(name: &str, count: usize, line: u32) -> Result<(), NyblError> {
    let expected = match name {
        "rand" | "try_call" | "panic" => Some((1, 1)),
        "range" => Some((1, 3)),
        "print" => None,
        _ => return Ok(()),
    };
    if let Some((min, max)) = expected
        && (count < min || count > max)
    {
        let expectation = if min == max {
            format!("{} argument{}", min, if min == 1 { "" } else { "s" })
        } else {
            format!("{min} to {max} arguments")
        };
        return Err(error(
            line,
            format!("`{name}` expects {expectation}, but got {count}"),
        ));
    }
    Ok(())
}

fn validate_builtin_method_arity(method: &str, count: usize, line: u32) -> Result<(), NyblError> {
    let Some(expected) = methods::builtin_method_arity(method) else {
        return Ok(());
    };
    if count != expected {
        return Err(error(
            line,
            format!(
                "`.{method}()` needs {expected} argument{}",
                if expected == 1 { "" } else { "s" }
            ),
        ));
    }
    Ok(())
}

struct VmOperationGuard<'a>(&'a Cell<bool>);

impl<'a> VmOperationGuard<'a> {
    fn begin(flag: &'a Cell<bool>) -> Result<Self, NyblError> {
        if flag.replace(true) {
            return Err(error(0, "A Nybl instance cannot be re-entered"));
        }
        Ok(Self(flag))
    }
}

impl Drop for VmOperationGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

// ─── Public entry points ──────────────────────────────────────────

/// Execute a pre-compiled [`Chunk`] against the supplied host.
///
/// The chunk is structurally validated before the VM starts, so malformed
/// hand-built or deserialized bytecode is reported as a [`NyblError`] rather
/// than panicking or silently jumping beyond the instruction stream.
pub fn execute<H: NyblHost>(
    chunk: Chunk,
    host: &mut H,
    limits: &NyblLimits,
) -> Result<(), NyblError> {
    crate::validate_chunk(&chunk)?;
    let vm = Vm::new(chunk, host, limits.clone());
    vm.run()
}

/// Parse, compile, and run Nybl source.
///
/// This mirrors [`nybl::run`] but routes through the bytecode VM.
pub fn run<H: NyblHost>(source: &str, host: &mut H, limits: &NyblLimits) -> Result<(), NyblError> {
    let stmts = nybl::parse(source)?;
    let chunk = crate::compile(&stmts)?;
    execute(chunk, host, limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct SilentHost;

    impl NyblHost for SilentHost {
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
    fn named_call_uses_host_once_then_exact_self_recursion_bypasses_it() {
        struct Host {
            calls: usize,
            prints: Vec<String>,
        }

        impl NyblHost for Host {
            fn call(
                &mut self,
                name: &str,
                _args: &[Value],
                _line: u32,
            ) -> Option<Result<Value, NyblError>> {
                if name != "f" {
                    return None;
                }
                self.calls += 1;
                (self.calls > 1).then_some(Ok(Value::Int(99)))
            }

            fn on_print(&mut self, message: &str) {
                self.prints.push(message.to_string());
            }
        }

        let mut host = Host {
            calls: 0,
            prints: Vec::new(),
        };
        run(
            "fn f(n) { if n == 0 { return 1 } return f(n - 1) }\nprint(f(2))",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.calls, 1);
        assert_eq!(host.prints, ["1"]);
    }

    #[test]
    fn method_bare_name_resolves_named_function_not_method_self() {
        struct PrintHost(Vec<String>);

        impl NyblHost for PrintHost {
            fn call(
                &mut self,
                _name: &str,
                _args: &[Value],
                _line: u32,
            ) -> Option<Result<Value, NyblError>> {
                None
            }

            fn on_print(&mut self, message: &str) {
                self.0.push(message.to_string());
            }
        }

        let mut host = PrintHost(Vec::new());
        run(
            "struct S {}\nfn m() { return 7 }\nfn S.m(self) { return m() }\nlet value = S {}\nprint(value.m())",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.0, ["7"]);
    }

    #[test]
    fn sequential_broken_loops_leave_the_value_stack_balanced() {
        let mut source = String::new();
        for _ in 0..128 {
            source.push_str("for item in [1, 2, 3] { break }\n");
            source.push_str("repeat 3 { break }\n");
        }
        let program = nybl::parse(&source).expect("parse");
        let chunk = crate::compile(&program).expect("compile");
        let mut host = SilentHost;
        let mut vm = Vm::new(chunk, &mut host, NyblLimits::standard());

        vm.run_internal().expect("execute");

        assert!(
            vm.stack.is_empty(),
            "clean completion left {} loop sidecars on the value stack",
            vm.stack.len()
        );
    }

    #[test]
    fn loop_control_leaves_value_and_type_scope_stacks_balanced() {
        let source = r#"repeat 2048 {
    if true { continue }
}
for outer in [1, 2] {
    repeat 3 {
        while true {
            if true { break }
        }
        let label = match outer {
            n if n < 0 => "negative",
            _ => "positive",
        }
        if outer == 1 { continue }
        break
    }
}"#;
        let program = nybl::parse(source).expect("parse");
        let chunk = crate::compile(&program).expect("compile");
        let mut host = SilentHost;
        let mut vm = Vm::new(chunk, &mut host, NyblLimits::standard());

        vm.run_internal().expect("execute");

        let frame = vm.frames.last().expect("top frame");
        assert_eq!(frame.scopes.len(), frame.scope_base);
        assert_eq!(vm.type_bindings.len(), frame.type_scope_base);
        assert!(
            vm.stack.is_empty(),
            "clean completion left {} values or loop sidecars",
            vm.stack.len()
        );
    }

    #[test]
    fn frame_scope_floors_preserve_base_scopes_and_pair_type_scopes() {
        let mut host = SilentHost;
        let mut vm = Vm::new(Chunk::new(), &mut host, NyblLimits::standard());

        // A redundant PopScope cannot remove the top-level binding map.
        vm.pop_scope();
        assert_eq!(vm.frames.last().unwrap().scopes.len(), 1);
        assert_eq!(vm.type_bindings.len(), 1);
        vm.push_scope();
        vm.pop_scope();
        assert_eq!(vm.frames.last().unwrap().scopes.len(), 1);
        assert_eq!(vm.type_bindings.len(), 1);

        // Named-function frames start with no value scope, but their first
        // pushed runtime scope is removable. Function exit also discards all
        // still-open type scopes, as an early return requires.
        vm.push_function_frame(
            Rc::new(Chunk::new()),
            Vec::new(),
            Vec::new(),
            0,
            None,
            FrameWrap::None,
        );
        vm.pop_scope();
        assert_eq!(vm.frames.last().unwrap().scopes.len(), 0);
        assert_eq!(vm.type_bindings.len(), 2);
        vm.push_scope();
        vm.pop_scope();
        assert_eq!(vm.frames.last().unwrap().scopes.len(), 0);
        assert_eq!(vm.type_bindings.len(), 2);
        vm.push_scope();
        vm.push_scope();
        vm.do_return(Value::None, 0).expect("return");
        assert_eq!(vm.frames.len(), 1);
        assert_eq!(vm.type_bindings.len(), 1);

        // Closure frames retain their capture map while removing a match
        // scope above it.
        let mut captures = BTreeMap::new();
        captures.insert(String::from("captured"), Value::Int(10));
        vm.push_function_frame(
            Rc::new(Chunk::new()),
            Vec::new(),
            vec![captures],
            vm.stack.len(),
            None,
            FrameWrap::None,
        );
        vm.push_scope();
        vm.pop_scope();
        let frame = vm.frames.last().unwrap();
        assert_eq!(frame.scopes.len(), 1);
        assert!(matches!(
            frame.scopes[0].get("captured"),
            Some(Value::Int(10))
        ));
        assert_eq!(vm.type_bindings.len(), 2);

        // Error unwinding must remove both the try-call frame and a nested
        // function's still-open runtime scopes, restoring the top-level type
        // depth exactly.
        vm.do_return(Value::None, 0).expect("closure return");
        vm.push_function_frame(
            Rc::new(Chunk::new()),
            Vec::new(),
            Vec::new(),
            vm.stack.len(),
            None,
            FrameWrap::TryCall { line: 1 },
        );
        vm.push_scope();
        vm.push_function_frame(
            Rc::new(Chunk::new()),
            Vec::new(),
            Vec::new(),
            vm.stack.len(),
            None,
            FrameWrap::None,
        );
        vm.push_scope();
        vm.unwind_to_try_call(error(1, "boom")).expect("unwind");
        assert_eq!(vm.frames.len(), 1);
        assert_eq!(vm.type_bindings.len(), 1);
    }

    #[test]
    fn named_call_frames_share_one_module_context() {
        let mut host = SilentHost;
        let mut vm = Vm::new(Chunk::new(), &mut host, NyblLimits::standard());
        let root_context = Rc::clone(&vm.root_lexical_context);

        vm.push_function_frame(
            Rc::new(Chunk::new()),
            Vec::new(),
            Vec::new(),
            0,
            Some(nybl::value::ROOT_MODULE_PATH.to_string()),
            FrameWrap::None,
        );

        assert!(Rc::ptr_eq(
            &vm.frames.last().unwrap().lexical_context,
            &root_context
        ));
        assert_eq!(vm.type_bindings.len(), 2);
        assert_eq!(vm.module_aliases.len(), 2);
        assert_eq!(vm.imported_functions.len(), 2);
        assert!(vm.type_bindings[1].is_empty());
        assert!(vm.module_aliases[1].is_empty());
        assert!(vm.imported_functions[1].is_empty());
    }

    #[test]
    fn root_type_publication_reuses_unique_context_storage() {
        const DECLARATIONS: usize = 1_024;
        let mut host = SilentHost;
        let mut vm = Vm::new(Chunk::new(), &mut host, NyblLimits::standard());
        let context = Rc::as_ptr(&vm.root_lexical_context);
        let bindings = Rc::as_ptr(&vm.root_lexical_context.type_bindings);

        for index in 0..DECLARATIONS {
            vm.bind_local_type(&format!("Type{index}"));
        }

        assert_eq!(Rc::as_ptr(&vm.root_lexical_context), context);
        assert_eq!(Rc::as_ptr(&vm.root_lexical_context.type_bindings), bindings);
        assert_eq!(
            vm.root_lexical_context.type_bindings.len(),
            DECLARATIONS + 3
        );
    }

    #[test]
    fn restored_lexical_snapshot_forks_without_leaking_abandoned_changes() {
        let mut host = SilentHost;
        let mut vm = Vm::new(Chunk::new(), &mut host, NyblLimits::standard());
        vm.publish_root_type_binding("A".to_string(), "root".to_string());
        let snapshot = Rc::clone(&vm.root_lexical_context);
        vm.publish_root_type_binding("B".to_string(), "leaked".to_string());

        vm.root_lexical_context = Rc::clone(&snapshot);
        vm.publish_root_type_binding("C".to_string(), "root".to_string());

        assert_eq!(
            vm.root_lexical_context.type_bindings.get("A"),
            Some(&"root".to_string())
        );
        assert!(!vm.root_lexical_context.type_bindings.contains_key("B"));
        assert_eq!(
            vm.root_lexical_context.type_bindings.get("C"),
            Some(&"root".to_string())
        );
    }

    #[test]
    fn repeated_publication_replaces_in_place_without_retaining_history() {
        let mut host = SilentHost;
        let mut vm = Vm::new(Chunk::new(), &mut host, NyblLimits::standard());
        let bindings = Rc::as_ptr(&vm.root_lexical_context.type_bindings);

        for index in 0..1_000 {
            vm.publish_root_type_binding("Repeated".to_string(), index.to_string());
        }

        assert_eq!(Rc::as_ptr(&vm.root_lexical_context.type_bindings), bindings);
        assert_eq!(vm.root_lexical_context.type_bindings.len(), 4);
        assert_eq!(
            vm.root_lexical_context.type_bindings.get("Repeated"),
            Some(&"999".to_string())
        );
    }

    #[test]
    fn capture_snapshot_distinguishes_missing_from_bound_none() {
        let program = nybl::parse(
            r#"let present = none
let read = fn() { return [missing, present] }"#,
        )
        .expect("parse");
        let chunk = crate::compile(&program).expect("compile");
        let lambda = chunk.functions[0].clone();
        let mut host = SilentHost;
        let mut vm = Vm::new(chunk, &mut host, NyblLimits::standard());
        vm.define_local("present".to_string(), Value::None);

        let captures = vm.snapshot_captures_for(&lambda);

        assert_eq!(captures.len(), 1, "missing bindings must not be invented");
        assert_eq!(captures[0].0, "present");
        assert!(matches!(captures[0].1, Value::None));
    }

    struct RetainingHost {
        retained: Option<Value>,
    }

    impl NyblHost for RetainingHost {
        fn call(
            &mut self,
            name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, NyblError>> {
            if name != "retain_large" {
                return None;
            }
            self.retained = Some(Value::new_str("x".repeat(16 * 1024)));
            Some(Ok(Value::None))
        }
    }

    #[test]
    fn instance_leaves_host_allocations_untracked_and_checks_final_returns() {
        let limits = NyblLimits {
            max_steps: 100,
            max_memory: 32,
        };
        let mut host = RetainingHost { retained: None };
        let mut instance = NyblInstance::load(
            "pub fn host_only() { retain_large() }\npub fn too_large() { return \"abcdefghijklmnopqrstuvwxyz0123456789\" }",
            &mut host,
            &limits,
        )
        .unwrap();
        instance.call("host_only", &[], &mut host).unwrap();
        assert!(host.retained.is_some());
        assert_eq!(instance.memory.__used(), 0);
        let error = instance.call("too_large", &[], &mut host).unwrap_err();
        assert!(error.is_fatal);
        assert!(error.message.contains("Memory limit"));
        assert_eq!(instance.memory.__used(), 0);
        instance.call("host_only", &[], &mut host).unwrap();
    }

    struct ExternalValueHost {
        value: Option<Value>,
    }

    impl NyblHost for ExternalValueHost {
        fn call(
            &mut self,
            name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, NyblError>> {
            (name == "take_external").then(|| Ok(self.value.take().unwrap_or(Value::None)))
        }
    }

    #[test]
    fn external_values_are_free_until_detach_and_memory_poison_is_fail_fast() {
        let external = Value::new_array((0..256).map(Value::Int).collect());
        let limits = NyblLimits {
            max_steps: 100,
            max_memory: 64,
        };
        let mut host = ExternalValueHost {
            value: Some(external),
        };
        let mut instance = NyblInstance::load(
            "let stored = none\npub fn keep() { stored = take_external() }\npub fn mutate() { stored.push(256) }\npub fn harmless() { return 1 }",
            &mut host,
            &limits,
        )
        .unwrap();

        instance.call("keep", &[], &mut host).unwrap();
        assert_eq!(instance.memory.__used(), 0);
        let mutation_error = instance.call("mutate", &[], &mut host).unwrap_err();
        assert!(mutation_error.is_fatal);
        assert!(instance.memory.__used() > limits.max_memory);
        let poisoned = instance.call("harmless", &[], &mut host).unwrap_err();
        assert!(poisoned.is_fatal);
        assert!(poisoned.message.contains("Memory limit"));
    }

    #[test]
    fn returned_receipts_release_on_last_drop_and_instances_do_not_cross_charge() {
        let mut host = SilentHost;
        let source = "pub fn make(x) { return [x, x, x, x] }\npub fn harmless() { return none }";
        let limits = NyblLimits::standard();
        let mut first = NyblInstance::load(source, &mut host, &limits).unwrap();
        let mut second = NyblInstance::load(source, &mut host, &limits).unwrap();

        let first_value = first.call("make", &[Value::Int(1)], &mut host).unwrap();
        let first_bytes = first.memory.__used();
        assert!(first_bytes > 0);
        assert_eq!(second.memory.__used(), 0);
        first.call("harmless", &[], &mut host).unwrap();
        assert_eq!(first.memory.__used(), first_bytes);

        let first_clone = first_value.clone();
        drop(first_value);
        assert_eq!(first.memory.__used(), first_bytes);
        let second_value = second.call("make", &[Value::Int(2)], &mut host).unwrap();
        let second_bytes = second.memory.__used();
        assert!(second_bytes > 0);
        assert_eq!(first.memory.__used(), first_bytes);
        drop(first_clone);
        assert_eq!(first.memory.__used(), 0);
        assert_eq!(second.memory.__used(), second_bytes);
        drop(second_value);
        assert_eq!(second.memory.__used(), 0);
    }

    struct HookAllocatingHost {
        retained: RefCell<Vec<Value>>,
    }

    impl HookAllocatingHost {
        fn retain_large(&self) {
            self.retained
                .borrow_mut()
                .push(Value::new_str("x".repeat(16 * 1024)));
        }
    }

    impl NyblHost for HookAllocatingHost {
        fn call(
            &mut self,
            name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, NyblError>> {
            self.retain_large();
            (name == "host_value").then_some(Ok(Value::None))
        }

        fn on_print(&mut self, _message: &str) {
            self.retain_large();
        }

        fn function_hint(&self) -> &str {
            self.retain_large();
            "host hint"
        }

        fn on_tick(&mut self) -> Result<(), NyblError> {
            self.retain_large();
            Ok(())
        }

        fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
            self.retain_large();
            (name == "hook").then(|| Ok(String::new()))
        }
    }

    #[test]
    fn every_instance_host_hook_leaves_accounting_unchanged() {
        let limits = NyblLimits {
            max_steps: 100,
            max_memory: 64,
        };
        let mut host = HookAllocatingHost {
            retained: RefCell::new(Vec::new()),
        };
        let mut instance = NyblInstance::load(
            "use hook\npub fn print_it() { print(\"ok\") }\npub fn host_it() { host_value() }\npub fn hint_it() { missing() }",
            &mut host,
            &limits,
        )
        .unwrap();
        assert_eq!(instance.memory.__used(), 0);
        instance.call("print_it", &[], &mut host).unwrap();
        instance.call("host_it", &[], &mut host).unwrap();
        let error = instance.call("hint_it", &[], &mut host).unwrap_err();
        assert!(!error.is_fatal);
        assert!(
            error
                .friendly_hint
                .as_deref()
                .is_some_and(|hint| hint.contains("host hint"))
        );
        assert_eq!(instance.memory.__used(), 0);
        assert!(host.retained.borrow().len() >= 8);
    }

    #[test]
    fn same_instance_reentry_rejection_precedes_target_and_arity_checks() {
        let mut host = SilentHost;
        let mut instance =
            NyblInstance::load("pub fn entry() {}", &mut host, &NyblLimits::standard()).unwrap();
        instance.in_operation.set(true);
        let error = instance
            .call("missing", &[Value::None], &mut host)
            .unwrap_err();
        instance.in_operation.set(false);
        assert_eq!(error.line, Some(0));
        assert!(error.message.contains("cannot be re-entered"));
    }

    struct MapModuleHost {
        modules: BTreeMap<String, String>,
    }

    impl NyblHost for MapModuleHost {
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
    fn vm_module_compilation_error_retains_transitive_source_context() {
        let root_source = "use outer";
        let inner_source = "let okay = 1\nlet broken =";
        let mut host = MapModuleHost {
            modules: BTreeMap::from([
                ("outer".to_string(), "use inner\nlet outer = 1".to_string()),
                ("inner".to_string(), inner_source.to_string()),
            ]),
        };

        let error = crate::run(root_source, &mut host, &NyblLimits::standard()).unwrap_err();
        let context = error.source_context.as_ref().expect("module context");

        assert_eq!(context.module_path, "inner");
        assert_eq!(context.source.as_deref(), Some(inner_source));
        let rendered = error.render(root_source);
        assert!(rendered.contains("in module `inner` at line 2"));
        assert!(rendered.contains("let broken ="));
        assert!(!rendered.contains("1 | use outer"));
    }

    #[test]
    fn module_compatibility_snapshots_do_not_force_named_cow_detaches() {
        let mut host = MapModuleHost {
            modules: BTreeMap::from([
                (
                    "leaf".to_string(),
                    "let items = [1, 2, 3]\nfn pop() { items.pop() }".to_string(),
                ),
                (
                    "facade".to_string(),
                    "use leaf\nfn pop() { items.pop() }".to_string(),
                ),
            ]),
        };
        let mut instance = NyblInstance::load(
            "use leaf as leaf\nuse facade as facade\npub fn facade_pop() { facade.pop() }\npub fn direct_pop() { leaf.pop() }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        let loaded_bytes = instance.memory.__used();
        assert!(loaded_bytes > 0);

        instance.call("facade_pop", &[], &mut host).unwrap();
        assert_eq!(instance.memory.__used(), loaded_bytes);
        instance.call("direct_pop", &[], &mut host).unwrap();
        assert_eq!(instance.memory.__used(), loaded_bytes);
    }

    #[test]
    fn facade_fanout_reuses_the_origin_compatibility_snapshot() {
        const FACADES: usize = 8;
        let items = (0..256)
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let mut modules = BTreeMap::from([(
            "leaf".to_string(),
            format!("let items = [{items}]\nfn size() {{ return items.len() }}"),
        )]);
        let mut source = String::new();
        for index in 0..FACADES {
            modules.insert(format!("facade{index}"), "use leaf".to_string());
            source.push_str(&format!("use facade{index} as f{index}\n"));
            source.push_str(&format!("let size{index} = f{index}.size()\n"));
        }
        let mut host = MapModuleHost { modules };
        let instance = NyblInstance::load(&source, &mut host, &NyblLimits::standard()).unwrap();

        let imports = instance.state.as_ref().unwrap().imports.borrow();
        let ImportSlot::Loaded(leaf) = imports.get("leaf").unwrap() else {
            panic!("leaf should be loaded")
        };
        let leaf_items = &leaf
            .bindings
            .iter()
            .find(|(name, _)| name == "items")
            .unwrap()
            .1;
        for index in 0..FACADES {
            let ImportSlot::Loaded(facade) = imports.get(&format!("facade{index}")).unwrap() else {
                panic!("facade should be loaded")
            };
            let facade_items = &facade
                .bindings
                .iter()
                .find(|(name, _)| name == "items")
                .unwrap()
                .1;
            assert!(leaf_items.__shares_backing_with(facade_items));
        }
    }

    #[test]
    fn recursive_module_snapshots_do_not_retain_replaced_instance_receipts() {
        let mut host = MapModuleHost {
            modules: BTreeMap::from([(
                "leaf".to_string(),
                "let items = [[\"abcdefghijklmnopqrstuvwxyz0123456789\"]]\nlet captured = \"zyxwvutsrqponmlkjihgfedcba9876543210\"\nlet callback = fn() { return captured }\nfn clear() { items = []; captured = none; callback = none }"
                    .to_string(),
            )]),
        };
        let mut instance = NyblInstance::load(
            "use leaf as leaf\npub fn clear() { leaf.clear() }",
            &mut host,
            &NyblLimits::standard(),
        )
        .unwrap();
        let loaded_bytes = instance.memory.__used();
        assert!(loaded_bytes > 0);

        instance.call("clear", &[], &mut host).unwrap();
        let cleared_bytes = instance.memory.__used();
        assert!(cleared_bytes < loaded_bytes);

        let mut empty_host = MapModuleHost {
            modules: BTreeMap::from([(
                "leaf".to_string(),
                "let items = []\nlet captured = none\nlet callback = none\nfn clear() { items = []; captured = none; callback = none }"
                    .to_string(),
            )]),
        };
        let empty = NyblInstance::load(
            "use leaf as leaf\npub fn clear() { leaf.clear() }",
            &mut empty_host,
            &NyblLimits::standard(),
        )
        .unwrap();
        assert_eq!(cleared_bytes, empty.memory.__used());
    }

    struct WrappingModuleHost {
        modules: BTreeMap<String, String>,
    }

    impl NyblHost for WrappingModuleHost {
        fn call(
            &mut self,
            name: &str,
            args: &[Value],
            line: u32,
        ) -> Option<Result<Value, NyblError>> {
            if name != "wrap" {
                return None;
            }
            Some(
                nybl::value::NyblModule::try_new(
                    "host.wrapper".to_string(),
                    vec![("child".to_string(), args[0].clone())],
                    Vec::new(),
                    line,
                )
                .map(Value::Module),
            )
        }

        fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
            self.modules.get(name).cloned().map(Ok)
        }
    }

    #[test]
    fn module_snapshots_externalize_host_module_binding_graphs() {
        let root = "use leaf as leaf\npub fn clear() { leaf.clear() }";
        let mut host = WrappingModuleHost {
            modules: BTreeMap::from([(
                "leaf".to_string(),
                "let child = [\"abcdefghijklmnopqrstuvwxyz0123456789\"]\nlet wrapped = wrap(child)\nfn clear() { child = none; wrapped = none }"
                    .to_string(),
            )]),
        };
        let mut instance = NyblInstance::load(root, &mut host, &NyblLimits::standard()).unwrap();
        let loaded_bytes = instance.memory.__used();
        assert!(loaded_bytes > 0);
        instance.call("clear", &[], &mut host).unwrap();
        let cleared_bytes = instance.memory.__used();
        assert!(cleared_bytes < loaded_bytes);

        let mut empty_host = WrappingModuleHost {
            modules: BTreeMap::from([(
                "leaf".to_string(),
                "let child = none\nlet wrapped = none\nfn clear() { child = none; wrapped = none }"
                    .to_string(),
            )]),
        };
        let empty = NyblInstance::load(root, &mut empty_host, &NyblLimits::standard()).unwrap();
        assert_eq!(cleared_bytes, empty.memory.__used());
    }
}
