#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use alloc_import::collections::{BTreeMap, BTreeSet};

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc as alloc_import;
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std as alloc_import;

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::rc::Rc;

#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::rc::Rc;

use core::cell::RefCell;

use crate::builtins::{
    self, error, error_at, error_fatal_with_hint, error_with_hint, error_with_hint_at,
};
use crate::error::NyblError;
use crate::lexer::StringPart;
use crate::methods;
use crate::ops;
use crate::parser::*;
use crate::value::{EnumPayload, FnBody, NyblFn, NyblFnOrigin, NyblTypeExports, Value};
use crate::{NyblHost, NyblLimits};

/// What the tree-walker stores for each imported module once it
/// has been loaded. Cached by dot-joined path so the same module
/// imported twice in one `run` only evaluates once.
///
/// `Loading` is the in-progress sentinel; if an import request
/// sees a module already in this state it's a circular import and
/// halts with a clear error.
enum ImportSlot {
    Loading,
    Loaded(Box<ModuleBindings>),
}

#[derive(Clone)]
struct ModuleBindings {
    /// Whether this module opted into an explicit `pub { ... }` allow-list.
    explicit_surface: bool,
    /// `(name, value)` pairs for every top-level `let` in the
    /// module (fns are handled separately — they also need to
    /// land in `self.functions` so cross-fn calls within the
    /// module resolve).
    bindings: Vec<(String, Value)>,
    binding_origins: BTreeMap<String, (String, String)>,
    /// Top-level `fn` declarations, keyed by name. The importer
    /// registers each both in its `self.functions` table
    /// (for nested call resolution) and as a scope binding
    /// (so the fn is also usable as a first-class value).
    fn_decls: Vec<(String, FunctionDefinition)>,
    /// Struct type declarations the module introduces, already
    /// qualified with their full identity `(module_path,
    /// type_name)`. The importer copies these into its own
    /// `struct_defs` without rewriting keys, so type identity
    /// stays pinned to the declaring module across the import
    /// boundary.
    struct_defs: Vec<((String, String), Vec<String>)>,
    /// Enum type declarations, qualified the same way as
    /// `struct_defs` above.
    enum_defs: Vec<((String, String), Vec<crate::parser::VariantDecl>)>,
    /// User methods the module declared, keyed by the *full
    /// type identity* of the receiver. `((module_path,
    /// type_name), method_name, fn_def)` — the importer merges
    /// these directly into its own `methods` table.
    methods: Vec<((String, String), String, FunctionDefinition)>,
    /// Public type names exposed by this module, retaining each type's
    /// declaration module across facade re-exports.
    type_exports: BTreeMap<String, String>,
    /// Exported value bindings that are themselves module namespaces. The
    /// original module object remains authoritative for path and surface.
    module_exports: BTreeMap<String, Rc<crate::value::NyblModule>>,
    /// Module-owned lexical metadata that named functions and methods need
    /// after the loading evaluator has gone away. Installed as a protected
    /// outer call frame so caller-local aliases/types cannot affect the
    /// callee, while the callee's own locals remain free to shadow it.
    lexical_context: Rc<ModuleLexicalContext>,
}

#[derive(Clone, Default)]
struct ModuleLexicalContext {
    type_bindings: Rc<BTreeMap<String, String>>,
    module_aliases: Rc<BTreeMap<String, Rc<crate::value::NyblModule>>>,
    imported_functions: Rc<BTreeMap<String, FunctionDefinition>>,
}

