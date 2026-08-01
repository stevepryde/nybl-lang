//! Value type for the Nybl interpreter.
//!
//! Heap-allocating variants use newtypes with private fields.
//! Public constructors create host-owned, untracked values. Runtime engines
//! use the context-aware internal constructors and mutation helpers, which
//! attach allocation receipts to the engine's explicit memory account. The
//! private inner fields prevent callers from bypassing either path.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    boxed::Box,
    collections::BTreeMap,
    format,
    rc::{Rc, Weak},
    string::{String, ToString},
    vec,
    vec::Vec,
};

#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::{
    boxed::Box,
    collections::BTreeMap,
    rc::{Rc, Weak},
};

use core::{any::Any, cell::RefCell};

use crate::error::NyblError;
use crate::memory::{MemoryContext, MemoryReceipt};
use crate::parser::Stmt;

mod dict_index;
use dict_index::DictKeyIndex;

/// Maximum number of recursively owned runtime values.
///
/// This is deliberately an unconditional runtime invariant rather than a
/// configurable [`crate::NyblLimits`] field. `Value`'s `Clone`, `Drop`,
/// `Display`, `Debug`, and equality implementations recurse through owned
/// children, so allowing an embedder (or an unchecked AOT run) to raise the
/// ceiling would re-introduce a native-stack escape from the sandbox.
pub const MAX_VALUE_DEPTH: u16 = 64;

pub const VALUE_DEPTH_ERROR_MESSAGE: &str = "Value nesting limit exceeded (maximum 64 levels)";

fn value_depth_error(line: u32) -> NyblError {
    NyblError::fatal(VALUE_DEPTH_ERROR_MESSAGE, line)
}

fn checked_owner_depth<'a>(
    values: impl IntoIterator<Item = &'a Value>,
    extra_depth: u16,
    line: u32,
) -> Result<u16, NyblError> {
    let child_depth = values
        .into_iter()
        .map(Value::ownership_depth)
        .max()
        .unwrap_or(0);
    let depth = child_depth.saturating_add(extra_depth);
    if depth > MAX_VALUE_DEPTH {
        Err(value_depth_error(line))
    } else {
        Ok(depth)
    }
}

fn trusted<T>(result: Result<T, NyblError>) -> T {
    result.unwrap_or_else(|_| panic!("{VALUE_DEPTH_ERROR_MESSAGE}"))
}

// ─── Tracked newtypes ──────────────────────────────────────────────────────
//
// Private inner fields prevent direct construction from outside this module.

pub struct NyblStr(Rc<NyblStrData>);

struct NyblStrData {
    text: String,
    _receipt: MemoryReceipt,
}

impl core::fmt::Debug for NyblStr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("NyblStr").field(&self.0.text).finish()
    }
}

impl PartialEq for NyblStr {
    fn eq(&self, other: &Self) -> bool {
        self.0.text == other.0.text
    }
}
impl Eq for NyblStr {}
impl PartialOrd for NyblStr {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for NyblStr {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.0.text.cmp(&other.0.text)
    }
}

/// An opaque, host-owned value that Nybl can retain and pass back to a host.
///
/// The payload is deliberately invisible to the language runtime. Cloning a
/// handle shares the same payload, while language equality compares handle
/// identity rather than attempting to inspect the host's Rust value.
#[derive(Clone)]
pub struct HostValue(Rc<HostValueData>);

struct HostValueData {
    type_name: &'static str,
    payload: Box<dyn Any>,
}

impl HostValue {
    /// Wrap an owned, `'static` Rust value for use in Nybl.
    pub fn new<T: 'static>(type_name: &'static str, value: T) -> Self {
        Self(Rc::new(HostValueData {
            type_name,
            payload: Box::new(value),
        }))
    }

    /// The host-defined type name exposed by Nybl's `.type()` method and
    /// runtime diagnostics.
    pub fn type_name(&self) -> &'static str {
        self.0.type_name
    }

    /// Whether the opaque payload has Rust type `T`.
    pub fn is<T: 'static>(&self) -> bool {
        self.0.payload.is::<T>()
    }

    /// Borrow the opaque payload when its Rust type is `T`.
    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.0.payload.downcast_ref::<T>()
    }

    /// Whether two handles refer to the exact same host value.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl core::fmt::Debug for HostValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HostValue")
            .field("type_name", &self.type_name())
            .finish_non_exhaustive()
    }
}

impl core::fmt::Display for HostValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "<host {}>", self.type_name())
    }
}

impl PartialEq for HostValue {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other)
    }
}

impl Eq for HostValue {}

#[derive(Debug, Clone)]
pub struct NyblArray(Rc<ArrayData>);

#[derive(Debug)]
struct ArrayData {
    items: Vec<Value>,
    depth: u16,
    depth_counts: OrderedDepthCounts,
    receipt: MemoryReceipt,
}

/// Exact child-depth frequencies for an insertion-ordered container. Flat
/// values dominate normal programs, so depth zero stays inline; only nested
/// values allocate entries. The sparse vector has at most
/// [`MAX_VALUE_DEPTH`] entries, keeping depth maintenance independent of the
/// container's length without inflating every `Value` by a fixed 64-element
/// table.
#[derive(Debug, Clone)]
struct OrderedDepthCounts {
    flat: usize,
    nested: Vec<(u16, usize)>,
    /// Stable depth recorded when each value enters the container. Re-reading
    /// an iterator's depth can fail conservatively while a host holds its
    /// `RefCell` borrow, so mutations use this parallel cache.
    child_depths: Vec<u16>,
}

impl OrderedDepthCounts {
    fn from_values(values: &[Value], line: u32) -> Result<Self, NyblError> {
        Self::from_depths(values.iter().map(Value::ownership_depth), line)
    }

    fn from_entries(entries: &[(String, Value)], line: u32) -> Result<Self, NyblError> {
        Self::from_depths(
            entries.iter().map(|(_, value)| value.ownership_depth()),
            line,
        )
    }

    fn from_depths(
        depths: impl ExactSizeIterator<Item = u16>,
        line: u32,
    ) -> Result<Self, NyblError> {
        let mut counts = Self {
            flat: 0,
            nested: Vec::new(),
            child_depths: Vec::new(),
        };
        counts
            .child_depths
            .try_reserve_exact(depths.len())
            .map_err(|_| NyblError::fatal("Memory limit exceeded", line))?;
        for depth in depths {
            counts.child_depths.push(depth);
            if depth == 0 {
                counts.flat += 1;
            } else if let Some((_, count)) = counts
                .nested
                .iter_mut()
                .find(|(entry_depth, _)| *entry_depth == depth)
            {
                *count += 1;
            } else {
                counts
                    .nested
                    .try_reserve(1)
                    .map_err(|_| NyblError::fatal("Memory limit exceeded", line))?;
                counts.nested.push((depth, 1));
            }
        }
        Ok(counts)
    }

    fn tracked_bytes(&self) -> usize {
        core::mem::size_of::<Self>()
            + self.nested.capacity() * core::mem::size_of::<(u16, usize)>()
            + self.child_depths.capacity() * core::mem::size_of::<u16>()
    }

    fn ensure_depth(&mut self, depth: u16, line: u32) -> Result<(), NyblError> {
        if depth == 0
            || self
                .nested
                .iter()
                .any(|(entry_depth, _)| *entry_depth == depth)
        {
            return Ok(());
        }
        self.nested
            .try_reserve(1)
            .map_err(|_| NyblError::fatal("Memory limit exceeded", line))?;
        Ok(())
    }

    fn add(&mut self, depth: u16) {
        if depth == 0 {
            self.flat += 1;
        } else if let Some((_, count)) = self
            .nested
            .iter_mut()
            .find(|(entry_depth, _)| *entry_depth == depth)
        {
            *count += 1;
        } else {
            // `ensure_depth` reserves this slot before any fallible mutation.
            self.nested.push((depth, 1));
        }
    }

    fn try_reserve_child(&mut self, line: u32) -> Result<(), NyblError> {
        self.child_depths
            .try_reserve(1)
            .map_err(|_| NyblError::fatal("Memory limit exceeded", line))?;
        Ok(())
    }

    fn remove(&mut self, depth: u16) {
        if depth == 0 {
            self.flat -= 1;
            return;
        }
        let index = self
            .nested
            .iter()
            .position(|(entry_depth, _)| *entry_depth == depth)
            .expect("container depth metadata contains every child");
        if self.nested[index].1 == 1 {
            self.nested.remove(index);
        } else {
            self.nested[index].1 -= 1;
        }
    }

    fn owner_depth(&self) -> u16 {
        self.nested
            .iter()
            .map(|(depth, _)| depth.saturating_add(1))
            .max()
            .unwrap_or(1)
    }

    fn clear(&mut self) {
        self.flat = 0;
        self.nested.clear();
        self.child_depths.clear();
    }
}

#[derive(Debug, Clone)]
pub struct NyblDict(Rc<DictData>);

#[derive(Debug)]
struct DictData {
    entries: Vec<(String, Value)>,
    key_bytes: usize,
    key_index: DictKeyIndex,
    depth: u16,
    depth_counts: OrderedDepthCounts,
    receipt: MemoryReceipt,
}

fn try_copy_dict_key(key: &str, line: u32) -> Result<String, NyblError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(key.len())
        .map_err(|_| NyblError::fatal("Memory limit exceeded", line))?;
    owned.push_str(key);
    Ok(owned)
}

/// A user-defined struct value. Carries the module it was
/// declared in plus the bare type name, so two modules that
/// happen to declare `struct Color { ... }` independently
/// produce distinct values even when they share a name. The
/// module path is `<root>` for the top-level program and
/// `<builtin>` for engine-registered builtins like
/// `RuntimeError`; for user modules it's the dot-joined `use`
/// path (`"std.math"`, `"game.entity"`, …). Fields are stored
/// in declaration order so iteration and `Display` stay stable.
#[derive(Debug, Clone)]
pub struct NyblStruct(Rc<StructData>);

#[derive(Debug)]
struct StructData {
    module_path: String,
    type_name: String,
    fields: Vec<(String, Value)>,
    depth: u16,
    receipt: MemoryReceipt,
}

/// A user-defined enum variant value — the concrete data side of
/// Nybl's sum types. Like [`NyblStruct`], it's identified by the
/// `(module_path, type_name)` pair, plus the selected variant's
/// name and payload. Two enums declared in different modules with
/// the same type name and even the same variants still compare
/// as distinct types.
#[derive(Debug, Clone)]
pub struct NyblEnumVariant(Rc<EnumVariantData>);

#[derive(Debug)]
struct EnumVariantData {
    module_path: String,
    type_name: String,
    variant: String,
    payload: EnumPayload,
    depth: u16,
    receipt: MemoryReceipt,
}

/// Module path used for engine-registered builtins (`Result`,
/// `RuntimeError`). Surfaces wherever a struct / enum value
/// needs to carry its declaring module; the engines all agree
/// on this literal so patterns + equality line up across
/// walker / VM / AOT.
pub const BUILTIN_MODULE_PATH: &str = "<builtin>";

/// Module path used for types declared directly in the program
/// root (not in any imported module). Same literal across every
/// engine.
pub const ROOT_MODULE_PATH: &str = "<root>";

/// Runtime payload attached to a `NyblEnumVariant`. Mirrors the
/// three variant shapes the parser recognises
/// (`VariantKind::{Unit, Tuple, Struct}`).
#[derive(Debug, Clone)]
pub enum EnumPayload {
    Unit,
    Tuple(Vec<Value>),
    Struct(Vec<(String, Value)>),
}

impl ArrayData {
    fn tracked_bytes(&self) -> usize {
        self.items.capacity() * core::mem::size_of::<Value>() + self.depth_counts.tracked_bytes()
    }

    fn clone_in(&self, memory: &MemoryContext) -> Self {
        let mut cloned = Self {
            items: self.items.clone(),
            depth: self.depth,
            depth_counts: self.depth_counts.clone(),
            receipt: MemoryReceipt::new_in(memory, 0),
        };
        cloned.receipt.resize(cloned.tracked_bytes());
        cloned
    }
}

impl Clone for ArrayData {
    fn clone(&self) -> Self {
        self.clone_in(&MemoryContext::__legacy_current())
    }
}

impl DictData {
    fn tracked_bytes(&self) -> usize {
        self.entries.capacity() * core::mem::size_of::<(String, Value)>()
            + self.key_bytes
            + self.key_index.tracked_bytes()
            + self.depth_counts.tracked_bytes()
    }

    fn sync_receipt(&mut self) {
        let bytes = self.tracked_bytes();
        self.receipt.resize(bytes);
    }

    fn clone_in(&self, memory: &MemoryContext) -> Self {
        let entries = self.entries.clone();
        let key_bytes = entries.iter().map(|(key, _)| key.capacity()).sum();
        let mut cloned = Self {
            entries,
            key_bytes,
            key_index: self.key_index.clone(),
            depth: self.depth,
            depth_counts: self.depth_counts.clone(),
            receipt: MemoryReceipt::new_in(memory, 0),
        };
        cloned.receipt.resize(cloned.tracked_bytes());
        cloned
    }
}

impl Clone for DictData {
    fn clone(&self) -> Self {
        self.clone_in(&MemoryContext::__legacy_current())
    }
}

impl StructData {
    fn tracked_bytes(&self) -> usize {
        let key_bytes: usize = self.fields.iter().map(|(key, _)| key.capacity()).sum();
        self.module_path.capacity()
            + self.type_name.capacity()
            + self.fields.capacity() * core::mem::size_of::<(String, Value)>()
            + key_bytes
    }

    fn clone_in(&self, memory: &MemoryContext) -> Self {
        let mut cloned = Self {
            module_path: self.module_path.clone(),
            type_name: self.type_name.clone(),
            fields: self.fields.clone(),
            depth: self.depth,
            receipt: MemoryReceipt::new_in(memory, 0),
        };
        cloned.receipt.resize(cloned.tracked_bytes());
        cloned
    }
}

impl Clone for StructData {
    fn clone(&self) -> Self {
        self.clone_in(&MemoryContext::__legacy_current())
    }
}

impl EnumVariantData {
    fn tracked_bytes(&self) -> usize {
        let base =
            self.module_path.capacity() + self.type_name.capacity() + self.variant.capacity();
        match &self.payload {
            EnumPayload::Unit => base,
            EnumPayload::Tuple(items) => base + items.capacity() * core::mem::size_of::<Value>(),
            EnumPayload::Struct(fields) => {
                let key_bytes: usize = fields.iter().map(|(key, _)| key.capacity()).sum();
                base + fields.capacity() * core::mem::size_of::<(String, Value)>() + key_bytes
            }
        }
    }

    fn clone_in(&self, memory: &MemoryContext) -> Self {
        let mut cloned = Self {
            module_path: self.module_path.clone(),
            type_name: self.type_name.clone(),
            variant: self.variant.clone(),
            payload: self.payload.clone(),
            depth: self.depth,
            receipt: MemoryReceipt::new_in(memory, 0),
        };
        cloned.receipt.resize(cloned.tracked_bytes());
        cloned
    }
}

impl Clone for EnumVariantData {
    fn clone(&self) -> Self {
        self.clone_in(&MemoryContext::__legacy_current())
    }
}

/// A Nybl function value — the runtime representation of a closure
/// or a reified `fn foo(...) { ... }` declaration. Shared by `Rc`
/// so first-class usage (`let g = f; pass(f)`) is cheap.
///
/// The body is engine-opaque: the tree-walker produces an
/// [`FnBody::Ast`] for direct interpretation; the bytecode VM
/// produces an [`FnBody::Compiled`] carrying a pre-compiled body.
/// Each engine only ever dispatches its own variant.
#[doc(hidden)]
#[derive(Clone)]
pub struct NyblFnOrigin(NyblFnOriginKind);

#[derive(Clone)]
enum NyblFnOriginKind {
    /// Constructed through the public compatibility constructors. A matching
    /// engine may execute it against the current operation.
    External,
    /// Created by one concrete loaded engine instance.
    Instance {
        engine: &'static str,
        identity: Rc<()>,
    },
}