type ImportCache = Rc<RefCell<alloc_import::collections::BTreeMap<String, ImportSlot>>>;
type LiveValueEnvironments = Rc<RefCell<BTreeMap<String, BTreeMap<String, Value>>>>;
type BindingOrigin = (String, String);
type LiveBindingOrigins = Rc<RefCell<BTreeMap<String, BTreeMap<String, BindingOrigin>>>>;

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
        let authoritative = environments
            .get_mut(origin_module)
            .and_then(|origin| origin.remove(origin_binding));
        if let Some(value) = authoritative {
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

fn restore_failed_module_environment(
    environments: &LiveValueEnvironments,
    live_origins: &LiveBindingOrigins,
    module_path: &str,
    scopes: &mut [BTreeMap<String, Value>],
    binding_origins: &BTreeMap<String, BindingOrigin>,
) {
    let failed_environment = scopes.first_mut().map(core::mem::take).unwrap_or_default();
    put_live_environment(
        environments,
        module_path,
        failed_environment,
        binding_origins,
    );
    // Forwarded values have been returned to their authoritative origins. The
    // failed module's own partial state must not become cacheable.
    environments.borrow_mut().remove(module_path);
    live_origins.borrow_mut().remove(module_path);
}

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

const MAX_CALL_DEPTH: usize = 64;

// `try` unwinds via `NyblError::try_return_signal` — a proper
// sentinel with `is_try_return = true` rather than the older
// magic-string approach. The value travels on
// `Evaluator::pending_try_return` so the `NyblError` itself
// doesn't need to carry a `Value` (which would introduce a
// module cycle between `nybl::error` and `nybl::value`).

// ─── Control flow signals ──────────────────────────────────────────────────

enum Signal {
    None,
    Break,
    Continue,
    Return(Value),
}

/// An executed declaration's canonical callable.
///
/// Named functions and methods are immutable after their declaration site is
/// reached. Sharing the same callable through every registry, import, and
/// first-class value keeps the AST owned once instead of rebuilding it on each
/// lookup or call.
type FunctionDefinition = Rc<NyblFn>;

fn parameter_metadata(params: &[Parameter]) -> (Vec<String>, Vec<ParamMode>) {
    (
        params.iter().map(|param| param.name.clone()).collect(),
        params.iter().map(|param| param.mode).collect(),
    )
}

#[allow(clippy::too_many_arguments)]
fn function_definition(
    params: &[Parameter],
    body: &[Stmt],
    self_name: Option<String>,
    module_path: String,
    origin: NyblFnOrigin,
    line: u32,
) -> Result<FunctionDefinition, NyblError> {
    let (param_names, param_modes) = parameter_metadata(params);
    NyblFn::try_new_ast_in_module_with_origin_and_modes(
        param_names,
        param_modes,
        Vec::new(),
        body.to_vec(),
        self_name,
        Some(module_path),
        origin,
        line,
    )
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BindingIdentity {
    scope: usize,
    name: String,
}

#[derive(Clone)]
enum PlaceProjectionExpr {
    Index(Expr),
    Field(String),
}

#[derive(Clone)]
enum PlaceProjection {
    Index(Value),
    Field(String),
}

#[derive(Clone)]
struct PlacePlan {
    target: BindingIdentity,
    projections: Vec<PlaceProjectionExpr>,
}

struct ResolvedPlace {
    target: BindingIdentity,
    root: Value,
    projections: Vec<PlaceProjection>,
}

struct MethodReceiver {
    value: Value,
    place: Option<ResolvedPlace>,
}

/// Lexical free-variable analysis for walker-created lambdas.
///
/// The VM performs the same analysis while compiling a lambda. The walker
/// needs an explicit pass because evaluating a lambda only has the AST and
/// the current runtime scopes available. Declarations take effect in source
/// order, while control-flow bodies and match arms introduce child scopes.
struct FreeVariableCollector {
    scopes: Vec<alloc_import::collections::BTreeSet<String>>,
    free: alloc_import::collections::BTreeSet<String>,
}

impl FreeVariableCollector {
    fn for_lambda(params: &[Parameter]) -> Self {
        Self {
            scopes: vec![params.iter().map(|param| param.name.clone()).collect()],
            free: alloc_import::collections::BTreeSet::new(),
        }
    }

    fn finish(mut self, body: &[Stmt]) -> alloc_import::collections::BTreeSet<String> {
        self.visit_statements(body);
        self.free
    }

    fn is_bound(&self, name: &str) -> bool {
        self.scopes.iter().rev().any(|scope| scope.contains(name))
    }

    fn reference(&mut self, name: &str) {
        if !self.is_bound(name) {
            self.free.insert(name.to_string());
        }
    }

    fn declare(&mut self, name: &str) {
        self.scopes
            .last_mut()
            .expect("free-variable scope")
            .insert(name.to_string());
    }

    fn visit_scoped_block(&mut self, bindings: impl IntoIterator<Item = String>, body: &[Stmt]) {
        self.scopes.push(bindings.into_iter().collect());
        self.visit_statements(body);
        self.scopes.pop();
    }

    fn visit_statements(&mut self, statements: &[Stmt]) {
        for statement in statements {
            self.visit_statement(statement);
        }
    }

    fn visit_statement(&mut self, statement: &Stmt) {
        match &statement.kind {
            StmtKind::Let { name, value, .. } => {
                // The initializer runs before the new name is visible.
                self.visit_expr(value);
                self.declare(name);
            }
            StmtKind::Assign { target, value, .. } => {
                self.visit_expr(value);
                self.visit_assign_target(target);
            }
            StmtKind::If {
                condition,
                body,
                else_ifs,
                else_body,
            } => {
                self.visit_expr(condition);
                self.visit_scoped_block(core::iter::empty(), body);
                for (condition, body) in else_ifs {
                    self.visit_expr(condition);
                    self.visit_scoped_block(core::iter::empty(), body);
                }
                if let Some(body) = else_body {
                    self.visit_scoped_block(core::iter::empty(), body);
                }
            }
            StmtKind::While { condition, body } => {
                self.visit_expr(condition);
                self.visit_scoped_block(core::iter::empty(), body);
            }
            StmtKind::Repeat { count, body } => {
                self.visit_expr(count);
                self.visit_scoped_block(core::iter::empty(), body);
            }
            StmtKind::ForIn {
                var,
                iterable,
                body,
            } => {
                self.visit_expr(iterable);
                self.visit_scoped_block(core::iter::once(var.clone()), body);
            }
            // Named functions and methods have their own call-time scope and
            // are not closures in either the walker or VM.
            StmtKind::FnDecl { .. } | StmtKind::MethodDecl { .. } => {}
            StmtKind::Return { value } => {
                if let Some(value) = value {
                    self.visit_expr(value);
                }
            }
            StmtKind::Use { items, alias, .. } => {
                if let Some(alias) = alias {
                    self.declare(alias);
                } else if let Some(items) = items {
                    for item in items {
                        self.declare(item);
                    }
                }
            }
            StmtKind::ExprStmt(expr) => self.visit_expr(expr),
            StmtKind::Break
            | StmtKind::Continue
            | StmtKind::PublicSurface { .. }
            | StmtKind::StructDecl { .. }
            | StmtKind::EnumDecl { .. } => {}
        }
    }

    fn visit_assign_target(&mut self, target: &AssignTarget) {
        match target {
            AssignTarget::Variable(name) => self.reference(name),
            AssignTarget::Index { object, index } => {
                self.visit_expr(index);
                self.visit_expr(object);
            }
            AssignTarget::Field { object, .. } => self.visit_expr(object),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Int(_)
            | ExprKind::Number(_)
            | ExprKind::Str(_)
            | ExprKind::Bool(_)
            | ExprKind::None => {}
            ExprKind::StringInterp(parts) => {
                for part in parts {
                    if let StringPart::Variable(name) = part {
                        self.reference(name);
                    }
                }
            }
            ExprKind::Ident(name) => self.reference(name),
            ExprKind::BinaryOp { left, right, .. } => {
                self.visit_expr(left);
                self.visit_expr(right);
            }
            ExprKind::UnaryOp { expr, .. } | ExprKind::Try(expr) => self.visit_expr(expr),
            ExprKind::Call { callee, args } => {
                self.visit_expr(callee);
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                self.visit_expr(object);
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            ExprKind::FieldAccess { object, .. } => self.visit_expr(object),
            ExprKind::StructConstruct {
                namespace, fields, ..
            } => {
                if let Some(namespace) = namespace {
                    self.reference(namespace);
                }
                for (_, value) in fields {
                    self.visit_expr(value);
                }
            }
            ExprKind::EnumConstruct {
                namespace, payload, ..
            } => {
                if let Some(namespace) = namespace {
                    self.reference(namespace);
                }
                match payload {
                    VariantPayload::Unit => {}
                    VariantPayload::Tuple(values) => {
                        for value in values {
                            self.visit_expr(value);
                        }
                    }
                    VariantPayload::Struct(fields) => {
                        for (_, value) in fields {
                            self.visit_expr(value);
                        }
                    }
                }
            }
            ExprKind::Index { object, index } => {
                self.visit_expr(object);
                self.visit_expr(index);
            }
            ExprKind::Array(values) => {
                for value in values {
                    self.visit_expr(value);
                }
            }
            ExprKind::Dict(entries) => {
                for (_, value) in entries {
                    self.visit_expr(value);
                }
            }
            ExprKind::IfExpr {
                condition,
                then_expr,
                else_expr,
            } => {
                self.visit_expr(condition);
                self.visit_expr(then_expr);
                self.visit_expr(else_expr);
            }
            ExprKind::Lambda { params, body } => {
                let nested = FreeVariableCollector::for_lambda(params).finish(body);
                for name in nested {
                    self.reference(&name);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.visit_expr(scrutinee);
                for arm in arms {
                    for namespace in arm.pattern.namespace_names() {
                        self.reference(&namespace);
                    }
                    let bindings = arm.pattern.binding_names();
                    self.scopes.push(bindings);
                    if let Some(guard) = &arm.guard {
                        self.visit_expr(guard);
                    }
                    self.visit_expr(&arm.body);
                    self.scopes.pop();
                }
            }
        }
    }
}

// ─── Evaluator ─────────────────────────────────────────────────────────────

pub struct Evaluator<'h, H: NyblHost + ?Sized> {
    scopes: Vec<BTreeMap<String, Value>>,
    functions: BTreeMap<String, FunctionDefinition>,
    binding_origins: BTreeMap<String, (String, String)>,
    /// Executed direct root declarations, with final-site replacement and an
    /// immutable target separate from ordinary runtime function lookup.
    abi_declarations: Vec<(String, Visibility, Rc<NyblFn>)>,
    live_value_environments: LiveValueEnvironments,
    live_binding_origins: LiveBindingOrigins,
    active_value_module: String,
    function_origin: NyblFnOrigin,
    /// Module this evaluator is running. Used to tag newly
    /// declared types with the module they live in, so two
    /// modules that declare `struct Color { ... }` produce
    /// distinct types rather than colliding by name. `<root>`
    /// for the top-level program, the dot-joined module path
    /// for a sub-evaluator loading a `use`'d module.
    current_module: String,
    /// User-defined struct types, keyed by their full identity
    /// `(module_path, type_name)` — the same pair the runtime
    /// values carry. Two modules declaring the same struct name
    /// coexist at different keys with independently-validated
    /// field lists.
    struct_defs: BTreeMap<(String, String), Vec<String>>,
    /// User-defined enum types, same `(module_path, type_name)`
    /// keying scheme as [`Self::struct_defs`].
    enum_defs: BTreeMap<(String, String), Vec<VariantDecl>>,
    /// User-defined methods. Outer key is the *full type
    /// identity* `(module_path, type_name)`; inner key is the
    /// method name. Methods receive the receiver as their first
    /// parameter (conventionally `self`). Dispatch looks up the
    /// receiver's own `(module_path, type_name)` so a method
    /// declared for `paint.Color` isn't accidentally called on
    /// `other.Color`.
    methods: BTreeMap<(String, String), BTreeMap<String, FunctionDefinition>>,
    /// Public type projection for the module currently being evaluated.
    /// Unlike `type_bindings`, this excludes builtins, aliases, and nested
    /// declarations and is therefore safe to cache as the module's exports.
    type_exports: BTreeMap<String, String>,
    /// Per-scope bare-name → module_path bindings for types
    /// *and* module aliases. Parallels `scopes`: a declared
    /// type or an aliased `use` binds `name → module_path` in
    /// the current scope. Bare-name lookup (`Color::Red`,
    /// `m.Color::Red`) walks this stack inside-out. `<builtin>`
    /// types (`Result`, `RuntimeError`) are seeded into scope
    /// 0 at evaluator construction so they're visible
    /// everywhere.
    ///
    /// Unlike `scopes`, this stack is *preserved* across
    /// function calls — type identity follows lexical scoping
    /// at the module level, so a `fn` declared inside the root
    /// program can still match patterns against types + aliases
    /// that were visible at its declaration site. Function
    /// bodies push a fresh frame on entry so any locally-
    /// declared types are discarded on return; module-scope
    /// entries below stay put.
    type_bindings: Vec<BTreeMap<String, String>>,
    /// Module-level aliases `m → Rc<NyblModule>`, populated by
    /// aliased `use` statements. Separate from `scopes` so
    /// aliases remain reachable from inside function bodies
    /// (where `self.scopes` is fresh per call). Field access /
    /// method dispatch on `m.foo` falls back to this map when
    /// `m` isn't a local binding.
    module_aliases: Vec<BTreeMap<String, Rc<crate::value::NyblModule>>>,
    /// Callable exports introduced by non-aliased `use`, paired with lexical
    /// value scopes. This keeps direct calls fast without publishing a local
    /// import in the evaluator-wide named-function registry.
    imported_functions: Vec<BTreeMap<String, FunctionDefinition>>,
    /// Current module namespace, refreshed only by top-level declarations and
    /// imports. Calls clone this one Rc and keep mutable locals in ordinary
    /// per-call maps.
    root_lexical_context: Rc<ModuleLexicalContext>,
    active_lexical_contexts: Vec<Rc<ModuleLexicalContext>>,
    host: &'h mut H,
    steps: u64,
    /// Header lines of the source loops currently executing,
    /// outermost first. Spans function calls (a callee runs on the
    /// same evaluator) but not module imports (those get a fresh
    /// sub-evaluator, mirroring the VM's nested module VM). Read
    /// only when the step budget trips, so the error points at the
    /// outermost active loop's header rather than whichever
    /// statement the counter happened to cross the limit on — see
    /// `tick`.
    loop_lines: Vec<u32>,
    call_depth: usize,
    /// Defining module for each active function call. This keeps module-owned
    /// sibling lookup private to that function instead of leaking aliased
    /// functions into the caller-visible `functions` map.
    function_modules: Vec<String>,
    /// First alias-frame index visible to the active function, excluding the
    /// always-visible module/root frame at index 0.
    alias_scope_floors: Vec<usize>,
    limits: NyblLimits,
    memory: crate::memory::MemoryContext,
    rand_state: u64,
    /// Shared across nested evaluators so recursive imports see
    /// the same cache — every sub-evaluator inherits the parent's
    /// `Rc` clone in `new_nested`.
    imports: ImportCache,
    /// Paths already injected into *this* evaluator's scope (not
    /// shared with sub-evaluators). Re-importing the same path at
    /// the same level is a no-op — matches Python's `import x;
    /// import x` behaviour.
    imported_here: Vec<alloc_import::collections::BTreeSet<String>>,
    /// Set by `try` when it sees an `Err(...)` variant and wants
    /// the enclosing fn to early-return with that value. The
    /// expression that raised stuffs the value here and returns
    /// a sentinel `NyblError`; the fn-call wrapper detects the
    /// sentinel, takes the value, and converts it to
    /// `Signal::Return`. Always `None` outside the narrow
    /// unwinding window.
    pending_try_return: Option<Value>,
    /// Binding identities that are staged `ref` parameters in the active
    /// function frame. They may be forwarded, but a nested closure may not
    /// capture them.
    active_ref_bindings: alloc_import::collections::BTreeSet<BindingIdentity>,
    /// Captures seeded into the active closure frame. Explicit `ref`
    /// arguments may not target these second-class bindings.
    active_capture_bindings: alloc_import::collections::BTreeSet<BindingIdentity>,
    /// Non-fatal runtime warnings accumulated during execution —
    /// currently only `use`-time name-shadowing events (glob
    /// imports bringing in a name that's already bound). Nested
    /// module evaluators transfer theirs into this root queue.
    runtime_warnings: Vec<crate::error::NyblWarning>,
}

fn write_runtime_warnings(warnings: &mut Vec<crate::error::NyblWarning>) {
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    for warning in warnings.drain(..) {
        eprintln!("warning: {}", warning.message);
    }
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    warnings.clear();
}

impl<'h, H: NyblHost + ?Sized> Evaluator<'h, H> {
    pub fn new(host: &'h mut H, limits: NyblLimits) -> Self {
        let memory = crate::memory::MemoryContext::__new(limits.max_memory);
        let (struct_defs, enum_defs, builtin_bindings) = seed_builtin_types();
        let root_lexical_context = Rc::new(ModuleLexicalContext {
            type_bindings: Rc::new(builtin_bindings.clone()),
            module_aliases: Rc::new(BTreeMap::new()),
            imported_functions: Rc::new(BTreeMap::new()),
        });
        Self {
            scopes: vec![BTreeMap::new()],
            functions: BTreeMap::new(),
            binding_origins: BTreeMap::new(),
            abi_declarations: Vec::new(),
            live_value_environments: Rc::new(RefCell::new(BTreeMap::new())),
            live_binding_origins: Rc::new(RefCell::new(BTreeMap::new())),
            active_value_module: String::from(crate::value::ROOT_MODULE_PATH),
            function_origin: NyblFnOrigin::__instance("walker"),
            current_module: String::from(crate::value::ROOT_MODULE_PATH),
            struct_defs,
            enum_defs,
            methods: BTreeMap::new(),
            type_exports: BTreeMap::new(),
            type_bindings: vec![builtin_bindings],
            module_aliases: vec![BTreeMap::new()],
            imported_functions: vec![BTreeMap::new()],
            root_lexical_context,
            active_lexical_contexts: Vec::new(),
            host,
            steps: 0,
            loop_lines: Vec::new(),
            call_depth: 0,
            function_modules: Vec::new(),
            alias_scope_floors: Vec::new(),
            limits,
            memory,
            rand_state: 0,
            imports: Rc::new(RefCell::new(alloc_import::collections::BTreeMap::new())),
            imported_here: vec![alloc_import::collections::BTreeSet::new()],
            pending_try_return: None,
            active_ref_bindings: alloc_import::collections::BTreeSet::new(),
            active_capture_bindings: alloc_import::collections::BTreeSet::new(),
            runtime_warnings: Vec::new(),
        }
    }

    /// Build a sub-evaluator for loading a module — inherits the
    /// parent's import cache, memory ceiling, and step budget, but
    /// runs with a fresh scope stack so module code can't see the
    /// importer's locals. `module_path` is the dot-joined name the
    /// module is being loaded under; types it declares tag their
    /// runtime values with this path.
    #[allow(clippy::too_many_arguments)]
    fn new_for_module(
        host: &'h mut H,
        limits: NyblLimits,
        imports: ImportCache,
        live_value_environments: LiveValueEnvironments,
        live_binding_origins: LiveBindingOrigins,
        function_origin: NyblFnOrigin,
        module_path: String,
        memory: crate::memory::MemoryContext,
    ) -> Self {
        let (struct_defs, enum_defs, builtin_bindings) = seed_builtin_types();
        let root_lexical_context = Rc::new(ModuleLexicalContext {
            type_bindings: Rc::new(builtin_bindings.clone()),
            module_aliases: Rc::new(BTreeMap::new()),
            imported_functions: Rc::new(BTreeMap::new()),
        });
        Self {
            scopes: vec![BTreeMap::new()],
            functions: BTreeMap::new(),
            binding_origins: BTreeMap::new(),
            abi_declarations: Vec::new(),
            live_value_environments,
            live_binding_origins,
            active_value_module: module_path.clone(),
            function_origin,
            current_module: module_path,
            struct_defs,
            enum_defs,
            methods: BTreeMap::new(),
            type_exports: BTreeMap::new(),
            type_bindings: vec![builtin_bindings],
            module_aliases: vec![BTreeMap::new()],
            imported_functions: vec![BTreeMap::new()],
            root_lexical_context,
            active_lexical_contexts: Vec::new(),
            host,
            steps: 0,
            loop_lines: Vec::new(),
            call_depth: 0,
            function_modules: Vec::new(),
            alias_scope_floors: Vec::new(),
            limits,
            memory,
            rand_state: 0,
            imports,
            imported_here: vec![alloc_import::collections::BTreeSet::new()],
            pending_try_return: None,
            active_ref_bindings: alloc_import::collections::BTreeSet::new(),
            active_capture_bindings: alloc_import::collections::BTreeSet::new(),
            runtime_warnings: Vec::new(),
        }
    }

    pub fn run(mut self, stmts: &[Stmt]) -> Result<(), NyblError> {
        let mut result = self.exec_block(stmts);
        if result.is_ok() && self.memory.__exceeded() {
            result = Err(error_fatal_with_hint(
                stmts.last().map_or(0, |stmt| stmt.line),
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            ));
        }
        // Surface warnings only after the root evaluator (including every
        // imported-module evaluator) has finished.
        write_runtime_warnings(&mut self.runtime_warnings);
        match result? {
            Signal::Break => {
                return Err(error(0, "break used outside of a loop"));
            }
            Signal::Continue => {
                return Err(error(0, "continue used outside of a loop"));
            }
            _ => {}
        }
        Ok(())
    }

    fn tick(&mut self, line: u32) -> Result<(), NyblError> {
        self.steps += 1;
        if self.steps > self.limits.max_steps {
            // Fatal — `try_call` must not swallow this, or the
            // sandbox invariant breaks.
            //
            // Which statement the budget trips on is an accident of
            // step-count phase (and differs from the VM's
            // per-instruction accounting), so report the outermost
            // active source loop's header instead — the VM does the
            // same, keeping the two engines' error lines aligned.
            // Outermost, not innermost: a finite inner loop inside
            // an infinite outer loop trips at an engine-dependent
            // phase (sometimes inside the inner loop, sometimes
            // between passes), but the outermost active loop is the
            // same either way.
            return Err(error_fatal_with_hint(
                self.loop_lines.first().copied().unwrap_or(line),
                "Your code took too many steps (possible infinite loop)",
                "Check your loops — make sure they have a condition that eventually stops them.",
            ));
        }
        if self.memory.__exceeded() {
            return Err(error_fatal_with_hint(
                line,
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            ));
        }
        self.host.on_tick()?;
        Ok(())
    }

    // ─── Scope ─────────────────────────────────────────────────────

    fn push_scope(&mut self) {
        self.scopes.push(BTreeMap::new());
        // Type bindings parallel the value scopes — same push /
        // pop rhythm keeps `use`-injected type names stack-scoped.
        self.type_bindings.push(BTreeMap::new());
        self.module_aliases.push(BTreeMap::new());
        self.imported_functions.push(BTreeMap::new());
        self.imported_here
            .push(alloc_import::collections::BTreeSet::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.type_bindings.pop();
        self.module_aliases.pop();
        self.imported_functions.pop();
        self.imported_here.pop();
    }

    fn module_alias(&self, name: &str) -> Option<&Rc<crate::value::NyblModule>> {
        let floor = self.alias_scope_floors.last().copied();
        self.module_aliases
            .iter()
            .enumerate()
            .rev()
            .filter(|(index, _)| floor.is_none_or(|floor| *index >= floor))
            .find_map(|(_, frame)| frame.get(name))
            .or_else(|| {
                self.active_lexical_contexts
                    .last()
                    .and_then(|context| context.module_aliases.get(name))
            })
    }

    fn imported_function(&self, name: &str) -> Option<&FunctionDefinition> {
        let floor = self.alias_scope_floors.last().copied();
        self.imported_functions
            .iter()
            .enumerate()
            .rev()
            .filter(|(index, _)| floor.is_none_or(|floor| *index >= floor))
            .find_map(|(_, frame)| frame.get(name))
            .or_else(|| {
                self.active_lexical_contexts
                    .last()
                    .and_then(|context| context.imported_functions.get(name))
            })
    }

    fn module_lexical_context(&self, module_path: &str) -> Rc<ModuleLexicalContext> {
        if module_path == self.current_module {
            return Rc::clone(&self.root_lexical_context);
        }
        let cache = self.imports.borrow();
        match cache.get(module_path) {
            Some(ImportSlot::Loaded(bindings)) => Rc::clone(&bindings.lexical_context),
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
        module: Option<Rc<crate::value::NyblModule>>,
    ) {
        let context = Rc::make_mut(&mut self.root_lexical_context);
        let aliases = Rc::make_mut(&mut context.module_aliases);
        if let Some(module) = module {
            aliases.insert(name, module);
        } else {
            aliases.remove(&name);
        }
    }

    fn publish_root_imported_function(&mut self, name: String, function: FunctionDefinition) {
        let context = Rc::make_mut(&mut self.root_lexical_context);
        Rc::make_mut(&mut context.imported_functions).insert(name, function);
    }

    fn is_module_top_scope(&self) -> bool {
        self.call_depth == 0 && self.scopes.len() == 1
    }

    /// Resolve a type reference to the module it was declared
    /// in. `namespace` is the explicit qualifier (from
    /// `m.Color::Red`) or `None` for bare names. For bare names
    /// the scope stack is walked inside-out; for namespaced
    /// references the alias is resolved via
    /// `validate_namespaced_type` and its backing module path
    /// returned. Returns `None` when no matching type is visible.
    fn resolve_type_ref(&self, namespace: Option<&str>, type_name: &str) -> Option<String> {
        resolve_type_in_evaluator_context(
            &self.scopes,
            &self.type_bindings,
            &self.module_aliases,
            self.alias_scope_floors.last().copied(),
            self.active_lexical_contexts.last().map(Rc::as_ref),
            namespace,
            type_name,
        )
    }

    /// Record a type declaration in the current module.
    /// Registers `(current_module, name)` in the appropriate
    /// table and binds the bare name in the current scope so
    /// subsequent references resolve to *this* module's version.
    /// Same-shape redeclarations in the same module are a no-op;
    /// different-shape ones error.
    fn bind_local_type(&mut self, name: &str) {
        if let Some(top) = self.type_bindings.last_mut() {
            top.insert(name.to_string(), self.current_module.clone());
        }
        if self.is_module_top_scope() {
            self.type_exports
                .insert(name.to_string(), self.current_module.clone());
            self.publish_root_type_binding(name.to_string(), self.current_module.clone());
        }
    }

    fn define(&mut self, name: String, value: Value) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name, value);
        }
    }

    fn get_var(&self, name: &str) -> Option<&Value> {
        for scope in self.scopes.iter().rev() {
            if let Some(val) = scope.get(name) {
                return Some(val);
            }
        }
        None
    }

    fn resolve_binding_identity(&self, name: &str) -> Option<BindingIdentity> {
        self.scopes
            .iter()
            .enumerate()
            .rev()
            .find(|(_, scope)| scope.contains_key(name))
            .map(|(scope, _)| BindingIdentity {
                scope,
                name: name.to_string(),
            })
    }

    fn binding_value(&self, target: &BindingIdentity) -> Option<Value> {
        self.scopes
            .get(target.scope)
            .and_then(|scope| scope.get(&target.name))
            .cloned()
    }

    fn pending_memory_error(&self, line: u32) -> Option<NyblError> {
        self.memory.__exceeded().then(|| {
            error_fatal_with_hint(
                line,
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            )
        })
    }

    fn decompose_place_expr(
        expr: &Expr,
        projections: &mut Vec<PlaceProjectionExpr>,
    ) -> Option<String> {
        match &expr.kind {
            ExprKind::Ident(name) => Some(name.clone()),
            ExprKind::Index { object, index } => {
                let root = Self::decompose_place_expr(object, projections)?;
                projections.push(PlaceProjectionExpr::Index((**index).clone()));
                Some(root)
            }
            ExprKind::FieldAccess { object, field } => {
                let root = Self::decompose_place_expr(object, projections)?;
                projections.push(PlaceProjectionExpr::Field(field.clone()));
                Some(root)
            }
            _ => None,
        }
    }

    fn preflight_place_expr(
        &self,
        expr: &Expr,
        position: usize,
        line: u32,
    ) -> Result<PlacePlan, NyblError> {
        let mut projections = Vec::new();
        let root = Self::decompose_place_expr(expr, &mut projections)
            .ok_or_else(|| crate::ref_params::invalid_ref_target(position, line))?;
        self.preflight_place_parts(root, projections, position, line)
    }

    fn read_place_plan(&self, expr: &Expr) -> Option<PlacePlan> {
        let mut projections = Vec::new();
        let root = Self::decompose_place_expr(expr, &mut projections)?;
        let target = self.resolve_binding_identity(&root)?;
        Some(PlacePlan {
            target,
            projections,
        })
    }

    fn validate_mutable_place(
        &self,
        place: &ResolvedPlace,
        position: usize,
        line: u32,
    ) -> Result<(), NyblError> {
        if crate::naming::is_constant_name(&place.target.name) {
            return Err(crate::error_messages::constant_mutation_error(
                &place.target.name,
                line,
            ));
        }
        if self.active_capture_bindings.contains(&place.target) {
            return Err(crate::ref_params::captured_ref_target(position, line));
        }
        Ok(())
    }

    fn preflight_place_parts(
        &self,
        root: String,
        projections: Vec<PlaceProjectionExpr>,
        position: usize,
        line: u32,
    ) -> Result<PlacePlan, NyblError> {
        if crate::naming::is_constant_name(&root) {
            return Err(crate::error_messages::constant_mutation_error(&root, line));
        }
        let target = self
            .resolve_binding_identity(&root)
            .ok_or_else(|| crate::ref_params::invalid_ref_target(position, line))?;
        if self.active_capture_bindings.contains(&target) {
            return Err(crate::ref_params::captured_ref_target(position, line));
        }
        Ok(PlacePlan {
            target,
            projections,
        })
    }

    fn assignment_place_parts(
        &self,
        root: String,
        projections: Vec<PlaceProjectionExpr>,
        line: u32,
    ) -> Result<PlacePlan, NyblError> {
        if crate::naming::is_constant_name(&root) {
            return Err(crate::error_messages::constant_mutation_error(&root, line));
        }
        let target = self.resolve_binding_identity(&root).ok_or_else(|| {
            error_with_hint(
                line,
                format!("Variable `{root}` doesn't exist yet"),
                format!("Use `let` to create a new variable: let {root} = ..."),
            )
        })?;
        // Unlike a `ref` call boundary, ordinary assignment may update the
        // closure's local captured snapshot during that invocation.
        Ok(PlacePlan {
            target,
            projections,
        })
    }

    fn preflight_ref_places(
        &self,
        args: &[CallArg],
        line: u32,
    ) -> Result<Vec<PlacePlan>, NyblError> {
        let mut places = Vec::new();
        let mut unique = alloc_import::collections::BTreeSet::new();
        for (index, arg) in args.iter().enumerate() {
            if arg.mode != ParamMode::Ref {
                continue;
            }
            let place = self.preflight_place_expr(&arg.value, index + 1, line)?;
            // A call stages and commits each root once. Rejecting two paths
            // into the same root keeps copy-out deterministic without an
            // alias analysis (`ref rows[0], ref rows[1]` is therefore an
            // intentional error).
            if !unique.insert(place.target.clone()) {
                return Err(crate::ref_params::duplicate_ref_target(line));
            }
            places.push(place);
        }
        Ok(places)
    }

    fn resolve_place(
        &mut self,
        plan: PlacePlan,
        position: usize,
        line: u32,
    ) -> Result<ResolvedPlace, NyblError> {
        let mut projections = Vec::with_capacity(plan.projections.len());
        for projection in plan.projections {
            projections.push(match projection {
                PlaceProjectionExpr::Index(index) => {
                    PlaceProjection::Index(self.eval_expr(&index)?)
                }
                PlaceProjectionExpr::Field(field) => PlaceProjection::Field(field),
            });
        }
        // Projection expressions may mutate the root. Snapshot only after
        // each projection has been evaluated so the resolved leaf observes
        // those effects (and so every expression still runs exactly once).
        let root = self
            .binding_value(&plan.target)
            .ok_or_else(|| crate::ref_params::invalid_ref_target(position, line))?;
        Ok(ResolvedPlace {
            target: plan.target,
            root,
            projections,
        })
    }

    fn place_value_from(
        &self,
        root: &Value,
        projections: &[PlaceProjection],
        line: u32,
    ) -> Result<Value, NyblError> {
        let mut value = root.clone();
        for projection in projections {
            value = match projection {
                PlaceProjection::Index(index) => {
                    ops::index_get_in(&value, index, line, &self.memory)?
                }
                PlaceProjection::Field(field) => match &value {
                    Value::Struct(struct_value) => {
                        struct_value.field(field).cloned().ok_or_else(|| {
                            error(
                                line,
                                crate::error_messages::struct_has_no_field(
                                    struct_value.type_name(),
                                    field,
                                ),
                            )
                        })?
                    }
                    other => {
                        return Err(error(
                            line,
                            crate::error_messages::cant_assign_field(field, other.type_name()),
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
        projections: &[PlaceProjection],
        value: Value,
        line: u32,
    ) -> Result<(), NyblError> {
        let Some((projection, tail)) = projections.split_first() else {
            *root = value;
            return Ok(());
        };
        if tail.is_empty() {
            return match projection {
                PlaceProjection::Index(index) => {
                    ops::index_set_in(root, index, value, line, &self.memory)
                }
                PlaceProjection::Field(field) => match root {
                    Value::Struct(struct_value) => {
                        let type_name = struct_value.type_name().to_string();
                        if struct_value.__try_set_field_in(field, value, line, &self.memory)? {
                            Ok(())
                        } else {
                            Err(error(
                                line,
                                crate::error_messages::struct_has_no_field(&type_name, field),
                            ))
                        }
                    }
                    other => Err(error(
                        line,
                        crate::error_messages::cant_assign_field(field, other.type_name()),
                    )),
                },
            };
        }
        let mut child = match projection {
            PlaceProjection::Index(index) => ops::index_get_in(root, index, line, &self.memory)?,
            PlaceProjection::Field(field) => match root {
                Value::Struct(struct_value) => {
                    struct_value.field(field).cloned().ok_or_else(|| {
                        error(
                            line,
                            crate::error_messages::struct_has_no_field(
                                struct_value.type_name(),
                                field,
                            ),
                        )
                    })?
                }
                other => {
                    return Err(error(
                        line,
                        crate::error_messages::cant_assign_field(field, other.type_name()),
                    ));
                }
            },
        };
        self.write_place_value(&mut child, tail, value, line)?;
        match projection {
            PlaceProjection::Index(index) => {
                ops::index_set_in(root, index, child, line, &self.memory)
            }
            PlaceProjection::Field(field) => match root {
                Value::Struct(struct_value) => {
                    let type_name = struct_value.type_name().to_string();
                    if struct_value.__try_set_field_in(field, child, line, &self.memory)? {
                        Ok(())
                    } else {
                        Err(error(
                            line,
                            crate::error_messages::struct_has_no_field(&type_name, field),
                        ))
                    }
                }
                other => Err(error(
                    line,
                    crate::error_messages::cant_assign_field(field, other.type_name()),
                )),
            },
        }
    }

    fn commit_ref_places(
        &mut self,
        places: &[ResolvedPlace],
        values: Vec<Value>,
        line: u32,
    ) -> Result<(), NyblError> {
        if places.len() != values.len()
            || places.iter().any(|place| {
                self.scopes
                    .get(place.target.scope)
                    .is_none_or(|scope| !scope.contains_key(&place.target.name))
            })
        {
            return Err(error(
                line,
                "a `ref` target stopped being available before commit",
            ));
        }
        let mut candidates = Vec::with_capacity(places.len());
        for (place, value) in places.iter().zip(values) {
            let mut root = place.root.clone();
            self.write_place_value(&mut root, &place.projections, value, line)?;
            candidates.push(root);
        }
        for (place, root) in places.iter().zip(candidates) {
            self.scopes[place.target.scope].insert(place.target.name.clone(), root);
        }
        Ok(())
    }

    fn get_var_value(&self, name: &str) -> Option<Value> {
        self.get_var(name).cloned().or_else(|| {
            self.module_alias(name)
                .map(|module| Value::Module(Rc::clone(module)))
        })
    }

    fn live_origin_value(&self, origin: &BindingOrigin) -> Option<Value> {
        if self.active_value_module == origin.0
            && let Some(value) = self
                .scopes
                .first()
                .and_then(|environment| environment.get(&origin.1))
        {
            return Some(value.clone());
        }
        if let Some(binding) = self
            .binding_origins
            .iter()
            .find_map(|(binding, candidate)| (candidate == origin).then_some(binding))
            && let Some(value) = self
                .scopes
                .first()
                .and_then(|environment| environment.get(binding))
        {
            return Some(value.clone());
        }
        self.live_value_environments
            .borrow()
            .get(&origin.0)
            .and_then(|environment| environment.get(&origin.1))
            .cloned()
    }

    fn module_binding(&self, module: &crate::value::NyblModule, name: &str) -> Option<Value> {
        // Establish export membership before consulting the defining module's
        // full live environment, which also contains private bindings.
        let exported = module.__binding(name)?;
        if module.path == self.active_value_module
            && let Some(value) = self
                .scopes
                .first()
                .and_then(|scope| scope.get(name))
                .cloned()
        {
            return Some(value);
        }
        let (origin_module, origin_binding) = module.__binding_origin(name);
        if origin_module == self.active_value_module
            && let Some(value) = self
                .scopes
                .first()
                .and_then(|scope| scope.get(&origin_binding))
                .cloned()
        {
            return Some(value);
        }
        if let Some(active_binding) =
            self.binding_origins
                .iter()
                .find_map(|(active_binding, active_origin)| {
                    (active_origin.0 == origin_module && active_origin.1 == origin_binding)
                        .then_some(active_binding)
                })
            && let Some(value) = self
                .scopes
                .first()
                .and_then(|scope| scope.get(active_binding))
        {
            return Some(value.clone());
        }
        Some(exported)
    }

    fn set_var(&mut self, name: &str, value: Value) -> bool {
        for scope in self.scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), value);
                return true;
            }
        }
        if self
            .active_lexical_contexts
            .last()
            .is_some_and(|context| context.module_aliases.contains_key(name))
        {
            self.scopes
                .first_mut()
                .expect("function call base scope")
                .insert(name.to_string(), value);
            return true;
        }
        false
    }

    // ─── "Did you mean?" candidate collectors ─────────────────
    //
    // Every name the user could reasonably have meant, gathered
    // into a single list so `nybl::suggest::did_you_mean` picks
    // the closest match. Separate methods for the "ident used
    // as a value" and "ident called as a function" paths since
    // the reachable sets differ (builtins are callable but
    // aren't scope values).

    /// Names reachable when an identifier is used as a value:
    /// any local from the enclosing scopes plus any top-level
    /// fn declaration (fns are first-class — `let g = some_fn`
    /// works, so they count as value-like).
    fn value_candidates_hint(&self, target: &str) -> Option<String> {
        let mut candidates: Vec<String> = Vec::new();
        for scope in &self.scopes {
            for k in scope.keys() {
                candidates.push(k.clone());
            }
        }
        for name in self.functions.keys() {
            candidates.push(name.clone());
        }
        crate::suggest::did_you_mean(target, candidates)
    }

    /// Names reachable in a call position: user fns, core
    /// builtins, plus `try_call`. Host builtins stay with the
    /// host's own `function_hint()` path — embedders often want
    /// to list theirs differently.
    fn callable_candidates_hint(&self, target: &str) -> Option<String> {
        let mut candidates: Vec<String> = self.functions.keys().cloned().collect();
        for builtin in crate::suggest::CORE_CALLABLE_BUILTINS {
            candidates.push((*builtin).to_string());
        }
        crate::suggest::did_you_mean(target, candidates)
    }

    /// Resolve a named function in the active function's defining module
    /// before falling back to caller-visible bindings. Loaded module artifacts
    /// are the module-scope environment: aliases therefore keep sibling calls
    /// working without exposing those siblings as bare names to the importer.
    fn lookup_function(&self, name: &str) -> Option<FunctionDefinition> {
        if let Some(module_path) = self.function_modules.last()
            && module_path != crate::value::ROOT_MODULE_PATH
        {
            let cache = self.imports.borrow();
            if let Some(ImportSlot::Loaded(bindings)) = cache.get(module_path)
                && let Some((_, function)) = bindings
                    .fn_decls
                    .iter()
                    .find(|(candidate, _)| candidate == name)
            {
                return Some(Rc::clone(function));
            }
        }
        if let Some(function) = self.imported_function(name) {
            return Some(Rc::clone(function));
        }
        let defining_module = self.function_modules.last().map(String::as_str);
        if let Some(module_path) = defining_module
            && let Some(function) = self
                .functions
                .get(name)
                .filter(|function| function.module_path.as_deref() == Some(module_path))
        {
            return Some(Rc::clone(function));
        }
        if defining_module.is_none_or(|path| path == self.current_module) {
            self.functions.get(name).map(Rc::clone)
        } else {
            None
        }
    }

    // ─── Statements ────────────────────────────────────────────────

    fn exec_block(&mut self, stmts: &[Stmt]) -> Result<Signal, NyblError> {
        for stmt in stmts {
            let signal = self.exec_stmt(stmt)?;
            match signal {
                Signal::None => {}
                other => return Ok(other),
            }
        }
        Ok(Signal::None)
    }

    fn exec_stmt(&mut self, stmt: &Stmt) -> Result<Signal, NyblError> {
        self.tick(stmt.line)?;

        match &stmt.kind {
            StmtKind::Let {
                name,
                value,
                is_const: _,
            } => {
                let val = self.eval_expr(value)?;
                let module = match &val {
                    Value::Module(module) => Some(Rc::clone(module)),
                    _ => None,
                };
                if self.is_module_top_scope()
                    && let Some(origin) = self.binding_origins.get(name).cloned()
                    && (origin.0 != self.current_module || origin.1 != name.as_str())
                    && let Some(previous) =
                        self.scopes.last_mut().and_then(|scope| scope.remove(name))
                {
                    self.live_value_environments
                        .borrow_mut()
                        .entry(origin.0)
                        .or_default()
                        .insert(origin.1, previous);
                }
                self.define(name.clone(), val);
                if self.is_module_top_scope() {
                    self.binding_origins
                        .insert(name.clone(), (self.current_module.clone(), name.clone()));
                }
                let published_module = module.as_ref().map(Rc::clone);
                let aliases = self.module_aliases.last_mut().expect("module alias scope");
                if let Some(module) = module {
                    aliases.insert(name.clone(), module);
                } else {
                    aliases.remove(name);
                }
                if self.is_module_top_scope() {
                    self.publish_root_module_alias(name.clone(), published_module);
                }
                Ok(Signal::None)
            }

            StmtKind::Assign { target, op, value } => {
                let new_val = self.eval_expr(value)?;
                self.exec_assign(target, op, new_val, stmt.line)?;
                Ok(Signal::None)
            }

            StmtKind::If {
                condition,
                body,
                else_ifs,
                else_body,
            } => {
                if self.eval_expr(condition)?.is_truthy() {
                    self.push_scope();
                    let sig = self.exec_block(body)?;
                    self.pop_scope();
                    return Ok(sig);
                }
                for (elif_cond, elif_body) in else_ifs {
                    if self.eval_expr(elif_cond)?.is_truthy() {
                        self.push_scope();
                        let sig = self.exec_block(elif_body)?;
                        self.pop_scope();
                        return Ok(sig);
                    }
                }
                if let Some(else_body) = else_body {
                    self.push_scope();
                    let sig = self.exec_block(else_body)?;
                    self.pop_scope();
                    return Ok(sig);
                }
                Ok(Signal::None)
            }

            StmtKind::While { condition, body } => {
                // The loop-line entry must come off on every exit —
                // including caught errors — so the body runs inside
                // a closure and the pop is unconditional.
                self.loop_lines.push(stmt.line);
                let result = (|| {
                    loop {
                        self.tick(stmt.line)?;
                        if !self.eval_expr(condition)?.is_truthy() {
                            break;
                        }
                        self.push_scope();
                        let sig = self.exec_block(body)?;
                        self.pop_scope();
                        match sig {
                            Signal::Break => break,
                            Signal::Continue => continue,
                            Signal::Return(v) => return Ok(Signal::Return(v)),
                            Signal::None => {}
                        }
                    }
                    Ok(Signal::None)
                })();
                self.loop_lines.pop();
                result
            }

            StmtKind::Repeat { count, body } => {
                let n = match self.eval_expr(count)? {
                    Value::Int(n) => n,
                    Value::Number(n) => n as i64,
                    other => {
                        return Err(error(
                            stmt.line,
                            format!("repeat needs a number, but got {}", other.type_name()),
                        ));
                    }
                };
                // Count evaluation stays outside the loop context —
                // it runs once, like the VM's `MakeRepeatCount`.
                self.loop_lines.push(stmt.line);
                let result = (|| {
                    for _ in 0..n.max(0) {
                        self.tick(stmt.line)?;
                        self.push_scope();
                        let sig = self.exec_block(body)?;
                        self.pop_scope();
                        match sig {
                            Signal::Break => break,
                            Signal::Continue => continue,
                            Signal::Return(v) => return Ok(Signal::Return(v)),
                            Signal::None => {}
                        }
                    }
                    Ok(Signal::None)
                })();
                self.loop_lines.pop();
                result
            }

            StmtKind::ForIn {
                var,
                iterable,
                body,
            } => {
                let val = self.eval_expr(iterable)?;
                // Fast path for the three built-in iterables:
                // materialise up-front and loop over the Vec
                // directly, skipping the method-dispatch cost of
                // the iterator protocol. Semantically identical
                // to `for x in v.iter()` for these types.
                if matches!(&val, Value::Array(_) | Value::Str(_) | Value::Dict(_)) {
                    let mut val = val;
                    let items: Vec<Value> = match &mut val {
                        Value::Array(arr) => arr.__take_in(&self.memory),
                        Value::Str(s) => s
                            .chars()
                            .map(|c| Value::__new_str_in(c.to_string(), &self.memory))
                            .collect(),
                        Value::Dict(d) => d
                            .iter()
                            .map(|(k, _)| Value::__new_str_in(k.clone(), &self.memory))
                            .collect(),
                        _ => unreachable!(),
                    };
                    self.loop_lines.push(stmt.line);
                    let result = (|| {
                        for item in items {
                            self.tick(stmt.line)?;
                            self.push_scope();
                            self.define(var.clone(), item);
                            let sig = self.exec_block(body)?;
                            self.pop_scope();
                            match sig {
                                Signal::Break => break,
                                Signal::Continue => continue,
                                Signal::Return(v) => return Ok(Signal::Return(v)),
                                Signal::None => {}
                            }
                        }
                        Ok(Signal::None)
                    })();
                    self.loop_lines.pop();
                    return result;
                }
                // Protocol path: anything else must either be an
                // iterator already (Value::Iter, or a user value
                // with a `.next()` method that returns
                // `Iter::Next/Done`) or an iterable — a value
                // whose `.iter()` method returns an iterator.
                // Primitives and callables that don't fit get a
                // clean "can't iterate over X" error instead of
                // a raw "no such method" surface from the
                // dispatcher below.
                let iterator = match &val {
                    Value::Iter(_) | Value::Struct(_) | Value::EnumVariant(_) => {
                        // Ask the value for an iterator. User
                        // structs typically implement `iter` to
                        // return either a built-in iterator or
                        // themselves.
                        self.call_method_full(&val, "iter", Vec::new(), stmt.line)?
                    }
                    other => {
                        return Err(error(
                            stmt.line,
                            crate::error_messages::cant_iterate_over(other.type_name()),
                        ));
                    }
                };
                // Iterator acquisition above stays outside the loop
                // context — it runs once, like the VM's `MakeIter`.
                self.loop_lines.push(stmt.line);
                let result = (|| {
                    loop {
                        self.tick(stmt.line)?;
                        let next_val =
                            self.call_method_full(&iterator, "next", Vec::new(), stmt.line)?;
                        let item = match unwrap_iter_result(&next_val) {
                            Some(IterStep::Next(v)) => v,
                            Some(IterStep::Done) => break,
                            None => {
                                return Err(error(
                                    stmt.line,
                                    format!(
                                        "`.next()` on a `for` iterator must return `Iter::Next(v)` or `Iter::Done`, got {}",
                                        next_val.inspect()
                                    ),
                                ));
                            }
                        };
                        self.push_scope();
                        self.define(var.clone(), item);
                        let sig = self.exec_block(body)?;
                        self.pop_scope();
                        match sig {
                            Signal::Break => break,
                            Signal::Continue => continue,
                            Signal::Return(v) => return Ok(Signal::Return(v)),
                            Signal::None => {}
                        }
                    }
                    Ok(Signal::None)
                })();
                self.loop_lines.pop();
                result
            }

            StmtKind::FnDecl {
                name,
                params,
                body,
                visibility,
            } => {
                let is_direct_root = self.is_module_top_scope()
                    && self.current_module == crate::value::ROOT_MODULE_PATH;
                let module_path = self
                    .function_modules
                    .last()
                    .cloned()
                    .unwrap_or_else(|| self.current_module.clone());
                let definition = function_definition(
                    params,
                    body,
                    Some(name.clone()),
                    module_path,
                    self.function_origin.clone(),
                    stmt.line,
                )?;
                self.functions.insert(name.clone(), Rc::clone(&definition));
                if is_direct_root {
                    self.abi_declarations
                        .retain(|(existing, _, _)| existing != name);
                    self.abi_declarations
                        .push((name.clone(), *visibility, definition));
                }
                Ok(Signal::None)
            }

            StmtKind::MethodDecl {
                type_name,
                method_name,
                params,
                body,
            } => {
                // Methods belong to a *specific* type identity —
                // `fn Color.area(self)` inside module `paint`
                // registers under `(paint, Color)` and doesn't
                // leak to `other.Color`. If the named type isn't
                // declared in this module, that's an error at
                // parse/emit consistency — fall back to the
                // current module as the owner.
                let type_key = (self.current_module.clone(), type_name.clone());
                let definition = function_definition(
                    params,
                    body,
                    None,
                    self.current_module.clone(),
                    self.function_origin.clone(),
                    stmt.line,
                )?;
                self.methods
                    .entry(type_key)
                    .or_default()
                    .insert(method_name.clone(), definition);
                Ok(Signal::None)
            }

            StmtKind::Return { value } => {
                let val = match value {
                    Some(expr) => self.eval_expr(expr)?,
                    None => Value::None,
                };
                Ok(Signal::Return(val))
            }

            StmtKind::Break => Ok(Signal::Break),
            StmtKind::Continue => Ok(Signal::Continue),

            StmtKind::Use { path, items, alias } => {
                self.exec_import(path, items.as_deref(), alias.as_deref(), stmt.line)?;
                Ok(Signal::None)
            }

            StmtKind::StructDecl { name, fields } => {
                // Reject duplicate field names at decl time so
                // downstream code doesn't have to re-check. Walker,
                // VM, and AOT all assume unique fields per struct.
                let mut seen = alloc_import::collections::BTreeSet::new();
                for f in fields {
                    if !seen.insert(f.clone()) {
                        return Err(error(
                            stmt.line,
                            format!("Struct `{name}` has duplicate field `{f}`"),
                        ));
                    }
                }
                // Type identity is `(current_module, name)` —
                // two different modules declaring the same name
                // coexist because their keys differ. A redecl
                // *in the same module* is a no-op when the shape
                // matches (mirrors the same-module idempotency
                // rule for re-imports) and a clash otherwise.
                let key = (self.current_module.clone(), name.clone());
                if let Some(existing) = self.struct_defs.get(&key) {
                    if existing == fields {
                        self.bind_local_type(name);
                        return Ok(Signal::None);
                    }
                    return Err(error(
                        stmt.line,
                        format!("Struct `{name}` is already declared"),
                    ));
                }
                self.struct_defs.insert(key, fields.clone());
                self.bind_local_type(name);
                Ok(Signal::None)
            }

            StmtKind::EnumDecl { name, variants } => {
                let key = (self.current_module.clone(), name.clone());
                if let Some(existing) = self.enum_defs.get(&key) {
                    // Same rule as `StructDecl` above — a
                    // same-module same-shape redeclaration is a
                    // no-op, anything else is a clash.
                    if variants_equivalent(existing, variants) {
                        self.bind_local_type(name);
                        return Ok(Signal::None);
                    }
                    return Err(error(
                        stmt.line,
                        format!("Enum `{name}` is already declared"),
                    ));
                }
                let mut seen_variants = alloc_import::collections::BTreeSet::new();
                for v in variants {
                    if !seen_variants.insert(v.name.clone()) {
                        return Err(error(
                            stmt.line,
                            format!("Enum `{}` has duplicate variant `{}`", name, v.name),
                        ));
                    }
                    if let VariantKind::Struct(fields) | VariantKind::Tuple(fields) = &v.kind {
                        let mut seen_fields = alloc_import::collections::BTreeSet::new();
                        for f in fields {
                            if !seen_fields.insert(f.clone()) {
                                return Err(error(
                                    stmt.line,
                                    format!(
                                        "Enum variant `{}::{}` has duplicate field `{}`",
                                        name, v.name, f
                                    ),
                                ));
                            }
                        }
                    }
                }
                self.enum_defs.insert(key, variants.clone());
                self.bind_local_type(name);
                Ok(Signal::None)
            }

            StmtKind::ExprStmt(expr) => {
                self.eval_expr(expr)?;
                Ok(Signal::None)
            }

            // Module export validation and filtering are handled by the
            // module surface pass. The declaration itself has no runtime
            // side effect.
            StmtKind::PublicSurface { .. } => Ok(Signal::None),
        }
    }

    /// Execute a `use` statement.
    ///
    /// Four shapes are dispatched from the parser:
    ///
    /// - `use foo`                 — glob: inject all public
    ///   (non-`_`-prefixed) exports into the caller's scope.
    ///   Name collisions emit a `NyblWarning` and the first
    ///   binding wins (already-present beats newcomer).
    /// - `use foo.{a, b}`          — selective: inject only the
    ///   listed names. Missing names raise a clear error. Names
    ///   that start with `_` are accepted when listed
    ///   explicitly (the selective form is how you opt-in to
    ///   private bindings).
    /// - `use foo as m`            — aliased: every export
    ///   (including `_`-prefixed) hangs off a `Value::Module`
    ///   bound as `m`. Access via `m.binding` or
    ///   `m.Type { ... }` / `m.Type::Variant(...)`.
    /// - `use foo.{a, b} as m`     — selective + aliased: same
    ///   filtering, and the resulting module only exposes the
    ///   listed names.
    ///
    /// Struct / enum / method declarations from the imported
    /// module still register globally on every form — types
    /// are first-come-first-served across the engine. Conflicts
    /// there produce the same "clashes with existing" error as
    /// before. (Qualified type names that genuinely disambiguate
    /// same-named types across modules are a future extension.)
    fn exec_import(
        &mut self,
        path: &str,
        items: Option<&[String]>,
        alias: Option<&str>,
        line: u32,
    ) -> Result<(), NyblError> {
        let refresh_root_context = self.is_module_top_scope();
        // Idempotent at the glob injection site: re-importing a
        // module already applied is a no-op. Aliased / selective
        // forms don't enter this cache — they always run, because
        // the same module imported with different shapes can
        // legitimately produce different scope effects.
        let is_plain_glob = items.is_none() && alias.is_none();
        if is_plain_glob
            && self
                .imported_here
                .last()
                .is_some_and(|imports| imports.contains(path))
        {
            return Ok(());
        }

        // A lazily evaluated dependency may import the module currently
        // executing this `use`. Park that active environment in the shared
        // registry so the nested evaluator moves the one authoritative value
        // set instead of consulting cached compatibility snapshots.
        let active_module = self.active_value_module.clone();
        let active_origins = self.binding_origins.clone();
        let active_environment = self
            .scopes
            .first_mut()
            .map(core::mem::take)
            .unwrap_or_default();
        self.live_binding_origins
            .borrow_mut()
            .insert(active_module.clone(), active_origins.clone());
        put_live_environment(
            &self.live_value_environments,
            &active_module,
            active_environment,
            &active_origins,
        );
        let loaded = self.load_module(path, line);
        let restored_environment = take_live_environment(
            &self.live_value_environments,
            &active_module,
            &active_origins,
        );
        if let Some(scope) = self.scopes.first_mut() {
            *scope = restored_environment;
        } else {
            self.scopes.push(restored_environment);
        }
        let bindings = loaded?;

        // Types always register under their *full identity*
        // `(module_path, type_name)`. That means two modules
        // declaring `struct Color { ... }` with different fields
        // coexist at different registry keys — no clash. Same-
        // identity reinsertion is a no-op; same key + different
        // shape would mean the same module got loaded twice with
        // a different source, which we treat as a hard error.
        for (key, fields) in &bindings.struct_defs {
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
        for (key, variants) in &bindings.enum_defs {
            if let Some(existing) = self.enum_defs.get(key) {
                if variants_equivalent(existing, variants) {
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
        for (type_key, method_name, fn_def) in &bindings.methods {
            let slot = self.methods.entry(type_key.clone()).or_default();
            slot.insert(method_name.clone(), fn_def.clone());
        }

        // Figure out which name-value pairs to surface based on
        // the (items, alias) combination. Fn declarations and
        // plain `let` bindings are threaded together so the
        // caller-visible order matches the module's declaration
        // order.
        let mut exports: Vec<(String, Value)> =
            Vec::with_capacity(bindings.fn_decls.len() + bindings.bindings.len());
        let mut fn_entries: BTreeMap<String, FunctionDefinition> = BTreeMap::new();
        for (name, fn_def) in &bindings.fn_decls {
            if bindings.bindings.iter().any(|(binding, _)| binding == name) {
                continue;
            }
            exports.push((name.clone(), Value::Fn(Rc::clone(fn_def))));
            fn_entries.insert(name.clone(), Rc::clone(fn_def));
        }
        for (name, value) in &bindings.bindings {
            exports.push((name.clone(), value.clone()));
        }
        // Module values and functions live in separate runtime tables, but a
        // single `use` projects one logical namespace. Sort the combined
        // surface so warning order is deterministic across all engines.
        exports.sort_by(|(left, _), (right, _)| left.cmp(right));

        // Selective filter: restrict to the listed names.
        // Missing names error loudly — a silent skip would make
        // typos hard to spot.
        if let Some(list) = items {
            let available: alloc_import::collections::BTreeSet<&str> =
                exports.iter().map(|(k, _)| k.as_str()).collect();
            for wanted in list {
                if !available.contains(wanted.as_str())
                    && !bindings.type_exports.contains_key(wanted)
                {
                    return Err(error(
                        line,
                        format!("`{wanted}` isn't exported from `{path}` (selective import)"),
                    ));
                }
            }
            let listed: alloc_import::collections::BTreeSet<String> =
                list.iter().cloned().collect();
            exports.retain(|(k, _)| listed.contains(k));
            fn_entries.retain(|name, _| listed.contains(name));
        }

        // Figure out which of the module's types the caller
        // should see *by bare name*. The selective form picks
        // exactly the listed names; the glob form takes
        // everything public; the aliased form never binds bare
        // names (the alias is the only way in).
        let exposed_types: BTreeMap<String, String> = match items {
            Some(list) => bindings
                .type_exports
                .iter()
                .filter(|(name, _)| list.iter().any(|item| item == *name))
                .map(|(name, origin)| (name.clone(), origin.clone()))
                .collect(),
            None => bindings
                .type_exports
                .iter()
                .filter(|(name, _)| {
                    alias.is_some() || bindings.explicit_surface || !crate::naming::is_private(name)
                })
                .map(|(name, origin)| (name.clone(), origin.clone()))
                .collect(),
        };

        if let Some(alias_name) = alias {
            // Aliased form: build a `Value::Module` and bind it
            // under the alias. The module carries its bindings
            // (let + fn) and the names of declared types so
            // namespaced constructors like `m.Entity { ... }`
            // can verify they're reaching for something the
            // module actually exports.
            if self
                .scopes
                .last()
                .map(|s| s.contains_key(alias_name))
                .unwrap_or(false)
                || self.functions.contains_key(alias_name)
                || self
                    .imported_functions
                    .last()
                    .is_some_and(|functions| functions.contains_key(alias_name))
            {
                return Err(error(
                    line,
                    format!("`{alias_name}` is already bound — can't use it as a module alias"),
                ));
            }
            // The callable values retain their defining module, so sibling
            // calls resolve through that module's cached bindings. Publishing
            // these entries in `self.functions` would make `helper()` visible
            // after `use module as alias`, violating the alias-only surface.
            let live_bindings = exports
                .iter()
                .map(|(name, _)| {
                    let origin = bindings
                        .binding_origins
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| (path.to_string(), name.clone()));
                    (name.clone(), origin)
                })
                .collect();
            let module_rc = crate::value::NyblModule::__try_new_live_with_type_exports(
                path.to_string(),
                exports,
                NyblTypeExports::from_origins(exposed_types),
                live_bindings,
                Rc::clone(&self.live_value_environments),
                line,
            )?;
            // Bind the alias three ways:
            //   1. as a Value::Module in the current value
            //      scope (for `m.helper(x)` style calls that
            //      happen directly at the callsite);
            //   2. in `module_aliases` so it survives the
            //      fresh-scope reset at function boundaries and
            //      stays reachable for field access inside fns;
            //   3. in `type_bindings` so patterns + construction
            //      can resolve `m.Type` inside function bodies
            //      without needing the Value::Module in value
            //      scope.
            self.define(alias_name.to_string(), Value::Module(Rc::clone(&module_rc)));
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
            // Glob / selective without alias — flat injection.
            // Privacy: glob-only drops `_`-prefixed names.
            // Selective lets the user reach into private
            // bindings explicitly.
            let skip_private = items.is_none() && !bindings.explicit_surface;
            let module_top_scope = self.is_module_top_scope();
            for (name, mut value) in exports {
                if skip_private && crate::naming::is_private(&name) {
                    continue;
                }
                if self
                    .scopes
                    .last()
                    .map(|s| s.contains_key(&name))
                    .unwrap_or(false)
                    || (module_top_scope && self.functions.contains_key(&name))
                {
                    if is_plain_glob {
                        self.runtime_warnings.push(crate::error::NyblWarning::at(
                            crate::error_messages::glob_shadow_warning(&name, path),
                            line,
                        ));
                    }
                    continue;
                }
                if let Some(fn_def) = fn_entries.remove(&name) {
                    self.imported_functions
                        .last_mut()
                        .expect("imported function scope")
                        .insert(name.clone(), Rc::clone(&fn_def));
                    if module_top_scope {
                        self.publish_root_imported_function(name.clone(), fn_def);
                    }
                }
                let module = bindings.module_exports.get(&name).cloned();
                if module_top_scope {
                    let origin = bindings
                        .binding_origins
                        .get(&name)
                        .cloned()
                        .unwrap_or_else(|| (path.to_string(), name.clone()));
                    self.binding_origins.insert(name.clone(), origin.clone());
                    if (origin.0 != self.active_value_module || origin.1 != name)
                        && let Some(authoritative) = self
                            .live_value_environments
                            .borrow_mut()
                            .get_mut(&origin.0)
                            .and_then(|environment| environment.remove(&origin.1))
                    {
                        value = authoritative;
                    }
                } else {
                    let origin = bindings
                        .binding_origins
                        .get(&name)
                        .cloned()
                        .unwrap_or_else(|| (path.to_string(), name.clone()));
                    if let Some(current) = self.live_origin_value(&origin) {
                        value = current;
                    }
                }
                self.define(name.clone(), value);
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
            // Bind the bare type names in the current scope's
            // type_bindings so subsequent `Color::Red` /
            // `Color { ... }` resolves to the module the name
            // came from. Shadowing here is silent (same as
            // values): first definition wins. Types without an
            // explicit bare binding remain reachable through
            // the alias form only.
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

    /// Resolve and evaluate a module, caching the result. Returns
    /// the module's exported bindings.
    fn load_module(&mut self, path: &str, line: u32) -> Result<ModuleBindings, NyblError> {
        // Fast path: already loaded.
        {
            let cache = self.imports.borrow();
            if let Some(ImportSlot::Loaded(bindings)) = cache.get(path) {
                return Ok((**bindings).clone());
            }
            if let Some(ImportSlot::Loading) = cache.get(path) {
                return Err(error(
                    line,
                    format!("Circular import: module `{path}` is still loading"),
                ));
            }
        }

        // Ask the host for source.
        let resolved = self.host.resolve_module(path);
        let source = match resolved {
            Some(Ok(s)) => s,
            Some(Err(e)) => return Err(e),
            None => {
                return Err(error(line, format!("Module `{path}` not found")));
            }
        };

        // Mark in-progress before we start evaluating so a
        // circular import surfaces as a clean error.
        self.imports
            .borrow_mut()
            .insert(path.to_string(), ImportSlot::Loading);

        let result = self
            .evaluate_module(path, &source, line)
            .map_err(|error| error.with_module_source(path, source.as_str()));

        match result {
            Ok(bindings) => {
                self.imports.borrow_mut().insert(
                    path.to_string(),
                    ImportSlot::Loaded(Box::new(bindings.clone())),
                );
                Ok(bindings)
            }
            Err(e) => {
                // Drop the Loading marker so a subsequent, non-
                // broken context could retry.
                self.imports.borrow_mut().remove(path);
                Err(e)
            }
        }
    }

    /// Parse and walk a module source in a fresh scope, returning
    /// its top-level bindings as a `ModuleBindings`. Reuses the
    /// parent evaluator's host, limits, and import cache.
    /// `module_path` is the dot-joined name the importer used —
    /// the sub-evaluator tags its own declared types with this
    /// path so runtime values keep a stable identity across the
    /// import boundary.
    fn evaluate_module(
        &mut self,
        module_path: &str,
        source: &str,
        line: u32,
    ) -> Result<ModuleBindings, NyblError> {
        let _ = line;
        let stmts = crate::parse(source)?;
        // Modules load lazily at their `use` site, so this is the
        // earliest point their source is visible — apply the same
        // disabled-builtin gate the root program got, before any of
        // the module's statements run.
        crate::check::enforce_disabled_builtins(&stmts, &self.limits.disabled_builtins)?;
        let public_surface: Option<BTreeSet<String>> = {
            let mut names = BTreeSet::new();
            let mut explicit = false;
            for stmt in &stmts {
                if let StmtKind::PublicSurface { names: listed } = &stmt.kind {
                    explicit = true;
                    names.extend(listed.iter().cloned());
                }
            }
            explicit.then_some(names)
        };
        let imports = Rc::clone(&self.imports);
        let live_value_environments = Rc::clone(&self.live_value_environments);
        let live_binding_origins = Rc::clone(&self.live_binding_origins);
        let limits = self.limits.clone();
        let mut sub = Evaluator::new_for_module(
            self.host,
            limits,
            imports,
            Rc::clone(&live_value_environments),
            Rc::clone(&live_binding_origins),
            self.function_origin.clone(),
            module_path.to_string(),
            self.memory.clone(),
        );
        sub.steps = self.steps;
        sub.rand_state = self.rand_state;
        // Run the module body to top — errors propagate as-is.
        let module_result = sub.exec_block(&stmts);
        self.steps = sub.steps;
        self.rand_state = sub.rand_state;
        // A module evaluator is an implementation detail of this root run.
        // Preserve its warnings (including transitive-module warnings) and
        // let only the root boundary write them to stderr.
        self.runtime_warnings.append(&mut sub.runtime_warnings);
        let module_signal = match module_result {
            Ok(signal) => signal,
            Err(module_error) => {
                restore_failed_module_environment(
                    &live_value_environments,
                    &live_binding_origins,
                    module_path,
                    &mut sub.scopes,
                    &sub.binding_origins,
                );
                return Err(module_error);
            }
        };
        match module_signal {
            Signal::Return(_) | Signal::None => {}
            Signal::Break => {
                restore_failed_module_environment(
                    &live_value_environments,
                    &live_binding_origins,
                    module_path,
                    &mut sub.scopes,
                    &sub.binding_origins,
                );
                return Err(error(0, "break used outside of a loop"));
            }
            Signal::Continue => {
                restore_failed_module_environment(
                    &live_value_environments,
                    &live_binding_origins,
                    module_path,
                    &mut sub.scopes,
                    &sub.binding_origins,
                );
                return Err(error(0, "continue used outside of a loop"));
            }
        }
        // Collect top-level `let` bindings from the module's
        // one remaining scope. Fns are handled separately so
        // the importer can register them in `self.functions`
        // as well as in the scope (see `exec_import`).
        let top_scope = sub
            .scopes
            .first_mut()
            .map(core::mem::take)
            .unwrap_or_default();
        let mut bindings = match snapshot_module_bindings(
            &top_scope,
            &sub.binding_origins,
            module_path,
            &self.imports,
            0,
        ) {
            Ok(bindings) => bindings,
            Err(snapshot_error) => {
                put_live_environment(
                    &self.live_value_environments,
                    module_path,
                    top_scope,
                    &sub.binding_origins,
                );
                self.live_value_environments
                    .borrow_mut()
                    .remove(module_path);
                self.live_binding_origins.borrow_mut().remove(module_path);
                return Err(snapshot_error);
            }
        };
        let mut binding_origins = sub.binding_origins;
        put_live_environment(
            &self.live_value_environments,
            module_path,
            top_scope,
            &binding_origins,
        );
        self.live_binding_origins
            .borrow_mut()
            .insert(module_path.to_string(), binding_origins.clone());
        let mut fn_decls: Vec<(String, FunctionDefinition)> = sub.functions.into_iter().collect();
        // Type decls and methods transfer with their full
        // identity. Engine builtins (`<builtin>` module path)
        // are seeded into every evaluator anyway, so we filter
        // those out here to avoid duplicating them in the
        // importer's merge step.
        let builtin_mp = crate::value::BUILTIN_MODULE_PATH;
        let struct_defs: Vec<((String, String), Vec<String>)> = sub
            .struct_defs
            .into_iter()
            .filter(|((mp, _), _)| mp != builtin_mp)
            .collect();
        let enum_defs: Vec<((String, String), Vec<crate::parser::VariantDecl>)> = sub
            .enum_defs
            .into_iter()
            .filter(|((mp, _), _)| mp != builtin_mp)
            .collect();
        let mut methods: Vec<((String, String), String, FunctionDefinition)> = Vec::new();
        for (type_key, by_method) in sub.methods {
            for (method_name, fn_def) in by_method {
                methods.push((type_key.clone(), method_name, fn_def));
            }
        }
        let mut type_exports = sub.type_exports;
        let all_module_exports: BTreeMap<String, Rc<crate::value::NyblModule>> = bindings
            .iter()
            .filter_map(|(name, value)| match value {
                Value::Module(module) => Some((name.clone(), Rc::clone(module))),
                _ => None,
            })
            .collect();
        let mut lexical_context = Rc::try_unwrap(core::mem::take(&mut sub.root_lexical_context))
            .unwrap_or_else(|context| (*context).clone());
        // Only exported module values are valid protected aliases after the
        // loading evaluator is gone. Type and function maps already reflect
        // every reached top-level publication incrementally.
        lexical_context.module_aliases = Rc::new(all_module_exports);
        let lexical_context = Rc::new(lexical_context);
        let explicit_surface = public_surface.is_some();
        if let Some(names) = public_surface {
            bindings.retain(|(name, _)| names.contains(name));
            binding_origins.retain(|name, _| names.contains(name));
            fn_decls.retain(|(name, _)| names.contains(name));
            type_exports.retain(|name, _| names.contains(name));
        }
        let module_exports: BTreeMap<String, Rc<crate::value::NyblModule>> = bindings
            .iter()
            .filter_map(|(name, value)| match value {
                Value::Module(module) => Some((name.clone(), Rc::clone(module))),
                _ => None,
            })
            .collect();
        Ok(ModuleBindings {
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

    /// Validate `ns.Type` — the caller wrote `ns.Type { ... }`
    /// or `ns.Type::Variant(...)`. `ns` must be a
    /// `Value::Module` in scope, and `Type` must appear in that
    /// module's exported type map. Construction and patterns use
    /// [`Self::resolve_type_ref`] to recover the declaration origin.
    fn validate_namespaced_type(
        &self,
        ns: &str,
        type_name: &str,
        line: u32,
    ) -> Result<(), NyblError> {
        // Prefer the local value scope so a shadowing binding
        // (`let p = 3` after `use paint as p`) is caught — but
        // fall back to the evaluator-level alias map so
        // namespaced references inside function bodies still
        // resolve.
        if let Some(v) = self.get_var(ns) {
            let module = match v {
                Value::Module(m) => m,
                _ => {
                    return Err(error(
                        line,
                        format!(
                            "`{}` is a {}, not a module alias — can't reach `{}` through it",
                            ns,
                            v.type_name(),
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

    fn eval_enum_construct(
        &mut self,
        namespace: Option<&str>,
        type_name: &str,
        variant: &str,
        payload: &VariantPayload,
        line: u32,
    ) -> Result<Value, NyblError> {
        // Resolve the type reference to its full
        // `(module_path, type_name)` identity before validating
        // the variant shape. `namespace` means the source wrote
        // `ns.Type::Variant(...)`; bare means we look the name up
        // in the current scope's type bindings.
        let module_path = match namespace {
            Some(ns) => {
                self.validate_namespaced_type(ns, type_name, line)?;
                // validate_namespaced_type guaranteed the alias
                // is a Module and the type is in its exports —
                // pick up the module path for the full identity.
                self.resolve_type_ref(Some(ns), type_name)
                    .unwrap_or_else(|| self.current_module.clone())
            }
            None => self
                .resolve_type_ref(None, type_name)
                .ok_or_else(|| error(line, crate::error_messages::enum_not_declared(type_name)))?,
        };
        let key = (module_path.clone(), type_name.to_string());
        let variants = self
            .enum_defs
            .get(&key)
            .ok_or_else(|| error(line, crate::error_messages::enum_not_declared(type_name)))?
            .clone();
        let decl = variants
            .iter()
            .find(|v| v.name == variant)
            .ok_or_else(|| {
                let msg = crate::error_messages::enum_has_no_variant(type_name, variant);
                let names = variants.iter().map(|v| v.name.as_str());
                match crate::suggest::did_you_mean(variant, names) {
                    Some(hint) => error_with_hint(line, msg, hint),
                    None => error(line, msg),
                }
            })?
            .clone();

        match (&decl.kind, payload) {
            (VariantKind::Unit, VariantPayload::Unit) => Ok(Value::__new_enum_unit_in(
                module_path,
                type_name.to_string(),
                variant.to_string(),
                &self.memory,
            )),
            (VariantKind::Tuple(fields), VariantPayload::Tuple(args)) => {
                if args.len() != fields.len() {
                    return Err(error(
                        line,
                        format!(
                            "`{}::{}` expects {} argument{}, but got {}",
                            type_name,
                            variant,
                            fields.len(),
                            if fields.len() == 1 { "" } else { "s" },
                            args.len()
                        ),
                    ));
                }
                let mut items = Vec::with_capacity(args.len());
                for arg in args {
                    items.push(self.eval_expr(arg)?);
                }
                Value::__try_new_enum_tuple_in(
                    module_path,
                    type_name.to_string(),
                    variant.to_string(),
                    items,
                    line,
                    &self.memory,
                )
            }
            (VariantKind::Struct(decl_fields), VariantPayload::Struct(provided)) => {
                let mut seen = alloc_import::collections::BTreeSet::new();
                for (fname, _) in provided {
                    if !seen.insert(fname.clone()) {
                        return Err(error(
                            line,
                            format!("Field `{fname}` specified twice in `{type_name}::{variant}`"),
                        ));
                    }
                    if !decl_fields.iter().any(|d| d == fname) {
                        return Err(error(
                            line,
                            crate::error_messages::variant_has_no_field(type_name, variant, fname),
                        ));
                    }
                }
                for decl_field in decl_fields {
                    if !seen.contains(decl_field) {
                        return Err(error(
                            line,
                            format!(
                                "Missing field `{decl_field}` in `{type_name}::{variant}` construction"
                            ),
                        ));
                    }
                }
                let mut provided_map: BTreeMap<String, Value> = BTreeMap::new();
                for (fname, fexpr) in provided {
                    provided_map.insert(fname.clone(), self.eval_expr(fexpr)?);
                }
                let mut values: Vec<(String, Value)> = Vec::with_capacity(decl_fields.len());
                for decl_field in decl_fields {
                    values.push((
                        decl_field.clone(),
                        provided_map
                            .remove(decl_field)
                            .expect("variant shape validated before payload evaluation"),
                    ));
                }
                Value::__try_new_enum_struct_in(
                    module_path,
                    type_name.to_string(),
                    variant.to_string(),
                    values,
                    line,
                    &self.memory,
                )
            }
            (VariantKind::Unit, _) => Err(error(
                line,
                format!("Variant `{type_name}::{variant}` takes no payload"),
            )),
            (VariantKind::Tuple(_), _) => Err(error(
                line,
                format!("Variant `{type_name}::{variant}` expects positional arguments `(…)`"),
            )),
            (VariantKind::Struct(_), _) => Err(error(
                line,
                format!("Variant `{type_name}::{variant}` expects named fields `{{ … }}`"),
            )),
        }
    }

    fn exec_assign(
        &mut self,
        target: &AssignTarget,
        op: &AssignOp,
        new_val: Value,
        line: u32,
    ) -> Result<(), NyblError> {
        match target {
            AssignTarget::Variable(name) => {
                let final_val = match op {
                    AssignOp::Eq => new_val,
                    _ => {
                        let current = self.get_var_value(name).ok_or_else(|| {
                            error(line, crate::error_messages::variable_not_found(name))
                        })?;
                        self.apply_compound_op(&current, op, &new_val, line)?
                    }
                };
                if !self.set_var(name, final_val) {
                    return Err(error_with_hint(
                        line,
                        format!("Variable `{name}` doesn't exist yet"),
                        format!("Use `let` to create a new variable: let {name} = ..."),
                    ));
                }
                if self.is_module_top_scope() {
                    let module = match self.get_var(name) {
                        Some(Value::Module(module)) => Some(Rc::clone(module)),
                        _ => None,
                    };
                    let published_module = module.as_ref().map(Rc::clone);
                    let aliases = self.module_aliases.last_mut().expect("module alias scope");
                    if let Some(module) = module {
                        aliases.insert(name.clone(), module);
                    } else {
                        aliases.remove(name);
                    }
                    self.publish_root_module_alias(name.clone(), published_module);
                }
                Ok(())
            }
            AssignTarget::Index { object, index } => {
                let mut projections = Vec::new();
                let root =
                    Self::decompose_place_expr(object, &mut projections).ok_or_else(|| {
                        error(
                            line,
                            "Can only assign through a mutable place rooted in a variable",
                        )
                    })?;
                projections.push(PlaceProjectionExpr::Index(index.clone()));
                let plan = self.assignment_place_parts(root, projections, line)?;
                let place = self.resolve_place(plan, 1, line)?;
                let val_to_set = match op {
                    AssignOp::Eq => new_val,
                    _ => {
                        let current =
                            self.place_value_from(&place.root, &place.projections, line)?;
                        self.apply_compound_op(&current, op, &new_val, line)?
                    }
                };
                self.commit_ref_places(&[place], vec![val_to_set], line)
            }
            AssignTarget::Field { object, field } => {
                let mut projections = Vec::new();
                let root =
                    Self::decompose_place_expr(object, &mut projections).ok_or_else(|| {
                        error(
                            line,
                            "Can only assign through a mutable place rooted in a variable",
                        )
                    })?;
                projections.push(PlaceProjectionExpr::Field(field.clone()));
                let plan = self.assignment_place_parts(root, projections, line)?;
                let place = self.resolve_place(plan, 1, line)?;
                let val_to_set = match op {
                    AssignOp::Eq => new_val,
                    _ => {
                        let current =
                            self.place_value_from(&place.root, &place.projections, line)?;
                        self.apply_compound_op(&current, op, &new_val, line)?
                    }
                };
                self.commit_ref_places(&[place], vec![val_to_set], line)
            }
        }
    }

    fn apply_compound_op(
        &self,
        left: &Value,
        op: &AssignOp,
        right: &Value,
        line: u32,
    ) -> Result<Value, NyblError> {
        match op {
            AssignOp::Eq => Ok(right.clone()),
            AssignOp::AddEq => ops::add_in(left, right, line, &self.memory),
            AssignOp::SubEq => ops::sub_in(left, right, line, &self.memory),
            AssignOp::MulEq => ops::mul_in(left, right, line, &self.memory),
            AssignOp::DivEq => ops::div_in(left, right, line, &self.memory),
            AssignOp::ModEq => ops::rem_in(left, right, line, &self.memory),
        }
    }

    // ─── Expressions ───────────────────────────────────────────────

    fn eval_expr(&mut self, expr: &Expr) -> Result<Value, NyblError> {
        match &expr.kind {
            ExprKind::Int(n) => Ok(Value::Int(*n)),
            ExprKind::Number(n) => Ok(Value::Number(*n)),
            ExprKind::Str(s) => Ok(Value::__new_str_in(s.clone(), &self.memory)),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::None => Ok(Value::None),

            ExprKind::StringInterp(parts) => {
                let mut result = String::new();
                for part in parts {
                    match part {
                        StringPart::Literal(s) => result.push_str(s),
                        StringPart::Variable(name) => {
                            let val = self.get_var_value(name).ok_or_else(|| {
                                error_at(
                                    expr.line,
                                    expr.column,
                                    crate::error_messages::variable_not_found(name),
                                )
                            })?;
                            result.push_str(&format!("{val}"));
                        }
                    }
                }
                Ok(Value::__new_str_in(result, &self.memory))
            }

            ExprKind::Ident(name) => {
                // Lexical lookup first — matches the intuition that
                // `let x = ...` locally shadows everything else.
                if let Some(v) = self.get_var_value(name) {
                    return Ok(v);
                }
                // Fall back to the canonical callable created when the named
                // declaration was reached. First-class access only clones the
                // `Rc`; it never clones or reconstructs the function AST.
                if let Some(f) = self.lookup_function(name) {
                    return Ok(Value::Fn(f));
                }
                // Typo? Offer a "did you mean" hint if something
                // close is visible in the current scope / fn
                // registry. Falls back to the original "did you
                // forget `let`" when no candidate is similar
                // enough.
                let hint = self
                    .value_candidates_hint(name)
                    .unwrap_or_else(|| "Did you forget to create it with `let`?".to_string());
                Err(error_with_hint_at(
                    expr.line,
                    expr.column,
                    crate::error_messages::variable_not_found(name),
                    hint,
                ))
            }

            ExprKind::BinaryOp { left, op, right } => {
                // Short-circuit for && and ||
                if matches!(op, BinOp::And) {
                    let lval = self.eval_expr(left)?;
                    if !lval.is_truthy() {
                        return Ok(Value::Bool(false));
                    }
                    let rval = self.eval_expr(right)?;
                    return Ok(Value::Bool(rval.is_truthy()));
                }
                if matches!(op, BinOp::Or) {
                    let lval = self.eval_expr(left)?;
                    if lval.is_truthy() {
                        return Ok(Value::Bool(true));
                    }
                    let rval = self.eval_expr(right)?;
                    return Ok(Value::Bool(rval.is_truthy()));
                }

                let lval = self.eval_expr(left)?;
                let rval = self.eval_expr(right)?;
                self.binary_op(&lval, op, &rval, expr.line)
            }

            ExprKind::UnaryOp { op, expr: inner } => {
                let val = self.eval_expr(inner)?;
                match op {
                    UnaryOp::Neg => ops::neg(&val, expr.line),
                    UnaryOp::Not => Ok(ops::not(&val)),
                }
            }

            ExprKind::Call { callee, args } => {
                if let ExprKind::Ident(name) = &callee.kind {
                    // Lexical callable first: if the name is bound
                    // to a `Value::Fn` in the current scope, call
                    // it. A bound non-callable is an explicit
                    // error — "shadowing a builtin with a number
                    // then calling it" should fail loudly, not
                    // silently dispatch to the builtin.
                    if let Some(v) = self.get_var(name).cloned() {
                        return match v {
                            Value::Fn(func) => self.eval_nybl_call(func, args, expr.line, name),
                            other => Err(error(
                                expr.line,
                                format!("`{}` is a {}, not a function", name, other.type_name()),
                            )),
                        };
                    }
                    let declaration_alias =
                        self.active_lexical_contexts.last().and_then(|context| {
                            if context.module_aliases.is_empty() {
                                None
                            } else {
                                context.module_aliases.get(name).cloned()
                            }
                        });
                    if declaration_alias.is_some() {
                        return Err(error(
                            expr.line,
                            format!("`{name}` is a module, not a function"),
                        ));
                    }
                    if let Some(function) = self.lookup_function(name) {
                        // Preserve the established host-before-named-function
                        // dispatch for legacy value-only calls. Ref-aware
                        // calls must use the retained Nybl callable metadata so
                        // their staged targets can be committed transactionally.
                        if function
                            .param_modes
                            .iter()
                            .all(|mode| *mode == ParamMode::Value)
                            && args.iter().all(|arg| arg.mode == ParamMode::Value)
                        {
                            if args.len() != function.params.len() {
                                return Err(error(
                                    expr.line,
                                    format!(
                                        "`{}` expects {} argument{}, but got {}",
                                        name,
                                        function.params.len(),
                                        if function.params.len() == 1 { "" } else { "s" },
                                        args.len()
                                    ),
                                ));
                            }
                            let mut eval_args = Vec::with_capacity(args.len());
                            for arg in args {
                                eval_args.push(self.eval_expr(&arg.value)?);
                            }
                            return self.call_function(name, eval_args, expr.line, Some(function));
                        }
                        return self.eval_nybl_call(function, args, expr.line, name);
                    }

                    let actual_modes: Vec<ParamMode> = args.iter().map(|arg| arg.mode).collect();
                    crate::validate_value_only_call_modes(name, &actual_modes, expr.line)?;
                    self.validate_builtin_arity(name, args.len(), expr.line)?;
                    let mut eval_args = Vec::with_capacity(args.len());
                    for arg in args {
                        eval_args.push(self.eval_expr(&arg.value)?);
                    }
                    return self.call_function(name, eval_args, expr.line, None);
                }

                // The callee expression is always resolved exactly once before
                // preflight and before any argument expression.
                let callee_val = self.eval_expr(callee)?;
                match callee_val {
                    Value::Fn(func) => {
                        let label = func.self_name.as_deref().unwrap_or("fn").to_string();
                        self.eval_nybl_call(func, args, expr.line, &label)
                    }
                    other => Err(error(
                        expr.line,
                        crate::error_messages::cant_call_a(other.type_name()),
                    )),
                }
            }

            ExprKind::Lambda { params, body } => {
                let free_variables = FreeVariableCollector::for_lambda(params).finish(body);
                if free_variables.iter().any(|name| {
                    self.resolve_binding_identity(name)
                        .is_some_and(|identity| self.active_ref_bindings.contains(&identity))
                }) {
                    return Err(crate::ref_params::ref_capture_error(expr.line));
                }
                let captures = self.snapshot_captures(&free_variables);
                let module_path = self
                    .function_modules
                    .last()
                    .cloned()
                    .unwrap_or_else(|| self.current_module.clone());
                let (param_names, param_modes) = parameter_metadata(params);
                NyblFn::try_new_ast_in_module_with_origin_and_modes(
                    param_names,
                    param_modes,
                    captures,
                    body.clone(),
                    None,
                    Some(module_path),
                    self.function_origin.clone(),
                    expr.line,
                )
                .map(Value::Fn)
            }

            ExprKind::MethodCall {
                object,
                method,
                args,
            } => {
                let actual_modes: Vec<ParamMode> = args.iter().map(|arg| arg.mode).collect();

                // Keep the direct-binding fast path for a bare array or dict.
                // It avoids manufacturing a second CoW owner immediately
                // before mutation, which preserves exact instance memory
                // accounting.
                if let ExprKind::Ident(name) = &object.kind
                    && self.get_var(name).is_some_and(|value| {
                        methods::is_mutating_collection_receiver(value, method)
                    })
                {
                    crate::validate_value_only_call_modes(method, &actual_modes, expr.line)?;
                    let expected = methods::builtin_method_arity(method)
                        .expect("mutating built-in has arity metadata");
                    if args.len() != expected {
                        return Err(error(
                            expr.line,
                            format!(
                                "`.{}()` needs {} argument{}",
                                method,
                                expected,
                                if expected == 1 { "" } else { "s" }
                            ),
                        ));
                    }
                    methods::reject_constant_array_mutation(name, method, expr.line)?;
                    let target = self
                        .resolve_binding_identity(name)
                        .ok_or_else(|| crate::ref_params::invalid_ref_target(1, expr.line))?;
                    if self.active_capture_bindings.contains(&target) {
                        return Err(crate::ref_params::captured_ref_target(1, expr.line));
                    }
                    let mut eval_args = Vec::with_capacity(args.len());
                    for arg in args {
                        eval_args.push(self.eval_expr(&arg.value)?);
                    }
                    if let Some(error) = self.pending_memory_error(expr.line) {
                        return Err(error);
                    }
                    let binding = self
                        .scopes
                        .get_mut(target.scope)
                        .and_then(|scope| scope.get_mut(&target.name))
                        .ok_or_else(|| crate::ref_params::invalid_ref_target(1, expr.line))?;
                    return methods::transactional_collection_method_in(
                        binding,
                        method,
                        eval_args,
                        expr.line,
                        &self.memory,
                    );
                }

                // Preserve a resolved place while evaluating an index/field
                // receiver exactly once. Read-only methods use only the leaf;
                // mutating built-ins and `ref self` methods can later commit a
                // replacement leaf through the retained projection chain.
                let mut receiver_place = if let Some(plan) = self.read_place_plan(object) {
                    Some(self.resolve_place(plan, 1, expr.line)?)
                } else {
                    None
                };
                let obj_val = if let Some(place) = &receiver_place {
                    self.place_value_from(&place.root, &place.projections, expr.line)?
                } else {
                    self.eval_expr(object)?
                };

                if methods::is_mutating_collection_receiver(&obj_val, method)
                    && receiver_place.is_some()
                {
                    crate::validate_value_only_call_modes(method, &actual_modes, expr.line)?;
                    let expected = methods::builtin_method_arity(method)
                        .expect("mutating built-in has arity metadata");
                    if args.len() != expected {
                        return Err(error(
                            expr.line,
                            format!(
                                "`.{}()` needs {} argument{}",
                                method,
                                expected,
                                if expected == 1 { "" } else { "s" }
                            ),
                        ));
                    }
                    let place = receiver_place
                        .take()
                        .expect("place presence checked for mutating receiver");
                    self.validate_mutable_place(&place, 1, expr.line)?;
                    let mut eval_args = Vec::with_capacity(args.len());
                    for arg in args {
                        eval_args.push(self.eval_expr(&arg.value)?);
                    }
                    if let Some(error) = self.pending_memory_error(expr.line) {
                        return Err(error);
                    }
                    // Arguments may mutate the same root. Copy-in therefore
                    // occurs after argument evaluation, using the already
                    // evaluated projection values.
                    let current_root = self
                        .binding_value(&place.target)
                        .ok_or_else(|| crate::ref_params::invalid_ref_target(1, expr.line))?;
                    let mut leaf =
                        self.place_value_from(&current_root, &place.projections, expr.line)?;
                    let result = methods::transactional_collection_method_in(
                        &mut leaf,
                        method,
                        eval_args,
                        expr.line,
                        &self.memory,
                    )?;
                    let committed = ResolvedPlace {
                        target: place.target,
                        root: current_root,
                        projections: place.projections,
                    };
                    self.commit_ref_places(&[committed], vec![leaf], expr.line)?;
                    return Ok(result);
                }
                let type_key: Option<(String, String)> = match &obj_val {
                    Value::Struct(s) => {
                        Some((s.module_path().to_string(), s.type_name().to_string()))
                    }
                    Value::EnumVariant(e) => {
                        Some((e.module_path().to_string(), e.type_name().to_string()))
                    }
                    _ => None,
                };
                if let Some(key) = type_key {
                    let user = self
                        .methods
                        .get(&key)
                        .and_then(|ms| ms.get(method))
                        .cloned();
                    if let Some(m) = user {
                        let receiver_place = if m
                            .param_modes
                            .first()
                            .is_some_and(|mode| *mode == ParamMode::Ref)
                        {
                            let Some(place) = receiver_place.take() else {
                                let mut error = NyblError::runtime(
                                    "a `ref` method receiver must be a mutable place",
                                    expr.line,
                                );
                                error.friendly_hint = Some(
                                    "Store the value in a `let` binding, or call through one of its fields or indexes."
                                        .to_string(),
                                );
                                return Err(error);
                            };
                            self.validate_mutable_place(&place, 1, expr.line)?;
                            Some(place)
                        } else {
                            None
                        };
                        return self.eval_user_method_call(
                            MethodReceiver {
                                value: obj_val,
                                place: receiver_place,
                            },
                            &key.1,
                            method,
                            m,
                            args,
                            expr.line,
                        );
                    }
                }

                // Module export calls retain callable metadata too. Common
                // value methods still win over an export with the same name.
                if let Value::Module(module) = &obj_val
                    && !matches!(method.as_str(), "type" | "to_str" | "inspect")
                {
                    let callee = self.module_binding(module, method).ok_or_else(|| {
                        error(
                            expr.line,
                            format!("`{}` isn't exported from `{}`", method, module.path),
                        )
                    })?;
                    return match callee {
                        Value::Fn(func) => self.eval_nybl_call(func, args, expr.line, method),
                        other => Err(error(
                            expr.line,
                            format!("`{}` is a {}, not a function", method, other.type_name()),
                        )),
                    };
                }

                // Index and field reads currently produce values,
                // not write-back places. Reject only built-in array
                // mutations on those syntactic receiver forms so
                // they cannot silently mutate-and-discard. Genuine
                // temporaries remain legal, and user methods above
                // retain precedence even when named `push`, `pop`,
                // and so on.
                if matches!(
                    &object.kind,
                    ExprKind::Index { .. } | ExprKind::FieldAccess { .. }
                ) {
                    methods::reject_nested_array_mutation(&obj_val, method, expr.line)?;
                }

                crate::validate_value_only_call_modes(method, &actual_modes, expr.line)?;
                if !matches!(&obj_val, Value::Host(_))
                    && let Some(expected) = methods::builtin_method_arity(method)
                    && args.len() != expected
                {
                    return Err(error(
                        expr.line,
                        format!(
                            "`.{}()` needs {} argument{}",
                            method,
                            expected,
                            if expected == 1 { "" } else { "s" }
                        ),
                    ));
                }
                let mut eval_args = Vec::with_capacity(args.len());
                for arg in args {
                    eval_args.push(self.eval_expr(&arg.value)?);
                }

                // Callable-taking Result methods — `r.map(f)`,
                // `r.map_err(f)`, `r.and_then(f)`. These need the
                // evaluator's call primitive to invoke `f`, which
                // `call_method` doesn't have access to, so they
                // dispatch inline before the pure `call_method`
                // fall-through. Receiver must be the built-in
                // Result; user enums named `Result` don't qualify.
                if methods::is_builtin_result(&obj_val)
                    && let Some(kind) = methods::is_result_callable_method(method)
                {
                    return self
                        .call_result_callable_method(&obj_val, kind, method, eval_args, expr.line);
                }

                let (ret, mutated) = self.call_method(&obj_val, method, &eval_args, expr.line)?;
                // A true temporary mutates its staged owned value and discards
                // the update. Named array receivers returned through the
                // transactional path above.
                let _ = mutated;
                Ok(ret)
            }

            ExprKind::Index { object, index } => {
                let obj = self.eval_expr(object)?;
                let idx = self.eval_expr(index)?;
                ops::index_get_in(&obj, &idx, expr.line, &self.memory)
            }

            ExprKind::FieldAccess { object, field } => {
                let obj = self.eval_expr(object)?;
                match &obj {
                    Value::Struct(s) => s.field(field).cloned().ok_or_else(|| {
                        let msg = crate::error_messages::struct_has_no_field(s.type_name(), field);
                        // Suggest from the struct's own declared
                        // field list — `p.z` when `Point` has `x`
                        // and `y` should point to `x`/`y`.
                        let field_names: Vec<&str> =
                            s.fields().iter().map(|(k, _)| k.as_str()).collect();
                        match crate::suggest::did_you_mean(field, field_names) {
                            Some(hint) => error_with_hint_at(expr.line, expr.column, msg, hint),
                            None => error_at(expr.line, expr.column, msg),
                        }
                    }),
                    Value::EnumVariant(e) => e.field(field).cloned().ok_or_else(|| {
                        error(
                            expr.line,
                            crate::error_messages::variant_has_no_field(
                                e.type_name(),
                                e.variant(),
                                field,
                            ),
                        )
                    }),
                    Value::Module(m) => {
                        // `alias.name` — look up an export in the
                        // aliased module.
                        if let Some(v) = self.module_binding(m, field) {
                            return Ok(v);
                        }
                        if m.has_type(field) {
                            // Types aren't first-class values; the
                            // parser should have taken the namespaced
                            // struct-lit / variant-ctor path
                            // already. Reaching a FieldAccess here
                            // means the user wrote `m.Type` in a
                            // value-returning position.
                            let alias = m.path.rsplit('.').next().unwrap_or(&m.path);
                            return Err(error_with_hint(
                                expr.line,
                                format!("`{}` in `{}` is a type, not a value", field, m.path),
                                format!(
                                    "construct through the alias: `{alias}.{field} {{ ... }}` or `{alias}.{field}::Variant(...)`",
                                ),
                            ));
                        }
                        Err(error(
                            expr.line,
                            format!("`{}` isn't exported from `{}`", field, m.path),
                        ))
                    }
                    other => Err(error(
                        expr.line,
                        crate::error_messages::cant_read_field(field, other.type_name()),
                    )),
                }
            }

            ExprKind::EnumConstruct {
                namespace,
                type_name,
                variant,
                payload,
            } => self.eval_enum_construct(
                namespace.as_deref(),
                type_name,
                variant,
                payload,
                expr.line,
            ),

            ExprKind::StructConstruct {
                namespace,
                type_name,
                fields,
            } => {
                let module_path = match namespace {
                    Some(ns) => {
                        self.validate_namespaced_type(ns, type_name, expr.line)?;
                        self.resolve_type_ref(Some(ns), type_name)
                            .unwrap_or_else(|| self.current_module.clone())
                    }
                    None => self.resolve_type_ref(None, type_name).ok_or_else(|| {
                        error(
                            expr.line,
                            crate::error_messages::struct_not_declared(type_name),
                        )
                    })?,
                };
                let key = (module_path.clone(), type_name.clone());
                let decl_fields = self
                    .struct_defs
                    .get(&key)
                    .ok_or_else(|| {
                        error(
                            expr.line,
                            crate::error_messages::struct_not_declared(type_name),
                        )
                    })?
                    .clone();
                // Enforce exactly-this-field-set at construction:
                // no duplicates, no unknown fields, no missing
                // fields. The per-field loops below produce the
                // specific messages the tests assert on.
                let mut seen = alloc_import::collections::BTreeSet::new();
                for (fname, _) in fields {
                    if !seen.insert(fname.clone()) {
                        return Err(error(
                            expr.line,
                            format!(
                                "Field `{fname}` specified twice in `{type_name}` construction"
                            ),
                        ));
                    }
                    if !decl_fields.iter().any(|d| d == fname) {
                        let msg = crate::error_messages::struct_has_no_field(type_name, fname);
                        let err = match crate::suggest::did_you_mean(
                            fname,
                            decl_fields.iter().map(|s| s.as_str()),
                        ) {
                            Some(hint) => error_with_hint_at(expr.line, expr.column, msg, hint),
                            None => error_at(expr.line, expr.column, msg),
                        };
                        return Err(err);
                    }
                }
                for decl in &decl_fields {
                    if !seen.contains(decl) {
                        return Err(error(
                            expr.line,
                            format!("Missing field `{decl}` in `{type_name}` construction"),
                        ));
                    }
                }
                let mut provided: BTreeMap<String, Value> = BTreeMap::new();
                for (fname, fexpr) in fields {
                    provided.insert(fname.clone(), self.eval_expr(fexpr)?);
                }
                let mut values: Vec<(String, Value)> = Vec::with_capacity(decl_fields.len());
                for decl in &decl_fields {
                    values.push((
                        decl.clone(),
                        provided
                            .remove(decl)
                            .expect("struct shape validated before payload evaluation"),
                    ));
                }
                Value::__try_new_struct_in(
                    module_path,
                    type_name.clone(),
                    values,
                    expr.line,
                    &self.memory,
                )
            }

            ExprKind::Array(elements) => {
                let mut items = Vec::new();
                for elem in elements {
                    items.push(self.eval_expr(elem)?);
                }
                Value::__try_new_array_in(items, expr.line, &self.memory)
            }

            ExprKind::Dict(entries) => {
                let mut result = Vec::new();
                for (key, value_expr) in entries {
                    let val = self.eval_expr(value_expr)?;
                    result.push((key.clone(), val));
                }
                Value::__try_new_dict_in(result, expr.line, &self.memory)
            }

            ExprKind::IfExpr {
                condition,
                then_expr,
                else_expr,
            } => {
                if self.eval_expr(condition)?.is_truthy() {
                    self.eval_expr(then_expr)
                } else {
                    self.eval_expr(else_expr)
                }
            }

            ExprKind::Match { scrutinee, arms } => self.eval_match(scrutinee, arms, expr.line),

            ExprKind::Try(inner) => self.eval_try(inner, expr.line),
        }
    }

    /// `try` expression handler. Evaluates the inner expression,
    /// matches the conventional Result-shape (any enum variant
    /// named `Ok` or `Err`), and either unwraps `Ok` or stashes
    /// the `Err` value into `pending_try_return` so the enclosing
    /// `call_nybl_fn` can convert it to `Signal::Return`.
    ///
    /// Top-level `try` on `Err` (call_depth == 0) surfaces as a
    /// real runtime error — there's no enclosing fn to return
    /// from, and the roadmap explicitly rejects the idea of
    /// swallowing it silently.
    fn eval_try(&mut self, inner: &Expr, line: u32) -> Result<Value, NyblError> {
        let value = self.eval_expr(inner)?;
        // An earlier `try` in `inner` might have already fired —
        // e.g. `try try foo()` — in which case we just keep
        // propagating without poking at the unwound value.
        if self.pending_try_return.is_some() {
            return Ok(Value::None);
        }
        match &value {
            Value::EnumVariant(ev) if ev.variant() == "Ok" => {
                // Extract the single payload for `Ok(v)`, or fall
                // back to `none` for `Ok` used as a unit variant.
                match ev.payload() {
                    EnumPayload::Tuple(items) if items.len() == 1 => Ok(items[0].clone()),
                    EnumPayload::Unit => Ok(Value::None),
                    EnumPayload::Tuple(items) => Err(error(
                        line,
                        format!(
                            "try: Ok variant must carry exactly one value, got {}",
                            items.len()
                        ),
                    )),
                    EnumPayload::Struct(_) => Err(error(
                        line,
                        "try: Ok variant must carry a single positional value, not named fields",
                    )),
                }
            }
            Value::EnumVariant(ev) if ev.variant() == "Err" => {
                if self.call_depth == 0 {
                    return Err(crate::error_messages::top_level_try_error(line));
                }
                self.pending_try_return = Some(value);
                // Sentinel error whose only job is to unwind up
                // to the nearest `call_nybl_fn`, which converts
                // it back into a `Signal::Return`. The `is_try_return`
                // flag on `NyblError` is the only thing that
                // matters — line is kept for debug clarity;
                // message is unused.
                Err(NyblError::try_return_signal(line))
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

    fn eval_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        line: u32,
    ) -> Result<Value, NyblError> {
        let value = self.eval_expr(scrutinee)?;
        for arm in arms {
            let mut bindings: Vec<(String, Value)> = Vec::new();
            // Resolve patterns' type references through the
            // walker's current scope. Capturing the three scope
            // tables as immutable slices / refs keeps the
            // closure free of the mutable-self borrow we need
            // right after for `push_scope` + binding injection.
            let scopes = &self.scopes;
            let type_bindings = &self.type_bindings;
            let module_aliases = &self.module_aliases;
            let alias_scope_floor = self.alias_scope_floors.last().copied();
            let resolver = |ns: Option<&str>, tn: &str| -> Option<String> {
                resolve_type_in_evaluator_context(
                    scopes,
                    type_bindings,
                    module_aliases,
                    alias_scope_floor,
                    self.active_lexical_contexts.last().map(Rc::as_ref),
                    ns,
                    tn,
                )
            };
            if !pattern_matches_in(&arm.pattern, &value, &mut bindings, &resolver, &self.memory) {
                continue;
            }
            // Pattern matched — open a fresh scope, bind every
            // captured name, evaluate the guard (in that scope).
            self.push_scope();
            for (name, v) in bindings {
                self.define(name, v);
            }
            let guard_ok = match &arm.guard {
                Some(g) => self.eval_expr(g)?.is_truthy(),
                None => true,
            };
            if !guard_ok {
                self.pop_scope();
                continue;
            }
            let result = self.eval_expr(&arm.body);
            self.pop_scope();
            return result;
        }
        Err(error(line, "No match arm matched the scrutinee"))
    }

    // ─── Binary operations ─────────────────────────────────────────

    fn binary_op(
        &self,
        left: &Value,
        op: &BinOp,
        right: &Value,
        line: u32,
    ) -> Result<Value, NyblError> {
        match op {
            BinOp::Add => ops::add_in(left, right, line, &self.memory),
            BinOp::Sub => ops::sub_in(left, right, line, &self.memory),
            BinOp::Mul => ops::mul_in(left, right, line, &self.memory),
            BinOp::Div => ops::div_in(left, right, line, &self.memory),
            BinOp::Mod => ops::rem_in(left, right, line, &self.memory),
            BinOp::Eq => Ok(ops::eq(left, right)),
            BinOp::NotEq => Ok(ops::not_eq(left, right)),
            BinOp::Lt => ops::lt_in(left, right, line, &self.memory),
            BinOp::Gt => ops::gt_in(left, right, line, &self.memory),
            BinOp::LtEq => ops::lt_eq_in(left, right, line, &self.memory),
            BinOp::GtEq => ops::gt_eq_in(left, right, line, &self.memory),
            BinOp::And | BinOp::Or => unreachable!("handled in eval_expr"),
        }
    }

    // ─── Function calls ────────────────────────────────────────────

    /// Snapshot only the referenced free variables that currently resolve to
    /// lexical values. Names belonging to builtins, hosts, or declared
    /// functions deliberately remain absent so call-time fallback can resolve
    /// them through their owning registries.
    fn snapshot_captures(
        &self,
        free_variables: &alloc_import::collections::BTreeSet<String>,
    ) -> Vec<(String, Value)> {
        free_variables
            .iter()
            .filter_map(|name| {
                // During a function call scope zero is the injected live
                // defining-module environment. Capturing it would freeze
                // globals inside a returned closure. At top level it remains a
                // genuine lexical scope and keeps the language's established
                // snapshot-capture semantics.
                self.scopes
                    .iter()
                    .skip(usize::from(self.call_depth > 0))
                    .rev()
                    .find_map(|scope| scope.get(name))
                    .cloned()
                    .map(|value| (name.clone(), value))
            })
            .collect()
    }

    fn eval_nybl_call(
        &mut self,
        func: Rc<NyblFn>,
        args: &[CallArg],
        line: u32,
        callable: &str,
    ) -> Result<Value, NyblError> {
        if !func.__is_allowed_by(&self.function_origin, "walker") {
            return Err(error(
                line,
                "This function belongs to a different Nybl engine instance",
            ));
        }
        if !matches!(func.body, FnBody::Ast(_)) {
            return Err(error(
                line,
                "This function was compiled for another engine and can't be run in the tree-walker",
            ));
        }
        if !crate::ref_params::accepts_arity(&func.param_modes, args.len()) {
            let required = crate::ref_params::required_arity(&func.param_modes);
            let variadic = matches!(func.param_modes.last(), Some(ParamMode::Rest));
            return Err(error(
                line,
                if variadic {
                    format!(
                        "`{callable}` expects at least {required} arguments, but got {}",
                        args.len()
                    )
                } else {
                    format!(
                        "`{callable}` expects {required} argument{}, but got {}",
                        if required == 1 { "" } else { "s" },
                        args.len()
                    )
                },
            ));
        }
        let actual_modes: Vec<ParamMode> = args.iter().map(|arg| arg.mode).collect();
        crate::validate_call_modes(callable, &func.param_modes, &actual_modes, line)?;
        let plans = self.preflight_ref_places(args, line)?;

        // Ordinary argument expressions run left-to-right. Ref places are
        // intentionally not read yet: side effects here must be visible in
        // the subsequent copy-in snapshot.
        let mut evaluated: Vec<Option<Value>> = Vec::with_capacity(args.len());
        for arg in args {
            evaluated.push(match arg.mode {
                ParamMode::Value => Some(self.eval_expr(&arg.value)?),
                ParamMode::Ref => None,
                ParamMode::Rest => unreachable!("rest is a declaration-only parameter mode"),
            });
        }

        let mut plan_iter = plans.into_iter();
        let mut places = Vec::new();
        let mut ref_positions = Vec::new();
        let mut call_args = Vec::with_capacity(args.len());
        for (index, (arg, value)) in args.iter().zip(evaluated).enumerate() {
            match arg.mode {
                ParamMode::Value => {
                    call_args.push(value.expect("value argument was evaluated"));
                }
                ParamMode::Ref => {
                    let plan = plan_iter.next().expect("place preflight matched modes");
                    let place = self.resolve_place(plan, index + 1, line)?;
                    let snapshot = self.place_value_from(&place.root, &place.projections, line)?;
                    call_args.push(snapshot);
                    places.push(place);
                    ref_positions.push(index);
                }
                ParamMode::Rest => unreachable!("rest is a declaration-only parameter mode"),
            }
        }

        let (value, staged) =
            self.call_nybl_fn_with_refs(&func, call_args, &ref_positions, line)?;
        self.commit_ref_places(&places, staged, line)?;
        Ok(value)
    }

    fn eval_user_method_call(
        &mut self,
        mut receiver: MethodReceiver,
        receiver_type: &str,
        method: &str,
        definition: FunctionDefinition,
        args: &[CallArg],
        line: u32,
    ) -> Result<Value, NyblError> {
        let actual_arity = args.len() + 1;
        if !crate::ref_params::accepts_arity(&definition.param_modes, actual_arity) {
            let required = crate::ref_params::required_arity(&definition.param_modes);
            let variadic = matches!(definition.param_modes.last(), Some(ParamMode::Rest));
            return Err(error(
                line,
                if variadic {
                    format!(
                        "`{receiver_type}.{method}` expects at least {required} arguments (including `self`), but got {actual_arity}"
                    )
                } else {
                    format!(
                        "`{receiver_type}.{method}` expects {required} argument{} (including `self`), but got {actual_arity}",
                        if required == 1 { "" } else { "s" }
                    )
                },
            ));
        }
        // The exact total-arity check above guarantees the implicit receiver
        // has a corresponding first parameter before we inspect ordinary
        // argument modes or evaluate any ordinary argument expressions.
        let expected_modes = &definition.param_modes[1..];
        let actual_modes: Vec<ParamMode> = args.iter().map(|arg| arg.mode).collect();
        crate::validate_call_modes(method, expected_modes, &actual_modes, line)?;
        let explicit_plans = self.preflight_ref_places(args, line)?;
        if let Some(receiver_place) = &receiver.place
            && explicit_plans
                .iter()
                .any(|place| place.target == receiver_place.target)
        {
            return Err(crate::ref_params::duplicate_ref_target(line));
        }

        let mut evaluated = Vec::with_capacity(args.len());
        for arg in args {
            evaluated.push(match arg.mode {
                ParamMode::Value => Some(self.eval_expr(&arg.value)?),
                ParamMode::Ref => None,
                ParamMode::Rest => unreachable!("rest is a declaration-only parameter mode"),
            });
        }
        let mut plan_iter = explicit_plans.into_iter();
        let mut explicit_places = Vec::new();
        let mut full_args = Vec::with_capacity(args.len() + 1);
        let mut ref_positions = Vec::new();
        if let Some(place) = &mut receiver.place {
            // Receiver projections were evaluated before argument preflight,
            // but copy-in observes argument side effects to the root.
            place.root = self
                .binding_value(&place.target)
                .ok_or_else(|| crate::ref_params::invalid_ref_target(1, line))?;
            full_args.push(self.place_value_from(&place.root, &place.projections, line)?);
            ref_positions.push(0);
        } else {
            full_args.push(receiver.value);
        }
        for (index, (arg, value)) in args.iter().zip(evaluated).enumerate() {
            match arg.mode {
                ParamMode::Value => {
                    full_args.push(value.expect("ordinary method argument was evaluated"));
                }
                ParamMode::Ref => {
                    let plan = plan_iter.next().expect("place count matched ref modes");
                    let place = self.resolve_place(plan, index + 1, line)?;
                    full_args.push(self.place_value_from(&place.root, &place.projections, line)?);
                    explicit_places.push(place);
                    ref_positions.push(index + 1);
                }
                ParamMode::Rest => unreachable!("rest is a declaration-only parameter mode"),
            }
        }
        let (value, staged) =
            self.call_nybl_fn_with_refs(&definition, full_args, &ref_positions, line)?;
        let mut places =
            Vec::with_capacity(explicit_places.len() + usize::from(receiver.place.is_some()));
        if let Some(place) = receiver.place {
            places.push(place);
        }
        places.extend(explicit_places);
        self.commit_ref_places(&places, staged, line)?;
        Ok(value)
    }

    fn validate_builtin_arity(&self, name: &str, count: usize, line: u32) -> Result<(), NyblError> {
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

    /// Call a value directly. Non-`Value::Fn` payloads are an
    /// explicit "not callable" error — this is the safety net for
    /// `let x = 5; x(1)` and friends.
    ///
    /// `name_hint` is the Ident text when the callee was a bare
    /// name, used only for error messages. Non-Ident callees pass
    /// `None`.
    fn call_value(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        line: u32,
        name_hint: Option<&str>,
    ) -> Result<Value, NyblError> {
        match &callee {
            Value::Fn(f) => {
                let f = Rc::clone(f);
                drop(callee);
                self.call_nybl_fn(&f, args, line)
            }
            other => match name_hint {
                Some(n) => Err(error(
                    line,
                    format!("`{}` is a {}, not a function", n, other.type_name()),
                )),
                None => Err(error(
                    line,
                    crate::error_messages::cant_call_a(other.type_name()),
                )),
            },
        }
    }

    /// Shared call path for every `Value::Fn` — the body of both
    /// `let f = fn(...) {...}; f(x)` and the reified version of
    /// `fn name(...) {...}; name(x)` come through here.
    fn call_nybl_fn(
        &mut self,
        func: &Rc<NyblFn>,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Value, NyblError> {
        let actual_modes = vec![ParamMode::Value; args.len()];
        crate::validate_call_modes(
            func.self_name.as_deref().unwrap_or("fn"),
            &func.param_modes,
            &actual_modes,
            line,
        )?;
        self.call_nybl_fn_with_refs(func, args, &[], line)
            .map(|(value, _)| value)
    }

    fn call_nybl_fn_with_refs(
        &mut self,
        func: &Rc<NyblFn>,
        mut args: Vec<Value>,
        ref_positions: &[usize],
        line: u32,
    ) -> Result<(Value, Vec<Value>), NyblError> {
        if !func.__is_allowed_by(&self.function_origin, "walker") {
            return Err(error(
                line,
                "This function belongs to a different Nybl engine instance",
            ));
        }
        let body = match &func.body {
            FnBody::Ast(stmts) => stmts,
            FnBody::Compiled(_) => {
                return Err(error(
                    line,
                    "This function was compiled for another engine and can't be run in the tree-walker",
                ));
            }
        };
        if !crate::ref_params::accepts_arity(&func.param_modes, args.len()) {
            let name = func.self_name.as_deref().unwrap_or("fn");
            let required = crate::ref_params::required_arity(&func.param_modes);
            let variadic = matches!(func.param_modes.last(), Some(ParamMode::Rest));
            return Err(error(
                line,
                if variadic {
                    format!(
                        "`{name}` expects at least {required} arguments, but got {}",
                        args.len()
                    )
                } else {
                    format!(
                        "`{name}` expects {required} argument{}, but got {}",
                        if required == 1 { "" } else { "s" },
                        args.len()
                    )
                },
            ));
        }
        if matches!(func.param_modes.last(), Some(ParamMode::Rest)) {
            let required = crate::ref_params::required_arity(&func.param_modes);
            let extras = args.split_off(required);
            args.push(Value::__try_new_array_in(extras, line, &self.memory)?);
        }

        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(error_with_hint(
                line,
                "Too many nested function calls (possible infinite recursion)",
                "Check that your recursive function has a base case that stops calling itself.",
            ));
        }

        // A function call gets a fresh scope stack: no outer
        // locals leak in. Captures and parameters seed the new
        // scope; self-reference via `self_name` lets recursive
        // lambdas see themselves.
        //
        // `type_bindings` is NOT reset — types and module
        // aliases declared at module scope need to be visible
        // inside nested fn bodies for construction + pattern
        // matching to work. We push a fresh frame so any
        // type decl inside the fn body still ends up scoped to
        // that fn and vanishes on return.
        self.call_depth += 1;
        let saved_ref_bindings = core::mem::take(&mut self.active_ref_bindings);
        let saved_capture_bindings = core::mem::take(&mut self.active_capture_bindings);
        let function_module = func
            .module_path
            .clone()
            .unwrap_or_else(|| crate::value::ROOT_MODULE_PATH.to_string());
        let lexical_context = self.module_lexical_context(&function_module);
        self.function_modules.push(function_module.clone());
        self.active_lexical_contexts.push(lexical_context);
        let mut saved_scopes = core::mem::take(&mut self.scopes);
        let caller_environment = if let Some(scope) = saved_scopes.first_mut() {
            core::mem::take(scope)
        } else {
            BTreeMap::new()
        };
        let caller_binding_origins = core::mem::take(&mut self.binding_origins);
        let caller_value_module =
            core::mem::replace(&mut self.active_value_module, function_module.clone());
        self.live_binding_origins
            .borrow_mut()
            .insert(caller_value_module.clone(), caller_binding_origins.clone());
        put_live_environment(
            &self.live_value_environments,
            &caller_value_module,
            caller_environment,
            &caller_binding_origins,
        );
        let defining_binding_origins = self
            .live_binding_origins
            .borrow()
            .get(&function_module)
            .cloned()
            .unwrap_or_default();
        let defining_environment = take_live_environment(
            &self.live_value_environments,
            &function_module,
            &defining_binding_origins,
        );
        self.binding_origins = defining_binding_origins;
        // Keep the live root environment below the call-local frame. This is
        // intentionally moved, not cloned: reads and writes observe retained
        // instance state, and named in-place mutation keeps a refcount-1
        // receiver instead of defeating collection CoW.
        self.scopes = vec![defining_environment, BTreeMap::new()];
        let type_scope_floor = self.type_bindings.len();
        self.type_bindings.push(BTreeMap::new());
        let alias_scope_floor = self.module_aliases.len();
        self.module_aliases.push(BTreeMap::new());
        self.imported_functions.push(BTreeMap::new());
        self.alias_scope_floors.push(alias_scope_floor);
        self.imported_here
            .push(alloc_import::collections::BTreeSet::new());

        // Captures go in first so parameters shadow them on
        // collision (matches the lexical snapshot semantics).
        for (name, value) in &func.captures {
            self.define(name.clone(), value.clone());
            if let Some(identity) = self.resolve_binding_identity(name) {
                self.active_capture_bindings.insert(identity);
            }
        }
        if let Some(self_name) = &func.self_name {
            // Make the reified `fn name` value visible inside its
            // own body so `fn fib(n) { return fib(n-1) ... }`
            // works without a special "named fn" path.
            self.define(self_name.clone(), Value::Fn(Rc::clone(func)));
        }
        for (index, (param, arg)) in func.params.iter().zip(args).enumerate() {
            self.define(param.clone(), arg);
            if let Some(identity) = self.resolve_binding_identity(param) {
                self.active_capture_bindings.remove(&identity);
                if func.param_modes.get(index) == Some(&ParamMode::Ref) {
                    self.active_ref_bindings.insert(identity);
                }
            }
        }

        let raw_result = self.exec_block(body);
        // Convert every normal language return shape before looking at staged
        // outputs. A `try Err(...)` sentinel is a normal return and commits;
        // every other error rolls back.
        let mut result = match raw_result {
            Ok(Signal::Return(value)) => Ok(value),
            Ok(Signal::None) => Ok(Value::None),
            Ok(Signal::Break) => Err(error(line, "break used outside of a loop")),
            Ok(Signal::Continue) => Err(error(line, "continue used outside of a loop")),
            Err(err) if err.is_try_return => self.pending_try_return.take().ok_or(err),
            Err(err) => Err(err),
        };
        // Allocation accounting can become exceeded during the final
        // expression/statement even when that operation returns a nominal
        // value. Convert it to a fatal error before reading staged outputs or
        // restoring the caller, so no ref target can commit across the limit.
        let final_memory_error = result
            .is_ok()
            .then(|| self.pending_memory_error(line))
            .flatten();
        if let Some(error) = final_memory_error {
            result = Err(error);
        }
        let staged_result = if result.is_ok() {
            ref_positions
                .iter()
                .map(|position| {
                    let name = func.params.get(*position).ok_or_else(|| {
                        error(line, "Function parameter mode metadata is invalid")
                    })?;
                    self.scopes
                        .get(1)
                        .and_then(|scope| scope.get(name))
                        .cloned()
                        .ok_or_else(|| {
                            error(line, format!("`ref` parameter `{name}` is unavailable"))
                        })
                })
                .collect::<Result<Vec<_>, _>>()
        } else {
            Ok(Vec::new())
        };
        let staged_outputs = match staged_result {
            Ok(values) => values,
            Err(err) => {
                result = Err(err);
                Vec::new()
            }
        };
        let updated_defining_environment = self
            .scopes
            .first_mut()
            .map(core::mem::take)
            .unwrap_or_default();
        let updated_defining_origins = core::mem::take(&mut self.binding_origins);
        put_live_environment(
            &self.live_value_environments,
            &function_module,
            updated_defining_environment,
            &updated_defining_origins,
        );
        self.live_binding_origins
            .borrow_mut()
            .insert(function_module, updated_defining_origins);
        let restored_caller_environment = take_live_environment(
            &self.live_value_environments,
            &caller_value_module,
            &caller_binding_origins,
        );
        if let Some(scope) = saved_scopes.first_mut() {
            *scope = restored_caller_environment;
        } else {
            saved_scopes.push(restored_caller_environment);
        }
        self.scopes = saved_scopes;
        self.binding_origins = caller_binding_origins;
        self.active_value_module = caller_value_module;
        self.type_bindings.truncate(type_scope_floor);
        self.module_aliases.truncate(alias_scope_floor);
        self.imported_functions.truncate(alias_scope_floor);
        self.alias_scope_floors.pop();
        self.imported_here.truncate(alias_scope_floor);
        self.active_lexical_contexts.pop();
        self.function_modules.pop();
        self.call_depth -= 1;
        self.active_ref_bindings = saved_ref_bindings;
        self.active_capture_bindings = saved_capture_bindings;

        match result {
            Ok(value) => Ok((value, staged_outputs)),
            Err(err) => {
                // A runtime error escaping the fn body carries line
                // numbers from the fn's *defining* module, not the
                // caller's source. Attach the owning module so
                // rendering never quotes an unrelated root-file (or
                // outer-module) line. Deepest context wins, so a
                // nested call that already attached its own module
                // is preserved; root-declared fns attach the root
                // sentinel, which renders as plain root source but
                // blocks an enclosing module fn (e.g. one invoking
                // a root callback) from claiming the error.
                let owner = func
                    .module_path
                    .as_deref()
                    .unwrap_or(crate::value::ROOT_MODULE_PATH);
                Err(err.with_module(owner))
            }
        }
    }

    /// Implement the `try_call(f)` builtin.
    ///
    /// Takes a zero-arg callable `f` and invokes it. On a clean
    /// return, yields `Result::Ok(value)`. On a **non-fatal**
    /// `NyblError`, yields `Result::Err(RuntimeError { message,
    /// line })` — both values are constructed directly by
    /// `nybl::builtins::make_try_call_ok` / `_err`, so they
    /// work even in programs that never declared `Result` or
    /// `RuntimeError` themselves.
    ///
    /// Fatal errors (resource-limit violations — see
    /// `NyblError::is_fatal`) bypass the wrap and propagate
    /// unchanged, preserving the sandbox invariant.
    fn builtin_try_call(&mut self, args: Vec<Value>, line: u32) -> Result<Value, NyblError> {
        if args.len() != 1 {
            return Err(error(
                line,
                format!("`try_call` expects 1 argument, but got {}", args.len()),
            ));
        }
        let callable = args.into_iter().next().unwrap();
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
        let result = match self.call_nybl_fn(&func, Vec::new(), line) {
            Ok(value) => builtins::make_try_call_ok_in(value, line, &self.memory),
            Err(err) => {
                if err.is_fatal {
                    Err(err)
                } else {
                    Ok(builtins::make_try_call_err_in(&err, &self.memory))
                }
            }
        };
        if result.is_ok() && self.memory.__exceeded() {
            Err(error_fatal_with_hint(
                line,
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            ))
        } else {
            result
        }
    }

    fn call_function(
        &mut self,
        name: &str,
        args: Vec<Value>,
        line: u32,
        resolved_function: Option<FunctionDefinition>,
    ) -> Result<Value, NyblError> {
        // Executed lexical function declarations shadow engine builtins while
        // preserving the established host-before-named-function precedence.
        // Direct-call classification passes the canonical declaration here,
        // avoiding a second lookup.
        if let Some(func) = resolved_function {
            if let Some(result) = self.host.call(name, &args, line) {
                return result;
            }
            return self.call_nybl_fn(&func, args, line);
        }

        // 1. Global builtins. The short list: anything variadic
        // (`print`), a collection constructor (`range`), session-
        // stateful (`rand`), or inherently takes a callable
        // (`try_call`). Every other global builtin that used to
        // live here is now a method on the receiver's type —
        // see `methods::common_method` and `methods::numeric_method`.
        match name {
            // Runtime backstop for host-disabled builtins: a call the
            // load-time pass could not prove (a shadowing binding
            // exists somewhere) still dies — fatally, so `try_call`
            // can't swallow it — the moment it reaches the builtin.
            "range" | "rand" | "print" | "try_call" | "panic"
                if !self.limits.disabled_builtins.is_empty()
                    && self.limits.disabled_builtins.contains(name) =>
            {
                return Err(crate::check::disabled_builtin_error(name, line));
            }
            "range" => {
                return builtins::builtin_range_in(&args, line, &mut self.rand_state, &self.memory);
            }
            "rand" => return builtins::builtin_rand(&args, line, &mut self.rand_state),
            "print" => {
                let message =
                    crate::formatting::__format_values_in(&args, " ", line, &self.memory)?;
                self.host.on_print(&message);
                if let Some(error) = self.host.print_error(line) {
                    return Err(error);
                }
                return Ok(Value::None);
            }
            "try_call" => return self.builtin_try_call(args, line),
            "panic" => return builtins::builtin_panic(&args, line),
            _ => {}
        }

        // 2. Host-provided builtins
        let host_result = self.host.call(name, &args, line);
        if let Some(result) = host_result {
            return result;
        }

        // 3. A dynamically published/imported user function not classified
        // by the direct-call path above.
        let func = self.lookup_function(name).ok_or_else(|| {
            // Preference order: "did you mean" suggestion first
            // (most specific to the user's typo), then the
            // host's generic function hint (embedder-specific
            // tips like "available host functions: …"), then a
            // bare error.
            if let Some(hint) = self.callable_candidates_hint(name) {
                error_with_hint(line, crate::error_messages::function_not_found(name), hint)
            } else {
                let host_hint = self.host.function_hint().to_string();
                if host_hint.is_empty() {
                    error(line, crate::error_messages::function_not_found(name))
                } else {
                    error_with_hint(
                        line,
                        crate::error_messages::function_not_found(name),
                        host_hint,
                    )
                }
            }
        })?;

        self.call_nybl_fn(&func, args, line)
    }

    // ─── Methods ───────────────────────────────────────────────────

    /// Full method dispatch: user methods win, then common,
    /// then type-specific (via [`Self::call_method`]). This is
    /// what call sites outside the `MethodCall` arm (for-loop
    /// iterator protocol, future trait-like uses) want — the
    /// normal `MethodCall` path inlines the same logic.
    fn call_method_full(
        &mut self,
        obj: &Value,
        method: &str,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Value, NyblError> {
        // User-method dispatch first — matches MethodCall's
        // priority so `fn MyType.iter(self)` wins over any
        // built-in method of the same name.
        let type_key: Option<(String, String)> = match obj {
            Value::Struct(s) => Some((s.module_path().to_string(), s.type_name().to_string())),
            Value::EnumVariant(e) => Some((e.module_path().to_string(), e.type_name().to_string())),
            _ => None,
        };
        if let Some(key) = type_key {
            let user = self
                .methods
                .get(&key)
                .and_then(|ms| ms.get(method))
                .cloned();
            if let Some(m) = user {
                if !crate::ref_params::accepts_arity(&m.param_modes, args.len() + 1) {
                    let required = crate::ref_params::required_arity(&m.param_modes);
                    let variadic = matches!(m.param_modes.last(), Some(ParamMode::Rest));
                    return Err(error(
                        line,
                        if variadic {
                            format!(
                                "`{}.{method}` expects at least {required} arguments (including `self`), but got {}",
                                key.1,
                                args.len() + 1
                            )
                        } else {
                            format!(
                                "`{}.{method}` expects {required} argument{} (including `self`), but got {}",
                                key.1,
                                if required == 1 { "" } else { "s" },
                                args.len() + 1
                            )
                        },
                    ));
                }
                let mut full_args = Vec::with_capacity(args.len() + 1);
                full_args.push(obj.clone());
                full_args.extend(args);
                return self.call_nybl_fn(&m, full_args, line);
            }
        }
        // Fall through to the built-in dispatch.
        let (ret, _mutated) = self.call_method(obj, method, &args, line)?;
        Ok(ret)
    }

    fn call_method(
        &mut self,
        obj: &Value,
        method: &str,
        args: &[Value],
        line: u32,
    ) -> Result<(Value, Option<Value>), NyblError> {
        // `type` / `to_str` / `inspect` work on every value.
        // Try the shared dispatcher first so we don't have to
        // duplicate those three names across every type-
        // specific method table.
        if let Some(result) = methods::common_method_in(obj, method, args, line, &self.memory)? {
            return Ok(result);
        }
        // Built-in Result combinators (`is_ok`, `is_err`,
        // `unwrap`, `expect`, `unwrap_or`). Callable-taking
        // variants (`map`, `map_err`, `and_then`) are dispatched
        // one level up in the MethodCall arm so they can invoke
        // the user callable via `call_value`.
        if methods::is_builtin_result(obj)
            && let Some(v) = methods::result_method(obj, method, args, line)?
        {
            return Ok((v, None));
        }
        match obj {
            Value::Array(arr) => methods::array_method_in(arr, method, args, line, &self.memory),
            Value::Str(s) => methods::string_method_in(s, method, args, line, &self.memory),
            Value::Dict(entries) => {
                methods::dict_method_in(entries, method, args, line, &self.memory)
            }
            Value::Int(_) | Value::Number(_) => methods::numeric_method(obj, method, args, line),
            Value::Bool(_) => methods::bool_method(obj, method, args, line),
            Value::Iter(_) => methods::iter_method_in(obj, method, args, line, &self.memory),
            Value::Host(value) => match self.host.call_method(value, method, args, line) {
                Some(result) => result.map(|value| (value, None)),
                None => Err(error(
                    line,
                    crate::error_messages::no_such_method(obj.type_name(), method),
                )),
            },
            _ => Err(error(
                line,
                crate::error_messages::no_such_method(obj.type_name(), method),
            )),
        }
    }

    /// Handle `r.map(f)`, `r.map_err(f)`, `r.and_then(f)` for a
    /// built-in `Result` receiver. Factored out of the
    /// `MethodCall` arm so the evaluator / VM / AOT can keep the
    /// same shape.
    fn call_result_callable_method(
        &mut self,
        receiver: &Value,
        kind: methods::ResultCallableKind,
        method: &str,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Value, NyblError> {
        use crate::builtins::expect_args;
        use methods::{ResultCallableKind, make_result_err_in, make_result_ok_in};
        expect_args(method, &args, 1, line)?;
        let callable = args
            .into_iter()
            .next()
            .expect("expect_args ensured len = 1");
        let variant_info = match receiver {
            Value::EnumVariant(e) => e,
            _ => return Err(error(line, "Result method called on non-Result")),
        };
        let payload = match variant_info.payload() {
            crate::value::EnumPayload::Tuple(items) if items.len() == 1 => items[0].clone(),
            // Unreachable for the built-in shape, but keep the
            // error clean rather than panicking.
            _ => {
                return Err(error(
                    line,
                    format!("malformed Result::{} payload", variant_info.variant()),
                ));
            }
        };
        let is_ok = variant_info.variant() == "Ok";
        match kind {
            ResultCallableKind::Map => {
                if is_ok {
                    let new_value = self.call_value(callable, vec![payload], line, Some(method))?;
                    make_result_ok_in(new_value, line, &self.memory)
                } else {
                    // Err passes through unchanged. Rebuild it so
                    // the caller sees the same type identity —
                    // matches the pure-Nybl combinator's behaviour.
                    make_result_err_in(payload, line, &self.memory)
                }
            }
            ResultCallableKind::MapErr => {
                if !is_ok {
                    let new_value = self.call_value(callable, vec![payload], line, Some(method))?;
                    make_result_err_in(new_value, line, &self.memory)
                } else {
                    make_result_ok_in(payload, line, &self.memory)
                }
            }
            ResultCallableKind::AndThen => {
                if is_ok {
                    // `f` is expected to return a Result; we
                    // surface whatever it returns verbatim. A
                    // match on the returned shape at the call
                    // site catches misuse.
                    self.call_value(callable, vec![payload], line, Some(method))
                } else {
                    make_result_err_in(payload, line, &self.memory)
                }
            }
        }
    }
}

// ─── Pattern matching ──────────────────────────────────────────────
//
// `pattern_matches` returns true if `value` fits `pattern`, filling
// `bindings` with every `Binding` name along the way. On a partial
// match it's the caller's responsibility to discard the bindings
// — `eval_match` does this by only consuming them after the match
// + guard both succeed.

/// Destructured view of an `Iter::Next(v) | Iter::Done` result.
/// Returned by [`unwrap_iter_result`] so the for-loop can bind
/// the payload cleanly without repeating the pattern-match.
enum IterStep {
    Next(Value),
    Done,
}

/// Inspect a value returned from an iterator's `.next()` method.
/// Returns `Some(step)` when the value is a well-formed
/// `Iter::Next(v)` / `Iter::Done` (built-in or not), or `None`
/// when it doesn't match so the caller can raise a clear error.
fn unwrap_iter_result(v: &Value) -> Option<IterStep> {
    let e = match v {
        Value::EnumVariant(e) => e,
        _ => return None,
    };
    if e.type_name() != "Iter" {
        return None;
    }
    match (e.variant(), e.payload()) {
        ("Next", crate::value::EnumPayload::Tuple(items)) if items.len() == 1 => {
            Some(IterStep::Next(items[0].clone()))
        }
        ("Done", crate::value::EnumPayload::Unit) => Some(IterStep::Done),
        _ => None,
    }
}

type BuiltinTypeTables = (
    BTreeMap<(String, String), Vec<String>>,
    BTreeMap<(String, String), Vec<VariantDecl>>,
    BTreeMap<String, String>,
);

/// Seed a fresh evaluator's type tables with the engine-wide
/// builtin types (`Result`, `RuntimeError`). The shapes come from
/// `crate::builtins` so walker / VM / AOT can never drift out of
/// sync — they all read the same source.
///
/// Returns:
/// - struct_defs keyed by `(module_path, type_name)` with the
///   builtin registered under `<builtin>`;
/// - enum_defs keyed the same way;
/// - a bare-name → module_path map seeded with entries for every
///   builtin, so the program's outermost scope can resolve
///   bare `Result::Ok(...)` and `RuntimeError { ... }` without
///   an explicit `use`.
fn seed_builtin_types() -> BuiltinTypeTables {
    use crate::value::BUILTIN_MODULE_PATH;
    let mut struct_defs: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut enum_defs: BTreeMap<(String, String), Vec<VariantDecl>> = BTreeMap::new();
    let mut type_bindings: BTreeMap<String, String> = BTreeMap::new();
    struct_defs.insert(
        (
            String::from(BUILTIN_MODULE_PATH),
            String::from("RuntimeError"),
        ),
        crate::builtins::builtin_runtime_error_fields(),
    );
    type_bindings.insert(
        String::from("RuntimeError"),
        String::from(BUILTIN_MODULE_PATH),
    );
    enum_defs.insert(
        (String::from(BUILTIN_MODULE_PATH), String::from("Result")),
        crate::builtins::builtin_result_variants(),
    );
    type_bindings.insert(String::from("Result"), String::from(BUILTIN_MODULE_PATH));
    enum_defs.insert(
        (String::from(BUILTIN_MODULE_PATH), String::from("Iter")),
        crate::builtins::builtin_iter_variants(),
    );
    type_bindings.insert(String::from("Iter"), String::from(BUILTIN_MODULE_PATH));
    (struct_defs, enum_defs, type_bindings)
}

/// True when two enum declarations describe the same runtime
/// shape. Strictly looser than `PartialEq` on `Vec<VariantDecl>`:
/// tuple variants compare by *arity* only (their payload field
/// names are positional stubs with no runtime meaning), while
/// struct variants still require matching field names. Same for
/// the outer variant ordering — positional.
///
/// Used by the "redeclare-same-shape" rules so a user program
/// that declares `enum Result { Ok(v), Err(e) }` is compatible
/// with the engine's builtin `enum Result { Ok(value), Err(error) }`.
fn variants_equivalent(a: &[VariantDecl], b: &[VariantDecl]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (va, vb) in a.iter().zip(b.iter()) {
        if va.name != vb.name {
            return false;
        }
        match (&va.kind, &vb.kind) {
            (VariantKind::Unit, VariantKind::Unit) => {}
            (VariantKind::Tuple(fa), VariantKind::Tuple(fb)) => {
                if fa.len() != fb.len() {
                    return false;
                }
            }
            (VariantKind::Struct(fa), VariantKind::Struct(fb)) => {
                if fa != fb {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Walk a pair of scope stacks to resolve a source-level type
/// reference. This is free-standing so the
/// pattern matcher can be called with a borrow of the
/// evaluator's tables without needing the evaluator itself.
/// Returns `None` if the name isn't in scope.
///
/// `module_aliases` is the persistent map of aliased `use`
/// modules so namespaced references (`m.Color`) resolve even
/// inside function bodies, where `scopes` no longer contains
/// `m` (the function call stack is reset per-call).
pub fn resolve_type_in(
    value_scopes: &[BTreeMap<String, Value>],
    type_scopes: &[BTreeMap<String, String>],
    module_aliases: &BTreeMap<String, Rc<crate::value::NyblModule>>,
    namespace: Option<&str>,
    type_name: &str,
) -> Option<String> {
    resolve_type_in_scoped(
        value_scopes,
        type_scopes,
        core::slice::from_ref(module_aliases),
        None,
        namespace,
        type_name,
    )
}

fn resolve_type_in_evaluator_context(
    value_scopes: &[BTreeMap<String, Value>],
    type_scopes: &[BTreeMap<String, String>],
    module_aliases: &[BTreeMap<String, Rc<crate::value::NyblModule>>],
    local_floor: Option<usize>,
    defining_context: Option<&ModuleLexicalContext>,
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
        for (index, aliases) in module_aliases.iter().enumerate().rev() {
            if local_floor.is_some_and(|floor| index < floor) {
                continue;
            }
            if let Some(module) = aliases.get(namespace) {
                return module.type_origin(type_name).map(str::to_string);
            }
        }
        return defining_context
            .and_then(|context| context.module_aliases.get(namespace))
            .and_then(|module| module.type_origin(type_name).map(str::to_string));
    }

    for (index, bindings) in type_scopes.iter().enumerate().rev() {
        if local_floor.is_some_and(|floor| index < floor) {
            continue;
        }
        if let Some(module_path) = bindings.get(type_name) {
            return Some(module_path.clone());
        }
    }
    defining_context.and_then(|context| context.type_bindings.get(type_name).cloned())
}

/// Scoped variant used by evaluators that retain one module-alias frame per
/// lexical value scope. At call time the defining module's context is copied
/// at `alias_scope_floor`, so every caller frame below that floor is hidden.
#[doc(hidden)]
pub fn resolve_type_in_scoped<T, A>(
    value_scopes: &[BTreeMap<String, Value>],
    type_scopes: &[T],
    module_aliases: &[A],
    alias_scope_floor: Option<usize>,
    namespace: Option<&str>,
    type_name: &str,
) -> Option<String>
where
    T: core::borrow::Borrow<BTreeMap<String, String>>,
    A: core::borrow::Borrow<BTreeMap<String, Rc<crate::value::NyblModule>>>,
{
    if let Some(ns) = namespace {
        // First: look the alias up in value scopes (catches
        // locally-bound modules, if any). Then fall back to
        // the evaluator-level alias map so function bodies can
        // reach module-level aliases even when their own value
        // scope is empty.
        for scope in value_scopes.iter().rev() {
            if let Some(value) = scope.get(ns) {
                if let Value::Module(module) = value {
                    return module.type_origin(type_name).map(str::to_string);
                }
                return None;
            }
        }
        for (index, frame) in module_aliases.iter().enumerate().rev() {
            if alias_scope_floor.is_some_and(|floor| index < floor) {
                continue;
            }
            if let Some(module) = frame.borrow().get(ns) {
                return module.type_origin(type_name).map(str::to_string);
            }
        }
        return None;
    }
    for (index, scope) in type_scopes.iter().enumerate().rev() {
        if alias_scope_floor.is_some_and(|floor| index < floor) {
            continue;
        }
        if let Some(mp) = scope.borrow().get(type_name) {
            return Some(mp.clone());
        }
    }
    None
}

/// Resolver that turns a source-level type reference — optional
/// namespace plus bare name (`m.Color` vs plain `Color`) — into
/// the declaring module's path. Pattern matching threads this
/// through so a pattern like `Color::Red` only matches values
/// whose module identity agrees with the current scope's binding
/// of `Color`.
///
/// Returning `None` means "no type with this name is visible in
/// the calling scope" — the matcher treats that as a mismatch
/// rather than a fallback, to avoid silently matching a
/// same-named type from a different module.
pub type TypeResolveFn<'a> = &'a dyn Fn(Option<&str>, &str) -> Option<String>;

/// Attempt to match `pattern` against `value`. On success, appends
/// any captured `(name, Value)` bindings to `bindings` and returns
/// `true`; on failure, returns `false` and leaves `bindings` in an
/// undefined state — it's the caller's responsibility to discard it.
///
/// `resolver` is consulted whenever a pattern names a user type
/// (`Pattern::Struct` or `Pattern::EnumVariant`) so the matcher
/// can compare the value's full identity `(module_path,
/// type_name)` against the source-level reference.
///
/// Exported so other engines (the bytecode VM, AOT transpiler)
/// can run the exact same structural matcher as the tree-walker
/// without re-implementing the rules.
pub fn pattern_matches(
    pattern: &Pattern,
    value: &Value,
    bindings: &mut Vec<(String, Value)>,
    resolver: TypeResolveFn<'_>,
) -> bool {
    pattern_matches_in(
        pattern,
        value,
        bindings,
        resolver,
        &crate::memory::MemoryContext::__legacy_current(),
    )
}

#[doc(hidden)]
pub fn pattern_matches_in(
    pattern: &Pattern,
    value: &Value,
    bindings: &mut Vec<(String, Value)>,
    resolver: TypeResolveFn<'_>,
    memory: &crate::memory::MemoryContext,
) -> bool {
    match pattern {
        Pattern::Wildcard => true,
        Pattern::Binding(name) => {
            bindings.push((name.clone(), value.clone()));
            true
        }
        Pattern::Literal(lit) => match (lit, value) {
            (LiteralPattern::Int(a), Value::Int(b)) => a == b,
            (LiteralPattern::Number(a), Value::Number(b)) => a == b,
            // Cross-type numeric literal patterns — same rule
            // as `values_equal`: `match 1 { 1.0 => ... }` and
            // `match 1.0 { 1 => ... }` both match.
            (LiteralPattern::Int(a), Value::Number(b)) => (*a as f64) == *b,
            (LiteralPattern::Number(a), Value::Int(b)) => *a == (*b as f64),
            (LiteralPattern::Str(a), Value::Str(b)) => a.as_str() == b.as_str(),
            (LiteralPattern::Bool(a), Value::Bool(b)) => a == b,
            (LiteralPattern::None, Value::None) => true,
            _ => false,
        },
        Pattern::EnumVariant {
            namespace,
            type_name,
            variant,
            payload,
        } => {
            let ev = match value {
                Value::EnumVariant(e) => e,
                _ => return false,
            };
            // Full identity check: resolve the pattern's type
            // reference to a module path, then compare against
            // the value's own module path.
            let expected_mp = match resolver(namespace.as_deref(), type_name) {
                Some(mp) => mp,
                None => return false,
            };
            if ev.module_path() != expected_mp.as_str()
                || ev.type_name() != type_name.as_str()
                || ev.variant() != variant.as_str()
            {
                return false;
            }
            match (payload, ev.payload()) {
                (VariantPatternPayload::Unit, crate::value::EnumPayload::Unit) => true,
                (VariantPatternPayload::Tuple(pats), crate::value::EnumPayload::Tuple(items)) => {
                    if pats.len() != items.len() {
                        return false;
                    }
                    for (p, v) in pats.iter().zip(items.iter()) {
                        if !pattern_matches_in(p, v, bindings, resolver, memory) {
                            return false;
                        }
                    }
                    true
                }
                (
                    VariantPatternPayload::Struct { fields, rest },
                    crate::value::EnumPayload::Struct(entries),
                ) => match_struct_fields(fields, *rest, entries, bindings, resolver, memory),
                _ => false,
            }
        }
        Pattern::Struct {
            namespace,
            type_name,
            fields,
            rest,
        } => {
            let st = match value {
                Value::Struct(s) => s,
                _ => return false,
            };
            let expected_mp = match resolver(namespace.as_deref(), type_name) {
                Some(mp) => mp,
                None => return false,
            };
            if st.module_path() != expected_mp.as_str() || st.type_name() != type_name.as_str() {
                return false;
            }
            match_struct_fields(fields, *rest, st.fields(), bindings, resolver, memory)
        }
        Pattern::Array { elements, rest } => {
            let items = match value {
                Value::Array(arr) => arr,
                _ => return false,
            };
            match rest {
                None => {
                    if elements.len() != items.len() {
                        return false;
                    }
                    for (p, v) in elements.iter().zip(items.iter()) {
                        if !pattern_matches_in(p, v, bindings, resolver, memory) {
                            return false;
                        }
                    }
                    true
                }
                Some(rest_kind) => {
                    if items.len() < elements.len() {
                        return false;
                    }
                    for (i, p) in elements.iter().enumerate() {
                        if !pattern_matches_in(p, &items[i], bindings, resolver, memory) {
                            return false;
                        }
                    }
                    if let ArrayRest::Named(name) = rest_kind {
                        let tail: Vec<Value> = items[elements.len()..].to_vec();
                        bindings.push((
                            name.clone(),
                            Value::__try_new_array_in(tail, 0, memory)
                                .expect("an array-rest slice cannot increase ownership depth"),
                        ));
                    }
                    true
                }
            }
        }
        Pattern::Or(alts) => {
            for alt in alts {
                let mut attempt: Vec<(String, Value)> = Vec::new();
                if pattern_matches_in(alt, value, &mut attempt, resolver, memory) {
                    bindings.extend(attempt);
                    return true;
                }
            }
            false
        }
    }
}

fn match_struct_fields(
    fields: &[(String, Pattern)],
    rest: bool,
    entries: &[(String, Value)],
    bindings: &mut Vec<(String, Value)>,
    resolver: TypeResolveFn<'_>,
    memory: &crate::memory::MemoryContext,
) -> bool {
    // Every declared field-pattern must find its value in the
    // struct. `rest` just relaxes the requirement that *every*
    // value's key must appear in the pattern — non-rest patterns
    // may still leave fields unmatched, which is fine: the walker
    // matches named-field lookup semantics, not whole-shape
    // destructuring.
    let _ = rest;
    for (fname, pat) in fields {
        let value = match entries.iter().find(|(k, _)| k == fname) {
            Some((_, v)) => v,
            None => return false,
        };
        if !pattern_matches_in(pat, value, bindings, resolver, memory) {
            return false;
        }
    }
    true
}

// ─── REPL session ──────────────────────────────────────────────────
//
// `Evaluator` itself is a one-shot: `Evaluator::new(host, limits)` +
// `eval.run(stmts)` + drop. That's the right shape for running a
// complete program from top to bottom. REPL workloads need the
// opposite: fresh *input* evaluated against *accumulated* state.
//
// `ReplSession` owns every Evaluator field that should survive
// across inputs — scopes, functions, user types, method tables,
// import caches, module aliases, rng state. The ephemeral
// per-run fields (host, step counter, call depth, the try-
// return sentinel slot, transient warnings) stay on the
// temporary `Evaluator` that the session spins up for each
// `eval` call.
//
// The session model also handles REPL-specific niceties:
//
//   - Bare expression statements at the end of an input yield
//     their value back to the caller, so the REPL can echo it.
//     `let x = 5` returns None; `x + 1` returns Some(Value).
//   - Scope depth is normalised back to the root on each
//     `eval` — if an earlier input errored mid-block, we
//     don't leave half-pushed scopes lying around for the next
//     input to trip over.

/// Persistent state for an interactive REPL session. Build
/// once with [`Self::new`], then call [`Self::eval`] for each
/// fresh line / block the user types. State carried across
/// calls: every `let` / `const` / `fn` / `struct` / `enum` /
/// method declaration, every `use`'d module's bindings and
/// aliases, the global rng seed, and the import cache.
///
/// A `ReplSession` is not a sandboxing boundary — embedders
/// that want a fresh identity between inputs should construct
/// a new session. Resource limits (steps, memory) are supplied
/// per `eval` call since they reset each time.
pub struct ReplSession {
    scopes: Vec<BTreeMap<String, Value>>,
    functions: BTreeMap<String, FunctionDefinition>,
    binding_origins: BTreeMap<String, (String, String)>,
    abi_declarations: Vec<(String, Visibility, Rc<NyblFn>)>,
    live_value_environments: LiveValueEnvironments,
    live_binding_origins: LiveBindingOrigins,
    active_value_module: String,
    function_origin: NyblFnOrigin,
    current_module: String,
    struct_defs: BTreeMap<(String, String), Vec<String>>,
    enum_defs: BTreeMap<(String, String), Vec<VariantDecl>>,
    methods: BTreeMap<(String, String), BTreeMap<String, FunctionDefinition>>,
    type_exports: BTreeMap<String, String>,
    type_bindings: Vec<BTreeMap<String, String>>,
    module_aliases: Vec<BTreeMap<String, Rc<crate::value::NyblModule>>>,
    imported_functions: Vec<BTreeMap<String, FunctionDefinition>>,
    root_lexical_context: Rc<ModuleLexicalContext>,
    imports: ImportCache,
    imported_here: Vec<alloc_import::collections::BTreeSet<String>>,
    rand_state: u64,
}

impl Default for ReplSession {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplSession {
    pub(crate) fn validate_instance_callable(
        &self,
        callable: &Value,
        arg_count: usize,
    ) -> Result<(), NyblError> {
        let function = match callable {
            Value::Fn(function) => function,
            other => {
                return Err(error(
                    0,
                    format!("expected function, got {}", other.type_name()),
                ));
            }
        };
        if !function.__is_allowed_by(&self.function_origin, "walker") {
            return Err(error(
                0,
                "This function belongs to a different Nybl engine instance",
            ));
        }
        if !crate::ref_params::accepts_arity(&function.param_modes, arg_count) {
            let required = crate::ref_params::required_arity(&function.param_modes);
            let variadic = matches!(function.param_modes.last(), Some(ParamMode::Rest));
            return Err(error(
                0,
                if variadic {
                    format!("Function expects at least {required} arguments, but got {arg_count}")
                } else {
                    format!(
                        "Function expects {required} argument{}, but got {arg_count}",
                        if required == 1 { "" } else { "s" }
                    )
                },
            ));
        }
        Ok(())
    }

    /// Build a fresh session. Type tables are pre-seeded with
    /// the engine builtins (`Result`, `RuntimeError`) so
    /// `Result::Ok(...)` resolves without an explicit `use`.
    pub fn new() -> Self {
        let (struct_defs, enum_defs, builtin_bindings) = seed_builtin_types();
        let root_lexical_context = Rc::new(ModuleLexicalContext {
            type_bindings: Rc::new(builtin_bindings.clone()),
            module_aliases: Rc::new(BTreeMap::new()),
            imported_functions: Rc::new(BTreeMap::new()),
        });
        Self {
            scopes: vec![BTreeMap::new()],
            functions: BTreeMap::new(),
            binding_origins: BTreeMap::new(),
            abi_declarations: Vec::new(),
            live_value_environments: Rc::new(RefCell::new(BTreeMap::new())),
            live_binding_origins: Rc::new(RefCell::new(BTreeMap::new())),
            active_value_module: String::from(crate::value::ROOT_MODULE_PATH),
            function_origin: NyblFnOrigin::__instance("walker"),
            current_module: String::from(crate::value::ROOT_MODULE_PATH),
            struct_defs,
            enum_defs,
            methods: BTreeMap::new(),
            type_exports: BTreeMap::new(),
            type_bindings: vec![builtin_bindings],
            module_aliases: vec![BTreeMap::new()],
            imported_functions: vec![BTreeMap::new()],
            root_lexical_context,
            imports: Rc::new(RefCell::new(alloc_import::collections::BTreeMap::new())),
            imported_here: vec![alloc_import::collections::BTreeSet::new()],
            rand_state: 0,
        }
    }

    /// Look up a binding by name. Convenience for tests and
    /// embedders that want to peek at the session's state
    /// between `eval` calls — e.g. checking that `let x = 5`
    /// actually stuck.
    pub fn get(&self, name: &str) -> Option<Value> {
        for scope in self.scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v.clone());
            }
        }
        None
    }

    /// Every currently-bound name in the root scope, sorted.
    /// Handy for REPL introspection commands (`:vars`) and
    /// for tab-completers that want to stay honest about
    /// what's actually in scope rather than guessing from
    /// observed tokens.
    pub fn binding_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .scopes
            .first()
            .map(|s| s.keys().cloned().collect())
            .unwrap_or_default();
        for name in self.functions.keys() {
            names.push(name.clone());
        }
        names.sort();
        names.dedup();
        names
    }

    /// Parse `source` and run its statements against this
    /// session's accumulated state.
    ///
    /// If the last statement is a bare expression
    /// (`ExprStmt`), the session evaluates it and returns its
    /// value as `Ok(Some(v))` so the REPL can echo the
    /// result. Every other shape — `let x = ...`, `fn foo`,
    /// `struct`, `use`, loops — returns `Ok(None)`.
    ///
    /// Partial failure semantics: if an earlier statement
    /// errors, the session's state reflects whatever ran
    /// before the failure. That matches what a user would
    /// expect from an interactive prompt.
    pub fn eval<H: NyblHost + ?Sized>(
        &mut self,
        source: &str,
        host: &mut H,
        limits: &NyblLimits,
    ) -> Result<Option<Value>, NyblError> {
        let stmts = crate::parse(source)?;
        self.run_stmts(&stmts, host, limits)
    }

    /// Like [`Self::eval`] but takes an already-parsed AST.
    /// Useful when the caller has already run a
    /// `parse_with_warnings` pass and wants to surface
    /// warnings before executing.
    pub fn run_stmts<H: NyblHost + ?Sized>(
        &mut self,
        stmts: &[Stmt],
        host: &mut H,
        limits: &NyblLimits,
    ) -> Result<Option<Value>, NyblError> {
        let memory = crate::memory::MemoryContext::__new(limits.max_memory);
        self.run_stmts_in(stmts, host, limits, &memory)
    }

    pub(crate) fn run_stmts_in<H: NyblHost + ?Sized>(
        &mut self,
        stmts: &[Stmt],
        host: &mut H,
        limits: &NyblLimits,
        memory: &crate::memory::MemoryContext,
    ) -> Result<Option<Value>, NyblError> {
        // If the last stmt is a bare expression we strip it
        // off, run the rest, and then evaluate it as an
        // expression so we can return its value. Anything
        // else runs as a normal statement block.
        let (tail_expr, body) = match stmts.last().map(|s| &s.kind) {
            Some(StmtKind::ExprStmt(_)) => {
                let (last, rest) = stmts.split_last().unwrap();
                let expr = match &last.kind {
                    StmtKind::ExprStmt(e) => e.clone(),
                    _ => unreachable!(),
                };
                (Some(expr), rest)
            }
            _ => (None, stmts),
        };

        // Spin up a temporary Evaluator around the session's
        // state. We hand off each field by `mem::take` /
        // `clone` of the Rc so the Evaluator owns working
        // copies; at the end we unconditionally write the
        // (possibly-mutated) state back so partial failures
        // still persist whatever changes happened before the
        // error.
        let mut eval = self.take_evaluator(host, limits.clone(), memory.clone());
        let mut result: Result<Option<Value>, NyblError> = (|| {
            let sig = eval.exec_block(body)?;
            match sig {
                Signal::Break => Err(error(0, "break used outside of a loop")),
                Signal::Continue => Err(error(0, "continue used outside of a loop")),
                Signal::Return(_) => Ok(None),
                Signal::None => match tail_expr {
                    Some(expr) => Ok(Some(eval.eval_expr(&expr)?)),
                    None => Ok(None),
                },
            }
        })();
        if result.is_ok() && memory.__exceeded() {
            result = Err(error_fatal_with_hint(
                stmts.last().map_or(0, |stmt| stmt.line),
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            ));
        }
        // Surface any warnings accumulated on this run
        // (currently: glob-import shadowing). Stderr delivery
        // matches `Evaluator::run`; embedders that want
        // structured access can call `eval` via a custom host
        // and take warnings out through that path.
        write_runtime_warnings(&mut eval.runtime_warnings);
        self.put_evaluator(eval);
        result
    }

    /// Call a Nybl closure value inside this session's current module state.
    ///
    /// Embedders that retain closure values from an earlier evaluation can use
    /// this to invoke them later while preserving the same functions, imports,
    /// type bindings, and module aliases that normal in-language calls see.
    pub fn call_fn<H: NyblHost + ?Sized>(
        &mut self,
        func: Value,
        args: Vec<Value>,
        host: &mut H,
        limits: &NyblLimits,
    ) -> Result<Value, NyblError> {
        let memory = crate::memory::MemoryContext::__new(limits.max_memory);
        self.call_fn_in(func, args, host, limits, &memory)
    }

    pub(crate) fn call_fn_in<H: NyblHost + ?Sized>(
        &mut self,
        func: Value,
        args: Vec<Value>,
        host: &mut H,
        limits: &NyblLimits,
        memory: &crate::memory::MemoryContext,
    ) -> Result<Value, NyblError> {
        let func = match &func {
            Value::Fn(f) => Rc::clone(f),
            other => {
                return Err(error(
                    0,
                    format!("expected function, got {}", other.type_name()),
                ));
            }
        };

        let mut eval = self.take_evaluator(host, limits.clone(), memory.clone());
        let result = eval.call_nybl_fn(&func, args, 0);
        write_runtime_warnings(&mut eval.runtime_warnings);
        self.put_evaluator(eval);
        result
    }

    pub(crate) fn instance_entries(&self) -> Vec<crate::EntryPoint> {
        self.abi_declarations
            .iter()
            .filter(|(_, visibility, _)| *visibility == Visibility::Public)
            .map(|(name, _, target)| {
                let required = crate::ref_params::required_arity(&target.param_modes);
                if matches!(target.param_modes.last(), Some(ParamMode::Rest)) {
                    crate::EntryPoint::__new_variadic(name.clone(), required)
                } else {
                    crate::EntryPoint::__new(name.clone(), required)
                }
            })
            .collect()
    }

    pub(crate) fn prepared_entry_target(&self, name: &str) -> Option<Rc<NyblFn>> {
        self.abi_declarations
            .iter()
            .find(|(entry_name, visibility, _)| {
                entry_name == name && *visibility == Visibility::Public
            })
            .map(|(_, _, target)| Rc::clone(target))
    }

    pub(crate) fn call_prepared_in<H: NyblHost + ?Sized>(
        &mut self,
        target: &Rc<NyblFn>,
        args: &[Value],
        host: &mut H,
        limits: &NyblLimits,
        memory: &crate::memory::MemoryContext,
    ) -> Result<Value, NyblError> {
        let mut eval = self.take_evaluator(host, limits.clone(), memory.clone());
        let result = eval
            .call_nybl_fn_with_refs(target, args.to_vec(), &[], 0)
            .map(|(value, _)| value);
        write_runtime_warnings(&mut eval.runtime_warnings);
        self.put_evaluator(eval);
        result
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn call_prepared_batch_in<H, A>(
        &mut self,
        target: &Rc<NyblFn>,
        name: &str,
        arity: usize,
        variadic: bool,
        calls: &[A],
        host: &mut H,
        limits: &NyblLimits,
        memory: &crate::memory::MemoryContext,
    ) -> Result<Vec<Value>, NyblError>
    where
        H: NyblHost + ?Sized,
        A: AsRef<[Value]>,
    {
        let mut eval = self.take_evaluator(host, limits.clone(), memory.clone());
        let mut values = Vec::with_capacity(calls.len());
        let result = (|| {
            for call in calls {
                let args = call.as_ref();
                let accepts_arity = if variadic {
                    args.len() >= arity
                } else {
                    args.len() == arity
                };
                if !accepts_arity {
                    return Err(error(
                        0,
                        if variadic {
                            format!(
                                "`{name}` expects at least {arity} arguments, but got {}",
                                args.len()
                            )
                        } else {
                            format!(
                                "`{name}` expects {arity} argument{}, but got {}",
                                if arity == 1 { "" } else { "s" },
                                args.len()
                            )
                        },
                    ));
                }
                eval.steps = 0;
                debug_assert_eq!(eval.call_depth, 0);
                let value = eval
                    .call_nybl_fn_with_refs(target, args.to_vec(), &[], 0)
                    .map(|(value, _)| value);
                write_runtime_warnings(&mut eval.runtime_warnings);
                values.push(value?);
            }
            Ok(values)
        })();
        write_runtime_warnings(&mut eval.runtime_warnings);
        self.put_evaluator(eval);
        result
    }

    pub(crate) fn call_named<H: NyblHost + ?Sized>(
        &mut self,
        name: &str,
        args: &[Value],
        host: &mut H,
        limits: &NyblLimits,
        memory: &crate::memory::MemoryContext,
    ) -> Result<Value, NyblError> {
        let target = self
            .abi_declarations
            .iter()
            .find(|(entry_name, visibility, _)| {
                entry_name == name && *visibility == Visibility::Public
            })
            .map(|(_, _, target)| Rc::clone(target))
            .ok_or_else(|| error(0, format!("Entry point `{name}` is not available")))?;
        if !crate::ref_params::accepts_arity(&target.param_modes, args.len()) {
            let required = crate::ref_params::required_arity(&target.param_modes);
            let variadic = matches!(target.param_modes.last(), Some(ParamMode::Rest));
            return Err(error(
                0,
                if variadic {
                    format!(
                        "`{name}` expects at least {required} arguments, but got {}",
                        args.len()
                    )
                } else {
                    format!(
                        "`{name}` expects {required} argument{}, but got {}",
                        if required == 1 { "" } else { "s" },
                        args.len()
                    )
                },
            ));
        }
        let mut eval = self.take_evaluator(host, limits.clone(), memory.clone());
        let result = eval.call_nybl_fn(&target, args.to_vec(), 0);
        write_runtime_warnings(&mut eval.runtime_warnings);
        self.put_evaluator(eval);
        result
    }

    /// Internal: move the session's state into a fresh
    /// Evaluator. `host` and `limits` are the ephemeral
    /// per-run bits.
    fn take_evaluator<'h, H: NyblHost + ?Sized>(
        &mut self,
        host: &'h mut H,
        limits: NyblLimits,
        memory: crate::memory::MemoryContext,
    ) -> Evaluator<'h, H> {
        let root_lexical_context = core::mem::replace(
            &mut self.root_lexical_context,
            Rc::new(ModuleLexicalContext::default()),
        );
        Evaluator {
            scopes: core::mem::take(&mut self.scopes),
            functions: core::mem::take(&mut self.functions),
            binding_origins: core::mem::take(&mut self.binding_origins),
            abi_declarations: core::mem::take(&mut self.abi_declarations),
            live_value_environments: Rc::clone(&self.live_value_environments),
            live_binding_origins: Rc::clone(&self.live_binding_origins),
            active_value_module: core::mem::take(&mut self.active_value_module),
            function_origin: self.function_origin.clone(),
            current_module: core::mem::take(&mut self.current_module),
            struct_defs: core::mem::take(&mut self.struct_defs),
            enum_defs: core::mem::take(&mut self.enum_defs),
            methods: core::mem::take(&mut self.methods),
            type_exports: core::mem::take(&mut self.type_exports),
            type_bindings: core::mem::take(&mut self.type_bindings),
            module_aliases: core::mem::take(&mut self.module_aliases),
            imported_functions: core::mem::take(&mut self.imported_functions),
            root_lexical_context,
            active_lexical_contexts: Vec::new(),
            imports: Rc::clone(&self.imports),
            imported_here: core::mem::take(&mut self.imported_here),
            rand_state: self.rand_state,
            host,
            steps: 0,
            loop_lines: Vec::new(),
            call_depth: 0,
            function_modules: Vec::new(),
            alias_scope_floors: Vec::new(),
            limits,
            memory,
            pending_try_return: None,
            active_ref_bindings: alloc_import::collections::BTreeSet::new(),
            active_capture_bindings: alloc_import::collections::BTreeSet::new(),
            runtime_warnings: Vec::new(),
        }
    }

    /// Internal: move the Evaluator's state back into the
    /// session. Also normalises scope depth back to the root
    /// so a mid-block error doesn't leave leftover pushed
    /// scopes hanging around for the next input.
    fn put_evaluator<'h, H: NyblHost + ?Sized>(&mut self, eval: Evaluator<'h, H>) {
        self.root_lexical_context = Rc::clone(&eval.root_lexical_context);
        let mut scopes = eval.scopes;
        if scopes.len() > 1 {
            scopes.truncate(1);
        }
        if scopes.is_empty() {
            scopes.push(BTreeMap::new());
        }
        let mut type_bindings = eval.type_bindings;
        if type_bindings.len() > 1 {
            type_bindings.truncate(1);
        }
        if type_bindings.is_empty() {
            // Re-seed so builtins stay visible.
            let (_, _, builtins) = seed_builtin_types();
            type_bindings.push(builtins);
        }
        self.scopes = scopes;
        self.functions = eval.functions;
        self.binding_origins = eval.binding_origins;
        self.abi_declarations = eval.abi_declarations;
        self.active_value_module = eval.active_value_module;
        self.current_module = eval.current_module;
        self.struct_defs = eval.struct_defs;
        self.enum_defs = eval.enum_defs;
        self.methods = eval.methods;
        self.type_exports = eval.type_exports;
        self.type_bindings = type_bindings;
        let mut module_aliases = eval.module_aliases;
        if module_aliases.len() > 1 {
            module_aliases.truncate(1);
        }
        if module_aliases.is_empty() {
            module_aliases.push(BTreeMap::new());
        }
        self.module_aliases = module_aliases;
        let mut imported_functions = eval.imported_functions;
        if imported_functions.len() > 1 {
            imported_functions.truncate(1);
        }
        if imported_functions.is_empty() {
            imported_functions.push(BTreeMap::new());
        }
        self.imported_functions = imported_functions;
        let mut imported_here = eval.imported_here;
        if imported_here.len() > 1 {
            imported_here.truncate(1);
        }
        if imported_here.is_empty() {
            imported_here.push(alloc_import::collections::BTreeSet::new());
        }
        self.imported_here = imported_here;
        self.rand_state = eval.rand_state;
        // `imports` is shared via `Rc` — nothing to do; the
        // cache the Evaluator wrote into is the same one the
        // session holds.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct TestHost {
        prints: Vec<String>,
    }

    impl NyblHost for TestHost {
        fn call(
            &mut self,
            _name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, NyblError>> {
            None
        }

        fn on_print(&mut self, message: &str) {
            self.prints.push(message.to_string());
        }
    }

    fn run(source: &str) -> Vec<String> {
        let statements = crate::parse(source).expect("program should parse");
        let mut host = TestHost::default();
        Evaluator::new(&mut host, crate::NyblLimits::standard())
            .run(&statements)
            .expect("program should run");
        host.prints
    }

    #[test]
    fn named_calls_reuse_one_shared_module_context() {
        let mut host = TestHost::default();
        let evaluator = Evaluator::new(&mut host, crate::NyblLimits::standard());
        let context = evaluator.module_lexical_context(crate::value::ROOT_MODULE_PATH);

        assert!(Rc::ptr_eq(&context, &evaluator.root_lexical_context));
        assert_eq!(Rc::strong_count(&context), 2);
    }

    #[test]
    fn root_type_publication_reuses_unique_context_storage() {
        const DECLARATIONS: usize = 1_024;
        let mut source = String::new();
        for index in 0..DECLARATIONS {
            source.push_str(&format!("struct Type{index} {{}}\n"));
        }
        let statements = crate::parse(&source).expect("type-heavy source should parse");
        let mut host = TestHost::default();
        let mut evaluator = Evaluator::new(&mut host, crate::NyblLimits::standard());
        let context = Rc::as_ptr(&evaluator.root_lexical_context);
        let bindings = Rc::as_ptr(&evaluator.root_lexical_context.type_bindings);

        evaluator
            .exec_block(&statements)
            .expect("declarations should execute");

        assert_eq!(Rc::as_ptr(&evaluator.root_lexical_context), context);
        assert_eq!(
            Rc::as_ptr(&evaluator.root_lexical_context.type_bindings),
            bindings
        );
        assert_eq!(
            evaluator.root_lexical_context.type_bindings.len(),
            DECLARATIONS + 3
        );
    }

    #[test]
    fn restored_lexical_snapshot_forks_without_leaking_abandoned_changes() {
        let mut host = TestHost::default();
        let mut evaluator = Evaluator::new(&mut host, crate::NyblLimits::standard());
        evaluator.publish_root_type_binding("A".to_string(), "root".to_string());
        let snapshot = Rc::clone(&evaluator.root_lexical_context);
        evaluator.publish_root_type_binding("B".to_string(), "leaked".to_string());

        evaluator.root_lexical_context = Rc::clone(&snapshot);
        evaluator.publish_root_type_binding("C".to_string(), "root".to_string());

        assert_eq!(
            evaluator.root_lexical_context.type_bindings.get("A"),
            Some(&"root".to_string())
        );
        assert!(
            !evaluator
                .root_lexical_context
                .type_bindings
                .contains_key("B")
        );
        assert_eq!(
            evaluator.root_lexical_context.type_bindings.get("C"),
            Some(&"root".to_string())
        );
    }

    #[test]
    fn repeated_publication_replaces_in_place_without_retaining_history() {
        let mut host = TestHost::default();
        let mut evaluator = Evaluator::new(&mut host, crate::NyblLimits::standard());
        let bindings = Rc::as_ptr(&evaluator.root_lexical_context.type_bindings);

        for index in 0..1_000 {
            evaluator.publish_root_type_binding("Repeated".to_string(), index.to_string());
        }

        assert_eq!(
            Rc::as_ptr(&evaluator.root_lexical_context.type_bindings),
            bindings
        );
        assert_eq!(evaluator.root_lexical_context.type_bindings.len(), 4);
        assert_eq!(
            evaluator.root_lexical_context.type_bindings.get("Repeated"),
            Some(&"999".to_string())
        );
    }

    #[test]
    fn named_lookups_and_first_class_values_share_one_large_function_ast() {
        const UNREACHABLE_STATEMENTS: usize = 4_096;

        let mut source = String::from("fn large() { return 7\n");
        for _ in 0..UNREACHABLE_STATEMENTS {
            source.push_str("none\n");
        }
        source.push_str("}\n");

        let statements = crate::parse(&source).expect("large function should parse");
        let mut host = TestHost::default();
        let mut evaluator = Evaluator::new(&mut host, crate::NyblLimits::standard());
        evaluator
            .exec_block(&statements)
            .expect("declaration should execute");

        let stored = Rc::clone(evaluator.functions.get("large").unwrap());
        let first_lookup = evaluator.lookup_function("large").unwrap();
        let second_lookup = evaluator.lookup_function("large").unwrap();
        assert!(Rc::ptr_eq(&stored, &first_lookup));
        assert!(Rc::ptr_eq(&stored, &second_lookup));
        assert!(matches!(
            &stored.body,
            FnBody::Ast(body) if body.len() == UNREACHABLE_STATEMENTS + 1
        ));

        let calls = crate::parse("let alias = large\nprint(large())\nprint(alias())").unwrap();
        evaluator.exec_block(&calls).expect("calls should execute");
        let Value::Fn(alias) = evaluator.scopes[0].get("alias").unwrap() else {
            panic!("alias should retain the declared function")
        };
        assert!(Rc::ptr_eq(&stored, alias));
        assert_eq!(host.prints, ["7", "7"]);
    }

    struct FanoutHost {
        modules: BTreeMap<String, String>,
    }

    impl NyblHost for FanoutHost {
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

    #[derive(Default)]
    struct DispatchHost {
        prints: Vec<String>,
        modules: BTreeMap<String, String>,
    }

    impl NyblHost for DispatchHost {
        fn call(
            &mut self,
            name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, NyblError>> {
            (name == "pick").then_some(Ok(Value::Int(99)))
        }

        fn on_print(&mut self, message: &str) {
            self.prints.push(message.to_string());
        }

        fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
            self.modules.get(name).cloned().map(Ok)
        }
    }

    #[test]
    fn shared_definitions_preserve_named_call_semantics() {
        let source = r#"
fn recurse(n) {
    if n == 0 { return 0 }
    return recurse(n - 1) + 1
}
let original = recurse
fn recurse(n) { return n + 10 }

fn bump(ref value) { value += 2 }
let bump_alias = bump
let value = 1
bump(ref value)
bump_alias(ref value)

fn pick() { return 1 }
let local_pick = pick

struct Box { value }
fn Box.add(self, amount) { return self.value + amount }

use funcs as funcs
print(original(4), recurse(4), value)
print(pick(), local_pick(), funcs.call(5))
print(Box { value: 2 }.add(3))
"#;
        let mut host = DispatchHost {
            modules: BTreeMap::from([(
                "funcs".to_string(),
                "fn twice(n) { return n * 2 }\nfn call(n) { return twice(n) + 1 }".to_string(),
            )]),
            ..DispatchHost::default()
        };
        let statements = crate::parse(source).expect("program should parse");
        Evaluator::new(&mut host, crate::NyblLimits::standard())
            .run(&statements)
            .expect("program should run");

        assert_eq!(host.prints, ["4 14 5", "99 1 11", "5"]);
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
        let statements = crate::parse(&source).unwrap();
        let mut host = FanoutHost { modules };
        let mut evaluator = Evaluator::new(&mut host, crate::NyblLimits::standard());
        evaluator.exec_block(&statements).unwrap();
        for index in 0..FACADES {
            assert!(matches!(
                evaluator.scopes[0].get(&format!("size{index}")),
                Some(Value::Int(256))
            ));
        }

        let imports = evaluator.imports.borrow();
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
    fn bare_array_mutation_observes_argument_effects_first() {
        assert_eq!(run("let a = [1, 2]\na.push(a.pop())\nprint(a)"), ["[1, 2]"]);
    }

    #[test]
    fn direct_mutation_preserves_value_semantic_aliases() {
        assert_eq!(
            run("let a = [1, 2]\nlet b = a\na.push(3)\nprint(a)\nprint(b)"),
            ["[1, 2, 3]", "[1, 2]"]
        );
    }
}