impl NyblFnOrigin {
    #[doc(hidden)]
    pub fn __instance(engine: &'static str) -> Self {
        Self(NyblFnOriginKind::Instance {
            engine,
            identity: Rc::new(()),
        })
    }

    fn external() -> Self {
        Self(NyblFnOriginKind::External)
    }

    fn allows(&self, function: &Self, engine: &'static str) -> bool {
        match &function.0 {
            NyblFnOriginKind::External => true,
            NyblFnOriginKind::Instance {
                engine: function_engine,
                identity: function_identity,
            } => match &self.0 {
                NyblFnOriginKind::Instance {
                    engine: instance_engine,
                    identity: instance_identity,
                } => {
                    *function_engine == engine
                        && *instance_engine == engine
                        && Rc::ptr_eq(instance_identity, function_identity)
                }
                NyblFnOriginKind::External => false,
            },
        }
    }
}

impl core::fmt::Debug for NyblFnOrigin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.0 {
            NyblFnOriginKind::External => f.write_str("External"),
            NyblFnOriginKind::Instance { engine, .. } => f
                .debug_struct("Instance")
                .field("engine", engine)
                .field("identity", &"<opaque>")
                .finish(),
        }
    }
}

pub struct NyblFn {
    pub params: Vec<String>,
    /// Positional passing modes retained by the callable value so aliases,
    /// closures, and module exports perform the same call-site preflight as
    /// direct calls.
    pub param_modes: Vec<crate::parser::ParamMode>,
    /// Values captured from the enclosing scope at construction
    /// time, cloned by value. Free variables in the body that
    /// aren't parameters and aren't in this list fall through to
    /// the outer module / global lookup at call time.
    pub captures: Vec<(String, Value)>,
    pub body: FnBody,
    /// `Some(name)` when this `NyblFn` is bound to its own name
    /// for self-reference (the lowering of `fn foo(...) { ... }`).
    /// Lambdas created from an `fn(...) { ... }` expression leave
    /// this `None`.
    pub self_name: Option<String>,
    /// Module scope that owns this function body. Module-exported functions
    /// retain this so sibling bare calls resolve inside their defining module
    /// without publishing those siblings into an alias importer's scope.
    pub module_path: Option<String>,
    origin: NyblFnOrigin,
    /// Maximum recursive ownership depth, including captures and any
    /// engine-owned values hidden inside an opaque compiled body.
    depth: u16,
}

/// Engine-specific representation of a function body.
///
/// - The tree-walker creates `Ast` bodies and re-walks the AST on
///   every call.
/// - The bytecode VM creates `Compiled` bodies carrying a
///   pre-compiled form (typically `Rc<nybl_vm::Chunk>`). `Rc<dyn
///   Any>` keeps `nybl-lang` from taking a dep on any particular
///   engine crate.
///
/// An engine that only understands one variant errors cleanly
/// when handed the other, rather than silently misbehaving.
pub enum FnBody {
    Ast(Vec<Stmt>),
    Compiled(Rc<dyn core::any::Any + 'static>),
}

impl core::fmt::Debug for NyblFn {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NyblFn")
            .field("params", &self.params)
            .field("param_modes", &self.param_modes)
            .field("captures", &self.captures.len())
            .field("body", &self.body)
            .field("self_name", &self.self_name)
            .field("module_path", &self.module_path)
            .field("origin", &self.origin)
            .finish()
    }
}

impl core::fmt::Debug for FnBody {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FnBody::Ast(stmts) => write!(f, "Ast({} stmts)", stmts.len()),
            FnBody::Compiled(_) => write!(f, "Compiled(<opaque>)"),
        }
    }
}

impl NyblFn {
    #[doc(hidden)]
    pub fn __is_allowed_by(&self, instance: &NyblFnOrigin, engine: &'static str) -> bool {
        instance.allows(&self.origin, engine)
    }

    /// Build an AST-backed function object for engine-internal dispatch paths
    /// that need the `Rc<NyblFn>` directly rather than a wrapping [`Value`].
    pub fn try_new_ast(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Vec<Stmt>,
        self_name: Option<String>,
        line: u32,
    ) -> Result<Rc<Self>, NyblError> {
        Self::try_new_ast_in_module(params, captures, body, self_name, None, line)
    }

    pub fn try_new_ast_in_module(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Vec<Stmt>,
        self_name: Option<String>,
        module_path: Option<String>,
        line: u32,
    ) -> Result<Rc<Self>, NyblError> {
        Self::try_new_ast_in_module_with_origin(
            params,
            captures,
            body,
            self_name,
            module_path,
            NyblFnOrigin::external(),
            line,
        )
    }

    pub fn try_new_ast_in_module_with_modes(
        params: Vec<String>,
        param_modes: Vec<crate::parser::ParamMode>,
        captures: Vec<(String, Value)>,
        body: Vec<Stmt>,
        self_name: Option<String>,
        module_path: Option<String>,
        line: u32,
    ) -> Result<Rc<Self>, NyblError> {
        Self::try_new_ast_in_module_with_origin_and_modes(
            params,
            param_modes,
            captures,
            body,
            self_name,
            module_path,
            NyblFnOrigin::external(),
            line,
        )
    }

    #[doc(hidden)]
    pub fn try_new_ast_in_module_with_origin(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Vec<Stmt>,
        self_name: Option<String>,
        module_path: Option<String>,
        origin: NyblFnOrigin,
        line: u32,
    ) -> Result<Rc<Self>, NyblError> {
        let param_modes = vec![crate::parser::ParamMode::Value; params.len()];
        Self::try_new_ast_in_module_with_origin_and_modes(
            params,
            param_modes,
            captures,
            body,
            self_name,
            module_path,
            origin,
            line,
        )
    }

    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn try_new_ast_in_module_with_origin_and_modes(
        params: Vec<String>,
        param_modes: Vec<crate::parser::ParamMode>,
        captures: Vec<(String, Value)>,
        body: Vec<Stmt>,
        self_name: Option<String>,
        module_path: Option<String>,
        origin: NyblFnOrigin,
        line: u32,
    ) -> Result<Rc<Self>, NyblError> {
        if params.len() != param_modes.len() {
            return Err(NyblError::runtime(
                "Function parameter mode metadata is invalid",
                line,
            ));
        }
        crate::ref_params::validate_parameter_modes(&param_modes, line)?;
        let depth = checked_owner_depth(captures.iter().map(|(_, value)| value), 1, line)?;
        let body = FnBody::Ast(body);
        Ok(Rc::new(Self {
            params,
            param_modes,
            captures,
            body,
            self_name,
            module_path,
            origin,
            depth,
        }))
    }

    pub fn try_new_compiled(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Rc<dyn core::any::Any + 'static>,
        self_name: Option<String>,
        opaque_body_depth: u16,
        line: u32,
    ) -> Result<Rc<Self>, NyblError> {
        Self::try_new_compiled_in_module(
            params,
            captures,
            body,
            self_name,
            None,
            opaque_body_depth,
            line,
        )
    }

    pub fn try_new_compiled_in_module(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Rc<dyn core::any::Any + 'static>,
        self_name: Option<String>,
        module_path: Option<String>,
        opaque_body_depth: u16,
        line: u32,
    ) -> Result<Rc<Self>, NyblError> {
        Self::try_new_compiled_in_module_with_origin(
            params,
            captures,
            body,
            self_name,
            module_path,
            NyblFnOrigin::external(),
            opaque_body_depth,
            line,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn try_new_compiled_in_module_with_modes(
        params: Vec<String>,
        param_modes: Vec<crate::parser::ParamMode>,
        captures: Vec<(String, Value)>,
        body: Rc<dyn core::any::Any + 'static>,
        self_name: Option<String>,
        module_path: Option<String>,
        opaque_body_depth: u16,
        line: u32,
    ) -> Result<Rc<Self>, NyblError> {
        Self::try_new_compiled_in_module_with_origin_and_modes(
            params,
            param_modes,
            captures,
            body,
            self_name,
            module_path,
            NyblFnOrigin::external(),
            opaque_body_depth,
            line,
        )
    }

    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn try_new_compiled_in_module_with_origin(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Rc<dyn core::any::Any + 'static>,
        self_name: Option<String>,
        module_path: Option<String>,
        origin: NyblFnOrigin,
        opaque_body_depth: u16,
        line: u32,
    ) -> Result<Rc<Self>, NyblError> {
        let param_modes = vec![crate::parser::ParamMode::Value; params.len()];
        Self::try_new_compiled_in_module_with_origin_and_modes(
            params,
            param_modes,
            captures,
            body,
            self_name,
            module_path,
            origin,
            opaque_body_depth,
            line,
        )
    }

    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn try_new_compiled_in_module_with_origin_and_modes(
        params: Vec<String>,
        param_modes: Vec<crate::parser::ParamMode>,
        captures: Vec<(String, Value)>,
        body: Rc<dyn core::any::Any + 'static>,
        self_name: Option<String>,
        module_path: Option<String>,
        origin: NyblFnOrigin,
        opaque_body_depth: u16,
        line: u32,
    ) -> Result<Rc<Self>, NyblError> {
        if params.len() != param_modes.len() {
            return Err(NyblError::runtime(
                "Function parameter mode metadata is invalid",
                line,
            ));
        }
        crate::ref_params::validate_parameter_modes(&param_modes, line)?;
        let capture_depth = captures
            .iter()
            .map(|(_, value)| value.ownership_depth())
            .max()
            .unwrap_or(0);
        let depth = capture_depth.max(opaque_body_depth).saturating_add(1);
        if depth > MAX_VALUE_DEPTH {
            return Err(value_depth_error(line));
        }
        let body = FnBody::Compiled(body);
        Ok(Rc::new(Self {
            params,
            param_modes,
            captures,
            body,
            self_name,
            module_path,
            origin,
            depth,
        }))
    }
}

// ─── Value enum ────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Value {
    /// 64-bit signed integer. The go-to type for counts,
    /// indices, and any arithmetic that wants exactness. Added
    /// in phase 6; produced by integer literals (`42`), the
    /// `int()` builtin, `len`, `range` elements, and the new
    /// `//` integer-division operator.
    Int(i64),
    /// 64-bit IEEE-754 float. Produced by decimal literals
    /// (`3.14`, `4.0`), the `float()` builtin, and by `/` on
    /// any numeric pair (Python-style: `/` always floats).
    Number(f64),
    Str(NyblStr),
    Bool(bool),
    None,
    /// Opaque host-owned capability or resource handle. Nybl never traverses
    /// or accounts the Rust payload; method calls are delegated to the active
    /// [`crate::NyblHost`].
    Host(HostValue),
    Array(NyblArray),
    Dict(NyblDict),
    Fn(Rc<NyblFn>),
    // Composite user values are small CoW handles. Their backing data stays
    // behind `Rc`, keeping `Value` compact while assignment, argument passing,
    // capture, and return only bump a reference count.
    Struct(NyblStruct),
    EnumVariant(NyblEnumVariant),
    /// Namespace value produced by an aliased `use` statement
    /// (`use std.math as m` binds `m` as a `Module`). Field
    /// access dispatches to the module's exported `let` / `fn`
    /// bindings; the runtime also consults the type list for
    /// `m.Type { ... }` / `m.Type::Variant(...)` forms so those
    /// namespaced constructors find the right declared type.
    Module(Rc<NyblModule>),
    /// Lazy iterator. Cloning shares state (like `Value::Fn`) —
    /// `let b = a; a.next(); b.next()` advances the same
    /// underlying position, matching iterator semantics in
    /// Python / Rust / JS. See [`NyblIter`] for the built-in
    /// variants; user-defined iterators are ordinary struct
    /// values that happen to implement `.next()`.
    Iter(Rc<RefCell<NyblIter>>),
}

/// Built-in lazy iterator shapes. Each one holds a snapshot of
/// the source sequence plus a cursor; advancing via [`Self::next`]
/// yields items until exhausted. A user-defined iterator doesn't
/// need to live here — it's just a struct with a `.next()`
/// method, dispatched through the ordinary method path.
#[derive(Debug)]
pub struct NyblIter {
    kind: NyblIterKind,
    depth: u16,
    _receipt: MemoryReceipt,
}

#[derive(Debug)]
enum NyblIterKind {
    /// Over a cloned-off array snapshot. Subsequent mutation of
    /// the original array doesn't affect the iterator — matches
    /// how most scripting languages present iteration.
    Array { items: Vec<Value>, pos: usize },
    /// Over a string's Unicode code points, one item per code
    /// point. Each yielded value is a single-char string.
    String { chars: Vec<char>, pos: usize },
    /// Over a dict's keys, in declaration order. Same shape
    /// `for k in dict` uses when the receiver is a plain dict.
    Dict { keys: Vec<String>, pos: usize },
}

impl NyblIter {
    /// Advance by one and return the next item, or `None` when
    /// the iterator is exhausted. Caller wraps the result in
    /// `Iter::Next(v)` / `Iter::Done` for user code.
    // This predates the Iterator impl below and remains an inherent method for
    // source compatibility; the trait method delegates here explicitly.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Value> {
        self.__next_in(&MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __next_in(&mut self, memory: &MemoryContext) -> Option<Value> {
        match &mut self.kind {
            NyblIterKind::Array { items, pos } => {
                if *pos < items.len() {
                    let v = items[*pos].clone();
                    *pos += 1;
                    Some(v)
                } else {
                    None
                }
            }
            NyblIterKind::String { chars, pos } => {
                if *pos < chars.len() {
                    let v = Value::__new_str_in(chars[*pos].to_string(), memory);
                    *pos += 1;
                    Some(v)
                } else {
                    None
                }
            }
            NyblIterKind::Dict { keys, pos } => {
                if *pos < keys.len() {
                    let v = Value::__new_str_in(keys[*pos].clone(), memory);
                    *pos += 1;
                    Some(v)
                } else {
                    None
                }
            }
        }
    }
}

impl Iterator for NyblIter {
    type Item = Value;

    fn next(&mut self) -> Option<Self::Item> {
        NyblIter::next(self)
    }
}

/// Public type surface of a module. Each exposed name retains the module that
/// originally declared it, so facade re-exports do not rewrite type identity.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NyblTypeExports(BTreeMap<String, String>);

impl NyblTypeExports {
    pub fn from_names(module_path: &str, names: Vec<String>) -> Self {
        Self(
            names
                .into_iter()
                .map(|name| (name, module_path.to_string()))
                .collect(),
        )
    }

    pub fn from_origins(origins: impl IntoIterator<Item = (String, String)>) -> Self {
        // A map makes duplicate exposed names unrepresentable. Engine export
        // projection establishes first-win/local-overwrite precedence before
        // construction; if a general caller supplies duplicates, the final
        // pair is authoritative just like `BTreeMap::collect`.
        Self(origins.into_iter().collect())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    pub fn origin(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, origin)| (name.as_str(), origin.as_str()))
    }
}

/// Exported surface of a module, as presented through an aliased
/// `use` statement. `Rc<NyblModule>` is what a `Value::Module`
/// carries so cloning the Value stays cheap.
type WeakLiveValueEnvironments = Weak<RefCell<BTreeMap<String, BTreeMap<String, Value>>>>;

#[derive(Debug)]
pub struct NyblModule {
    /// The dotted path the module was loaded from ("std.math",
    /// "game.entity", …). Useful for error messages.
    pub path: String,
    /// Exported `let` / `fn` / `const` bindings, in declaration
    /// order. Accessed via `m.name` field reads.
    pub bindings: Vec<(String, Value)>,
    /// Exported type names retained for source compatibility with embedders.
    /// Origin-sensitive engine logic uses [`Self::type_origin`].
    pub types: Vec<String>,
    /// Exposed struct / enum names and the module that declared each type.
    type_exports: NyblTypeExports,
    /// Instance-owned authoritative environments. `bindings` remains the
    /// public compatibility snapshot; engines read through `__binding`.
    live_environments: Option<WeakLiveValueEnvironments>,
    live_bindings: BTreeMap<String, (String, String)>,
    depth: u16,
}

impl NyblModule {
    /// Construct a shared module object for engines that also retain it in an
    /// alias table outside the wrapping [`Value::Module`].
    pub fn try_new(
        path: String,
        bindings: Vec<(String, Value)>,
        types: Vec<String>,
        line: u32,
    ) -> Result<Rc<Self>, NyblError> {
        let type_exports = NyblTypeExports::from_names(&path, types.clone());
        Self::try_new_parts(path, bindings, types, type_exports, line)
    }

    pub fn try_new_with_type_exports(
        path: String,
        bindings: Vec<(String, Value)>,
        type_exports: NyblTypeExports,
        line: u32,
    ) -> Result<Rc<Self>, NyblError> {
        let types: Vec<String> = type_exports.names().map(str::to_string).collect();
        Self::try_new_parts(path, bindings, types, type_exports, line)
    }

    #[doc(hidden)]
    pub fn __try_new_live_with_type_exports(
        path: String,
        bindings: Vec<(String, Value)>,
        type_exports: NyblTypeExports,
        live_bindings: BTreeMap<String, (String, String)>,
        live_environments: Rc<RefCell<BTreeMap<String, BTreeMap<String, Value>>>>,
        line: u32,
    ) -> Result<Rc<Self>, NyblError> {
        let types = type_exports.names().map(str::to_string).collect();
        let depth = checked_owner_depth(bindings.iter().map(|(_, value)| value), 1, line)?;
        Ok(Rc::new(Self {
            path,
            bindings,
            types,
            type_exports,
            live_environments: Some(Rc::downgrade(&live_environments)),
            live_bindings,
            depth,
        }))
    }

    fn try_new_parts(
        path: String,
        bindings: Vec<(String, Value)>,
        types: Vec<String>,
        type_exports: NyblTypeExports,
        line: u32,
    ) -> Result<Rc<Self>, NyblError> {
        let depth = checked_owner_depth(bindings.iter().map(|(_, value)| value), 1, line)?;
        let live_bindings = BTreeMap::new();
        Ok(Rc::new(Self {
            path,
            bindings,
            types,
            type_exports,
            live_environments: None,
            live_bindings,
            depth,
        }))
    }

    pub fn has_type(&self, name: &str) -> bool {
        self.type_exports.contains(name)
    }

    pub fn type_origin(&self, name: &str) -> Option<&str> {
        self.type_exports.origin(name)
    }

    pub fn type_names(&self) -> impl Iterator<Item = &str> {
        self.type_exports.names()
    }

    #[doc(hidden)]
    pub fn __binding(&self, name: &str) -> Option<Value> {
        // A live environment contains the module's private implementation
        // bindings too. Membership in the immutable exported snapshot (or in
        // its explicit live-origin map) is therefore a capability check, not
        // merely a cache lookup.
        if !self.live_bindings.contains_key(name)
            && !self.bindings.iter().any(|(binding, _)| binding == name)
        {
            return None;
        }
        self.live_environments
            .as_ref()
            .and_then(Weak::upgrade)
            .and_then(|environments| {
                let (module_path, binding_name) = self
                    .live_bindings
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| (self.path.clone(), name.to_string()));
                environments
                    .borrow()
                    .get(&module_path)
                    .and_then(|bindings| bindings.get(&binding_name))
                    .cloned()
            })
            .or_else(|| {
                self.bindings
                    .iter()
                    .find(|(binding, _)| binding == name)
                    .map(|(_, value)| value.clone())
            })
    }

    #[doc(hidden)]
    pub fn __binding_origin(&self, name: &str) -> (String, String) {
        self.live_bindings
            .get(name)
            .cloned()
            .unwrap_or_else(|| (self.path.clone(), name.to_string()))
    }
}

// ─── Constructors ──────────────────────────────────────────────────────────
//
// Public constructors are host-facing compatibility APIs and are untracked
// unless a std-only legacy account was explicitly installed. Engines call the
// `*_in` variants with their own MemoryContext. Constructors attach receipts;
// engine checkpoints and high-risk operation preflights enforce the limit.

impl Value {
    /// Recursive ownership depth used to keep all `Value` trait operations
    /// within a known-safe native stack bound.
    pub fn ownership_depth(&self) -> u16 {
        match self {
            Value::Array(value) => value.0.depth,
            Value::Dict(value) => value.0.depth,
            Value::Fn(value) => value.depth,
            Value::Struct(value) => value.0.depth,
            Value::EnumVariant(value) => value.0.depth,
            Value::Module(value) => value.depth,
            // A host may ask for the depth while it holds a mutable iterator
            // borrow. Treat that conservatively as already-at-the-limit
            // rather than panicking through `RefCell::borrow`.
            Value::Iter(value) => value
                .try_borrow()
                .map(|iter| iter.depth)
                .unwrap_or(MAX_VALUE_DEPTH),
            Value::Int(_)
            | Value::Number(_)
            | Value::Str(_)
            | Value::Bool(_)
            | Value::None
            | Value::Host(_) => 0,
        }
    }

    /// Wrap an owned Rust value as an opaque host handle.
    pub fn new_host<T: 'static>(type_name: &'static str, value: T) -> Self {
        Value::Host(HostValue::new(type_name, value))
    }

    pub fn new_str(s: String) -> Self {
        Self::__new_str_in(s, &MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __new_str_in(s: String, memory: &MemoryContext) -> Self {
        let bytes = s.capacity();
        Value::Str(NyblStr(Rc::new(NyblStrData {
            text: s,
            _receipt: MemoryReceipt::new_in(memory, bytes),
        })))
    }

    /// Trusted compatibility constructor. Runtime engines must use
    /// [`Self::try_new_array`] so a source line can be attached to a clean
    /// fatal diagnostic.
    ///
    /// # Panics
    ///
    /// Panics with `"Value nesting limit exceeded (maximum 64 levels)"` if
    /// `items` would make the returned value's [`Self::ownership_depth`]
    /// exceed [`MAX_VALUE_DEPTH`]. Use [`Self::try_new_array`] to handle that
    /// condition without panicking.
    pub fn new_array(items: Vec<Value>) -> Self {
        trusted(Self::try_new_array(items, 0))
    }

    pub fn try_new_array(items: Vec<Value>, line: u32) -> Result<Self, NyblError> {
        Self::__try_new_array_in(items, line, &MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __try_new_array_in(
        items: Vec<Value>,
        line: u32,
        memory: &MemoryContext,
    ) -> Result<Self, NyblError> {
        let depth = checked_owner_depth(&items, 1, line)?;
        let depth_counts = OrderedDepthCounts::from_values(&items, line)?;
        let mut data = ArrayData {
            items,
            depth,
            depth_counts,
            receipt: MemoryReceipt::new_in(memory, 0),
        };
        data.receipt.resize(data.tracked_bytes());
        Ok(Value::Array(NyblArray(Rc::new(data))))
    }

    /// Tracked storage required by an array whose children are all flat
    /// values, such as the String `.split()` result.
    #[doc(hidden)]
    pub fn __flat_array_tracked_bytes(item_capacity: usize, item_count: usize) -> Option<usize> {
        item_capacity
            .checked_mul(core::mem::size_of::<Value>())?
            .checked_add(core::mem::size_of::<OrderedDepthCounts>())?
            .checked_add(item_count.checked_mul(core::mem::size_of::<u16>())?)
    }

    /// Trusted compatibility constructor for a dictionary.
    ///
    /// # Panics
    ///
    /// Panics with `"Value nesting limit exceeded (maximum 64 levels)"` if an
    /// entry value would make the returned value's
    /// [`Self::ownership_depth`] exceed [`MAX_VALUE_DEPTH`]. Use
    /// [`Self::try_new_dict`] to handle that condition without panicking.
    pub fn new_dict(entries: Vec<(String, Value)>) -> Self {
        trusted(Self::try_new_dict(entries, 0))
    }

    pub fn try_new_dict(entries: Vec<(String, Value)>, line: u32) -> Result<Self, NyblError> {
        Self::__try_new_dict_in(entries, line, &MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __try_new_dict_in(
        entries: Vec<(String, Value)>,
        line: u32,
        memory: &MemoryContext,
    ) -> Result<Self, NyblError> {
        let depth = checked_owner_depth(entries.iter().map(|(_, value)| value), 1, line)?;
        let depth_counts = OrderedDepthCounts::from_entries(&entries, line)?;
        let key_index = DictKeyIndex::try_from_entries(&entries, line)?;
        let key_bytes = entries.iter().map(|(key, _)| key.capacity()).sum();
        let mut data = DictData {
            entries,
            key_bytes,
            key_index,
            depth,
            depth_counts,
            receipt: MemoryReceipt::new_in(memory, 0),
        };
        data.receipt.resize(data.tracked_bytes());
        Ok(Value::Dict(NyblDict(Rc::new(data))))
    }

    /// Build a user-defined struct value. `module_path` is the
    /// module in which the type was declared (`<root>` at the
    /// top level, `<builtin>` for engine-registered shapes like
    /// `RuntimeError`, or the dot-joined `use` path for user
    /// modules). Two structs are only the same type when both
    /// the module path *and* the type name match — so a
    /// `struct Color { ... }` declared in two separate modules
    /// produces genuinely distinct values.
    ///
    /// # Panics
    ///
    /// Panics with `"Value nesting limit exceeded (maximum 64 levels)"` if a
    /// field value would make the returned value's
    /// [`Self::ownership_depth`] exceed [`MAX_VALUE_DEPTH`]. Use
    /// [`Self::try_new_struct`] to handle that condition without panicking.
    pub fn new_struct(
        module_path: String,
        type_name: String,
        fields: Vec<(String, Value)>,
    ) -> Self {
        trusted(Self::try_new_struct(module_path, type_name, fields, 0))
    }

    pub fn try_new_struct(
        module_path: String,
        type_name: String,
        fields: Vec<(String, Value)>,
        line: u32,
    ) -> Result<Self, NyblError> {
        Self::__try_new_struct_in(
            module_path,
            type_name,
            fields,
            line,
            &MemoryContext::__legacy_current(),
        )
    }

    #[doc(hidden)]
    pub fn __try_new_struct_in(
        module_path: String,
        type_name: String,
        fields: Vec<(String, Value)>,
        line: u32,
        memory: &MemoryContext,
    ) -> Result<Self, NyblError> {
        let depth = checked_owner_depth(fields.iter().map(|(_, value)| value), 1, line)?;
        let mut data = StructData {
            module_path,
            type_name,
            fields,
            depth,
            receipt: MemoryReceipt::new_in(memory, 0),
        };
        data.receipt.resize(data.tracked_bytes());
        Ok(Value::Struct(NyblStruct(Rc::new(data))))
    }

    /// Build a built-in iterator that yields each item of
    /// `items` in order. Cloning the returned `Value::Iter`
    /// shares the iteration cursor, so `let b = a; a.next()`
    /// advances `b` too.
    ///
    /// # Panics
    ///
    /// Panics with `"Value nesting limit exceeded (maximum 64 levels)"` if
    /// `items` would make the returned iterator's
    /// [`Self::ownership_depth`] exceed [`MAX_VALUE_DEPTH`]. Use
    /// [`Self::try_new_array_iter`] to handle that condition without
    /// panicking.
    pub fn new_array_iter(items: Vec<Value>) -> Self {
        trusted(Self::try_new_array_iter(items, 0))
    }

    pub fn try_new_array_iter(items: Vec<Value>, line: u32) -> Result<Self, NyblError> {
        Self::__try_new_array_iter_in(items, line, &MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __try_new_array_iter_in(
        items: Vec<Value>,
        line: u32,
        memory: &MemoryContext,
    ) -> Result<Self, NyblError> {
        let depth = checked_owner_depth(&items, 1, line)?;
        let bytes = items.capacity() * core::mem::size_of::<Value>();
        Ok(Value::Iter(Rc::new(RefCell::new(NyblIter {
            kind: NyblIterKind::Array { items, pos: 0 },
            depth,
            _receipt: MemoryReceipt::new_in(memory, bytes),
        }))))
    }

    /// Build a built-in iterator over a string's Unicode code
    /// points.
    pub fn new_string_iter(chars: Vec<char>) -> Self {
        Self::__new_string_iter_in(chars, &MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __new_string_iter_in(chars: Vec<char>, memory: &MemoryContext) -> Self {
        let bytes = chars.capacity() * core::mem::size_of::<char>();
        Value::Iter(Rc::new(RefCell::new(NyblIter {
            kind: NyblIterKind::String { chars, pos: 0 },
            depth: 1,
            _receipt: MemoryReceipt::new_in(memory, bytes),
        })))
    }

    /// Build a built-in iterator over a dict's keys (declaration
    /// order).
    pub fn new_dict_iter(keys: Vec<String>) -> Self {
        Self::__new_dict_iter_in(keys, &MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __new_dict_iter_in(keys: Vec<String>, memory: &MemoryContext) -> Self {
        let key_bytes: usize = keys.iter().map(|k| k.capacity()).sum();
        let bytes = keys.capacity() * core::mem::size_of::<String>() + key_bytes;
        Value::Iter(Rc::new(RefCell::new(NyblIter {
            kind: NyblIterKind::Dict { keys, pos: 0 },
            depth: 1,
            _receipt: MemoryReceipt::new_in(memory, bytes),
        })))
    }

    pub fn new_enum_unit(module_path: String, type_name: String, variant: String) -> Self {
        Self::__new_enum_unit_in(
            module_path,
            type_name,
            variant,
            &MemoryContext::__legacy_current(),
        )
    }

    #[doc(hidden)]
    pub fn __new_enum_unit_in(
        module_path: String,
        type_name: String,
        variant: String,
        memory: &MemoryContext,
    ) -> Self {
        let mut data = EnumVariantData {
            module_path,
            type_name,
            variant,
            payload: EnumPayload::Unit,
            depth: 1,
            receipt: MemoryReceipt::new_in(memory, 0),
        };
        data.receipt.resize(data.tracked_bytes());
        Value::EnumVariant(NyblEnumVariant(Rc::new(data)))
    }

    /// Build a tuple-payload enum variant.
    ///
    /// # Panics
    ///
    /// Panics with `"Value nesting limit exceeded (maximum 64 levels)"` if
    /// `items` would make the returned value's [`Self::ownership_depth`]
    /// exceed [`MAX_VALUE_DEPTH`]. Use [`Self::try_new_enum_tuple`] to handle
    /// that condition without panicking.
    pub fn new_enum_tuple(
        module_path: String,
        type_name: String,
        variant: String,
        items: Vec<Value>,
    ) -> Self {
        trusted(Self::try_new_enum_tuple(
            module_path,
            type_name,
            variant,
            items,
            0,
        ))
    }

    pub fn try_new_enum_tuple(
        module_path: String,
        type_name: String,
        variant: String,
        items: Vec<Value>,
        line: u32,
    ) -> Result<Self, NyblError> {
        Self::__try_new_enum_tuple_in(
            module_path,
            type_name,
            variant,
            items,
            line,
            &MemoryContext::__legacy_current(),
        )
    }

    #[doc(hidden)]
    pub fn __try_new_enum_tuple_in(
        module_path: String,
        type_name: String,
        variant: String,
        items: Vec<Value>,
        line: u32,
        memory: &MemoryContext,
    ) -> Result<Self, NyblError> {
        let depth = checked_owner_depth(&items, 1, line)?;
        let mut data = EnumVariantData {
            module_path,
            type_name,
            variant,
            payload: EnumPayload::Tuple(items),
            depth,
            receipt: MemoryReceipt::new_in(memory, 0),
        };
        data.receipt.resize(data.tracked_bytes());
        Ok(Value::EnumVariant(NyblEnumVariant(Rc::new(data))))
    }

    /// Build a struct-payload enum variant.
    ///
    /// # Panics
    ///
    /// Panics with `"Value nesting limit exceeded (maximum 64 levels)"` if a
    /// field value would make the returned value's
    /// [`Self::ownership_depth`] exceed [`MAX_VALUE_DEPTH`]. Use
    /// [`Self::try_new_enum_struct`] to handle that condition without
    /// panicking.
    pub fn new_enum_struct(
        module_path: String,
        type_name: String,
        variant: String,
        fields: Vec<(String, Value)>,
    ) -> Self {
        trusted(Self::try_new_enum_struct(
            module_path,
            type_name,
            variant,
            fields,
            0,
        ))
    }

    pub fn try_new_enum_struct(
        module_path: String,
        type_name: String,
        variant: String,
        fields: Vec<(String, Value)>,
        line: u32,
    ) -> Result<Self, NyblError> {
        Self::__try_new_enum_struct_in(
            module_path,
            type_name,
            variant,
            fields,
            line,
            &MemoryContext::__legacy_current(),
        )
    }

    #[doc(hidden)]
    pub fn __try_new_enum_struct_in(
        module_path: String,
        type_name: String,
        variant: String,
        fields: Vec<(String, Value)>,
        line: u32,
        memory: &MemoryContext,
    ) -> Result<Self, NyblError> {
        let depth = checked_owner_depth(fields.iter().map(|(_, value)| value), 1, line)?;
        let mut data = EnumVariantData {
            module_path,
            type_name,
            variant,
            payload: EnumPayload::Struct(fields),
            depth,
            receipt: MemoryReceipt::new_in(memory, 0),
        };
        data.receipt.resize(data.tracked_bytes());
        Ok(Value::EnumVariant(NyblEnumVariant(Rc::new(data))))
    }

    /// Build a tree-walker-ready closure value. The AST body moves
    /// into a shared [`NyblFn`] behind an `Rc`; subsequent clones
    /// of the resulting `Value::Fn` just bump the refcount.
    ///
    /// # Panics
    ///
    /// Panics with `"Value nesting limit exceeded (maximum 64 levels)"` if a
    /// captured value would make the returned function's
    /// [`Self::ownership_depth`] exceed [`MAX_VALUE_DEPTH`]. Use
    /// [`Self::try_new_fn`] to handle that condition without panicking.
    pub fn new_fn(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Vec<Stmt>,
        self_name: Option<String>,
    ) -> Self {
        trusted(Self::try_new_fn(params, captures, body, self_name, 0))
    }

    pub fn try_new_fn(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Vec<Stmt>,
        self_name: Option<String>,
        line: u32,
    ) -> Result<Self, NyblError> {
        NyblFn::try_new_ast(params, captures, body, self_name, line).map(Value::Fn)
    }

    pub fn try_new_fn_with_modes(
        params: Vec<String>,
        param_modes: Vec<crate::parser::ParamMode>,
        captures: Vec<(String, Value)>,
        body: Vec<Stmt>,
        self_name: Option<String>,
        line: u32,
    ) -> Result<Self, NyblError> {
        NyblFn::try_new_ast_in_module_with_modes(
            params,
            param_modes,
            captures,
            body,
            self_name,
            None,
            line,
        )
        .map(Value::Fn)
    }

    pub fn try_new_module_fn(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Vec<Stmt>,
        self_name: Option<String>,
        module_path: String,
        line: u32,
    ) -> Result<Self, NyblError> {
        NyblFn::try_new_ast_in_module(params, captures, body, self_name, Some(module_path), line)
            .map(Value::Fn)
    }

    /// Build a closure value with an engine-opaque compiled body.
    /// Used by the bytecode VM (and any future engine) to carry
    /// its pre-compiled form inside a `Value::Fn` without
    /// `nybl-lang` depending on the engine crate.
    ///
    /// # Panics
    ///
    /// Panics with `"Value nesting limit exceeded (maximum 64 levels)"` if a
    /// captured value would make the returned function's
    /// [`Self::ownership_depth`] exceed [`MAX_VALUE_DEPTH`]. Use
    /// [`Self::try_new_compiled_fn`] to provide the engine-opaque body's
    /// ownership depth and handle depth-limit failures without panicking.
    pub fn new_compiled_fn(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Rc<dyn core::any::Any + 'static>,
        self_name: Option<String>,
    ) -> Self {
        trusted(Self::try_new_compiled_fn(
            params, captures, body, self_name, 0, 0,
        ))
    }

    /// Fallible compiled-function constructor. `opaque_body_depth` is the
    /// maximum depth of `Value`s owned inside the engine-specific body but
    /// hidden behind `dyn Any` (notably AOT Rust closure captures).
    pub fn try_new_compiled_fn(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Rc<dyn core::any::Any + 'static>,
        self_name: Option<String>,
        opaque_body_depth: u16,
        line: u32,
    ) -> Result<Self, NyblError> {
        NyblFn::try_new_compiled(params, captures, body, self_name, opaque_body_depth, line)
            .map(Value::Fn)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn try_new_compiled_fn_with_modes(
        params: Vec<String>,
        param_modes: Vec<crate::parser::ParamMode>,
        captures: Vec<(String, Value)>,
        body: Rc<dyn core::any::Any + 'static>,
        self_name: Option<String>,
        opaque_body_depth: u16,
        line: u32,
    ) -> Result<Self, NyblError> {
        NyblFn::try_new_compiled_in_module_with_modes(
            params,
            param_modes,
            captures,
            body,
            self_name,
            None,
            opaque_body_depth,
            line,
        )
        .map(Value::Fn)
    }

    pub fn try_new_compiled_module_fn(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Rc<dyn core::any::Any + 'static>,
        self_name: Option<String>,
        module_path: String,
        opaque_body_depth: u16,
        line: u32,
    ) -> Result<Self, NyblError> {
        NyblFn::try_new_compiled_in_module(
            params,
            captures,
            body,
            self_name,
            Some(module_path),
            opaque_body_depth,
            line,
        )
        .map(Value::Fn)
    }

    /// Build a namespace value while accounting for recursively owned
    /// exported bindings.
    pub fn new_module(
        path: String,
        bindings: Vec<(String, Value)>,
        types: Vec<String>,
        line: u32,
    ) -> Result<Self, NyblError> {
        NyblModule::try_new(path, bindings, types, line).map(Value::Module)
    }

    pub fn new_module_with_type_exports(
        path: String,
        bindings: Vec<(String, Value)>,
        type_exports: NyblTypeExports,
        line: u32,
    ) -> Result<Self, NyblError> {
        NyblModule::try_new_with_type_exports(path, bindings, type_exports, line).map(Value::Module)
    }
}

// ─── Clone (tracks allocations) ────────────────────────────────────────────
//
// Composite containers are CoW handles. Cloning them only bumps an `Rc`;
// the first shared mutation uses `ensure_owner` to clone the backing into the
// active engine's explicit account. Immutable strings also use a shared
// backing, so ordinary
// assignment, argument passing, capture, and return stay O(1).

impl Clone for Value {
    fn clone(&self) -> Self {
        match self {
            Value::Int(n) => Value::Int(*n),
            Value::Number(n) => Value::Number(*n),
            Value::Bool(b) => Value::Bool(*b),
            Value::None => Value::None,
            Value::Host(value) => Value::Host(value.clone()),
            Value::Str(s) => Value::Str(NyblStr(Rc::clone(&s.0))),
            Value::Array(arr) => Value::Array(arr.clone()),
            Value::Dict(dict) => Value::Dict(dict.clone()),
            Value::Struct(value) => Value::Struct(value.clone()),
            // Closures are reference-counted: cloning a Value::Fn
            // is O(1) and doesn't duplicate the body or captures.
            // Tracking the captures' memory happens once, at the
            // moment the NyblFn is constructed (by `new_fn`), via
            // their own Value Clone/Drop hooks.
            Value::Fn(f) => Value::Fn(Rc::clone(f)),
            // Modules are reference-counted — same cheap clone as fns. The
            // sandbox budget counts retained child Value backings, not engine
            // metadata such as export-name/type maps.
            Value::Module(m) => Value::Module(Rc::clone(m)),
            // Iterators are reference-counted and intentionally
            // share their cursor — cloning `a = b` doesn't fork
            // the iteration state, matching iterator semantics
            // in Python / Rust / JS. The buffer was tracked once
            // by the constructor and is dealloc'd by NyblIter's
            // receipt when the last clone goes away.
            Value::Iter(it) => Value::Iter(Rc::clone(it)),
            Value::EnumVariant(value) => Value::EnumVariant(value.clone()),
        }
    }
}

impl Value {
    /// Clone an engine-facing compatibility snapshot of the inspectable Value
    /// graph without retaining its instance-owned allocation receipts. Module
    /// export metadata is immutable, but its public `bindings` field must
    /// neither raise an authoritative CoW refcount nor keep replaced nested
    /// values charged to the instance. AST and VM chunk function bodies are
    /// receipt-free; arbitrary opaque compiled bodies are immutable code and
    /// are shared without introspection.
    /// Shared DAG nodes remain shared in the snapshot; Nybl's public
    /// constructors and circular-import rejection make owned Value graphs
    /// acyclic, so completed-node memoization is sufficient.
    #[doc(hidden)]
    pub fn __compatibility_snapshot(&self, line: u32) -> Result<Self, NyblError> {
        self.compatibility_snapshot_inner(&mut BTreeMap::new(), line)
    }

    #[doc(hidden)]
    pub fn __compatibility_snapshot_bindings(
        bindings: &BTreeMap<String, Value>,
        line: u32,
    ) -> Result<Vec<(String, Value)>, NyblError> {
        let mut memo = BTreeMap::new();
        bindings
            .iter()
            .map(|(name, value)| {
                value
                    .compatibility_snapshot_inner(&mut memo, line)
                    .map(|value| (name.clone(), value))
            })
            .collect()
    }

    /// Whether two engine compatibility values reuse the same reference-counted
    /// backing. This is an internal diagnostic used to guard against facade
    /// fanout accidentally deep-copying an origin module's public snapshot.
    #[doc(hidden)]
    pub fn __shares_backing_with(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Str(left), Self::Str(right)) => Rc::ptr_eq(&left.0, &right.0),
            (Self::Array(left), Self::Array(right)) => Rc::ptr_eq(&left.0, &right.0),
            (Self::Dict(left), Self::Dict(right)) => Rc::ptr_eq(&left.0, &right.0),
            (Self::Struct(left), Self::Struct(right)) => Rc::ptr_eq(&left.0, &right.0),
            (Self::EnumVariant(left), Self::EnumVariant(right)) => Rc::ptr_eq(&left.0, &right.0),
            (Self::Iter(left), Self::Iter(right)) => Rc::ptr_eq(left, right),
            (Self::Fn(left), Self::Fn(right)) => Rc::ptr_eq(left, right),
            (Self::Module(left), Self::Module(right)) => Rc::ptr_eq(left, right),
            (Self::Host(left), Self::Host(right)) => left.ptr_eq(right),
            _ => false,
        }
    }

    fn compatibility_snapshot_key(&self) -> Option<(u8, usize)> {
        match self {
            Value::Str(value) => Some((0, Rc::as_ptr(&value.0) as usize)),
            Value::Array(value) => Some((1, Rc::as_ptr(&value.0) as usize)),
            Value::Dict(value) => Some((2, Rc::as_ptr(&value.0) as usize)),
            Value::Struct(value) => Some((3, Rc::as_ptr(&value.0) as usize)),
            Value::EnumVariant(value) => Some((4, Rc::as_ptr(&value.0) as usize)),
            Value::Iter(value) => Some((5, Rc::as_ptr(value) as usize)),
            Value::Fn(value) => Some((6, Rc::as_ptr(value) as usize)),
            Value::Module(value) => Some((7, Rc::as_ptr(value) as usize)),
            Value::Host(value) => Some((8, Rc::as_ptr(&value.0) as usize)),
            Value::Int(_) | Value::Number(_) | Value::Bool(_) | Value::None => None,
        }
    }

    fn compatibility_snapshot_inner(
        &self,
        memo: &mut BTreeMap<(u8, usize), Value>,
        line: u32,
    ) -> Result<Self, NyblError> {
        let key = self.compatibility_snapshot_key();
        if let Some(snapshot) = key.as_ref().and_then(|key| memo.get(key)) {
            return Ok(snapshot.clone());
        }
        let snapshot = match self {
            Value::Int(value) => Value::Int(*value),
            Value::Number(value) => Value::Number(*value),
            Value::Bool(value) => Value::Bool(*value),
            Value::None => Value::None,
            Value::Host(value) => Value::Host(value.clone()),
            Value::Str(value) => {
                let text = value.0.text.clone();
                let bytes = text.capacity();
                Value::Str(NyblStr(Rc::new(NyblStrData {
                    text,
                    _receipt: MemoryReceipt::new_in(&MemoryContext::__untracked(), bytes),
                })))
            }
            Value::Array(value) => {
                let items = value
                    .0
                    .items
                    .iter()
                    .map(|value| value.compatibility_snapshot_inner(memo, line))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut data = ArrayData {
                    items,
                    depth: value.0.depth,
                    depth_counts: value.0.depth_counts.clone(),
                    receipt: MemoryReceipt::new_in(&MemoryContext::__untracked(), 0),
                };
                data.receipt.resize(data.tracked_bytes());
                Value::Array(NyblArray(Rc::new(data)))
            }
            Value::Dict(value) => {
                let entries = value
                    .0
                    .entries
                    .iter()
                    .map(|(key, value)| {
                        value
                            .compatibility_snapshot_inner(memo, line)
                            .map(|value| (key.clone(), value))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let key_bytes = entries.iter().map(|(key, _)| key.capacity()).sum();
                let mut data = DictData {
                    entries,
                    key_bytes,
                    key_index: value.0.key_index.clone(),
                    depth: value.0.depth,
                    depth_counts: value.0.depth_counts.clone(),
                    receipt: MemoryReceipt::new_in(&MemoryContext::__untracked(), 0),
                };
                data.receipt.resize(data.tracked_bytes());
                Value::Dict(NyblDict(Rc::new(data)))
            }
            Value::Struct(value) => {
                let fields = value
                    .0
                    .fields
                    .iter()
                    .map(|(name, value)| {
                        value
                            .compatibility_snapshot_inner(memo, line)
                            .map(|value| (name.clone(), value))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let mut data = StructData {
                    module_path: value.0.module_path.clone(),
                    type_name: value.0.type_name.clone(),
                    fields,
                    depth: value.0.depth,
                    receipt: MemoryReceipt::new_in(&MemoryContext::__untracked(), 0),
                };
                data.receipt.resize(data.tracked_bytes());
                Value::Struct(NyblStruct(Rc::new(data)))
            }
            Value::EnumVariant(value) => {
                let payload = match &value.0.payload {
                    EnumPayload::Unit => EnumPayload::Unit,
                    EnumPayload::Tuple(items) => EnumPayload::Tuple(
                        items
                            .iter()
                            .map(|value| value.compatibility_snapshot_inner(memo, line))
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                    EnumPayload::Struct(fields) => EnumPayload::Struct(
                        fields
                            .iter()
                            .map(|(name, value)| {
                                value
                                    .compatibility_snapshot_inner(memo, line)
                                    .map(|value| (name.clone(), value))
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                };
                let mut data = EnumVariantData {
                    module_path: value.0.module_path.clone(),
                    type_name: value.0.type_name.clone(),
                    variant: value.0.variant.clone(),
                    payload,
                    depth: value.0.depth,
                    receipt: MemoryReceipt::new_in(&MemoryContext::__untracked(), 0),
                };
                data.receipt.resize(data.tracked_bytes());
                Value::EnumVariant(NyblEnumVariant(Rc::new(data)))
            }
            Value::Iter(value) => {
                let iter = value.try_borrow().map_err(|_| {
                    NyblError::runtime(
                        "Cannot snapshot a module iterator while it is borrowed",
                        line,
                    )
                })?;
                let (kind, bytes) = match &iter.kind {
                    NyblIterKind::Array { items, pos } => {
                        let items: Vec<Value> = items
                            .iter()
                            .map(|value| value.compatibility_snapshot_inner(memo, line))
                            .collect::<Result<Vec<_>, _>>()?;
                        let bytes = items.capacity() * core::mem::size_of::<Value>();
                        (NyblIterKind::Array { items, pos: *pos }, bytes)
                    }
                    NyblIterKind::String { chars, pos } => {
                        let chars = chars.clone();
                        let bytes = chars.capacity() * core::mem::size_of::<char>();
                        (NyblIterKind::String { chars, pos: *pos }, bytes)
                    }
                    NyblIterKind::Dict { keys, pos } => {
                        let keys = keys.clone();
                        let key_bytes: usize = keys.iter().map(|key| key.capacity()).sum();
                        let bytes = keys.capacity() * core::mem::size_of::<String>() + key_bytes;
                        (NyblIterKind::Dict { keys, pos: *pos }, bytes)
                    }
                };
                Value::Iter(Rc::new(RefCell::new(NyblIter {
                    kind,
                    depth: iter.depth,
                    _receipt: MemoryReceipt::new_in(&MemoryContext::__untracked(), bytes),
                })))
            }
            Value::Fn(value) => {
                let body = match &value.body {
                    FnBody::Ast(statements) => FnBody::Ast(statements.clone()),
                    FnBody::Compiled(body) => FnBody::Compiled(Rc::clone(body)),
                };
                let captures = value
                    .captures
                    .iter()
                    .map(|(name, value)| {
                        value
                            .compatibility_snapshot_inner(memo, line)
                            .map(|value| (name.clone(), value))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Value::Fn(Rc::new(NyblFn {
                    params: value.params.clone(),
                    param_modes: value.param_modes.clone(),
                    captures,
                    body,
                    self_name: value.self_name.clone(),
                    module_path: value.module_path.clone(),
                    origin: value.origin.clone(),
                    depth: value.depth,
                }))
            }
            Value::Module(value) => {
                let bindings = value
                    .bindings
                    .iter()
                    .map(|(name, value)| {
                        value
                            .compatibility_snapshot_inner(memo, line)
                            .map(|value| (name.clone(), value))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Value::Module(Rc::new(NyblModule {
                    path: value.path.clone(),
                    bindings,
                    types: value.types.clone(),
                    type_exports: value.type_exports.clone(),
                    live_environments: None,
                    live_bindings: BTreeMap::new(),
                    depth: value.depth,
                }))
            }
        };
        if let Some(key) = key {
            memo.insert(key, snapshot.clone());
        }
        Ok(snapshot)
    }
}

// ─── Drop (tracks deallocations) ───────────────────────────────────────────

// ─── Display ───────────────────────────────────────────────────────────────

impl core::fmt::Display for Value {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{n}"),
            Value::Number(n) => {
                if n.is_finite() && *n == (*n as i64 as f64) {
                    write!(f, "{}", *n as i64)
                } else {
                    write!(f, "{n}")
                }
            }
            Value::Str(s) => write!(f, "{}", s.0.text),
            Value::Bool(b) => write!(f, "{b}"),
            Value::None => write!(f, "none"),
            Value::Host(value) => core::fmt::Display::fmt(value, f),
            Value::Array(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", Inspected(item))?;
                }
                write!(f, "]")
            }
            Value::Dict(entries) => {
                write!(f, "{{")?;
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "\"{}\": {}", k, Inspected(v))?;
                }
                write!(f, "}}")
            }
            Value::Fn(func) => match &func.self_name {
                Some(name) => write!(f, "<fn {name}>"),
                None => write!(f, "<fn>"),
            },
            Value::Module(m) => write!(f, "<module {}>", m.path),
            Value::Iter(it) => {
                // Peek at the inner state for a useful Display —
                // callers see `<iter array 0/3>` rather than a
                // bare `<iter>`. If the RefCell is already
                // borrowed (nested Display during a panic
                // backtrace, say), fall back to the bare form.
                match it.try_borrow() {
                    Ok(inner) => match &inner.kind {
                        NyblIterKind::Array { items, pos } => {
                            write!(f, "<iter array {}/{}>", pos, items.len())
                        }
                        NyblIterKind::String { chars, pos } => {
                            write!(f, "<iter string {}/{}>", pos, chars.len())
                        }
                        NyblIterKind::Dict { keys, pos } => {
                            write!(f, "<iter dict {}/{}>", pos, keys.len())
                        }
                    },
                    Err(_) => write!(f, "<iter>"),
                }
            }
            Value::Struct(s) => {
                write!(f, "{} {{", s.type_name())?;
                for (i, (k, v)) in s.fields().iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, " {}: {}", k, Inspected(v))?;
                }
                if !s.fields().is_empty() {
                    write!(f, " ")?;
                }
                write!(f, "}}")
            }
            Value::EnumVariant(e) => match e.payload() {
                EnumPayload::Unit => write!(f, "{}::{}", e.type_name(), e.variant()),
                EnumPayload::Tuple(items) => {
                    write!(f, "{}::{}(", e.type_name(), e.variant())?;
                    for (i, v) in items.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", Inspected(v))?;
                    }
                    write!(f, ")")
                }
                EnumPayload::Struct(fields) => {
                    write!(f, "{}::{} {{", e.type_name(), e.variant())?;
                    for (i, (k, v)) in fields.iter().enumerate() {
                        if i > 0 {
                            write!(f, ",")?;
                        }
                        write!(f, " {}: {}", k, Inspected(v))?;
                    }
                    if !fields.is_empty() {
                        write!(f, " ")?;
                    }
                    write!(f, "}}")
                }
            },
        }
    }
}

impl core::fmt::Display for NyblStr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0.text)
    }
}

struct Inspected<'a>(&'a Value);

impl core::fmt::Display for Inspected<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Value::Str(value) => write!(f, "\"{}\"", value.0.text),
            value => core::fmt::Display::fmt(value, f),
        }
    }
}

// ─── Value helpers ─────────────────────────────────────────────────────────

impl Value {
    pub fn inspect(&self) -> String {
        format!("{}", Inspected(self))
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Number(_) => "number",
            Value::Str(_) => "string",
            Value::Bool(_) => "bool",
            Value::None => "none",
            Value::Host(value) => value.type_name(),
            Value::Array(_) => "array",
            Value::Dict(_) => "dict",
            Value::Fn(_) => "fn",
            // Generic bucket — the *specific* type name lives on
            // the value itself (`struct_type_name()`). `type()`
            // returns the Nybl type name via the display path, so
            // `type(Point { ... })` shows `"Point"`.
            Value::Struct(_) => "struct",
            Value::EnumVariant(_) => "enum",
            Value::Module(_) => "module",
            Value::Iter(_) => "iter",
        }
    }

    /// The user-facing name for this value's type. For struct
    /// values it's the declared type (`"Point"`); for enum
    /// variants it's the enum's type name; for built-in
    /// variants it matches [`Self::type_name`].
    pub fn display_type_name(&self) -> String {
        match self {
            Value::Struct(s) => s.type_name().to_string(),
            Value::EnumVariant(e) => e.type_name().to_string(),
            other => other.type_name().to_string(),
        }
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::None => false,
            Value::Host(_) => true,
            Value::Int(n) => *n != 0,
            Value::Number(n) => *n != 0.0,
            Value::Str(s) => !s.0.text.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Dict(d) => !d.is_empty(),
            // A callable is always a "thing" — match other
            // non-empty runtime objects.
            Value::Fn(_) => true,
            // Structs carry fielded data and are always truthy,
            // even if they have no fields (the "unit struct"
            // use case) — matching how classes / records behave
            // in most scripting languages.
            Value::Struct(_) => true,
            // Enum variants represent a tagged choice; always
            // truthy regardless of payload.
            Value::EnumVariant(_) => true,
            // A module is always a concrete thing — matches
            // fn's behaviour.
            Value::Module(_) => true,
            // Iterators are always truthy, even when exhausted.
            // Callers check `Iter::Done` via `.next()`, not via
            // truthiness — matches how fns / modules behave.
            Value::Iter(_) => true,
        }
    }
}

// ─── Deref for read access ─────────────────────────────────────────────────

impl NyblStr {
    pub fn as_str(&self) -> &str {
        &self.0.text
    }
}

impl core::ops::Deref for NyblStr {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0.text
    }
}

impl core::ops::Deref for NyblArray {
    type Target = [Value];
    fn deref(&self) -> &[Value] {
        &self.0.items
    }
}

impl core::ops::Deref for NyblDict {
    type Target = [(String, Value)];
    fn deref(&self) -> &[(String, Value)] {
        &self.0.entries
    }
}

// ─── Mutation methods ──────────────────────────────────────────────────────

impl NyblArray {
    fn ensure_owner(&mut self, memory: &MemoryContext) {
        if Rc::strong_count(&self.0) > 1 || !self.0.receipt.owner_matches(memory) {
            self.0 = Rc::new(self.0.clone_in(memory));
        }
    }

    /// Take the inner Vec, leaving an empty array. Deallocates the buffer
    /// from the memory tracker since it's leaving Value's control.
    pub fn take(&mut self) -> Vec<Value> {
        self.__take_in(&MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __take_in(&mut self, memory: &MemoryContext) -> Vec<Value> {
        self.ensure_owner(memory);
        let data = Rc::get_mut(&mut self.0).expect("array backing was made unique");
        let taken = core::mem::take(&mut data.items);
        data.depth = 1;
        data.depth_counts.clear();
        let bytes = data.tracked_bytes();
        data.receipt.resize(bytes);
        taken
    }

    fn check_child_depth(value: &Value, line: u32) -> Result<u16, NyblError> {
        let child_depth = value.ownership_depth();
        if child_depth.saturating_add(1) > MAX_VALUE_DEPTH {
            Err(value_depth_error(line))
        } else {
            Ok(child_depth)
        }
    }

    fn try_reserve_item(data: &mut ArrayData, line: u32) -> Result<(), NyblError> {
        data.items
            .try_reserve(1)
            .map_err(|_| NyblError::fatal("Memory limit exceeded", line))?;
        let bytes = data.tracked_bytes();
        data.receipt.resize(bytes);
        Ok(())
    }

    fn refresh_depth(data: &mut ArrayData) {
        data.depth = data.depth_counts.owner_depth();
    }

    /// Append one value without cloning the existing array. Capacity growth is
    /// charged exactly once and all fallible checks happen before insertion.
    pub fn try_push(&mut self, value: Value, line: u32) -> Result<(), NyblError> {
        self.__try_push_in(value, line, &MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __try_push_in(
        &mut self,
        value: Value,
        line: u32,
        memory: &MemoryContext,
    ) -> Result<(), NyblError> {
        let child_depth = Self::check_child_depth(&value, line)?;
        self.ensure_owner(memory);
        let data = Rc::get_mut(&mut self.0).expect("array backing was made unique");
        data.depth_counts.ensure_depth(child_depth, line)?;
        data.depth_counts.try_reserve_child(line)?;
        Self::try_reserve_item(data, line)?;
        data.items.push(value);
        data.depth_counts.child_depths.push(child_depth);
        data.depth_counts.add(child_depth);
        Self::refresh_depth(data);
        Ok(())
    }

    /// Remove and return the final value, if any.
    pub fn pop(&mut self) -> Option<Value> {
        self.__pop_in(&MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __pop_in(&mut self, memory: &MemoryContext) -> Option<Value> {
        if self.is_empty() {
            return None;
        }
        self.ensure_owner(memory);
        let data = Rc::get_mut(&mut self.0).expect("array backing was made unique");
        let child_depth = data.depth_counts.child_depths.pop()?;
        let value = data.items.pop()?;
        data.depth_counts.remove(child_depth);
        Self::refresh_depth(data);
        Some(value)
    }

    /// Insert a value at an already-normalized endpoint-inclusive index.
    pub fn try_insert(&mut self, index: usize, value: Value, line: u32) -> Result<(), NyblError> {
        self.__try_insert_in(index, value, line, &MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __try_insert_in(
        &mut self,
        index: usize,
        value: Value,
        line: u32,
        memory: &MemoryContext,
    ) -> Result<(), NyblError> {
        let child_depth = Self::check_child_depth(&value, line)?;
        self.ensure_owner(memory);
        let data = Rc::get_mut(&mut self.0).expect("array backing was made unique");
        data.depth_counts.ensure_depth(child_depth, line)?;
        data.depth_counts.try_reserve_child(line)?;
        Self::try_reserve_item(data, line)?;
        data.items.insert(index, value);
        data.depth_counts.child_depths.insert(index, child_depth);
        data.depth_counts.add(child_depth);
        Self::refresh_depth(data);
        Ok(())
    }

    /// Remove and return a value at an already-normalized element index.
    pub fn remove(&mut self, index: usize) -> Value {
        self.__remove_in(index, &MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __remove_in(&mut self, index: usize, memory: &MemoryContext) -> Value {
        self.ensure_owner(memory);
        let data = Rc::get_mut(&mut self.0).expect("array backing was made unique");
        let child_depth = data.depth_counts.child_depths.remove(index);
        let value = data.items.remove(index);
        data.depth_counts.remove(child_depth);
        Self::refresh_depth(data);
        value
    }

    pub fn reverse(&mut self) {
        self.__reverse_in(&MemoryContext::__legacy_current());
    }

    #[doc(hidden)]
    pub fn __reverse_in(&mut self, memory: &MemoryContext) {
        self.ensure_owner(memory);
        let data = Rc::get_mut(&mut self.0).expect("array backing was made unique");
        data.items.reverse();
        data.depth_counts.child_depths.reverse();
    }

    pub fn sort_by(&mut self, compare: impl FnMut(&Value, &Value) -> core::cmp::Ordering) {
        self.__sort_by_in(compare, &MemoryContext::__legacy_current());
    }

    #[doc(hidden)]
    pub fn __sort_by_in(
        &mut self,
        compare: impl FnMut(&Value, &Value) -> core::cmp::Ordering,
        memory: &MemoryContext,
    ) {
        self.ensure_owner(memory);
        let data = Rc::get_mut(&mut self.0).expect("array backing was made unique");
        let mut compare = compare;
        let mut order: Vec<usize> = (0..data.items.len()).collect();
        order.sort_by(|a, b| compare(&data.items[*a], &data.items[*b]));

        // Convert `new position -> old position` into `old position -> new
        // position`, then apply that permutation to values and cached depths
        // together. This preserves stable sort semantics without re-reading a
        // potentially borrowed iterator's depth.
        let mut target = vec![0usize; order.len()];
        for (new_position, old_position) in order.into_iter().enumerate() {
            target[old_position] = new_position;
        }
        for position in 0..target.len() {
            while target[position] != position {
                let destination = target[position];
                data.items.swap(position, destination);
                data.depth_counts.child_depths.swap(position, destination);
                target.swap(position, destination);
            }
        }
    }

    /// Set a value at the given index. The old value at that index is dropped,
    /// releasing any allocation receipts it owns. No capacity change.
    pub fn set(&mut self, index: usize, val: Value) {
        trusted(self.try_set(index, val, 0));
    }

    /// Line-aware, atomic variant of [`Self::set`]. The existing element is
    /// left untouched if the replacement would exceed [`MAX_VALUE_DEPTH`].
    pub fn try_set(&mut self, index: usize, val: Value, line: u32) -> Result<(), NyblError> {
        self.__try_set_in(index, val, line, &MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __try_set_in(
        &mut self,
        index: usize,
        val: Value,
        line: u32,
        memory: &MemoryContext,
    ) -> Result<(), NyblError> {
        let new_child_depth = Self::check_child_depth(&val, line)?;
        let old_child_depth = self.0.depth_counts.child_depths[index];
        self.ensure_owner(memory);
        let data = Rc::get_mut(&mut self.0).expect("array backing was made unique");
        if new_child_depth != old_child_depth {
            data.depth_counts.ensure_depth(new_child_depth, line)?;
        }
        data.items[index] = val;
        data.depth_counts.child_depths[index] = new_child_depth;
        if new_child_depth != old_child_depth {
            data.depth_counts.remove(old_child_depth);
            data.depth_counts.add(new_child_depth);
            Self::refresh_depth(data);
        }
        let bytes = data.tracked_bytes();
        data.receipt.resize(bytes);
        Ok(())
    }
}

impl NyblStruct {
    fn ensure_owner(&mut self, memory: &MemoryContext) {
        if Rc::strong_count(&self.0) > 1 || !self.0.receipt.owner_matches(memory) {
            self.0 = Rc::new(self.0.clone_in(memory));
        }
    }

    pub fn type_name(&self) -> &str {
        &self.0.type_name
    }

    /// Module this struct type was declared in. Forms one half
    /// of the type's identity — the other half is the bare
    /// [`Self::type_name`].
    pub fn module_path(&self) -> &str {
        &self.0.module_path
    }

    pub fn fields(&self) -> &[(String, Value)] {
        &self.0.fields
    }

    /// Look up a field by name. `None` if no such field.
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.0
            .fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    /// Replace the value of an existing field. Returns `true` if
    /// the field was present; `false` if the caller should raise
    /// a "no such field" error. The old value is dropped (firing
    /// its allocation tracking); no capacity change in the Vec.
    pub fn set_field(&mut self, name: &str, value: Value) -> bool {
        trusted(self.try_set_field(name, value, 0))
    }

    /// Line-aware, atomic variant of [`Self::set_field`].
    pub fn try_set_field(
        &mut self,
        name: &str,
        value: Value,
        line: u32,
    ) -> Result<bool, NyblError> {
        self.__try_set_field_in(name, value, line, &MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __try_set_field_in(
        &mut self,
        name: &str,
        value: Value,
        line: u32,
        memory: &MemoryContext,
    ) -> Result<bool, NyblError> {
        let Some(index) = self.0.fields.iter().position(|(key, _)| key == name) else {
            return Ok(false);
        };
        // Struct keys are a fixed declared field set: writes cannot add fields
        // incrementally, and field counts are typically small. Keeping their
        // lookup and depth refresh linear avoids a second index/metadata
        // allocation without recreating dictionary construction's O(n²) path.
        let depth = checked_owner_depth(
            self.0
                .fields
                .iter()
                .enumerate()
                .filter_map(|(i, (_, value))| (i != index).then_some(value))
                .chain(core::iter::once(&value)),
            1,
            line,
        )?;
        self.ensure_owner(memory);
        let data = Rc::get_mut(&mut self.0).expect("struct backing was made unique");
        data.fields[index].1 = value;
        data.depth = depth;
        Ok(true)
    }
}

impl NyblEnumVariant {
    pub fn type_name(&self) -> &str {
        &self.0.type_name
    }

    /// Module this enum type was declared in. Paired with
    /// [`Self::type_name`] to form the type's identity.
    pub fn module_path(&self) -> &str {
        &self.0.module_path
    }

    pub fn variant(&self) -> &str {
        &self.0.variant
    }

    pub fn payload(&self) -> &EnumPayload {
        &self.0.payload
    }

    /// Field access for struct-variant payloads — mirrors
    /// [`NyblStruct::field`]. Returns `None` for unit / tuple
    /// variants or when the field isn't in this variant's
    /// payload.
    pub fn field(&self, name: &str) -> Option<&Value> {
        match &self.0.payload {
            EnumPayload::Struct(fields) => fields.iter().find(|(k, _)| k == name).map(|(_, v)| v),
            _ => None,
        }
    }
}

impl NyblDict {
    fn ensure_owner(&mut self, memory: &MemoryContext) {
        if Rc::strong_count(&self.0) > 1 || !self.0.receipt.owner_matches(memory) {
            self.0 = Rc::new(self.0.clone_in(memory));
        }
    }

    /// Set a key-value pair. If the key exists, replaces the value.
    /// If new, tracks the key's allocation and any Vec capacity growth
    /// from the push (Vec may reallocate to a larger buffer).
    pub fn set_key(&mut self, key: &str, val: Value) {
        trusted(self.try_set_key(key, val, 0));
    }

    /// Line-aware, atomic variant of [`Self::set_key`].
    ///
    /// Depth rejection and key-copy allocation happen before CoW detachment.
    /// A later recoverable allocation error can leave a detached backing or
    /// larger reserved capacities, but entries, lookup mappings, depth counts,
    /// and cached key bytes remain logically unchanged and mutually coherent.
    pub fn try_set_key(&mut self, key: &str, val: Value, line: u32) -> Result<(), NyblError> {
        self.__try_set_key_in(key, val, line, &MemoryContext::__legacy_current())
    }

    #[doc(hidden)]
    pub fn __try_set_key_in(
        &mut self,
        key: &str,
        val: Value,
        line: u32,
        memory: &MemoryContext,
    ) -> Result<(), NyblError> {
        let new_child_depth = val.ownership_depth();
        if new_child_depth.saturating_add(1) > MAX_VALUE_DEPTH {
            return Err(value_depth_error(line));
        }
        let existing = self.0.key_index.get(&self.0.entries, key);
        let owned_key = if existing.is_none() {
            Some(try_copy_dict_key(key, line)?)
        } else {
            None
        };
        let old_child_depth = existing.map(|index| self.0.depth_counts.child_depths[index]);

        self.ensure_owner(memory);
        let data = Rc::get_mut(&mut self.0).expect("dict backing was made unique");
        if let Some(index) = existing {
            if old_child_depth != Some(new_child_depth) {
                data.depth_counts.ensure_depth(new_child_depth, line)?;
            }
            data.entries[index].1 = val;
            data.depth_counts.child_depths[index] = new_child_depth;
            if let Some(old_child_depth) =
                old_child_depth.filter(|old_child_depth| *old_child_depth != new_child_depth)
            {
                data.depth_counts.remove(old_child_depth);
                data.depth_counts.add(new_child_depth);
                data.depth = data.depth_counts.owner_depth();
            }
        } else {
            let updated_key_bytes = data
                .key_bytes
                .checked_add(
                    owned_key
                        .as_ref()
                        .expect("absent dictionary key was copied before mutation")
                        .capacity(),
                )
                .ok_or_else(|| NyblError::fatal("Memory limit exceeded", line))?;
            data.depth_counts.ensure_depth(new_child_depth, line)?;
            data.sync_receipt();
            data.depth_counts.try_reserve_child(line)?;
            data.sync_receipt();
            data.entries
                .try_reserve(1)
                .map_err(|_| NyblError::fatal("Memory limit exceeded", line))?;
            data.sync_receipt();
            data.key_index.ensure_insert_capacity(&data.entries, line)?;
            data.sync_receipt();
            let key = owned_key.expect("absent dictionary key was copied before mutation");
            let entry_index = data.entries.len();
            data.entries.push((key, val));
            data.key_index
                .insert_new(&data.entries[entry_index].0, entry_index);
            data.key_bytes = updated_key_bytes;
            data.depth_counts.child_depths.push(new_child_depth);
            data.depth_counts.add(new_child_depth);
            data.depth = data.depth_counts.owner_depth();
        }
        data.sync_receipt();
        Ok(())
    }
}

// ─── Equality ──────────────────────────────────────────────────────────────

fn dicts_equal(left: &NyblDict, right: &NyblDict) -> bool {
    if left.len() != right.len() {
        return false;
    }

    // Pointer identity is only a shortcut when every entry owns a distinct
    // key. Compatibility constructors may preserve duplicate keys, whose
    // equality deliberately retains first-match semantics. Values also are
    // not universally reflexive (for example NaN and iterators), so shared
    // dictionaries must still check their values rather than returning true.
    if Rc::ptr_eq(&left.0, &right.0) && left.0.key_index.entry_count() == left.len() {
        return left.iter().all(|(_, value)| values_equal(value, value));
    }

    left.iter().all(|(key, value)| {
        right
            .0
            .key_index
            .get(&right.0.entries, key)
            .is_some_and(|index| values_equal(value, &right.0.entries[index].1))
    })
}

pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => x == y,
        // Cross-type numeric equality: `1 == 1.0` is true, same
        // as Python / JS. Widens the Int through f64 for the
        // comparison — lossy for magnitudes above 2^53, but
        // that's the cost of the convenience. Stricter-typed
        // code can call `int()` / `float()` explicitly first.
        (Value::Int(x), Value::Number(y)) => (*x as f64) == *y,
        (Value::Number(x), Value::Int(y)) => *x == (*y as f64),
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::None, Value::None) => true,
        (Value::Host(a), Value::Host(b)) => a.ptr_eq(b),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| values_equal(a, b))
        }
        (Value::Dict(x), Value::Dict(y)) => dicts_equal(x, y),
        // Functions have identity-based equality: two references
        // to the same `NyblFn` compare equal; structurally identical
        // closures constructed independently do not.
        (Value::Fn(a), Value::Fn(b)) => Rc::ptr_eq(a, b),
        // Structural equality for user structs: full type
        // identity (module_path + type_name) AND every field
        // equal in declaration order. Two structs with the same
        // name declared in different modules deliberately compare
        // as *not equal* — they're distinct types.
        (Value::Struct(a), Value::Struct(b)) => {
            a.module_path() == b.module_path()
                && a.type_name() == b.type_name()
                && a.fields().len() == b.fields().len()
                && a.fields()
                    .iter()
                    .zip(b.fields().iter())
                    .all(|((ka, va), (kb, vb))| ka == kb && values_equal(va, vb))
        }
        // Enum variants: same full type identity (module_path +
        // type_name), same variant name, same payload shape +
        // structural equality on payload items.
        (Value::EnumVariant(a), Value::EnumVariant(b)) => {
            a.module_path() == b.module_path()
                && a.type_name() == b.type_name()
                && a.variant() == b.variant()
                && match (a.payload(), b.payload()) {
                    (EnumPayload::Unit, EnumPayload::Unit) => true,
                    (EnumPayload::Tuple(ax), EnumPayload::Tuple(bx)) => {
                        ax.len() == bx.len()
                            && ax.iter().zip(bx.iter()).all(|(x, y)| values_equal(x, y))
                    }
                    (EnumPayload::Struct(af), EnumPayload::Struct(bf)) => {
                        af.len() == bf.len()
                            && af
                                .iter()
                                .zip(bf.iter())
                                .all(|((ka, va), (kb, vb))| ka == kb && values_equal(va, vb))
                    }
                    _ => false,
                }
        }
        _ => false,
    }
}

#[cfg(test)]
mod equality_tests {
    use super::*;

    fn dict(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        Value::new_dict(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect(),
        )
    }

    #[test]
    fn dict_equality_is_insertion_order_independent() {
        let left = dict([("first", Value::Int(1)), ("second", Value::Number(2.5))]);
        let right = dict([("second", Value::Number(2.5)), ("first", Value::Int(1))]);

        assert!(values_equal(&left, &right));
        assert!(values_equal(&right, &left));
    }

    #[test]
    fn dict_equality_rejects_different_keys_values_and_lengths() {
        let expected = dict([("first", Value::Int(1)), ("second", Value::Int(2))]);

        assert!(!values_equal(
            &expected,
            &dict([("first", Value::Int(1)), ("other", Value::Int(2))])
        ));
        assert!(!values_equal(
            &expected,
            &dict([("first", Value::Int(1)), ("second", Value::Int(3))])
        ));
        assert!(!values_equal(&expected, &dict([("first", Value::Int(1))])));
    }

    #[test]
    fn dict_equality_preserves_cross_type_numeric_values() {
        let integers = dict([("positive", Value::Int(1)), ("negative", Value::Int(-7))]);
        let numbers = dict([
            ("negative", Value::Number(-7.0)),
            ("positive", Value::Number(1.0)),
        ]);
        let different = dict([
            ("negative", Value::Number(-7.5)),
            ("positive", Value::Number(1.0)),
        ]);

        assert!(values_equal(&integers, &numbers));
        assert!(values_equal(&numbers, &integers));
        assert!(!values_equal(&integers, &different));
    }

    #[test]
    fn dict_equality_remains_structural_for_nested_values() {
        let left = dict([
            (
                "array",
                Value::new_array(vec![Value::Int(1), Value::Number(2.0)]),
            ),
            (
                "dict",
                dict([("inner-a", Value::Bool(true)), ("inner-b", Value::None)]),
            ),
        ]);
        let equal = dict([
            (
                "dict",
                dict([("inner-b", Value::None), ("inner-a", Value::Bool(true))]),
            ),
            (
                "array",
                Value::new_array(vec![Value::Number(1.0), Value::Int(2)]),
            ),
        ]);
        let unequal = dict([
            (
                "dict",
                dict([("inner-b", Value::None), ("inner-a", Value::Bool(false))]),
            ),
            (
                "array",
                Value::new_array(vec![Value::Number(1.0), Value::Int(2)]),
            ),
        ]);

        assert!(values_equal(&left, &equal));
        assert!(!values_equal(&left, &unequal));
    }

    #[test]
    fn shared_dict_equality_preserves_non_reflexive_cases() {
        let ordinary = dict([("value", Value::Int(1))]);
        let ordinary_clone = ordinary.clone();
        assert!(values_equal(&ordinary, &ordinary_clone));

        let nan = dict([("value", Value::Number(f64::NAN))]);
        let nan_clone = nan.clone();
        assert!(!values_equal(&nan, &nan_clone));

        let duplicate = Value::new_dict(vec![
            ("same".into(), Value::Int(1)),
            ("same".into(), Value::Int(2)),
        ]);
        let duplicate_clone = duplicate.clone();
        assert!(!values_equal(&duplicate, &duplicate_clone));
    }

    #[test]
    fn distinct_dict_equality_uses_amortized_constant_time_key_lookups() {
        const ENTRY_COUNT: usize = 4_096;

        let left = Value::new_dict(
            (0..ENTRY_COUNT)
                .map(|index| (format!("key-{index}"), Value::Int(index as i64)))
                .collect(),
        );
        let right = Value::new_dict(
            (0..ENTRY_COUNT)
                .rev()
                .map(|index| (format!("key-{index}"), Value::Int(index as i64)))
                .collect(),
        );
        let Value::Dict(right_dict) = &right else {
            unreachable!()
        };
        right_dict.0.key_index.reset_probes();

        assert!(values_equal(&left, &right));

        let probes = right_dict.0.key_index.probes();
        assert!(
            (ENTRY_COUNT..ENTRY_COUNT * 16).contains(&probes),
            "equality should perform one indexed lookup per key, got {probes} probes"
        );
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;

    #[test]
    fn number_display_preserves_integral_and_non_finite_forms() {
        assert_eq!(format!("{}", Value::Number(1.0)), "1");
        assert_eq!(format!("{}", Value::Number(-0.0)), "0");
        assert_eq!(format!("{}", Value::Number(f64::NAN)), "NaN");
        assert_eq!(format!("{}", Value::Number(f64::INFINITY)), "inf");
        assert_eq!(format!("{}", Value::Number(f64::NEG_INFINITY)), "-inf");
    }

    #[test]
    fn builtin_iter_preserves_inherent_and_trait_next_calls() {
        fn assert_iterator<I: Iterator<Item = Value>>() {}
        assert_iterator::<NyblIter>();

        let value = Value::new_array_iter(vec![Value::Int(1), Value::Int(2)]);
        let Value::Iter(iter) = &value else {
            panic!("expected a built-in iterator");
        };
        let mut iter = iter.borrow_mut();
        assert!(matches!(NyblIter::next(&mut iter), Some(Value::Int(1))));
        assert!(matches!(Iterator::next(&mut *iter), Some(Value::Int(2))));
        assert!(NyblIter::next(&mut iter).is_none());
    }
}

#[cfg(test)]
mod host_value_tests {
    use super::*;

    #[test]
    fn host_values_are_opaque_identity_handles() {
        let value = HostValue::new("counter", 41_i64);
        let alias = value.clone();
        let distinct = HostValue::new("counter", 41_i64);

        assert!(value.is::<i64>());
        assert_eq!(value.downcast_ref::<i64>(), Some(&41));
        assert!(value.downcast_ref::<String>().is_none());
        assert!(value.ptr_eq(&alias));
        assert_eq!(value, alias);
        assert_ne!(value, distinct);
        assert_eq!(format!("{value}"), "<host counter>");
        assert_eq!(
            format!("{value:?}"),
            "HostValue { type_name: \"counter\", .. }"
        );

        let wrapped = Value::Host(value);
        assert_eq!(wrapped.type_name(), "counter");
        assert_eq!(wrapped.inspect(), "<host counter>");
        assert!(wrapped.is_truthy());
        assert_eq!(wrapped.ownership_depth(), 0);
        assert!(values_equal(&wrapped, &wrapped.clone()));
        assert!(!values_equal(&wrapped, &Value::new_host("counter", 41_i64)));
    }
}

#[cfg(test)]
mod module_type_export_tests {
    use super::*;

    #[test]
    fn direct_module_types_default_to_the_module_path() {
        let module = NyblModule::try_new(
            "leaf".to_string(),
            Vec::new(),
            vec![
                "Signal".to_string(),
                "Point".to_string(),
                "Signal".to_string(),
            ],
            1,
        )
        .unwrap();

        assert_eq!(module.type_origin("Point"), Some("leaf"));
        assert_eq!(module.type_origin("Signal"), Some("leaf"));
        assert_eq!(module.type_origin("Missing"), None);
        assert_eq!(module.types, ["Signal", "Point", "Signal"]);
    }

    #[test]
    fn facade_module_types_retain_declaration_origins() {
        let exports = NyblTypeExports::from_origins([
            ("Point".to_string(), "leaf".to_string()),
            ("Local".to_string(), "facade".to_string()),
            ("Point".to_string(), "replacement".to_string()),
        ]);
        let module =
            NyblModule::try_new_with_type_exports("facade".to_string(), Vec::new(), exports, 1)
                .unwrap();

        assert_eq!(module.type_origin("Point"), Some("replacement"));
        assert_eq!(module.type_origin("Local"), Some("facade"));
        assert!(module.has_type("Point"));
        assert_eq!(module.type_names().collect::<Vec<_>>(), ["Local", "Point"]);
        assert_eq!(module.types, ["Local", "Point"]);
    }
}

#[cfg(test)]
mod depth_tests {
    use super::*;
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    use crate::memory::{nybl_memory_init, nybl_memory_used};

    fn nested_array(depth: u16) -> Value {
        let mut value = Value::None;
        for _ in 0..depth {
            value = Value::try_new_array(vec![value], 7).expect("depth within limit");
        }
        value
    }

    fn assert_depth_error(result: Result<Value, NyblError>, line: u32) {
        let error = result.expect_err("construction should exceed the value depth limit");
        assert!(error.is_fatal);
        assert_eq!(error.line, Some(line));
        assert_eq!(error.message, VALUE_DEPTH_ERROR_MESSAGE);
    }

    #[test]
    fn maximum_depth_is_safe_for_recursive_value_operations() {
        let value = nested_array(MAX_VALUE_DEPTH);
        assert_eq!(value.ownership_depth(), MAX_VALUE_DEPTH);

        let cloned = value.clone();
        assert!(values_equal(&value, &cloned));
        let displayed = format!("{value}");
        assert_eq!(
            displayed.len(),
            "none".len() + usize::from(MAX_VALUE_DEPTH) * 2
        );
        assert_eq!(value.inspect(), displayed);

        assert_depth_error(Value::try_new_array(vec![value], 19), 19);
        drop(cloned);
    }

    #[cfg(any(feature = "std", not(feature = "no_std")))]
    #[test]
    fn compatibility_snapshots_preserve_shared_dags_without_retaining_receipts() {
        nybl_memory_init(usize::MAX);
        let mut value = Value::Int(0);
        for _ in 0..50 {
            value = Value::new_array(vec![value.clone(), value]);
        }
        let authoritative_bytes = nybl_memory_used();
        assert!(authoritative_bytes > 0);

        let bindings = BTreeMap::from([
            ("first".to_string(), value.clone()),
            ("second".to_string(), value.clone()),
        ]);
        let snapshots = Value::__compatibility_snapshot_bindings(&bindings, 0).unwrap();
        let Value::Array(first_root) = &snapshots[0].1 else {
            panic!("expected first root array")
        };
        let Value::Array(second_root) = &snapshots[1].1 else {
            panic!("expected second root array")
        };
        assert!(Rc::ptr_eq(&first_root.0, &second_root.0));
        let snapshot = snapshots[0].1.clone();
        assert_eq!(nybl_memory_used(), authoritative_bytes);
        drop(bindings);
        drop(value);
        assert_eq!(nybl_memory_used(), 0);

        let mut cursor = &snapshot;
        for remaining in (1..=50).rev() {
            let Value::Array(array) = cursor else {
                panic!("expected shared array DAG")
            };
            assert_eq!(array.len(), 2);
            if remaining > 1 {
                let Value::Array(left) = &array[0] else {
                    panic!("expected left array child")
                };
                let Value::Array(right) = &array[1] else {
                    panic!("expected right array child")
                };
                assert!(Rc::ptr_eq(&left.0, &right.0));
            } else {
                assert!(array.iter().all(|value| matches!(value, Value::Int(0))));
            }
            cursor = &array[0];
        }
    }

    #[test]
    fn every_recursive_owner_enforces_the_same_boundary() {
        let child = nested_array(MAX_VALUE_DEPTH - 1);

        assert_eq!(
            Value::try_new_dict(vec![("x".into(), child.clone())], 1)
                .unwrap()
                .ownership_depth(),
            MAX_VALUE_DEPTH
        );
        assert_eq!(
            Value::try_new_struct("m".into(), "S".into(), vec![("x".into(), child.clone())], 1)
                .unwrap()
                .ownership_depth(),
            MAX_VALUE_DEPTH
        );
        assert_eq!(
            Value::try_new_enum_tuple("m".into(), "E".into(), "V".into(), vec![child.clone()], 1,)
                .unwrap()
                .ownership_depth(),
            MAX_VALUE_DEPTH
        );
        assert_eq!(
            Value::try_new_enum_struct(
                "m".into(),
                "E".into(),
                "V".into(),
                vec![("x".into(), child.clone())],
                1,
            )
            .unwrap()
            .ownership_depth(),
            MAX_VALUE_DEPTH
        );
        assert_eq!(
            Value::try_new_fn(
                Vec::new(),
                vec![("x".into(), child.clone())],
                Vec::new(),
                None,
                1,
            )
            .unwrap()
            .ownership_depth(),
            MAX_VALUE_DEPTH
        );
        assert_eq!(
            Value::try_new_compiled_fn(
                Vec::new(),
                Vec::new(),
                Rc::new(()),
                None,
                MAX_VALUE_DEPTH - 1,
                1,
            )
            .unwrap()
            .ownership_depth(),
            MAX_VALUE_DEPTH
        );
        assert_eq!(
            Value::new_module("m".into(), vec![("x".into(), child.clone())], Vec::new(), 1,)
                .unwrap()
                .ownership_depth(),
            MAX_VALUE_DEPTH
        );
        assert_eq!(
            Value::try_new_array_iter(vec![child], 1)
                .unwrap()
                .ownership_depth(),
            MAX_VALUE_DEPTH
        );

        assert_depth_error(
            Value::try_new_compiled_fn(
                Vec::new(),
                Vec::new(),
                Rc::new(()),
                None,
                MAX_VALUE_DEPTH,
                23,
            ),
            23,
        );
    }

    #[test]
    fn every_infallible_depth_checked_constructor_has_the_documented_panic() {
        fn assert_depth_panic(constructor: impl FnOnce() -> Value) {
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(constructor))
                .expect_err("compatibility constructor should panic at the depth cap");
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied());
            assert_eq!(message, Some(VALUE_DEPTH_ERROR_MESSAGE));
        }

        let child = nested_array(MAX_VALUE_DEPTH);

        assert_depth_panic(|| Value::new_array(vec![child.clone()]));
        assert_depth_panic(|| Value::new_dict(vec![("x".into(), child.clone())]));
        assert_depth_panic(|| {
            Value::new_struct("m".into(), "S".into(), vec![("x".into(), child.clone())])
        });
        assert_depth_panic(|| Value::new_array_iter(vec![child.clone()]));
        assert_depth_panic(|| {
            Value::new_enum_tuple("m".into(), "E".into(), "V".into(), vec![child.clone()])
        });
        assert_depth_panic(|| {
            Value::new_enum_struct(
                "m".into(),
                "E".into(),
                "V".into(),
                vec![("x".into(), child.clone())],
            )
        });
        assert_depth_panic(|| {
            Value::new_fn(
                Vec::new(),
                vec![("x".into(), child.clone())],
                Vec::new(),
                None,
            )
        });
        assert_depth_panic(|| {
            Value::new_compiled_fn(Vec::new(), vec![("x".into(), child)], Rc::new(()), None)
        });
    }

    #[test]
    fn function_module_metadata_preserves_identity_without_rc_cycles() {
        let value = Value::try_new_module_fn(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some("helper".into()),
            "shared".into(),
            1,
        )
        .expect("module function");
        let Value::Fn(function) = &value else {
            unreachable!()
        };
        assert_eq!(function.module_path.as_deref(), Some("shared"));
        assert!(format!("{function:?}").contains("shared"));
        let weak = Rc::downgrade(function);

        let cloned = value.clone();
        assert!(values_equal(&value, &cloned));
        drop(value);
        assert!(weak.upgrade().is_some());
        drop(cloned);
        assert!(weak.upgrade().is_none());

        let plain =
            Value::try_new_fn(Vec::new(), Vec::new(), Vec::new(), None, 1).expect("plain function");
        let Value::Fn(plain) = &plain else {
            unreachable!()
        };
        assert_eq!(plain.module_path, None);
    }

    #[test]
    fn mutations_reject_atomically() {
        let child = nested_array(MAX_VALUE_DEPTH);

        let mut array = Value::try_new_array(vec![Value::Int(1)], 1).unwrap();
        let Value::Array(array_value) = &mut array else {
            unreachable!()
        };
        let error = array_value.try_set(0, child.clone(), 31).unwrap_err();
        assert!(error.is_fatal);
        assert!(matches!(array_value.first(), Some(Value::Int(1))));
        assert_eq!(array_value.0.depth, 1);

        let mut dict = Value::try_new_dict(vec![("x".into(), Value::Int(1))], 1).unwrap();
        let dict_snapshot = dict.clone();
        let Value::Dict(dict_value) = &mut dict else {
            unreachable!()
        };
        let original_backing = Rc::as_ptr(&dict_value.0);
        let original_key_bytes = dict_value.0.key_bytes;
        let original_index_entries = dict_value.0.key_index.entry_count();
        dict_value.try_set_key("x", child.clone(), 32).unwrap_err();
        dict_value.try_set_key("y", child.clone(), 32).unwrap_err();
        assert_eq!(Rc::as_ptr(&dict_value.0), original_backing);
        assert!(matches!(&dict_value[0].1, Value::Int(1)));
        assert_eq!(dict_value.len(), 1);
        assert_eq!(dict_value.0.depth, 1);
        assert_eq!(dict_value.0.depth_counts.child_depths, [0]);
        assert_eq!(dict_value.0.key_bytes, original_key_bytes);
        assert_eq!(dict_value.0.key_index.entry_count(), original_index_entries);
        assert_eq!(
            dict_value.0.key_index.get(&dict_value.0.entries, "x"),
            Some(0)
        );
        assert_eq!(dict_value.0.key_index.get(&dict_value.0.entries, "y"), None);
        assert!(matches!(
            dict_snapshot,
            Value::Dict(ref snapshot) if Rc::ptr_eq(&snapshot.0, &dict_value.0)
        ));

        let mut structure =
            Value::try_new_struct("m".into(), "S".into(), vec![("x".into(), Value::Int(1))], 1)
                .unwrap();
        let Value::Struct(struct_value) = &mut structure else {
            unreachable!()
        };
        struct_value.try_set_field("x", child, 33).unwrap_err();
        assert!(matches!(struct_value.field("x"), Some(Value::Int(1))));
        assert_eq!(struct_value.0.depth, 1);
    }

    #[test]
    fn array_mutations_keep_exact_sparse_depth_metadata() {
        let mut value = Value::try_new_array(vec![Value::Int(1)], 1).unwrap();
        let Value::Array(array) = &mut value else {
            unreachable!()
        };

        array.try_push(nested_array(4), 1).unwrap();
        array.try_push(nested_array(2), 1).unwrap();
        assert_eq!(array.0.depth, 5);
        assert_eq!(array.0.depth_counts.flat, 1);
        assert_eq!(array.0.depth_counts.nested.len(), 2);

        array.try_set(1, Value::Int(2), 1).unwrap();
        assert_eq!(array.0.depth, 3);
        assert_eq!(array.0.depth_counts.flat, 2);

        let removed = array.remove(2);
        assert_eq!(removed.ownership_depth(), 2);
        assert_eq!(array.0.depth, 1);
        assert!(array.0.depth_counts.nested.is_empty());

        assert!(matches!(array.pop(), Some(Value::Int(2))));
        assert!(matches!(array.pop(), Some(Value::Int(1))));
        assert!(array.is_empty());
        assert_eq!(array.0.depth, 1);
        assert_eq!(array.0.depth_counts.flat, 0);
    }

    #[test]
    fn dict_mutations_keep_exact_sparse_depth_metadata_and_insertion_order() {
        let mut value = Value::try_new_dict(
            vec![
                ("flat".into(), Value::Int(1)),
                ("deep".into(), nested_array(4)),
                ("middle".into(), nested_array(2)),
            ],
            1,
        )
        .unwrap();
        let Value::Dict(dict) = &mut value else {
            unreachable!()
        };

        assert_eq!(dict.0.depth, 5);
        assert_eq!(dict.0.depth_counts.flat, 1);
        assert_eq!(dict.0.depth_counts.nested.len(), 2);
        assert_eq!(dict.0.depth_counts.child_depths, [0, 4, 2]);

        dict.try_set_key("deep", Value::Int(2), 1).unwrap();
        assert_eq!(dict.0.depth, 3);
        assert_eq!(dict.0.depth_counts.flat, 2);
        assert_eq!(dict.0.depth_counts.nested, [(2, 1)]);
        assert_eq!(dict.0.depth_counts.child_depths, [0, 0, 2]);

        dict.try_set_key("flat", nested_array(2), 1).unwrap();
        assert_eq!(dict.0.depth_counts.flat, 1);
        assert_eq!(dict.0.depth_counts.nested, [(2, 2)]);
        assert_eq!(dict.0.depth_counts.child_depths, [2, 0, 2]);

        dict.try_set_key("tail", nested_array(5), 1).unwrap();
        assert_eq!(dict.0.depth, 6);
        assert_eq!(dict.0.depth_counts.child_depths, [2, 0, 2, 5]);
        assert_eq!(
            dict.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
            ["flat", "deep", "middle", "tail"]
        );
    }

    #[test]
    fn array_growth_rejects_deep_values_without_partial_mutation() {
        let child = nested_array(MAX_VALUE_DEPTH);
        let mut value = Value::try_new_array(vec![Value::Int(1)], 1).unwrap();
        let Value::Array(array) = &mut value else {
            unreachable!()
        };

        let error = array.try_push(child.clone(), 41).unwrap_err();
        assert!(error.is_fatal);
        assert_eq!(error.line, Some(41));
        assert_eq!(error.message, VALUE_DEPTH_ERROR_MESSAGE);
        assert_eq!(array.len(), 1);
        assert!(matches!(array.first(), Some(Value::Int(1))));
        assert_eq!(array.0.depth, 1);

        let error = array.try_insert(0, child, 42).unwrap_err();
        assert!(error.is_fatal);
        assert_eq!(error.line, Some(42));
        assert_eq!(array.len(), 1);
        assert!(matches!(array.first(), Some(Value::Int(1))));
        assert_eq!(array.0.depth, 1);
    }

    #[test]
    fn array_mutation_uses_cached_depth_while_iterator_is_borrowed() {
        let iterator = Value::new_array_iter(vec![Value::Int(1)]);
        let handle = match &iterator {
            Value::Iter(handle) => Rc::clone(handle),
            _ => unreachable!(),
        };
        let mut value = Value::try_new_array(vec![iterator], 1).unwrap();
        let borrow = handle.borrow_mut();
        let Value::Array(array) = &mut value else {
            unreachable!()
        };
        let popped = array.pop().expect("iterator should pop");
        assert!(matches!(popped, Value::Iter(_)));
        assert!(array.is_empty());
        assert_eq!(array.0.depth, 1);
        drop(borrow);

        let iterator = Value::new_array_iter(vec![Value::Int(1)]);
        let handle = match &iterator {
            Value::Iter(handle) => Rc::clone(handle),
            _ => unreachable!(),
        };
        let mut value = Value::try_new_array(vec![Value::Int(0), iterator], 1).unwrap();
        let borrow = handle.borrow_mut();
        let Value::Array(array) = &mut value else {
            unreachable!()
        };
        let removed = array.remove(1);
        assert!(matches!(removed, Value::Iter(_)));
        assert_eq!(array.len(), 1);
        assert!(matches!(array.first(), Some(Value::Int(0))));
        assert_eq!(array.0.depth, 1);
        drop(borrow);

        let iterator = Value::new_array_iter(vec![Value::Int(1)]);
        let handle = match &iterator {
            Value::Iter(handle) => Rc::clone(handle),
            _ => unreachable!(),
        };
        let mut value = Value::try_new_array(vec![iterator], 1).unwrap();
        let borrow = handle.borrow_mut();
        let Value::Array(array) = &mut value else {
            unreachable!()
        };
        array.try_set(0, Value::Int(9), 51).unwrap();
        assert!(matches!(array.first(), Some(Value::Int(9))));
        assert_eq!(array.0.depth, 1);
        drop(borrow);
    }

    #[test]
    fn dict_mutation_uses_cached_depth_while_iterator_is_borrowed() {
        let iterator = Value::new_array_iter(vec![Value::Int(1)]);
        let handle = match &iterator {
            Value::Iter(handle) => Rc::clone(handle),
            _ => unreachable!(),
        };
        let mut value = Value::try_new_dict(
            vec![
                ("target".into(), Value::Int(0)),
                ("borrowed".into(), iterator),
            ],
            1,
        )
        .unwrap();
        let borrow = handle.borrow_mut();
        let Value::Dict(dict) = &mut value else {
            unreachable!()
        };

        dict.try_set_key("target", nested_array(2), 51).unwrap();
        dict.try_set_key("inserted", Value::Int(2), 51).unwrap();
        dict.try_set_key("borrowed", Value::Int(3), 51).unwrap();

        assert_eq!(dict.0.depth, 3);
        assert_eq!(dict.0.depth_counts.child_depths, [2, 0, 0]);
        assert_eq!(
            dict.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
            ["target", "borrowed", "inserted"]
        );
        drop(borrow);
    }

    #[test]
    fn large_dict_updates_do_not_reinspect_existing_values() {
        const ENTRY_COUNT: usize = 4_096;
        const UPDATE_COUNT: usize = 4_096;

        let iterator = Value::new_array_iter(vec![Value::Int(1)]);
        let handle = match &iterator {
            Value::Iter(handle) => Rc::clone(handle),
            _ => unreachable!(),
        };
        let mut entries = Vec::with_capacity(ENTRY_COUNT);
        entries.push(("borrowed-sentinel".into(), iterator));
        for index in 1..ENTRY_COUNT - 1 {
            entries.push((format!("key-{index}"), Value::Int(index as i64)));
        }
        entries.push(("target".into(), Value::Int(0)));

        let mut value = Value::try_new_dict(entries, 1).unwrap();
        let borrow = handle.borrow_mut();
        let Value::Dict(dict) = &mut value else {
            unreachable!()
        };
        dict.0.key_index.reset_probes();

        // A full-value scan would call `ownership_depth` on the borrowed first
        // entry and reject the write; a linear key scan would compare all
        // 4,096 keys to find the final entry. Counted index probes establish
        // that neither path is used without relying on wall-clock timing.
        for update in 0..UPDATE_COUNT {
            let replacement = if update % 2 == 0 {
                nested_array(1)
            } else {
                Value::Int(update as i64)
            };
            dict.try_set_key("target", replacement, 61).unwrap();
        }

        assert_eq!(dict.len(), ENTRY_COUNT);
        assert_eq!(dict[0].0, "borrowed-sentinel");
        assert_eq!(dict[ENTRY_COUNT - 1].0, "target");
        assert_eq!(dict.0.depth, 2);
        assert_eq!(dict.0.depth_counts.child_depths.len(), ENTRY_COUNT);
        assert!(
            dict.0.key_index.probes() < UPDATE_COUNT * 8,
            "existing-key updates performed too many lookup probes: {}",
            dict.0.key_index.probes()
        );
        drop(borrow);
    }

    #[cfg(any(feature = "std", not(feature = "no_std")))]
    #[test]
    fn incremental_dict_construction_has_amortized_index_work_and_exact_accounting() {
        const ENTRY_COUNT: usize = 4_096;

        nybl_memory_init(usize::MAX);
        let mut value = Value::try_new_dict(Vec::new(), 1).unwrap();
        let Value::Dict(dict) = &mut value else {
            unreachable!()
        };
        let unique_backing = Rc::as_ptr(&dict.0);
        dict.0.key_index.reset_probes();

        for index in 0..ENTRY_COUNT {
            dict.try_set_key(&format!("key-{index}"), Value::Int(index as i64), 1)
                .unwrap();
        }

        assert_eq!(dict.len(), ENTRY_COUNT);
        assert_eq!(dict.0.key_index.entry_count(), ENTRY_COUNT);
        assert!(dict.0.key_index.slot_count() >= ENTRY_COUNT * 2);
        assert!(dict.0.key_index.slot_count().is_power_of_two());
        assert!(
            dict.0.key_index.rehash_moves() < ENTRY_COUNT * 2,
            "geometric rehashing moved too many entries: {}",
            dict.0.key_index.rehash_moves()
        );
        assert!(
            dict.0.key_index.probes() < ENTRY_COUNT * 16,
            "incremental construction performed too many lookup probes: {}",
            dict.0.key_index.probes()
        );
        assert_eq!(
            dict.0.key_bytes,
            dict.iter().map(|(key, _)| key.capacity()).sum()
        );
        assert_eq!(
            Rc::as_ptr(&dict.0),
            unique_backing,
            "unique incremental writes must not detach the dictionary backing"
        );
        assert_eq!(nybl_memory_used(), dict.0.tracked_bytes());

        drop(value);
        assert_eq!(nybl_memory_used(), 0);
    }

    #[test]
    fn duplicate_constructor_keys_keep_first_match_replacement_semantics() {
        let mut value = Value::try_new_dict(
            vec![
                ("same".into(), Value::Int(1)),
                ("same".into(), Value::Int(2)),
            ],
            1,
        )
        .unwrap();
        let Value::Dict(dict) = &mut value else {
            unreachable!()
        };

        assert_eq!(dict.0.key_index.entry_count(), 1);
        dict.try_set_key("same", Value::Int(3), 1).unwrap();
        assert!(matches!(dict[0].1, Value::Int(3)));
        assert!(matches!(dict[1].1, Value::Int(2)));
        assert_eq!(dict.len(), 2);
    }

    #[cfg(any(feature = "std", not(feature = "no_std")))]
    #[test]
    fn dict_compatibility_snapshots_copy_exact_depth_metadata() {
        nybl_memory_init(usize::MAX);
        let value = Value::try_new_dict(
            vec![
                ("flat".into(), Value::Int(1)),
                ("nested".into(), nested_array(3)),
            ],
            1,
        )
        .unwrap();
        let authoritative_bytes = nybl_memory_used();
        let snapshot_value = value.__compatibility_snapshot(1).unwrap();
        assert_eq!(
            nybl_memory_used(),
            authoritative_bytes,
            "compatibility snapshot must not retain the active account"
        );

        let (Value::Dict(original), Value::Dict(snapshot)) = (&value, &snapshot_value) else {
            unreachable!()
        };
        assert!(!Rc::ptr_eq(&original.0, &snapshot.0));
        assert_eq!(snapshot.0.depth, original.0.depth);
        assert_eq!(
            snapshot.0.depth_counts.child_depths,
            original.0.depth_counts.child_depths
        );
        assert_eq!(
            snapshot.0.depth_counts.nested,
            original.0.depth_counts.nested
        );
        assert_eq!(snapshot.0.depth_counts.flat, original.0.depth_counts.flat);
        assert_eq!(
            snapshot.0.key_index.get(&snapshot.0.entries, "nested"),
            Some(1)
        );
        assert_eq!(
            snapshot.0.key_bytes,
            snapshot.iter().map(|(key, _)| key.capacity()).sum()
        );

        drop(value);
        assert_eq!(nybl_memory_used(), 0);
        drop(snapshot_value);
        assert_eq!(nybl_memory_used(), 0);
    }

    #[test]
    fn array_reordering_keeps_child_depths_aligned() {
        let mut value =
            Value::try_new_array(vec![nested_array(2), Value::Int(1), nested_array(1)], 1).unwrap();
        let Value::Array(array) = &mut value else {
            unreachable!()
        };

        array.sort_by(|left, right| left.ownership_depth().cmp(&right.ownership_depth()));
        assert_eq!(array.0.depth_counts.child_depths, [0, 1, 2]);
        assert_eq!(
            array.iter().map(Value::ownership_depth).collect::<Vec<_>>(),
            array.0.depth_counts.child_depths
        );

        array.reverse();
        assert_eq!(array.0.depth_counts.child_depths, [2, 1, 0]);
        assert_eq!(
            array.iter().map(Value::ownership_depth).collect::<Vec<_>>(),
            array.0.depth_counts.child_depths
        );
    }

    #[cfg(any(feature = "std", not(feature = "no_std")))]
    #[test]
    fn array_cow_charges_once_and_unique_pushes_do_not_copy() {
        nybl_memory_init(usize::MAX);

        let mut items = Vec::with_capacity(8);
        items.extend([Value::Int(1), Value::Int(2), Value::Int(3)]);
        let mut value = Value::try_new_array(items, 1).unwrap();

        // Leave spare room in both the values buffer and its parallel depth
        // cache, then prove a unique push neither detaches nor allocates.
        let Value::Array(array) = &mut value else {
            unreachable!()
        };
        assert!(matches!(array.pop(), Some(Value::Int(3))));
        let unique_ptr = Rc::as_ptr(&array.0);
        let before_unique_push = nybl_memory_used();
        array.try_push(Value::Int(3), 1).unwrap();
        assert_eq!(Rc::as_ptr(&array.0), unique_ptr);
        assert_eq!(nybl_memory_used(), before_unique_push);

        // Recreate spare cache capacity before sharing. The first push through
        // the original handle must detach exactly once; the next push remains
        // on that detached allocation.
        assert!(matches!(array.pop(), Some(Value::Int(3))));
        let before_clone = nybl_memory_used();
        let snapshot = Value::Array(array.clone());
        assert_eq!(nybl_memory_used(), before_clone, "Rc clone must be O(1)");
        let shared_ptr = Rc::as_ptr(&array.0);

        array.try_push(Value::Int(4), 1).unwrap();
        assert_ne!(Rc::as_ptr(&array.0), shared_ptr);
        let detached_usage = nybl_memory_used();
        assert_eq!(
            detached_usage - before_clone,
            array.0.tracked_bytes(),
            "shared push must charge one detached backing store"
        );
        assert!(matches!(&snapshot, Value::Array(old) if old.len() == 2));

        let detached_ptr = Rc::as_ptr(&array.0);
        array.try_push(Value::Int(5), 1).unwrap();
        assert_eq!(Rc::as_ptr(&array.0), detached_ptr);
        assert_eq!(nybl_memory_used(), detached_usage);

        drop(snapshot);
        assert_eq!(nybl_memory_used(), array.0.tracked_bytes());
        drop(value);
        assert_eq!(nybl_memory_used(), 0);
    }

    #[cfg(any(feature = "std", not(feature = "no_std")))]
    #[test]
    fn dict_and_struct_detach_once_while_enum_clones_stay_shared() {
        nybl_memory_init(usize::MAX);

        let mut dict = Value::try_new_dict(vec![("x".into(), Value::Int(1))], 1).unwrap();
        let dict_clone = dict.clone();
        let after_dict_clone = nybl_memory_used();
        let Value::Dict(dict_value) = &mut dict else {
            unreachable!()
        };
        let old_dict_ptr = Rc::as_ptr(&dict_value.0);
        dict_value.try_set_key("x", Value::Int(2), 1).unwrap();
        assert_ne!(Rc::as_ptr(&dict_value.0), old_dict_ptr);
        let after_dict_detach = nybl_memory_used();
        assert_eq!(
            after_dict_detach - after_dict_clone,
            dict_value.0.tracked_bytes()
        );
        dict_value.try_set_key("x", Value::Int(3), 1).unwrap();
        assert_eq!(nybl_memory_used(), after_dict_detach);
        assert!(matches!(&dict_clone, Value::Dict(old) if matches!(&old[0].1, Value::Int(1))));

        let mut structure =
            Value::try_new_struct("m".into(), "S".into(), vec![("x".into(), Value::Int(1))], 1)
                .unwrap();
        let before_struct_clone = nybl_memory_used();
        let struct_clone = structure.clone();
        assert_eq!(nybl_memory_used(), before_struct_clone);
        let Value::Struct(struct_value) = &mut structure else {
            unreachable!()
        };
        let old_struct_ptr = Rc::as_ptr(&struct_value.0);
        struct_value.try_set_field("x", Value::Int(2), 1).unwrap();
        assert_ne!(Rc::as_ptr(&struct_value.0), old_struct_ptr);
        let after_struct_detach = nybl_memory_used();
        struct_value.try_set_field("x", Value::Int(3), 1).unwrap();
        assert_eq!(nybl_memory_used(), after_struct_detach);
        assert!(
            matches!(struct_clone, Value::Struct(ref old) if matches!(old.field("x"), Some(Value::Int(1))))
        );

        let variant = Value::try_new_enum_struct(
            "m".into(),
            "E".into(),
            "V".into(),
            vec![("x".into(), Value::Int(1))],
            1,
        )
        .unwrap();
        let before_enum_clone = nybl_memory_used();
        let variant_clone = variant.clone();
        assert_eq!(nybl_memory_used(), before_enum_clone);
        assert!(matches!(
            (&variant, &variant_clone),
            (Value::EnumVariant(a), Value::EnumVariant(b)) if Rc::ptr_eq(&a.0, &b.0)
        ));

        drop(variant_clone);
        drop(variant);
        drop(struct_clone);
        drop(structure);
        drop(dict_clone);
        drop(dict);
        assert_eq!(nybl_memory_used(), 0);
    }

    #[cfg(any(feature = "std", not(feature = "no_std")))]
    #[test]
    fn shared_dict_insertion_keeps_cloned_key_and_index_accounting_exact() {
        nybl_memory_init(usize::MAX);

        let mut key = String::with_capacity(64);
        key.push_str("existing");
        let mut value = Value::try_new_dict(vec![(key, Value::Int(1))], 1).unwrap();
        let snapshot = value.clone();
        let before_detach = nybl_memory_used();
        let Value::Dict(dict) = &mut value else {
            unreachable!()
        };
        let shared_backing = Rc::as_ptr(&dict.0);

        dict.try_set_key("inserted", Value::Int(2), 1).unwrap();
        assert_ne!(Rc::as_ptr(&dict.0), shared_backing);
        assert_eq!(
            dict.0.key_bytes,
            dict.iter().map(|(key, _)| key.capacity()).sum()
        );
        let Value::Dict(snapshot_dict) = &snapshot else {
            unreachable!()
        };
        assert_eq!(
            nybl_memory_used() - before_detach,
            dict.0.tracked_bytes(),
            "shared insertion must charge exactly one detached backing"
        );
        assert_eq!(
            nybl_memory_used(),
            dict.0.tracked_bytes() + snapshot_dict.0.tracked_bytes()
        );

        drop(value);
        drop(snapshot);
        assert_eq!(nybl_memory_used(), 0);
    }

    #[cfg(any(feature = "std", not(feature = "no_std")))]
    #[test]
    fn shared_container_dag_releases_storage_with_its_last_owner() {
        nybl_memory_init(usize::MAX);

        let child = Value::try_new_array(vec![Value::Int(1)], 1).unwrap();
        let outer = Value::try_new_array(vec![child.clone(), child.clone()], 1).unwrap();
        let used_once = nybl_memory_used();
        let outer_clone = outer.clone();
        assert_eq!(nybl_memory_used(), used_once);

        drop(child);
        drop(outer);
        assert_eq!(nybl_memory_used(), used_once);
        drop(outer_clone);
        assert_eq!(nybl_memory_used(), 0);
    }
}
