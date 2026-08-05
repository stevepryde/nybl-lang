//! AST → Rust source emitter.
//!
//! The generated file is one self-contained module. At the top it
//! pulls in `nybl-lang` (`nybl` crate) for the runtime surface and
//! declares a tiny set of helpers; then each user-defined Nybl
//! function becomes a Rust fn, and the top-level program becomes
//! `run_program`. When `Options::emit_main` is set, a `main()`
//! drives everything through `nybl_sys::StandardHost`.
//!
//! # Codegen shape
//!
//! - Each Nybl expression lowers to a Rust expression of type
//!   [`nybl::value::Value`]. Fallible expressions propagate with `?`.
//! - Composite expressions (ops, calls, collection literals) wrap
//!   their sub-expressions in a `{ let __a = ...; ... }` block so
//!   that sequential evaluation is explicit and the borrow checker
//!   is happy when sub-expressions both borrow `ctx`.
//! - Variables lower to hygienically-mangled Rust locals (always
//!   `mut` — we don't know if a later statement will reassign, and
//!   `#![allow(unused_mut)]` silences the warning).
//! - User functions take `&mut Ctx<'_>` plus their Nybl parameters
//!   as `Value`. Recursion and nested fns work because Rust allows
//!   both fn-in-fn definitions and forward references within a
//!   block.
//!
//! Unsupported constructs (string interpolation, method calls,
//! indexed writes) return a `NyblError::runtime` naming the feature
//! so the caller sees a clear "not yet supported" message instead of
//! broken Rust.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use nybl::ast_visit::{DeclarationSiteVisitor, visit_declaration_sites};
use nybl::error::NyblError;
use nybl::lexer::StringPart;
use nybl::methods;
use nybl::parser::{
    AssignOp, AssignTarget, BinOp, CallArg, Expr, ExprKind, MatchArm, ParamMode, Parameter, Stmt,
    StmtKind, UnaryOp, VariantDecl, VariantKind, VariantPayload, Visibility,
};

use crate::Options;

mod runtime_templates;

use runtime_templates::{
    CTX_BASE, CTX_SANDBOX, HEADER, MAIN_FN, MAIN_FN_SANDBOX, PUBLIC_ENTRY, PUBLIC_ENTRY_SANDBOX,
    RUNTIME_HEADER, RUNTIME_SHARED, TICK_HELPER,
};

pub(crate) fn emit(stmts: &[Stmt], opts: &Options) -> Result<String, NyblError> {
    // Pre-resolve every import in the program's transitive graph.
    // Failures here (missing resolver, module not found, cycle)
    // surface before any Rust is written.
    let modules = build_module_graph(stmts, opts)?;
    let info = collect_fn_info(stmts);
    let types = collect_type_registry(stmts, &modules)?;
    let mut persistent_constants = HashMap::new();
    persistent_constants.insert(
        nybl::value::ROOT_MODULE_PATH.to_string(),
        direct_persistent_constants(stmts),
    );
    for (module, entry) in &modules.modules {
        persistent_constants.insert(module.clone(), direct_persistent_constants(&entry.ast));
    }
    let mut emitter = Emitter::new(opts.clone(), info, modules, types, persistent_constants);
    emitter.emit_program(stmts)?;
    Ok(emitter.finish())
}

fn direct_persistent_constants(stmts: &[Stmt]) -> HashSet<String> {
    stmts
        .iter()
        .filter_map(|stmt| match &stmt.kind {
            StmtKind::Let {
                name,
                is_const: true,
                ..
            } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

// ─── Persistent function-site catalogue ──────────────────────────

/// A unique source declaration that can participate in a module
/// environment. Names are deliberately not keys: repeated declarations must
/// remain distinct so runtime source order can select the final executed site.
#[derive(Clone)]
struct FunctionSite {
    id: usize,
    module_path: String,
    name: String,
    params: Vec<Parameter>,
    visibility: Visibility,
    line: u32,
    abi_eligible: bool,
}

#[derive(Clone, Default)]
struct FunctionRegistry {
    sites: Vec<FunctionSite>,
    sites_by_module_name: HashMap<(String, String), Vec<usize>>,
}

// ─── Type / method registry ────────────────────────────────────────
//
// The pre-pass catalogues every user-defined type and method declaration site
// across the root program and transitively imported modules. It never folds
// sites or diagnoses clashes: generated code registers a site's static
// descriptor only when execution reaches that declaration. Method bodies are
// lifted once, but their adapters are published only at the source site.

pub(crate) struct TypeRegistry {
    /// Every syntactic struct declaration, including sites nested in
    /// control flow, callables, and lambda bodies. Sites deliberately do not
    /// fold by `(module, name)`: declaration compatibility is a runtime
    /// property because only executed sites participate.
    pub struct_sites: Vec<StructSite>,
    /// Ordered enum declaration sites. Variant order and struct-payload field
    /// order are part of the runtime shape; tuple payload names are not.
    pub enum_sites: Vec<EnumSite>,
    /// Lexically module-top-level declarations only. Lifted callables are
    /// emitted ahead of source execution and may resolve these names, but must
    /// never inherit declarations owned by a block or another callable.
    top_level_types: HashMap<String, BTreeSet<String>>,
    /// Every syntactic method declaration. Sites are never folded by key:
    /// source execution selects which uniquely lifted adapter is active.
    pub method_sites: Vec<MethodSite>,
    /// Dense, deterministic slot keys shared by all sites with the same full
    /// receiver identity `(declaring module, type, method)`.
    pub method_slots: Vec<(String, String, String)>,
}

#[derive(Clone)]
pub(crate) struct StructSite {
    pub module_path: String,
    pub name: String,
}

#[derive(Clone)]
pub(crate) struct EnumSite {
    pub module_path: String,
    pub name: String,
}

impl TypeRegistry {
    fn has_type(&self, module_path: &str, name: &str) -> bool {
        self.struct_sites
            .iter()
            .any(|site| site.module_path == module_path && site.name == name)
            || self
                .enum_sites
                .iter()
                .any(|site| site.module_path == module_path && site.name == name)
    }

    fn module_has_type_sites(&self, module_path: &str) -> bool {
        self.struct_sites
            .iter()
            .any(|site| site.module_path == module_path)
            || self
                .enum_sites
                .iter()
                .any(|site| site.module_path == module_path)
    }

    fn method_slot(&self, module_path: &str, type_name: &str, method_name: &str) -> usize {
        self.method_slots
            .binary_search_by(|(module, ty, method)| {
                (module.as_str(), ty.as_str(), method.as_str()).cmp(&(
                    module_path,
                    type_name,
                    method_name,
                ))
            })
            .expect("every method site has a dense slot")
    }

    fn module_method_slots(&self, module_path: &str) -> Vec<usize> {
        self.method_slots
            .iter()
            .enumerate()
            .filter_map(|(slot, (module, _, _))| (module == module_path).then_some(slot))
            .collect()
    }
}

#[derive(Clone)]
pub(crate) struct MethodSite {
    pub id: usize,
    pub module_path: String,
    pub type_name: String,
    pub method_name: String,
    pub params: Vec<Parameter>,
    pub body: Vec<Stmt>,
    pub module_prefix: String,
    pub line: u32,
    pub column: Option<core::num::NonZeroU32>,
}

fn collect_type_registry(root: &[Stmt], modules: &ModuleGraph) -> Result<TypeRegistry, NyblError> {
    let mut reg = TypeRegistry {
        struct_sites: Vec::new(),
        enum_sites: Vec::new(),
        top_level_types: HashMap::new(),
        method_sites: Vec::new(),
        method_slots: Vec::new(),
    };

    // Engine-wide builtins (`Result`, `RuntimeError`) go into
    // the registry before any user source is inspected, keyed
    // under `<builtin>` so a user-declared `enum Result { ... }`
    // at the program root lives under `<root>.Result` instead
    // and the two coexist without clashing.
    let builtin_mp = nybl::value::BUILTIN_MODULE_PATH.to_string();
    reg.struct_sites.push(StructSite {
        module_path: builtin_mp.clone(),
        name: String::from("RuntimeError"),
    });
    reg.enum_sites.push(EnumSite {
        module_path: builtin_mp.clone(),
        name: String::from("Result"),
    });
    reg.enum_sites.push(EnumSite {
        module_path: builtin_mp.clone(),
        name: String::from("Iter"),
    });

    // Root program contributes under `<root>`.
    collect_types_from_stmts(root, "", nybl::value::ROOT_MODULE_PATH, &mut reg);
    // Then every module's AST. The function prefix uses the same
    // injective component encoding as the emitter. The source `name`
    // is still what shows up in errors and at runtime as the module's
    // type identity.
    for name in &modules.order {
        if let Some(entry) = modules.modules.get(name) {
            let prefix = module_fn_prefix(name);
            collect_types_from_stmts(&entry.ast, &prefix, name, &mut reg);
        }
    }
    reg.method_slots = reg
        .method_sites
        .iter()
        .map(|site| {
            (
                site.module_path.clone(),
                site.type_name.clone(),
                site.method_name.clone(),
            )
        })
        .collect();
    reg.method_slots.sort();
    reg.method_slots.dedup();
    Ok(reg)
}

fn collect_types_from_stmts(
    stmts: &[Stmt],
    prefix: &str,
    module_path: &str,
    reg: &mut TypeRegistry,
) {
    let mut collector = RegistryDeclarationCollector::default();
    visit_declaration_sites(stmts, &mut collector);
    reg.struct_sites
        .extend(collector.struct_names.into_iter().map(|name| StructSite {
            module_path: module_path.to_string(),
            name,
        }));
    reg.enum_sites
        .extend(collector.enum_names.into_iter().map(|name| EnumSite {
            module_path: module_path.to_string(),
            name,
        }));
    for method in collector.methods {
        let id = reg.method_sites.len();
        reg.method_sites.push(MethodSite {
            id,
            module_path: module_path.to_string(),
            type_name: method.type_name,
            method_name: method.method_name,
            params: method.params,
            body: method.body,
            module_prefix: prefix.to_string(),
            line: method.line,
            column: method.column,
        });
    }

    // Only declarations directly in the module statement list are visible to
    // lifted callables before source execution. Recursive declaration sites
    // deliberately do not enter this lexical-name seed.
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::StructDecl { name, .. } | StmtKind::EnumDecl { name, .. } => {
                reg.top_level_types
                    .entry(module_path.to_string())
                    .or_default()
                    .insert(name.clone());
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct RegistryDeclarationCollector {
    struct_names: Vec<String>,
    enum_names: Vec<String>,
    methods: Vec<CollectedMethodSite>,
}

struct CollectedMethodSite {
    type_name: String,
    method_name: String,
    params: Vec<Parameter>,
    body: Vec<Stmt>,
    line: u32,
    column: Option<core::num::NonZeroU32>,
}

impl DeclarationSiteVisitor for RegistryDeclarationCollector {
    fn visit_struct(&mut self, name: &str, _fields: &[String], _stmt: &Stmt) {
        self.struct_names.push(name.to_string());
    }

    fn visit_enum(&mut self, name: &str, _variants: &[VariantDecl], _stmt: &Stmt) {
        self.enum_names.push(name.to_string());
    }

    fn visit_method(
        &mut self,
        type_name: &str,
        method_name: &str,
        params: &[Parameter],
        body: &[Stmt],
        stmt: &Stmt,
    ) {
        self.methods.push(CollectedMethodSite {
            type_name: type_name.to_string(),
            method_name: method_name.to_string(),
            params: params.to_vec(),
            body: body.to_vec(),
            line: stmt.line,
            column: stmt.column,
        });
    }
}

// ─── Module graph ──────────────────────────────────────────────────

/// Transitively-resolved modules, keyed by dot-joined path and
/// ordered so each module comes after its eager top-level imports
/// (topological / leaves-first). Lazy imports in nested runtime
/// bodies are included in the graph without imposing an eager
/// ordering edge. Produced once per transpile and handed to the
/// emitter.
pub(crate) struct ModuleGraph {
    pub order: Vec<String>,
    pub modules: HashMap<String, ModuleEntry>,
}

#[derive(Clone)]
pub(crate) struct ModuleEntry {
    /// Original module source retained for diagnostics emitted after graph
    /// discovery, when only the parsed AST would otherwise remain.
    pub source: String,
    pub ast: Vec<Stmt>,
    pub own_fns: HashMap<String, Vec<Parameter>>,
    /// Every name reachable from this module's final scope —
    /// its own `let`s and `fn`s plus, transitively, its imports'
    /// effective exports. Matches the walker's injection
    /// semantics (`use` re-exports by default).
    pub effective_exports: Vec<String>,
    /// Whether this module declares an explicit `pub { ... }` allow-list.
    /// Legacy modules continue underscore-filtered glob behavior.
    explicit_surface: bool,
    /// Presence-aware function candidates propagated through flat imports.
    /// Unlike value exports, any one of these may be absent at runtime.
    effective_function_exports: BTreeSet<String>,
    /// Every exposed type name and its declaration module. Facades retain the
    /// origin rather than rebinding imported types to their own path.
    pub effective_types: BTreeMap<String, String>,
    /// Names whose final exported representation is a runtime local rather
    /// than an otherwise same-named lifted function wrapper.
    effective_value_exports: BTreeSet<String>,
    /// Exported value bindings that are module namespaces. Descriptors are
    /// static codegen evidence only; generated loaders publish the live Value.
    effective_module_exports: BTreeMap<String, ModuleValueExport>,
    /// Any module-valued binding visible while the module body executes. This
    /// lets lifted callables compile without publishing future runtime state.
    module_alias_candidates: BTreeMap<String, ModuleValueExport>,
    // Kept as analysis metadata for consumers that need the module's
    // syntactic top-level declarations; the emitter currently uses the
    // source-order-aware `effective_value_exports` set instead.
    #[allow(dead_code)]
    pub own_lets: Vec<String>,
    #[allow(dead_code)]
    pub direct_imports: Vec<String>,
}

#[derive(Clone)]
struct ModuleValueExport {
    module_path: String,
    exposed_bindings: Vec<String>,
    exposed_types: BTreeMap<String, String>,
}

#[derive(Clone)]
struct ModuleImport {
    path: String,
    items: Option<Vec<String>>,
    alias: Option<String>,
    line: u32,
}

struct ResolvedModule {
    source: String,
    ast: Vec<Stmt>,
}

fn build_module_graph(root: &[Stmt], opts: &Options) -> Result<ModuleGraph, NyblError> {
    let root_imports = collect_imports_in_stmts(root);
    if root_imports.is_empty() {
        return Ok(ModuleGraph {
            order: Vec::new(),
            modules: HashMap::new(),
        });
    }
    let resolver = match &opts.module_resolver {
        Some(r) => r.clone(),
        None => {
            return Err(NyblError::runtime(
                "nybl-compile: `use` requires `Options::module_resolver` to be set so the transpiler can inline the imported modules",
                root_imports.first().map(|import| import.line).unwrap_or(0),
            ));
        }
    };

    // Discovery and eager-dependency analysis are deliberately
    // separate. A `use` inside a fn/block/lambda must make its
    // module available to codegen, but it neither runs while the
    // containing module loads nor contributes re-exports. Treating
    // that lazy edge like a top-level edge would also report false
    // cycles for harmless patterns such as `fn f() { use self }`.
    let mut resolved: HashMap<String, ResolvedModule> = HashMap::new();
    let mut discovery_order: Vec<(String, u32)> = Vec::new();
    for import in &root_imports {
        resolve_module_tree(
            &import.path,
            import.line,
            &resolver,
            &mut resolved,
            &mut discovery_order,
        )?;
    }

    let mut graph = ModuleGraph {
        order: Vec::new(),
        modules: HashMap::new(),
    };
    let mut visiting: BTreeSet<String> = BTreeSet::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    for (name, line) in discovery_order {
        analyze_module(
            &name,
            line,
            &resolved,
            &mut graph,
            &mut visiting,
            &mut visited,
            opts.sandbox,
        )?;
    }
    Ok(graph)
}

/// Resolve and parse every module reachable through any runtime
/// statement body. The AST is cached before following imports so a
/// lazy self/mutual reference is discovery-idempotent rather than a
/// compile-time cycle. Real load-time cycles are checked later from
/// top-level edges only.
fn resolve_module_tree(
    name: &str,
    line: u32,
    resolver: &crate::ModuleResolver,
    resolved: &mut HashMap<String, ResolvedModule>,
    discovery_order: &mut Vec<(String, u32)>,
) -> Result<(), NyblError> {
    if resolved.contains_key(name) {
        return Ok(());
    }

    let source = {
        let mut r = resolver.borrow_mut();
        match r(name) {
            Some(Ok(s)) => s,
            Some(Err(e)) => return Err(e.with_module(name)),
            None => {
                return Err(NyblError::runtime(
                    format!("Module `{name}` not found"),
                    line,
                ));
            }
        }
    };
    let ast =
        nybl::parse(&source).map_err(|error| error.with_module_source(name, source.as_str()))?;
    let imports = collect_imports_in_stmts(&ast);

    // Insert before recursion: this is both the resolver-work cache
    // and the guard against false cycles through lazy import sites.
    resolved.insert(
        name.to_string(),
        ResolvedModule {
            source: source.clone(),
            ast,
        },
    );
    discovery_order.push((name.to_string(), line));
    for import in &imports {
        resolve_module_tree(
            &import.path,
            import.line,
            resolver,
            resolved,
            discovery_order,
        )
        .map_err(|error| error.with_module_source(name, source.as_str()))?;
    }
    Ok(())
}

/// Compute load order and effective exports from eager (module
/// top-level) imports. Nested imports were resolved in the discovery
/// phase, but are intentionally absent here because their bindings
/// are local to the runtime body that executes them.
fn analyze_module(
    name: &str,
    line: u32,
    resolved: &HashMap<String, ResolvedModule>,
    graph: &mut ModuleGraph,
    visiting: &mut BTreeSet<String>,
    visited: &mut BTreeSet<String>,
    sandbox: bool,
) -> Result<(), NyblError> {
    if visiting.contains(name) {
        return Err(NyblError::runtime(
            format!("Circular import: module `{name}`"),
            line,
        ));
    }
    if visited.contains(name) {
        return Ok(());
    }
    visiting.insert(name.to_string());

    let resolved_module = resolved
        .get(name)
        .expect("discovered module has a parsed AST");
    let ast = resolved_module.ast.clone();
    let source = resolved_module.source.clone();

    // Only top-level imports execute as part of module loading and
    // can contribute bindings/types to this module's exports.
    let direct_imports = collect_top_level_imports(&ast);
    for import in &direct_imports {
        analyze_module(
            &import.path,
            import.line,
            resolved,
            graph,
            visiting,
            visited,
            sandbox,
        )
        .map_err(|error| error.with_module_source(name, source.as_str()))?;
    }

    let own_lets = collect_top_level_lets(&ast);
    let own_fns = collect_top_level_fn_params(&ast);
    let function_candidates: BTreeSet<String> = collect_fn_info(&ast).all_fns.into_iter().collect();
    let own_types = collect_top_level_types(&ast);
    let public_surface = ast.iter().fold(None, |surface, stmt| {
        if let StmtKind::PublicSurface { names } = &stmt.kind {
            let mut merged = surface.unwrap_or_else(BTreeSet::new);
            merged.extend(names.iter().cloned());
            Some(merged)
        } else {
            surface
        }
    });

    // Effective exports mirror the bindings that each `use` shape
    // actually introduces into this module's scope. A plain glob
    // propagates public dependency names, a selective import only
    // propagates the listed names, and an aliased import contributes
    // only its alias value (never the dependency's bare exports).
    // De-dup while preserving import/declaration order.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut exports: Vec<String> = Vec::new();
    let mut effective_function_exports = BTreeSet::new();
    for import in &direct_imports {
        if let Some(alias) = &import.alias {
            push_unique(&mut exports, &mut seen, alias);
            continue;
        }
        let Some(module) = graph.modules.get(&import.path) else {
            continue;
        };
        match &import.items {
            Some(items) => {
                for item in items {
                    if module.effective_exports.iter().any(|name| name == item) {
                        push_unique(&mut exports, &mut seen, item);
                        if module.effective_function_exports.contains(item) {
                            effective_function_exports.insert(item.clone());
                        }
                    }
                }
            }
            None => {
                for name in &module.effective_exports {
                    if module.explicit_surface || !name.starts_with('_') {
                        push_unique(&mut exports, &mut seen, name);
                        if module.effective_function_exports.contains(name) {
                            effective_function_exports.insert(name.clone());
                        }
                    }
                }
            }
        }
    }
    for name in &own_lets {
        push_unique(&mut exports, &mut seen, name);
    }
    if sandbox {
        for name in &function_candidates {
            push_unique(&mut exports, &mut seen, name);
            effective_function_exports.insert(name.clone());
        }
    } else {
        for name in own_fns.keys() {
            push_unique(&mut exports, &mut seen, name);
        }
    }

    // Types follow the same flat glob/selective projection. Aliases
    // are value bindings and therefore never add bare type names.
    // Definitions live in the global AOT registry; this map retains
    // each exposed name's declaration origin for namespace identity.
    let mut type_exports: BTreeMap<String, String> = BTreeMap::new();
    for import in &direct_imports {
        if import.alias.is_some() {
            continue;
        }
        let Some(module) = graph.modules.get(&import.path) else {
            continue;
        };
        match &import.items {
            Some(items) => {
                for item in items {
                    if let Some(origin) = module.effective_types.get(item) {
                        type_exports
                            .entry(item.clone())
                            .or_insert_with(|| origin.clone());
                    }
                }
            }
            None => {
                for (type_name, origin) in &module.effective_types {
                    if module.explicit_surface || !type_name.starts_with('_') {
                        type_exports
                            .entry(type_name.clone())
                            .or_insert_with(|| origin.clone());
                    }
                }
            }
        }
    }
    for ty in &own_types {
        type_exports.insert(ty.clone(), name.to_string());
    }

    let (mut module_value_exports, module_alias_candidates, mut effective_value_exports) =
        effective_module_value_exports(&ast, &graph.modules);
    let explicit_surface = public_surface.is_some();
    if let Some(surface) = &public_surface {
        exports.retain(|name| surface.contains(name));
        effective_function_exports.retain(|name| surface.contains(name));
        type_exports.retain(|name, _| surface.contains(name));
        effective_value_exports.retain(|name| surface.contains(name));
        module_value_exports.retain(|name, _| surface.contains(name));
    }

    graph.modules.insert(
        name.to_string(),
        ModuleEntry {
            source,
            ast,
            own_fns,
            own_lets,
            direct_imports: direct_imports
                .into_iter()
                .map(|import| import.path)
                .collect(),
            effective_exports: exports,
            explicit_surface,
            effective_function_exports,
            effective_types: type_exports,
            effective_value_exports,
            effective_module_exports: module_value_exports,
            module_alias_candidates,
        },
    );
    graph.order.push(name.to_string());
    visiting.remove(name);
    visited.insert(name.to_string());
    Ok(())
}

fn effective_module_value_exports(
    stmts: &[Stmt],
    modules: &HashMap<String, ModuleEntry>,
) -> (
    BTreeMap<String, ModuleValueExport>,
    BTreeMap<String, ModuleValueExport>,
    BTreeSet<String>,
) {
    let mut value_bindings = BTreeSet::new();
    let mut module_exports = BTreeMap::new();
    let mut candidates = BTreeMap::new();
    let mut functions = BTreeSet::new();

    for stmt in stmts {
        match &stmt.kind {
            StmtKind::Use { path, items, alias } => {
                let Some(module) = modules.get(path) else {
                    continue;
                };
                if let Some(alias_name) = alias {
                    if value_bindings.contains(alias_name) || functions.contains(alias_name) {
                        continue;
                    }
                    let exposed_bindings = match items {
                        Some(items) => items
                            .iter()
                            .filter(|name| {
                                module.effective_exports.iter().any(|item| item == *name)
                            })
                            .cloned()
                            .collect(),
                        None => module.effective_exports.clone(),
                    };
                    let exposed_types = match items {
                        Some(items) => items
                            .iter()
                            .filter_map(|name| {
                                module
                                    .effective_types
                                    .get(name)
                                    .map(|origin| (name.clone(), origin.clone()))
                            })
                            .collect(),
                        None => module.effective_types.clone(),
                    };
                    value_bindings.insert(alias_name.clone());
                    let descriptor = ModuleValueExport {
                        module_path: path.clone(),
                        exposed_bindings,
                        exposed_types,
                    };
                    module_exports.insert(alias_name.clone(), descriptor.clone());
                    candidates.insert(alias_name.clone(), descriptor);
                    continue;
                }

                let exposed_names: Vec<&String> = match items {
                    Some(items) => items
                        .iter()
                        .filter(|name| module.effective_exports.iter().any(|item| item == *name))
                        .collect(),
                    None => module
                        .effective_exports
                        .iter()
                        .filter(|name| module.explicit_surface || !name.starts_with('_'))
                        .collect(),
                };
                for name in exposed_names {
                    if value_bindings.contains(name) || functions.contains(name) {
                        continue;
                    }
                    if module.effective_function_exports.contains(name)
                        && !module.effective_value_exports.contains(name)
                    {
                        functions.insert(name.clone());
                        continue;
                    }
                    value_bindings.insert(name.clone());
                    if let Some(module_export) = module.effective_module_exports.get(name) {
                        module_exports.insert(name.clone(), module_export.clone());
                        candidates.insert(name.clone(), module_export.clone());
                    }
                }
            }
            StmtKind::Let { name, value, .. } => {
                value_bindings.insert(name.clone());
                if let ExprKind::Ident(source) = &value.kind {
                    if let Some(module_export) = module_exports.get(source).cloned() {
                        module_exports.insert(name.clone(), module_export.clone());
                        candidates.insert(name.clone(), module_export);
                    } else {
                        module_exports.remove(name);
                    }
                } else {
                    module_exports.remove(name);
                }
            }
            StmtKind::FnDecl { name, .. } => {
                functions.insert(name.clone());
            }
            StmtKind::Assign {
                target: AssignTarget::Variable(name),
                value,
                ..
            } => {
                if let ExprKind::Ident(source) = &value.kind {
                    if let Some(module_export) = module_exports.get(source).cloned() {
                        module_exports.insert(name.clone(), module_export.clone());
                        candidates.insert(name.clone(), module_export);
                    } else {
                        module_exports.remove(name);
                    }
                } else {
                    module_exports.remove(name);
                }
            }
            _ => {}
        }
    }

    (module_exports, candidates, value_bindings)
}

/// Names in a module's top-level statement list that can ever exist as
/// a rebindable `(module, name)` runtime binding. Only module-top-scope
/// `let`s and imports create those entries (fn declarations only claim
/// their name), and a glob import's surface is statically known from
/// the resolved module graph. A named call outside this set can never
/// resolve through `__nybl_binding_value`, so its dynamic pre-dispatch
/// probe can be elided. Returns `None` when an import's surface can't
/// be resolved — callers must then keep the probe on every call.
fn rebindable_binding_surface(
    stmts: &[Stmt],
    modules: &HashMap<String, ModuleEntry>,
) -> Option<HashSet<String>> {
    let mut names = HashSet::new();
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::Let { name, .. } => {
                names.insert(name.clone());
            }
            StmtKind::Use { path, items, alias } => {
                if let Some(alias_name) = alias {
                    names.insert(alias_name.clone());
                    continue;
                }
                if let Some(items) = items {
                    names.extend(items.iter().cloned());
                    continue;
                }
                let entry = modules.get(path)?;
                names.extend(entry.effective_exports.iter().cloned());
            }
            _ => {}
        }
    }
    Some(names)
}

fn module_imports_from_exports(exports: &BTreeMap<String, ModuleValueExport>) -> Vec<ModuleImport> {
    exports
        .iter()
        .map(|(alias, export)| {
            let mut items = export.exposed_bindings.clone();
            for type_name in export.exposed_types.keys() {
                if !items.contains(type_name) {
                    items.push(type_name.clone());
                }
            }
            ModuleImport {
                path: export.module_path.clone(),
                items: Some(items),
                alias: Some(alias.clone()),
                line: 0,
            }
        })
        .collect()
}

fn push_unique(names: &mut Vec<String>, seen: &mut BTreeSet<String>, name: &str) {
    if seen.insert(name.to_string()) {
        names.push(name.to_string());
    }
}

fn collect_imports_in_stmts(stmts: &[Stmt]) -> Vec<ModuleImport> {
    let mut out = Vec::new();
    for stmt in stmts {
        collect_imports_in_stmt(stmt, &mut out);
    }
    out
}

fn collect_top_level_imports(stmts: &[Stmt]) -> Vec<ModuleImport> {
    stmts.iter().filter_map(module_import_from_stmt).collect()
}

fn module_import_from_stmt(stmt: &Stmt) -> Option<ModuleImport> {
    let StmtKind::Use { path, items, alias } = &stmt.kind else {
        return None;
    };
    Some(ModuleImport {
        path: path.clone(),
        items: items.clone(),
        alias: alias.clone(),
        line: stmt.line,
    })
}

fn collect_imports_in_stmt(stmt: &Stmt, out: &mut Vec<ModuleImport>) {
    match &stmt.kind {
        StmtKind::Use { .. } => {
            out.push(module_import_from_stmt(stmt).expect("matched use statement"));
        }
        StmtKind::Let { value, .. } => collect_imports_in_expr(value, out),
        StmtKind::Assign { target, value, .. } => {
            collect_imports_in_assign_target(target, out);
            collect_imports_in_expr(value, out);
        }
        StmtKind::If {
            condition,
            body,
            else_ifs,
            else_body,
        } => {
            collect_imports_in_expr(condition, out);
            collect_imports_in_stmts_into(body, out);
            for (condition, body) in else_ifs {
                collect_imports_in_expr(condition, out);
                collect_imports_in_stmts_into(body, out);
            }
            if let Some(body) = else_body {
                collect_imports_in_stmts_into(body, out);
            }
        }
        StmtKind::While { condition, body } => {
            collect_imports_in_expr(condition, out);
            collect_imports_in_stmts_into(body, out);
        }
        StmtKind::Repeat { count, body } => {
            collect_imports_in_expr(count, out);
            collect_imports_in_stmts_into(body, out);
        }
        StmtKind::ForIn { iterable, body, .. } => {
            collect_imports_in_expr(iterable, out);
            collect_imports_in_stmts_into(body, out);
        }
        StmtKind::FnDecl { body, .. } | StmtKind::MethodDecl { body, .. } => {
            collect_imports_in_stmts_into(body, out);
        }
        StmtKind::Return { value } => {
            if let Some(value) = value {
                collect_imports_in_expr(value, out);
            }
        }
        StmtKind::ExprStmt(expr) => collect_imports_in_expr(expr, out),
        StmtKind::Break
        | StmtKind::Continue
        | StmtKind::PublicSurface { .. }
        | StmtKind::StructDecl { .. }
        | StmtKind::EnumDecl { .. } => {}
    }
}

fn collect_imports_in_stmts_into(stmts: &[Stmt], out: &mut Vec<ModuleImport>) {
    for stmt in stmts {
        collect_imports_in_stmt(stmt, out);
    }
}

fn collect_imports_in_assign_target(target: &AssignTarget, out: &mut Vec<ModuleImport>) {
    match target {
        AssignTarget::Variable(_) => {}
        AssignTarget::Index { object, index } => {
            collect_imports_in_expr(object, out);
            collect_imports_in_expr(index, out);
        }
        AssignTarget::Field { object, .. } => collect_imports_in_expr(object, out),
    }
}

fn collect_imports_in_expr(expr: &Expr, out: &mut Vec<ModuleImport>) {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Number(_)
        | ExprKind::Str(_)
        | ExprKind::StringInterp(_)
        | ExprKind::Bool(_)
        | ExprKind::None
        | ExprKind::Ident(_) => {}
        ExprKind::BinaryOp { left, right, .. } => {
            collect_imports_in_expr(left, out);
            collect_imports_in_expr(right, out);
        }
        ExprKind::UnaryOp { expr, .. } | ExprKind::Try(expr) => {
            collect_imports_in_expr(expr, out);
        }
        ExprKind::Call { callee, args } => {
            collect_imports_in_expr(callee, out);
            for arg in args {
                collect_imports_in_expr(arg, out);
            }
        }
        ExprKind::MethodCall { object, args, .. } => {
            collect_imports_in_expr(object, out);
            for arg in args {
                collect_imports_in_expr(arg, out);
            }
        }
        ExprKind::FieldAccess { object, .. } => collect_imports_in_expr(object, out),
        ExprKind::StructConstruct { fields, .. } => {
            for (_, value) in fields {
                collect_imports_in_expr(value, out);
            }
        }
        ExprKind::EnumConstruct { payload, .. } => match payload {
            VariantPayload::Unit => {}
            VariantPayload::Tuple(values) => {
                for value in values {
                    collect_imports_in_expr(value, out);
                }
            }
            VariantPayload::Struct(fields) => {
                for (_, value) in fields {
                    collect_imports_in_expr(value, out);
                }
            }
        },
        ExprKind::Index { object, index } => {
            collect_imports_in_expr(object, out);
            collect_imports_in_expr(index, out);
        }
        ExprKind::Array(values) => {
            for value in values {
                collect_imports_in_expr(value, out);
            }
        }
        ExprKind::Dict(entries) => {
            for (_, value) in entries {
                collect_imports_in_expr(value, out);
            }
        }
        ExprKind::IfExpr {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_imports_in_expr(condition, out);
            collect_imports_in_expr(then_expr, out);
            collect_imports_in_expr(else_expr, out);
        }
        ExprKind::Lambda { body, .. } => collect_imports_in_stmts_into(body, out),
        ExprKind::Match { scrutinee, arms } => {
            collect_imports_in_expr(scrutinee, out);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_imports_in_expr(guard, out);
                }
                collect_imports_in_expr(&arm.body, out);
            }
        }
    }
}

fn collect_top_level_lets(stmts: &[Stmt]) -> Vec<String> {
    stmts
        .iter()
        .filter_map(|s| match &s.kind {
            StmtKind::Let { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// Gather the names of struct / enum types declared at the top
/// level of `stmts`. Used to seed a module's `effective_types`
/// map so aliased `use foo as m` can populate its origin-aware
/// runtime namespace surface for `m.Entity { ... }` etc.
fn collect_top_level_types(stmts: &[Stmt]) -> Vec<String> {
    stmts
        .iter()
        .filter_map(|s| match &s.kind {
            StmtKind::StructDecl { name, .. } => Some(name.clone()),
            StmtKind::EnumDecl { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

fn collect_top_level_fn_params(stmts: &[Stmt]) -> HashMap<String, Vec<Parameter>> {
    let mut out = HashMap::new();
    for stmt in stmts {
        if let StmtKind::FnDecl { name, params, .. } = &stmt.kind {
            out.insert(name.clone(), params.clone());
        }
    }
    out
}

/// Result of the pre-pass over the AST. Function names, declaration counts,
/// and ref-mode presence are tracked independently: runtime-active site
/// metadata remains authoritative when same-name declarations differ.
#[derive(Clone, Default)]
struct FnInfo {
    all_fns: HashSet<String>,
    top_level_fns: HashSet<String>,
    site_counts: HashMap<String, usize>,
    names_with_ref_sites: HashSet<String>,
    names_with_value_only_sites: HashSet<String>,
}

impl FnInfo {
    fn record_site(&mut self, name: &str, params: &[Parameter]) {
        self.all_fns.insert(name.to_string());
        *self.site_counts.entry(name.to_string()).or_default() += 1;
        if params.iter().any(|param| param.mode == ParamMode::Ref) {
            self.names_with_ref_sites.insert(name.to_string());
        } else {
            self.names_with_value_only_sites.insert(name.to_string());
        }
    }
}

fn collect_fn_info(stmts: &[Stmt]) -> FnInfo {
    let mut info = FnInfo::default();
    for stmt in stmts {
        if let StmtKind::FnDecl {
            name, params, body, ..
        } = &stmt.kind
        {
            info.record_site(name, params);
            info.top_level_fns.insert(name.clone());
            collect_nested_fns(body, &mut info);
        } else {
            collect_nested_fns_in_stmt(stmt, &mut info);
        }
    }
    info
}

fn collect_nested_fns(stmts: &[Stmt], info: &mut FnInfo) {
    for stmt in stmts {
        collect_nested_fns_in_stmt(stmt, info);
    }
}

fn collect_nested_fns_in_stmt(stmt: &Stmt, info: &mut FnInfo) {
    match &stmt.kind {
        StmtKind::Let { value, .. } => collect_nested_fns_in_expr(value, info),
        StmtKind::Assign { target, value, .. } => {
            collect_nested_fns_in_assign_target(target, info);
            collect_nested_fns_in_expr(value, info);
        }
        StmtKind::FnDecl {
            name, params, body, ..
        } => {
            info.record_site(name, params);
            collect_nested_fns(body, info);
        }
        StmtKind::If {
            condition,
            body,
            else_ifs,
            else_body,
        } => {
            collect_nested_fns_in_expr(condition, info);
            collect_nested_fns(body, info);
            for (condition, b) in else_ifs {
                collect_nested_fns_in_expr(condition, info);
                collect_nested_fns(b, info);
            }
            if let Some(b) = else_body {
                collect_nested_fns(b, info);
            }
        }
        StmtKind::While { condition, body } => {
            collect_nested_fns_in_expr(condition, info);
            collect_nested_fns(body, info);
        }
        StmtKind::Repeat { count, body } => {
            collect_nested_fns_in_expr(count, info);
            collect_nested_fns(body, info);
        }
        StmtKind::ForIn { iterable, body, .. } => {
            collect_nested_fns_in_expr(iterable, info);
            collect_nested_fns(body, info);
        }
        StmtKind::MethodDecl { body, .. } => collect_nested_fns(body, info),
        StmtKind::Return { value: Some(value) } | StmtKind::ExprStmt(value) => {
            collect_nested_fns_in_expr(value, info);
        }
        _ => {}
    }
}

fn collect_nested_fns_in_assign_target(target: &AssignTarget, info: &mut FnInfo) {
    match target {
        AssignTarget::Variable(_) => {}
        AssignTarget::Index { object, index } => {
            collect_nested_fns_in_expr(object, info);
            collect_nested_fns_in_expr(index, info);
        }
        AssignTarget::Field { object, .. } => collect_nested_fns_in_expr(object, info),
    }
}

fn collect_nested_fns_in_expr(expr: &Expr, info: &mut FnInfo) {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Number(_)
        | ExprKind::Str(_)
        | ExprKind::StringInterp(_)
        | ExprKind::Bool(_)
        | ExprKind::None
        | ExprKind::Ident(_) => {}
        ExprKind::BinaryOp { left, right, .. }
        | ExprKind::Index {
            object: left,
            index: right,
        } => {
            collect_nested_fns_in_expr(left, info);
            collect_nested_fns_in_expr(right, info);
        }
        ExprKind::UnaryOp { expr, .. }
        | ExprKind::Try(expr)
        | ExprKind::FieldAccess { object: expr, .. } => {
            collect_nested_fns_in_expr(expr, info);
        }
        ExprKind::Call { callee, args } => {
            collect_nested_fns_in_expr(callee, info);
            for arg in args {
                collect_nested_fns_in_expr(arg, info);
            }
        }
        ExprKind::MethodCall { object, args, .. } => {
            collect_nested_fns_in_expr(object, info);
            for arg in args {
                collect_nested_fns_in_expr(arg, info);
            }
        }
        ExprKind::StructConstruct { fields, .. } => {
            for (_, value) in fields {
                collect_nested_fns_in_expr(value, info);
            }
        }
        ExprKind::EnumConstruct { payload, .. } => match payload {
            VariantPayload::Unit => {}
            VariantPayload::Tuple(values) => {
                for value in values {
                    collect_nested_fns_in_expr(value, info);
                }
            }
            VariantPayload::Struct(fields) => {
                for (_, value) in fields {
                    collect_nested_fns_in_expr(value, info);
                }
            }
        },
        ExprKind::Array(values) => {
            for value in values {
                collect_nested_fns_in_expr(value, info);
            }
        }
        ExprKind::Dict(entries) => {
            for (_, value) in entries {
                collect_nested_fns_in_expr(value, info);
            }
        }
        ExprKind::IfExpr {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_nested_fns_in_expr(condition, info);
            collect_nested_fns_in_expr(then_expr, info);
            collect_nested_fns_in_expr(else_expr, info);
        }
        ExprKind::Lambda { body, .. } => collect_nested_fns(body, info),
        ExprKind::Match { scrutinee, arms } => {
            collect_nested_fns_in_expr(scrutinee, info);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_nested_fns_in_expr(guard, info);
                }
                collect_nested_fns_in_expr(&arm.body, info);
            }
        }
    }
}

fn callable_assignment_names(stmts: &[Stmt]) -> HashSet<String> {
    fn visit_expr(expr: &Expr, names: &mut HashSet<String>) {
        match &expr.kind {
            ExprKind::BinaryOp { left, right, .. }
            | ExprKind::Index {
                object: left,
                index: right,
            } => {
                visit_expr(left, names);
                visit_expr(right, names);
            }
            ExprKind::UnaryOp { expr, .. }
            | ExprKind::Try(expr)
            | ExprKind::FieldAccess { object: expr, .. } => visit_expr(expr, names),
            ExprKind::Call { callee, args } => {
                visit_expr(callee, names);
                for arg in args {
                    if arg.mode == ParamMode::Ref
                        && let ExprKind::Ident(name) = &arg.value.kind
                    {
                        names.insert(name.clone());
                    }
                    visit_expr(&arg.value, names);
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                visit_expr(object, names);
                for arg in args {
                    if arg.mode == ParamMode::Ref
                        && let ExprKind::Ident(name) = &arg.value.kind
                    {
                        names.insert(name.clone());
                    }
                    visit_expr(&arg.value, names);
                }
            }
            ExprKind::StructConstruct { fields, .. } | ExprKind::Dict(fields) => {
                for (_, value) in fields {
                    visit_expr(value, names);
                }
            }
            ExprKind::EnumConstruct { payload, .. } => match payload {
                VariantPayload::Unit => {}
                VariantPayload::Tuple(values) => {
                    for value in values {
                        visit_expr(value, names);
                    }
                }
                VariantPayload::Struct(fields) => {
                    for (_, value) in fields {
                        visit_expr(value, names);
                    }
                }
            },
            ExprKind::Array(values) => {
                for value in values {
                    visit_expr(value, names);
                }
            }
            ExprKind::IfExpr {
                condition,
                then_expr,
                else_expr,
            } => {
                visit_expr(condition, names);
                visit_expr(then_expr, names);
                visit_expr(else_expr, names);
            }
            ExprKind::Match { scrutinee, arms } => {
                visit_expr(scrutinee, names);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        visit_expr(guard, names);
                    }
                    visit_expr(&arm.body, names);
                }
            }
            // Nested lambdas are separate callable mutation domains.
            ExprKind::Lambda { .. }
            | ExprKind::Int(_)
            | ExprKind::Number(_)
            | ExprKind::Str(_)
            | ExprKind::StringInterp(_)
            | ExprKind::Bool(_)
            | ExprKind::None
            | ExprKind::Ident(_) => {}
        }
    }

    fn visit(stmts: &[Stmt], names: &mut HashSet<String>) {
        for stmt in stmts {
            match &stmt.kind {
                StmtKind::Let { value, .. } => visit_expr(value, names),
                StmtKind::Assign {
                    target: AssignTarget::Variable(name),
                    value,
                    ..
                } => {
                    names.insert(name.clone());
                    visit_expr(value, names);
                }
                StmtKind::Assign { target, value, .. } => {
                    match target {
                        AssignTarget::Index { object, index } => {
                            visit_expr(object, names);
                            visit_expr(index, names);
                        }
                        AssignTarget::Field { object, .. } => visit_expr(object, names),
                        AssignTarget::Variable(_) => unreachable!(),
                    }
                    visit_expr(value, names);
                }
                StmtKind::If {
                    condition,
                    body,
                    else_ifs,
                    else_body,
                } => {
                    visit_expr(condition, names);
                    visit(body, names);
                    for (condition, body) in else_ifs {
                        visit_expr(condition, names);
                        visit(body, names);
                    }
                    if let Some(body) = else_body {
                        visit(body, names);
                    }
                }
                StmtKind::While { condition, body } => {
                    visit_expr(condition, names);
                    visit(body, names);
                }
                StmtKind::Repeat { count, body } => {
                    visit_expr(count, names);
                    visit(body, names);
                }
                StmtKind::ForIn { iterable, body, .. } => {
                    visit_expr(iterable, names);
                    visit(body, names);
                }
                StmtKind::Return { value: Some(value) } | StmtKind::ExprStmt(value) => {
                    visit_expr(value, names);
                }
                // Nested named functions and lambdas are separate callable
                // mutation domains and are analysed when emitted.
                _ => {}
            }
        }
    }
    let mut names = HashSet::new();
    visit(stmts, &mut names);
    names
}

// ─── Emitter ───────────────────────────────────────────────────────

#[derive(Default)]
struct EmissionScope {
    locals: HashSet<String>,
    persistent_bindings: HashSet<String>,
    constants: HashSet<String>,
    captured_locals: HashSet<String>,
    ref_parameters: HashSet<String>,
    optional_imports: HashMap<String, OptionalImportBinding>,
    module_alias_locals: HashSet<String>,
    mutated_locals: HashSet<String>,
    plain_glob_imports: HashSet<String>,
    function_sites: HashMap<String, usize>,
    claimed_functions: HashSet<String>,
}

#[derive(Clone)]
enum OptionalImportBinding {
    Local {
        value: String,
        function_site: String,
    },
    Persistent,
}

#[derive(Clone)]
enum BindingStorage {
    RustLocal {
        ident: String,
    },
    Persistent {
        module: String,
        name: String,
    },
    OptionalImport {
        bindings: Vec<OptionalImportBinding>,
        fallback: Option<Box<BindingStorage>>,
        module: String,
        name: String,
    },
}

enum RefTargetCandidate {
    DirectLocal(String),
    OptionalLocal(String),
    Persistent { module: String, name: String },
}

#[derive(Clone)]
enum AotPlaceProjection {
    Field(String),
    Index(Expr),
}

#[derive(Clone)]
struct AotPlace {
    root: String,
    projections: Vec<AotPlaceProjection>,
}

#[derive(Clone)]
struct EmittedPlaceTarget {
    place: AotPlace,
    root_read: String,
    root_write: String,
}

fn aot_place(expr: &Expr) -> Option<AotPlace> {
    fn walk(expr: &Expr, projections: &mut Vec<AotPlaceProjection>) -> Option<String> {
        match &expr.kind {
            ExprKind::Ident(name) => Some(name.clone()),
            ExprKind::FieldAccess { object, field } => {
                let root = walk(object, projections)?;
                projections.push(AotPlaceProjection::Field(field.clone()));
                Some(root)
            }
            ExprKind::Index { object, index } => {
                let root = walk(object, projections)?;
                projections.push(AotPlaceProjection::Index((**index).clone()));
                Some(root)
            }
            _ => None,
        }
    }

    let mut projections = Vec::new();
    let root = walk(expr, &mut projections)?;
    Some(AotPlace { root, projections })
}

#[derive(Clone)]
struct ModuleAliasBinding {
    exposed_types: BTreeMap<String, String>,
}

struct Emitter {
    out: String,
    indent: usize,
    opts: Options,
    fn_info: FnInfo,
    modules: ModuleGraph,
    types: TypeRegistry,
    /// Unique top-level declaration sites used by sandboxed persistent-state
    /// lowering. Kept separate from name-keyed call-resolution metadata.
    functions: FunctionRegistry,
    /// Declaration occurrence pointer -> catalogue site. Registration and
    /// emission walk the same stable parsed statement slices, so identical
    /// same-line declarations remain distinguishable without a source key.
    function_sites_by_stmt: HashMap<usize, usize>,
    /// Direct module/root declaration sites in source order. Runtime module
    /// bodies replay this exact sequence to activate declarations as reached.
    direct_function_sites: HashMap<String, Vec<usize>>,
    /// Counter for temporary locals (`__t0`, `__t1`, …). Reset at
    /// the start of each fn / top-level program so the names stay
    /// short.
    tmp_counter: usize,
    /// Unique suffix for block-local static type descriptors. Declaration
    /// sites may execute repeatedly, but their shape lives in read-only data.
    type_site_counter: usize,
    /// Non-empty while emitting an imported module's body — the
    /// collision-free prefix is applied to every user fn name so
    /// modules can't collide on function identifiers.
    module_prefix: String,
    /// Source-level path of the module whose body we're currently
    /// emitting. `<root>` while emitting the top-level program,
    /// the dot-joined `use` path while inside `emit_one_module`.
    /// Mirrors the walker / VM `current_module`: newly declared
    /// types tag their runtime values with this string, so two
    /// modules declaring the same name produce distinct
    /// identities.
    current_module: String,
    /// Per-scope map of bare type names → module path. Populated
    /// at emit time whenever a type is declared (→ current
    /// module) or brought into scope by a `use` statement
    /// (→ the imported module's path). Bare-name construction
    /// (`Color::Red`) and pattern matching resolve through this
    /// stack to produce the correct module path literal in the
    /// emitted Rust.
    type_bindings: Vec<HashMap<String, String>>,
    /// Per-scope maps of module aliases. Each binding retains both
    /// the module path and the type surface selected by the `use`
    /// statement, so `use shapes.{Circle} as s` cannot resolve
    /// `s.Square` during construction or pattern matching. Kept in
    /// lockstep with `type_bindings` so aliases introduced in a fn,
    /// lambda, or block cannot leak into sibling/enclosing scopes.
    module_aliases: Vec<HashMap<String, ModuleAliasBinding>>,
    /// Stack of generated Rust scopes. Each frame owns both its
    /// `let`-bound Nybl names and the plain-glob imports that emitted
    /// those bindings. Keeping them together prevents an import in
    /// one module, function, lambda, or block from suppressing the
    /// declarations needed by another. Used for:
    ///
    /// - Ident resolution (local vs top-level fn vs error).
    /// - Free-variable analysis for lambda capture.
    ///
    /// Each block (if / while / repeat / for / fn / lambda) pushes
    /// a fresh set on entry and pops on exit. User-fn and
    /// lambda parameters are inserted into the freshly-pushed set.
    scope_stack: Vec<EmissionScope>,
    /// True while emitting the body of `run_program` (the Rust fn
    /// that returns `Result<(), NyblError>`). User fns and lambdas
    /// toggle this off for the duration of their body — inside
    /// them, the enclosing Rust fn returns `Result<Value,
    /// NyblError>`, so `return Ok(value)` is the Nybl-level return
    /// path. Read by `try`'s codegen to decide whether an `Err`
    /// arm should propagate via `return Ok(...)` (fn body) or
    /// raise a real error (top-level program).
    in_top_level: bool,
    /// True while emitting a named function, method, or lambda body. Combined
    /// with `scope_stack` depth to distinguish a module's direct use-site from
    /// a nested lexical import when applying named-function first-win rules.
    in_callable_body: bool,
    /// Method definitions retain declaration module context but never capture
    /// surrounding value bindings. Unknown identifiers must therefore compile
    /// to the same runtime lookup error as walker/VM, not an undefined Rust
    /// local. Nested lambdas still capture method params through `is_local`.
    in_non_capturing_method: bool,
    /// Aliased top-level imports owned by the module whose lifted callables are
    /// currently being emitted. Their runtime values are resolved lazily from
    /// `Ctx::module_aliases` because lifted Rust functions cannot capture the
    /// loader's source-ordered locals.
    declaration_aliases: Vec<ModuleImport>,
    /// Per-callable declaration-alias binding names. Each required alias gets
    /// separate call-local overlay and read-cache slots. Reads consult an
    /// assigned overlay first, then the cache and live declaration context.
    /// Named callables additionally write through to their declaration scope;
    /// lambdas keep ordinary value-capture semantics.
    declaration_alias_overlays: Vec<HashSet<String>>,
    /// Whether the current declaration-alias overlay belongs to a lifted
    /// named callable. Named functions and methods execute against their
    /// declaration scope, so writes must update the authoritative persistent
    /// binding as well as the call-local snapshot used by descendant
    /// closures. Lambda overlays remain value captures and never write
    /// through to the declaration scope.
    declaration_alias_write_through: Vec<bool>,
    /// Whole-callable assignment pre-analysis. Namespace aliases assigned on
    /// any control-flow path use runtime type identity even at construction
    /// sites that appear textually before the assignment (for example, on a
    /// later loop iteration).
    callable_mutations: Vec<HashSet<String>>,
    /// Per-module set of names that can ever appear as a rebindable
    /// `(module, name)` binding at runtime — top-level `let`s plus every
    /// import surface (glob surfaces come from the resolved graph). Named
    /// calls outside this set skip the dynamic `__nybl_binding_value`
    /// pre-dispatch probe entirely. `None` marks a module whose import
    /// surface couldn't be resolved: probe every call there.
    rebindable_call_surfaces: HashMap<String, Option<HashSet<String>>>,
    /// Root copy restored after temporarily emitting imported modules/methods.
    root_declaration_aliases: Vec<ModuleImport>,
    persistent_constants: HashMap<String, HashSet<String>>,
}

impl Emitter {
    fn new(
        opts: Options,
        fn_info: FnInfo,
        modules: ModuleGraph,
        types: TypeRegistry,
        persistent_constants: HashMap<String, HashSet<String>>,
    ) -> Self {
        // Seed the outermost type_bindings frame with the
        // engine-wide builtins so bare `Result::Ok(...)` and
        // `RuntimeError { ... }` resolve to `<builtin>` without
        // an explicit `use`. Same invariant the walker / VM
        // set up at construction time.
        let mut builtin_frame: HashMap<String, String> = HashMap::new();
        builtin_frame.insert(
            String::from("Result"),
            String::from(nybl::value::BUILTIN_MODULE_PATH),
        );
        builtin_frame.insert(
            String::from("RuntimeError"),
            String::from(nybl::value::BUILTIN_MODULE_PATH),
        );
        builtin_frame.insert(
            String::from("Iter"),
            String::from(nybl::value::BUILTIN_MODULE_PATH),
        );
        Self {
            out: String::new(),
            indent: 0,
            opts,
            fn_info,
            modules,
            types,
            functions: FunctionRegistry::default(),
            function_sites_by_stmt: HashMap::new(),
            direct_function_sites: HashMap::new(),
            tmp_counter: 0,
            type_site_counter: 0,
            module_prefix: String::new(),
            current_module: String::from(nybl::value::ROOT_MODULE_PATH),
            type_bindings: vec![builtin_frame],
            module_aliases: vec![HashMap::new()],
            scope_stack: Vec::new(),
            in_top_level: false,
            in_callable_body: false,
            in_non_capturing_method: false,
            declaration_aliases: Vec::new(),
            declaration_alias_overlays: Vec::new(),
            declaration_alias_write_through: Vec::new(),
            callable_mutations: Vec::new(),
            rebindable_call_surfaces: HashMap::new(),
            root_declaration_aliases: Vec::new(),
            persistent_constants,
        }
    }

    fn push_scope(&mut self) {
        self.scope_stack.push(EmissionScope::default());
        self.type_bindings.push(HashMap::new());
        self.module_aliases.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scope_stack.pop();
        if self.type_bindings.len() > 1 {
            self.type_bindings.pop();
        }
        if self.module_aliases.len() > 1 {
            self.module_aliases.pop();
        }
    }

    /// Resolve a source-level type reference to its declaring
    /// module path, using the emitter's per-scope type_bindings
    /// and module_aliases state. Returns `None` if the name isn't
    /// in scope — callers treat that as a type-not-declared
    /// error at emit time.
    fn resolve_type_module(&self, namespace: Option<&str>, type_name: &str) -> Option<String> {
        if let Some(ns) = namespace {
            for frame in self.module_aliases.iter().rev() {
                if let Some(binding) = frame.get(ns) {
                    if let Some(origin) = binding.exposed_types.get(type_name)
                        && self.types.has_type(origin, type_name)
                    {
                        return Some(origin.clone());
                    }
                    return None;
                }
            }
            return None;
        }
        for frame in self.type_bindings.iter().rev() {
            if let Some(mp) = frame.get(type_name) {
                return Some(mp.clone());
            }
        }
        None
    }

    /// Bind a bare type name in the current scope to the
    /// module that declared it. Called from StructDecl /
    /// EnumDecl emission and from the type-import arm of `use`
    /// statement handling.
    fn bind_type(&mut self, name: &str, module_path: &str) {
        if let Some(frame) = self.type_bindings.last_mut() {
            frame.insert(name.to_string(), module_path.to_string());
        }
    }

    /// Imported types are first-win within one lexical frame. A
    /// new inner frame remains free to shadow an outer binding.
    fn bind_imported_type(&mut self, name: &str, module_path: &str) {
        if let Some(frame) = self.type_bindings.last_mut() {
            frame
                .entry(name.to_string())
                .or_insert_with(|| module_path.to_string());
        }
    }

    fn bind_module_alias(&mut self, alias: &str, exposed_types: &[(String, String)]) {
        if let Some(frame) = self.module_aliases.last_mut() {
            frame.insert(
                alias.to_string(),
                ModuleAliasBinding {
                    exposed_types: exposed_types.iter().cloned().collect(),
                },
            );
        }
    }

    fn bind_module_export(&mut self, name: &str, export: &ModuleValueExport) {
        if let Some(frame) = self.module_aliases.last_mut() {
            frame.insert(
                name.to_string(),
                ModuleAliasBinding {
                    exposed_types: export.exposed_types.clone(),
                },
            );
        }
    }

    fn has_module_alias_candidate(&self, name: &str) -> bool {
        self.module_aliases
            .iter()
            .rev()
            .any(|aliases| aliases.contains_key(name))
    }

    /// Emit Rust source for a `__resolver` closure that turns
    /// `(namespace, type_name)` pairs into the declaring
    /// module's path. Bare names consult the generated runtime
    /// binding stack so lifted bodies observe declarations and
    /// imports only after execution reaches them. Namespaces
    /// continue to resolve through live module values.
    fn emit_resolver_closure_src(
        &self,
        pattern_namespaces: &[String],
        resolve_bare: bool,
    ) -> String {
        // Module aliases resolve from runtime Values. A local/parameter binding
        // hard-shadows declaration context; otherwise a lifted callable checks
        // the defining module's source-ordered alias map.
        let mut alias_arms = String::new();
        let mut alias_prelude = String::new();
        let mut aliases: Vec<(&String, &ModuleAliasBinding)> = Vec::new();
        let mut seen_aliases: HashSet<String> = HashSet::new();
        for frame in self.module_aliases.iter().rev() {
            let mut frame_aliases: Vec<(&String, &ModuleAliasBinding)> = frame.iter().collect();
            frame_aliases.sort_by(|a, b| a.0.cmp(b.0));
            for (alias, binding) in frame_aliases {
                if seen_aliases.insert(alias.clone()) {
                    aliases.push((alias, binding));
                }
            }
        }
        aliases.sort_by(|a, b| a.0.cmp(b.0));
        for (alias, _) in aliases {
            let optional_storage = self
                .binding_storage(alias)
                .and_then(|storage| self.storage_optional_value_src(&storage, 0));
            let value = if let Some(optional) = optional_storage {
                let snapshot = declaration_alias_pattern_snapshot_ident(alias);
                alias_prelude.push_str(&format!("            let {snapshot} = {optional};\n",));
                format!("{snapshot}.as_ref()")
            } else if self.is_local(alias) {
                format!("::std::option::Option::Some(&{})", rust_user_ident(alias))
            } else if self.opts.sandbox {
                let snapshot = declaration_alias_pattern_snapshot_ident(alias);
                alias_prelude.push_str(&format!(
                    "            let {snapshot} = __nybl_binding_value(ctx, {module}, {alias});\n",
                    module = rust_string_literal(&self.current_module),
                    alias = rust_string_literal(alias),
                ));
                format!("{snapshot}.as_ref()",)
            } else if let Some(overlay) = self.declaration_alias_overlay(alias) {
                let snapshot = declaration_alias_pattern_snapshot_ident(alias);
                alias_prelude.push_str(&format!(
                    "            let {snapshot} = __nybl_declaration_alias_optional(ctx, &mut {overlay}, {module}, {alias});\n",
                    module = rust_string_literal(&self.current_module),
                    alias = rust_string_literal(alias),
                ));
                format!("{snapshot}.as_ref()",)
            } else if self
                .declaration_aliases
                .iter()
                .any(|import| import.alias.as_deref() == Some(alias.as_str()))
            {
                format!(
                    "ctx.module_aliases.get(&({module}.to_string(), {alias}.to_string()))",
                    module = rust_string_literal(&self.current_module),
                    alias = rust_string_literal(alias),
                )
            } else {
                "::std::option::Option::None".to_string()
            };
            alias_arms.push_str(&format!(
                "                        (::std::option::Option::Some({alias}), __tn) => match {value} {{ ::std::option::Option::Some(::nybl::value::Value::Module(__module)) => __module.type_origin(__tn).map(str::to_string), _ => ::std::option::Option::None }},\n",
                alias = rust_string_literal(alias),
                value = value,
            ));
        }
        let mut dynamic_namespaces = pattern_namespaces.to_vec();
        dynamic_namespaces.sort();
        dynamic_namespaces.dedup();
        for namespace in dynamic_namespaces {
            if seen_aliases.contains(&namespace) || !self.is_local(&namespace) {
                continue;
            }
            alias_arms.push_str(&format!(
                "                        (::std::option::Option::Some({namespace}), __tn) => match ::std::option::Option::Some(&{value}) {{ ::std::option::Option::Some(::nybl::value::Value::Module(__module)) => __module.type_origin(__tn).map(str::to_string), _ => ::std::option::Option::None }},\n",
                namespace = rust_string_literal(&namespace),
                value = rust_user_ident(&namespace),
            ));
        }
        let bare_arm = if resolve_bare {
            "                        (::std::option::Option::None, __tn) => __nybl_type_bindings.iter().rev().find_map(|__frame| __frame.get(__tn).cloned()),\n"
        } else {
            ""
        };
        format!(
            "{alias_prelude}            let __resolver = |__ns: ::std::option::Option<&str>, __tn: &str| -> ::std::option::Option<String> {{\n\
             \x20                   match (__ns, __tn) {{\n\
             {alias_arms}{bare_arm}                        _ => ::std::option::Option::None,\n\
             \x20                   }}\n\
             \x20           }};\n",
        )
    }

    fn bind_local(&mut self, name: &str) {
        if let Some(top) = self.scope_stack.last_mut() {
            top.locals.insert(name.to_string());
        }
    }

    fn mark_constant(&mut self, name: &str) {
        if let Some(top) = self.scope_stack.last_mut() {
            top.constants.insert(name.to_string());
        }
    }

    fn mark_captured_local(&mut self, name: &str) {
        if let Some(top) = self.scope_stack.last_mut() {
            top.captured_locals.insert(name.to_string());
        }
    }

    fn mark_ref_parameter(&mut self, name: &str) {
        if let Some(top) = self.scope_stack.last_mut() {
            top.ref_parameters.insert(name.to_string());
        }
    }

    fn binding_has_flag(&self, name: &str, flag: impl Fn(&EmissionScope) -> bool) -> bool {
        for scope in self.scope_stack.iter().rev() {
            if scope.locals.contains(name)
                || scope.persistent_bindings.contains(name)
                || scope.optional_imports.contains_key(name)
            {
                return flag(scope);
            }
        }
        false
    }

    fn is_constant_binding(&self, name: &str) -> bool {
        for scope in self.scope_stack.iter().rev() {
            if scope.locals.contains(name) {
                return scope.constants.contains(name);
            }
            if scope.persistent_bindings.contains(name) {
                return scope.constants.contains(name)
                    || self
                        .persistent_constants
                        .get(&self.current_module)
                        .is_some_and(|constants| constants.contains(name));
            }
        }
        self.persistent_constants
            .get(&self.current_module)
            .is_some_and(|constants| constants.contains(name))
    }

    fn is_captured_binding(&self, name: &str) -> bool {
        for scope in self.scope_stack.iter().rev() {
            if scope.captured_locals.contains(name) {
                return true;
            }
            if scope.locals.contains(name)
                || scope.persistent_bindings.contains(name)
                || scope.optional_imports.contains_key(name)
            {
                return false;
            }
        }
        false
    }

    fn is_ref_parameter(&self, name: &str) -> bool {
        self.binding_has_flag(name, |scope| scope.ref_parameters.contains(name))
    }

    fn bind_persistent(&mut self, name: &str) {
        self.scope_stack
            .last_mut()
            .expect("persistent bindings are emitted inside a scope")
            .persistent_bindings
            .insert(name.to_string());
    }

    fn is_persistent_binding(&self, name: &str) -> bool {
        self.scope_stack
            .iter()
            .rev()
            .any(|scope| scope.persistent_bindings.contains(name))
    }

    fn emit_module_alias_context_sync(&mut self, name: &str) {
        if self.opts.sandbox && !self.is_module_top_scope() {
            // Sandboxed callables resolve declaration aliases from the
            // authoritative state binding. Their Rust locals and
            // presence-aware flat imports are lexical shadows, so none of
            // them may rewrite loader-owned alias metadata.
            return;
        }
        let authoritative_alias = (self.opts.sandbox
            && (self.has_declaration_alias_overlay(name) || self.has_module_alias_candidate(name)))
            || self.declaration_alias_writes_through(name);
        if !self.is_module_top_scope() && !authoritative_alias {
            return;
        }
        if self.opts.sandbox
            && let Some(storage @ BindingStorage::OptionalImport { .. }) =
                self.binding_storage(name)
        {
            let value = self
                .storage_optional_value_src(&storage, 0)
                .expect("optional import storage");
            let value_tmp = self.fresh_tmp();
            self.line(&format!("let {value_tmp} = {value};"));
            self.line(&format!(
                    "match {value_tmp} {{ ::std::option::Option::Some(__value @ ::nybl::value::Value::Module(_)) => {{ ctx.module_aliases.insert(({module}.to_string(), {name}.to_string()), __value); }}, ::std::option::Option::Some(_) | ::std::option::Option::None => {{ ctx.module_aliases.remove(&({module}.to_string(), {name}.to_string())); }}, }}",
                    module = rust_string_literal(&self.current_module),
                    name = rust_string_literal(name),
                ));
            return;
        }
        let value = if self.is_local(name) {
            format!("{}.clone()", rust_user_ident(name))
        } else if let Some(storage) = self.binding_storage(name) {
            self.storage_read_src(&storage, 0)
        } else {
            format!(
                "__nybl_read_binding(ctx, {}, {}, 0)?",
                rust_string_literal(&self.current_module),
                rust_string_literal(name),
            )
        };
        let value_tmp = self.fresh_tmp();
        self.line(&format!("let {value_tmp} = {value};"));
        self.line(&format!(
            "if matches!(&{value_tmp}, ::nybl::value::Value::Module(_)) {{ ctx.module_aliases.insert(({module}.to_string(), {name}.to_string()), {value_tmp}.clone()); }} else {{ ctx.module_aliases.remove(&({module}.to_string(), {name}.to_string())); }}",
            module = rust_string_literal(&self.current_module),
            name = rust_string_literal(name),
        ));
    }

    fn emit_runtime_type_scope(&mut self) {
        self.line("let mut __nybl_type_bindings = __nybl_type_bindings.clone();");
        self.line("__nybl_type_bindings.push(::std::rc::Rc::new(__NyblTypeFrame::new()));");
    }

    fn statements_bind_runtime_types(&self, stmts: &[Stmt]) -> bool {
        stmts.iter().any(|stmt| match &stmt.kind {
            StmtKind::StructDecl { .. } | StmtKind::EnumDecl { .. } => true,
            StmtKind::Use {
                path,
                items,
                alias: None,
            } => !self
                .imported_type_names(path, items.as_deref(), false)
                .is_empty(),
            _ => false,
        })
    }

    fn statements_use_runtime_type_bindings(&self, stmts: &[Stmt]) -> bool {
        stmts.iter().any(|stmt| match &stmt.kind {
            StmtKind::Let { value, .. } => expr_uses_runtime_type_bindings(value),
            StmtKind::Assign { target, value, .. } => {
                let target_uses_types = match target {
                    AssignTarget::Variable(_) => false,
                    AssignTarget::Index { object, index } => {
                        expr_uses_runtime_type_bindings(object)
                            || expr_uses_runtime_type_bindings(index)
                    }
                    AssignTarget::Field { object, .. } => expr_uses_runtime_type_bindings(object),
                };
                target_uses_types || expr_uses_runtime_type_bindings(value)
            }
            StmtKind::If {
                condition,
                body,
                else_ifs,
                else_body,
            } => {
                expr_uses_runtime_type_bindings(condition)
                    || self.statements_use_runtime_type_bindings(body)
                    || else_ifs.iter().any(|(condition, body)| {
                        expr_uses_runtime_type_bindings(condition)
                            || self.statements_use_runtime_type_bindings(body)
                    })
                    || else_body
                        .as_deref()
                        .is_some_and(|body| self.statements_use_runtime_type_bindings(body))
            }
            StmtKind::While { condition, body } => {
                expr_uses_runtime_type_bindings(condition)
                    || self.statements_use_runtime_type_bindings(body)
            }
            StmtKind::Repeat { count, body } => {
                expr_uses_runtime_type_bindings(count)
                    || self.statements_use_runtime_type_bindings(body)
            }
            StmtKind::ForIn { iterable, body, .. } => {
                expr_uses_runtime_type_bindings(iterable)
                    || self.statements_use_runtime_type_bindings(body)
            }
            // Nested named functions own their call-time context.
            StmtKind::FnDecl { .. } | StmtKind::MethodDecl { .. } => false,
            StmtKind::Return { value } => {
                value.as_ref().is_some_and(expr_uses_runtime_type_bindings)
            }
            StmtKind::Break | StmtKind::Continue => false,
            StmtKind::Use {
                path,
                items,
                alias: None,
            } => !self
                .imported_type_names(path, items.as_deref(), false)
                .is_empty(),
            StmtKind::Use { alias: Some(_), .. } => false,
            StmtKind::PublicSurface { .. } => false,
            StmtKind::StructDecl { .. } | StmtKind::EnumDecl { .. } => true,
            StmtKind::ExprStmt(expr) => expr_uses_runtime_type_bindings(expr),
        })
    }

    fn emit_runtime_type_scope_for(&mut self, stmts: &[Stmt]) {
        if self.statements_bind_runtime_types(stmts) {
            self.emit_runtime_type_scope();
        }
    }

    fn emit_type_context_publish(&mut self, name: &str, origin: &str, imported: bool) {
        if !self.is_module_top_scope() {
            return;
        }
        let helper = if imported {
            "__nybl_publish_imported_type_binding"
        } else {
            "__nybl_publish_type_binding"
        };
        self.line(&format!(
            "{helper}(ctx, {}, {}, {});",
            rust_string_literal(&self.current_module),
            rust_string_literal(name),
            rust_string_literal(origin),
        ));
    }

    fn is_local(&self, name: &str) -> bool {
        self.scope_stack
            .iter()
            .rev()
            .any(|scope| scope.locals.contains(name))
    }

    fn is_local_in_current_scope(&self, name: &str) -> bool {
        self.scope_stack
            .last()
            .is_some_and(|scope| scope.locals.contains(name))
    }

    fn binding_storage(&self, name: &str) -> Option<BindingStorage> {
        let mut optional = Vec::new();
        let module = self.current_module.clone();
        for scope in self.scope_stack.iter().rev() {
            let hard = if scope.locals.contains(name) {
                Some(BindingStorage::RustLocal {
                    ident: rust_user_ident(name),
                })
            } else if scope.persistent_bindings.contains(name) {
                Some(BindingStorage::Persistent {
                    module: module.clone(),
                    name: name.to_string(),
                })
            } else {
                None
            };
            if let Some(hard) = hard {
                return if optional.is_empty() {
                    Some(hard)
                } else {
                    Some(BindingStorage::OptionalImport {
                        bindings: optional,
                        fallback: Some(Box::new(hard)),
                        module,
                        name: name.to_string(),
                    })
                };
            }
            if let Some(binding) = scope.optional_imports.get(name) {
                optional.push(binding.clone());
            }
        }
        if optional.is_empty() {
            None
        } else {
            Some(BindingStorage::OptionalImport {
                bindings: optional,
                fallback: None,
                module,
                name: name.to_string(),
            })
        }
    }

    fn optional_import_in_current_scope(&self, name: &str) -> Option<OptionalImportBinding> {
        self.scope_stack
            .last()
            .and_then(|scope| scope.optional_imports.get(name))
            .cloned()
    }

    fn bind_optional_import(&mut self, name: &str, binding: OptionalImportBinding) {
        self.scope_stack
            .last_mut()
            .expect("imports are emitted inside a scope")
            .optional_imports
            .insert(name.to_string(), binding);
    }

    fn optional_import_value_src(&self, binding: &OptionalImportBinding, name: &str) -> String {
        match binding {
            OptionalImportBinding::Local { value, .. } => format!("{value}.clone()"),
            OptionalImportBinding::Persistent => format!(
                "__nybl_binding_value(ctx, {}, {})",
                rust_string_literal(&self.current_module),
                rust_string_literal(name),
            ),
        }
    }

    fn storage_optional_value_src(&self, storage: &BindingStorage, line: u32) -> Option<String> {
        let BindingStorage::OptionalImport {
            bindings,
            fallback,
            name,
            ..
        } = storage
        else {
            return None;
        };
        let mut parts = bindings
            .iter()
            .map(|binding| self.optional_import_value_src(binding, name))
            .collect::<Vec<_>>();
        if let Some(fallback) = fallback {
            parts.push(format!(
                "::std::option::Option::Some({})",
                self.storage_read_src(fallback, line)
            ));
        }
        Some(parts.join(".or(") + &")".repeat(parts.len().saturating_sub(1)))
    }

    fn storage_read_src(&self, storage: &BindingStorage, line: u32) -> String {
        match storage {
            BindingStorage::RustLocal { ident } => format!("{ident}.clone()"),
            BindingStorage::Persistent { module, name } => format!(
                "__nybl_read_binding(ctx, {}, {}, {line})?",
                rust_string_literal(module),
                rust_string_literal(name),
            ),
            BindingStorage::OptionalImport { module, name, .. } => {
                let optional = self
                    .storage_optional_value_src(storage, line)
                    .expect("optional storage");
                format!(
                    "match {optional} {{ ::std::option::Option::Some(__value) => __value, ::std::option::Option::None => __nybl_active_function_value(ctx, {}, {}, {line})? }}",
                    rust_string_literal(module),
                    rust_string_literal(name),
                )
            }
        }
    }

    fn optional_module_namespace_src(&self, name: &str, line: u32) -> Option<String> {
        if !self.opts.sandbox || !self.has_module_alias_candidate(name) {
            return None;
        }
        let storage = self.binding_storage(name)?;
        let optional = self.storage_optional_value_src(&storage, line)?;
        Some(format!(
            "__nybl_optional_alias_namespace({optional}, {}, {line})?",
            rust_string_literal(name),
        ))
    }

    fn storage_mut_option_sources(storage: &BindingStorage) -> Vec<String> {
        match storage {
            BindingStorage::RustLocal { ident } => {
                vec![format!("::std::option::Option::Some(&mut {ident})")]
            }
            BindingStorage::Persistent { module, name } => vec![format!(
                "__nybl_binding_mut_option(ctx, {}, {})",
                rust_string_literal(module),
                rust_string_literal(name),
            )],
            BindingStorage::OptionalImport {
                bindings,
                fallback,
                module,
                name,
            } => {
                let mut sources = bindings
                    .iter()
                    .map(|binding| match binding {
                        OptionalImportBinding::Local { value, .. } => {
                            format!("{value}.as_mut()")
                        }
                        OptionalImportBinding::Persistent => format!(
                            "__nybl_binding_mut_option(ctx, {}, {})",
                            rust_string_literal(module),
                            rust_string_literal(name),
                        ),
                    })
                    .collect::<Vec<_>>();
                if let Some(fallback) = fallback {
                    sources.extend(Self::storage_mut_option_sources(fallback));
                }
                sources
            }
        }
    }

    fn storage_mut_src(&self, storage: &BindingStorage, name: &str, line: u32) -> String {
        let sources = Self::storage_mut_option_sources(storage);
        let mut body = String::from("{");
        for (index, source) in sources.iter().enumerate() {
            if index == 0 {
                write!(
                    body,
                    " if let ::std::option::Option::Some(__value) = {source} {{ __value }}"
                )
                .unwrap();
            } else {
                write!(
                    body,
                    " else if let ::std::option::Option::Some(__value) = {source} {{ __value }}"
                )
                .unwrap();
            }
        }
        write!(
            body,
            " else {{ return Err(::nybl::error::NyblError::runtime(::nybl::error_messages::variable_not_found({}), {line})); }} }}",
            rust_string_literal(name),
        )
        .unwrap();
        body
    }

    fn ref_target_candidates(storage: &BindingStorage, out: &mut Vec<RefTargetCandidate>) {
        match storage {
            BindingStorage::RustLocal { ident } => {
                out.push(RefTargetCandidate::DirectLocal(ident.clone()));
            }
            BindingStorage::Persistent { module, name } => {
                out.push(RefTargetCandidate::Persistent {
                    module: module.clone(),
                    name: name.clone(),
                });
            }
            BindingStorage::OptionalImport {
                bindings,
                fallback,
                module,
                name,
            } => {
                for binding in bindings {
                    match binding {
                        OptionalImportBinding::Local { value, .. } => {
                            out.push(RefTargetCandidate::OptionalLocal(value.clone()));
                        }
                        OptionalImportBinding::Persistent => {
                            out.push(RefTargetCandidate::Persistent {
                                module: module.clone(),
                                name: name.clone(),
                            });
                        }
                    }
                }
                if let Some(fallback) = fallback {
                    Self::ref_target_candidates(fallback, out);
                }
            }
        }
    }

    /// Generate a preflight token plus snapshot/commit accessors for one ref
    /// target. Persistent bindings resolve their origin chain exactly once
    /// before ordinary arguments execute; commit then uses the stable key via
    /// an infallible internal accessor. Presence-aware optional imports also
    /// retain the exact local/persistent candidate selected at preflight.
    fn ref_target_sources(
        &mut self,
        storage: &BindingStorage,
        exposed_name: &str,
        line: u32,
    ) -> (String, String, String) {
        let mut candidates = Vec::new();
        Self::ref_target_candidates(storage, &mut candidates);
        if candidates.len() == 1 {
            match &candidates[0] {
                RefTargetCandidate::DirectLocal(ident) => {
                    return (
                        String::new(),
                        format!("{ident}.clone()"),
                        format!("&mut {ident}"),
                    );
                }
                RefTargetCandidate::OptionalLocal(value) => {
                    let prelude = format!(
                        "if {value}.is_none() {{ return Err(::nybl::error::NyblError::runtime(::nybl::error_messages::variable_not_found({}), {line})); }} ",
                        rust_string_literal(exposed_name),
                    );
                    return (
                        prelude,
                        format!(
                            "{value}.as_ref().expect(\"pre-resolved optional ref binding\").clone()"
                        ),
                        format!("{value}.as_mut().expect(\"pre-resolved optional ref binding\")"),
                    );
                }
                RefTargetCandidate::Persistent { module, name } => {
                    let key = self.fresh_tmp();
                    let prelude = format!(
                        "let {key} = __nybl_resolve_binding_key(ctx, {}, {}).ok_or_else(|| ::nybl::error::NyblError::runtime(::nybl::error_messages::variable_not_found({}), {line}))?; ",
                        rust_string_literal(module),
                        rust_string_literal(name),
                        rust_string_literal(exposed_name),
                    );
                    return (
                        prelude,
                        format!("__nybl_binding_key_value(ctx, &{key}).clone()"),
                        format!("__nybl_binding_key_mut(ctx, &{key})"),
                    );
                }
            }
        }

        let selector = self.fresh_tmp();
        let key = self.fresh_tmp();
        let mut selection = String::new();
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        for (index, candidate) in candidates.iter().enumerate() {
            let prefix = if index == 0 { "if" } else { "else if" };
            match candidate {
                RefTargetCandidate::DirectLocal(ident) => {
                    write!(
                        selection,
                        "{prefix} true {{ ({index}usize, ::std::option::Option::None) }} "
                    )
                    .unwrap();
                    reads.push(format!("{index} => {ident}.clone()"));
                    writes.push(format!("{index} => &mut {ident}"));
                }
                RefTargetCandidate::OptionalLocal(value) => {
                    write!(
                        selection,
                        "{prefix} {value}.is_some() {{ ({index}usize, ::std::option::Option::None) }} "
                    )
                    .unwrap();
                    reads.push(format!(
                        "{index} => {value}.as_ref().expect(\"pre-resolved optional ref binding\").clone()"
                    ));
                    writes.push(format!(
                        "{index} => {value}.as_mut().expect(\"pre-resolved optional ref binding\")"
                    ));
                }
                RefTargetCandidate::Persistent { module, name } => {
                    let candidate_key = self.fresh_tmp();
                    write!(
                        selection,
                        "{prefix} let ::std::option::Option::Some({candidate_key}) = __nybl_resolve_binding_key(ctx, {}, {}) {{ ({index}usize, ::std::option::Option::Some({candidate_key})) }} ",
                        rust_string_literal(module),
                        rust_string_literal(name),
                    )
                    .unwrap();
                    reads.push(format!(
                        "{index} => __nybl_binding_key_value(ctx, {key}.as_ref().expect(\"persistent ref key\")).clone()"
                    ));
                    writes.push(format!(
                        "{index} => __nybl_binding_key_mut(ctx, {key}.as_ref().expect(\"persistent ref key\"))"
                    ));
                }
            }
        }
        write!(
            selection,
            "else {{ return Err(::nybl::error::NyblError::runtime(::nybl::error_messages::variable_not_found({}), {line})); }}",
            rust_string_literal(exposed_name),
        )
        .unwrap();
        let prelude = format!("let ({selector}, {key}) = {selection}; ");
        (
            prelude,
            format!(
                "match {selector} {{ {}, _ => unreachable!(\"pre-resolved ref selector\") }}",
                reads.join(", "),
            ),
            format!(
                "match {selector} {{ {}, _ => unreachable!(\"pre-resolved ref selector\") }}",
                writes.join(", "),
            ),
        )
    }

    fn emitted_place_target(&mut self, place: AotPlace, line: u32) -> (String, EmittedPlaceTarget) {
        let storage =
            self.binding_storage(&place.root)
                .unwrap_or_else(|| BindingStorage::Persistent {
                    module: self.current_module.clone(),
                    name: place.root.clone(),
                });
        let (preflight, root_read, root_write) =
            self.ref_target_sources(&storage, &place.root, line);
        (
            preflight,
            EmittedPlaceTarget {
                place,
                root_read,
                root_write,
            },
        )
    }

    /// Emit projection expressions in root-to-leaf order and retain their
    /// values in a reusable path vector. Callers choose where this text is
    /// placed to preserve the language's call-entry ordering.
    fn place_steps_src(&mut self, place: &AotPlace) -> Result<(String, String), NyblError> {
        let mut prelude = String::new();
        let mut steps = Vec::with_capacity(place.projections.len());
        for projection in &place.projections {
            match projection {
                AotPlaceProjection::Field(field) => steps.push(format!(
                    "__NyblPlaceStep::Field({})",
                    rust_string_literal(field)
                )),
                AotPlaceProjection::Index(index) => {
                    let value = self.expr_src(index)?;
                    let tmp = self.fresh_tmp();
                    write!(prelude, "let {tmp} = {value}; ").unwrap();
                    steps.push(format!("__NyblPlaceStep::Index({tmp})"));
                }
            }
        }
        let path = self.fresh_tmp();
        write!(
            prelude,
            "let {path}: ::std::vec::Vec<__NyblPlaceStep> = ::std::vec![{}]; ",
            steps.join(", ")
        )
        .unwrap();
        Ok((prelude, path))
    }

    fn is_dynamic_namespace_local(&self, name: &str) -> bool {
        for scope in self.scope_stack.iter().rev() {
            if scope.locals.contains(name) {
                return !scope.module_alias_locals.contains(name)
                    || scope.mutated_locals.contains(name)
                    || self
                        .callable_mutations
                        .last()
                        .is_some_and(|names| names.contains(name));
            }
        }
        self.has_declaration_alias_overlay(name)
            || (self.has_module_alias_candidate(name)
                && self.binding_storage(name).is_some()
                && (self.opts.sandbox
                    || self
                        .callable_mutations
                        .last()
                        .is_some_and(|names| names.contains(name))))
    }

    fn mark_local_mutated(&mut self, name: &str) {
        for scope in self.scope_stack.iter_mut().rev() {
            if scope.locals.contains(name) {
                scope.mutated_locals.insert(name.to_string());
                break;
            }
        }
    }

    fn declaration_alias_overlay(&self, name: &str) -> Option<String> {
        if self.opts.sandbox {
            return None;
        }
        self.declaration_alias_overlays
            .last()
            .filter(|aliases| aliases.contains(name))
            .map(|_| declaration_alias_overlay_ident(name))
    }

    fn declaration_alias_writes_through(&self, name: &str) -> bool {
        self.has_declaration_alias_overlay(name)
            && self
                .declaration_alias_write_through
                .last()
                .copied()
                .unwrap_or(false)
    }

    fn has_declaration_alias_overlay(&self, name: &str) -> bool {
        self.declaration_alias_overlays
            .last()
            .is_some_and(|aliases| aliases.contains(name))
    }

    fn declaration_alias_read_src(&self, name: &str, line: u32) -> Option<String> {
        if self.opts.sandbox {
            return None;
        }
        if self.declaration_alias_writes_through(name) {
            return Some(format!(
                "__nybl_read_binding(ctx, {}, {}, {line})?",
                rust_string_literal(&self.current_module),
                rust_string_literal(name),
            ));
        }
        self.declaration_alias_overlay(name).map(|overlay| {
            format!(
                "__nybl_declaration_alias_read(ctx, &mut {overlay}, {module}, {alias}, {line})?",
                module = rust_string_literal(&self.current_module),
                alias = rust_string_literal(name),
            )
        })
    }

    fn declaration_alias_namespace_src(&self, name: &str, line: u32) -> Option<String> {
        if self.opts.sandbox {
            return self.has_declaration_alias_overlay(name).then(|| {
                format!(
                    "__nybl_authoritative_alias_namespace(ctx, {}, {}, {line})?",
                    rust_string_literal(&self.current_module),
                    rust_string_literal(name),
                )
            });
        }
        if self.declaration_alias_writes_through(name) {
            return Some(format!(
                "__nybl_binding_value(ctx, {module}, {alias}).ok_or_else(|| ::nybl::error::NyblError::runtime(format!(\"`{{}}` isn't a module alias in scope\", {alias}), {line}))?",
                module = rust_string_literal(&self.current_module),
                alias = rust_string_literal(name),
            ));
        }
        self.declaration_alias_overlay(name).map(|overlay| {
            format!(
                "__nybl_declaration_alias_namespace(ctx, &mut {overlay}, {module}, {alias}, {line})?",
                module = rust_string_literal(&self.current_module),
                alias = rust_string_literal(name),
            )
        })
    }

    fn declaration_alias_optional_src(&self, name: &str) -> Option<String> {
        if self.opts.sandbox {
            return None;
        }
        if self.declaration_alias_writes_through(name) {
            return Some(format!(
                "__nybl_binding_value(ctx, {}, {})",
                rust_string_literal(&self.current_module),
                rust_string_literal(name),
            ));
        }
        self.declaration_alias_overlay(name).map(|overlay| {
            format!(
                "__nybl_declaration_alias_optional(ctx, &mut {overlay}, {module}, {alias})",
                module = rust_string_literal(&self.current_module),
                alias = rust_string_literal(name),
            )
        })
    }

    fn declaration_alias_mut_src(&self, name: &str, line: u32) -> Option<String> {
        if self.opts.sandbox {
            return None;
        }
        if self.declaration_alias_writes_through(name) {
            return Some(format!(
                "__nybl_binding_mut(ctx, {}, {}, {line})?",
                rust_string_literal(&self.current_module),
                rust_string_literal(name),
            ));
        }
        self.declaration_alias_overlay(name).map(|overlay| {
            format!(
                "__nybl_declaration_alias_mut(&mut {overlay}, {alias}, {line})?",
                alias = rust_string_literal(name),
            )
        })
    }

    fn declaration_alias_assign_src(&self, name: &str, value: &str, line: u32) -> Option<String> {
        if self.opts.sandbox {
            return None;
        }
        self.declaration_alias_overlay(name).map(|overlay| {
            if self.declaration_alias_writes_through(name) {
                return format!(
                    "if !__nybl_has_binding(ctx, {module}, {alias}) {{ return Err(::nybl::error::NyblError::runtime({missing}, {line})); }} \
                     let __nybl_alias_value = {value}; \
                     *__nybl_binding_mut(ctx, {module}, {alias}, {line})? = __nybl_alias_value.clone(); \
                     {overlay}.overlay = ::std::option::Option::Some(__nybl_alias_value);",
                    module = rust_string_literal(&self.current_module),
                    alias = rust_string_literal(name),
                    missing = rust_string_literal(&format!(
                        "Variable `{name}` doesn't exist yet"
                    )),
                );
            }
            format!(
                "__nybl_declaration_alias_assign(ctx, &mut {overlay}, {value}, {module}, {alias}, {line})?;",
                module = rust_string_literal(&self.current_module),
                alias = rust_string_literal(name),
            )
        })
    }

    fn refresh_write_through_alias_overlay(&mut self, name: &str) {
        if !self.declaration_alias_writes_through(name) {
            return;
        }
        let overlay = declaration_alias_overlay_ident(name);
        self.line(&format!(
            "{overlay}.overlay = __nybl_binding_value(ctx, {module}, {name});",
            module = rust_string_literal(&self.current_module),
            name = rust_string_literal(name),
        ));
    }

    fn ident_value_src(&self, name: &str, line: u32) -> Result<String, NyblError> {
        if self.is_local(name) {
            Ok(format!("{}.clone()", rust_user_ident(name)))
        } else if let Some(alias) = self.declaration_alias_read_src(name, line) {
            Ok(alias)
        } else if let Some(storage) = self.binding_storage(name) {
            Ok(self.storage_read_src(&storage, line))
        } else if let Some(site) = self.local_function_site(name) {
            Ok(format!("__nybl_function_site_value(ctx, {site}, {line})?"))
        } else if self.fn_info.top_level_fns.contains(name)
            || self.fn_info.all_fns.contains(name)
            || self.has_declaration_alias_overlay(name)
        {
            Ok(format!(
                "__nybl_active_function_value(ctx, {}, {}, {})?",
                rust_string_literal(&self.current_module),
                rust_string_literal(name),
                line,
            ))
        } else if self.in_non_capturing_method {
            Ok(format!(
                "::std::result::Result::<::nybl::value::Value, ::nybl::error::NyblError>::Err(::nybl::error::NyblError::runtime(::nybl::error_messages::variable_not_found({}), {}))?",
                rust_string_literal(name),
                line,
            ))
        } else if self.in_callable_body {
            Ok(format!(
                "__nybl_read_binding(ctx, {}, {}, {})?",
                rust_string_literal(&self.current_module),
                rust_string_literal(name),
                line,
            ))
        } else {
            Ok(format!("{}.clone()", rust_user_ident(name)))
        }
    }

    fn is_module_top_scope(&self) -> bool {
        !self.in_callable_body && self.scope_stack.len() == 1
    }

    /// Whether a named-call site emitted for the current module still
    /// needs the dynamic `__nybl_binding_value` pre-dispatch probe.
    /// Conservative: unknown modules and unresolved import surfaces
    /// keep the probe so #95 / #114 rebinding semantics can't regress.
    fn call_may_be_rebound(&self, name: &str) -> bool {
        match self.rebindable_call_surfaces.get(&self.current_module) {
            Some(Some(surface)) => surface.contains(name),
            Some(None) | None => true,
        }
    }

    fn wrapper_fn_name(&self, name: &str) -> String {
        wrapper_fn_name_with(&self.module_prefix, name)
    }

    fn register_function_site(
        &mut self,
        name: &str,
        params: &[Parameter],
        visibility: Visibility,
        line: u32,
        abi_eligible: bool,
    ) -> usize {
        let id = self.functions.sites.len();
        self.functions.sites.push(FunctionSite {
            id,
            module_path: self.current_module.clone(),
            name: name.to_string(),
            params: params.to_vec(),
            visibility,
            line,
            abi_eligible,
        });
        self.functions
            .sites_by_module_name
            .entry((self.current_module.clone(), name.to_string()))
            .or_default()
            .push(id);
        id
    }

    fn function_site_for_stmt(&self, stmt: &Stmt) -> usize {
        self.function_sites_by_stmt[&(stmt as *const Stmt as usize)]
    }

    fn function_name_has_single_site(&self, name: &str) -> bool {
        self.fn_info.site_counts.get(name) == Some(&1)
    }

    /// A unique, non-rebindable module-level function can keep native AOT's
    /// typed Rust-call fast path. The reached-site bit preserves source-order
    /// semantics without paying for a name-keyed hash lookup on every call.
    fn native_static_function_site(&self, name: &str) -> Option<usize> {
        if self.opts.sandbox
            || !self.fn_info.top_level_fns.contains(name)
            || self.call_may_be_rebound(name)
        {
            return None;
        }
        if self.fn_info.site_counts.get(name) != Some(&1) {
            return None;
        }
        self.functions
            .sites_by_module_name
            .get(&(self.current_module.clone(), name.to_string()))
            .and_then(|sites| sites.first().copied())
    }

    fn bind_function_site(&mut self, name: &str, site: usize) {
        self.scope_stack
            .last_mut()
            .expect("function declarations are emitted inside a scope")
            .function_sites
            .insert(name.to_string(), site);
    }

    fn local_function_site(&self, name: &str) -> Option<usize> {
        self.scope_stack
            .iter()
            .rev()
            .find_map(|scope| scope.function_sites.get(name).copied())
    }

    fn claim_function_in_current_scope(&mut self, name: &str) {
        self.scope_stack
            .last_mut()
            .expect("function claims are emitted inside a scope")
            .claimed_functions
            .insert(name.to_string());
    }

    fn function_claimed_in_current_scope(&self, name: &str) -> bool {
        self.scope_stack
            .last()
            .is_some_and(|scope| scope.claimed_functions.contains(name))
    }

    fn exact_function_site_call_src(
        &self,
        site_id: usize,
        arg_names: &[String],
        line: u32,
    ) -> String {
        let site = &self.functions.sites[site_id];
        if arg_names.len() != site.params.len() {
            return self.function_site_arity_error_src(site_id, arg_names.len(), line);
        }
        let args = if arg_names.is_empty() {
            String::new()
        } else {
            format!(", {}", arg_names.join(", "))
        };
        format!(
            "{}(ctx{args}, {line})?",
            guarded_function_site_fn_name(site_id),
        )
    }

    fn function_site_arity_error_src(
        &self,
        site_id: usize,
        actual_arity: usize,
        line: u32,
    ) -> String {
        let site = &self.functions.sites[site_id];
        format!(
            "return Err(::nybl::error::NyblError::runtime(format!(\"`{}` expects {} argument{}, but got {}\"), {}))",
            site.name,
            site.params.len(),
            if site.params.len() == 1 { "" } else { "s" },
            actual_arity,
            line,
        )
    }

    fn reached_function_site_call_src(
        &self,
        site_id: usize,
        arg_names: &[String],
        line: u32,
    ) -> String {
        let site = &self.functions.sites[site_id];
        let call = self.exact_function_site_call_src(site_id, arg_names, line);
        format!(
            "if !ctx.reached_function_sites[{site_id}] {{ return Err(::nybl::error::NyblError::runtime(::nybl::error_messages::function_not_found({name}), {line})); }} {call}",
            name = rust_string_literal(&site.name),
        )
    }

    fn finish(self) -> String {
        self.out
    }

    /// Pre-pass over a statement list to populate
    /// `type_bindings` + `module_aliases` from every top-level
    /// `use` statement *before* any fn body is emitted. The
    /// AOT lifts fn decls out of source order, so a naïve walk
    /// would try to emit `fn label(c) { match c { p.Color::Red
    /// => ... } }` before seeing `use paint as p` and the
    /// pattern resolver would come up empty.
    fn seed_uses(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            if let StmtKind::Use { path, items, alias } = &stmt.kind {
                let exposed_types =
                    self.imported_type_names(path, items.as_deref(), alias.is_some());
                if let Some(alias_name) = alias {
                    self.bind_module_alias(alias_name, &exposed_types);
                } else {
                    for (name, origin) in exposed_types {
                        self.bind_imported_type(&name, &origin);
                    }
                }
            }
        }
    }

    fn seed_module_alias_candidates(&mut self, candidates: &BTreeMap<String, ModuleValueExport>) {
        if let Some(frame) = self.module_aliases.last_mut() {
            frame.clear();
        }
        for (name, export) in candidates {
            self.bind_module_export(name, export);
        }
    }

    fn declaration_aliases_for_callable(&self, params: &[Parameter], body: &[Stmt]) -> Vec<String> {
        let mut outer = EmissionScope::default();
        for import in &self.declaration_aliases {
            if let Some(alias) = &import.alias {
                outer.locals.insert(alias.clone());
            }
        }
        let mut known: HashSet<String> = params.iter().map(|param| param.name.clone()).collect();
        let mut referenced = FreeVarDependencies::for_declaration_aliases();
        scan_free_vars_stmts(
            body,
            &mut known,
            &mut referenced,
            core::slice::from_ref(&outer),
            &self.fn_info,
        );
        referenced.required.extend(referenced.pattern_namespaces);
        let mut aliases = Vec::new();
        for import in &self.declaration_aliases {
            let Some(alias) = &import.alias else {
                continue;
            };
            if !referenced.required.contains(alias) || self.is_local(alias) {
                continue;
            }
            aliases.push(alias.clone());
        }
        aliases
    }

    fn emit_declaration_alias_overlays(
        &mut self,
        aliases: &[String],
        initializers: &HashMap<String, String>,
    ) -> HashSet<String> {
        let mut overlays = HashSet::new();
        for alias in aliases {
            overlays.insert(alias.clone());
            let initializer = initializers
                .get(alias)
                .map(String::as_str)
                .unwrap_or("::std::option::Option::None");
            self.line(&format!(
                "let mut {overlay} = __NyblDeclarationAliasBinding {{ overlay: {initializer}, cached: ::std::option::Option::None }};",
                overlay = declaration_alias_overlay_ident(alias),
            ));
        }
        overlays
    }

    fn imported_type_names(
        &self,
        module_path: &str,
        items: Option<&[String]>,
        aliased: bool,
    ) -> Vec<(String, String)> {
        let Some(entry) = self.modules.modules.get(module_path) else {
            return Vec::new();
        };
        match items {
            Some(items) => items
                .iter()
                .filter_map(|name| {
                    entry
                        .effective_types
                        .get(name)
                        .map(|origin| (name.clone(), origin.clone()))
                })
                .collect(),
            None => entry
                .effective_types
                .iter()
                .filter(|(name, _)| aliased || !name.starts_with('_'))
                .map(|(name, origin)| (name.clone(), origin.clone()))
                .collect(),
        }
    }

    /// Pre-seed the current `type_bindings` frame with module-top-level
    /// declarations only. Called before methods
    /// and top-level fns get emitted so their bodies can
    /// resolve bare `MyType { ... }` even though the AST walk
    /// hasn't reached the `struct MyType` decl yet.
    fn seed_types_for_module(&mut self, module_path: &str) {
        let names = self
            .types
            .top_level_types
            .get(module_path)
            .cloned()
            .unwrap_or_default();
        for n in names {
            self.bind_type(&n, module_path);
        }
    }

    fn emit_program(&mut self, stmts: &[Stmt]) -> Result<(), NyblError> {
        let module_name = self.opts.module_name.clone();
        if let Some(ref name) = module_name {
            writeln!(self.out, "pub mod {name} {{").unwrap();
        }
        self.emit_header();
        self.emit_runtime_preamble();
        let (_, root_alias_candidates, _) =
            effective_module_value_exports(stmts, &self.modules.modules);
        self.rebindable_call_surfaces.insert(
            String::from(nybl::value::ROOT_MODULE_PATH),
            rebindable_binding_surface(stmts, &self.modules.modules),
        );
        for (path, entry) in &self.modules.modules {
            self.rebindable_call_surfaces.insert(
                path.clone(),
                rebindable_binding_surface(&entry.ast, &self.modules.modules),
            );
        }
        self.root_declaration_aliases = module_imports_from_exports(&root_alias_candidates);
        self.declaration_aliases = self.root_declaration_aliases.clone();
        // Seed the outermost type_bindings frame with the
        // root program's declared types so methods + top-level
        // fns (emitted ahead of the main AST walk) can resolve
        // bare type names in their bodies. Same pre-pass for
        // `use` statements so aliases + selective-type imports
        // are visible to the fn bodies we're about to emit.
        self.seed_types_for_module(nybl::value::ROOT_MODULE_PATH);
        self.seed_uses(stmts);
        self.seed_module_alias_candidates(&root_alias_candidates);
        // Imported modules emit first. Eager top-level edges are
        // topo-ordered leaves-first; lazy nested edges need no Rust
        // item ordering but are present in the same resolved graph.
        self.emit_imported_modules()?;
        // Top-level fn declarations move out of `run_program`'s
        // body to module scope. That way the `__nybl_user_fn_value_*`
        // wrappers (also at module scope) can reference them, and
        // `let g = fib` works even before `fib` is called.
        self.emit_top_level_fn_decls(stmts)?;
        // Every method statement gets a unique lifted body and uniform
        // adapter. Runtime declaration statements publish those adapters into
        // dense slots; lifting itself does not make a method visible.
        self.emit_method_sites()?;
        self.emit_method_dispatcher();
        self.emit_method_ref_dispatcher();
        self.emit_run_program(stmts)?;
        self.emit_function_site_dispatcher();
        self.emit_function_site_catalogue();
        self.emit_fn_value_wrappers();
        self.emit_public_entry();
        if self.opts.emit_main && module_name.is_none() {
            self.emit_main();
        }
        if module_name.is_some() {
            self.out.push_str("}\n");
        }
        Ok(())
    }

    /// Emit every module in the pre-resolved graph, in topo
    /// order. Each module gets: its user fn decls (prefixed by
    /// the module slug), its fn-value wrappers, its exports
    /// struct, and its load fn.
    fn emit_imported_modules(&mut self) -> Result<(), NyblError> {
        let order = self.modules.order.clone();
        for name in &order {
            let entry = self
                .modules
                .modules
                .get(name)
                .cloned()
                .expect("module present in graph");
            self.emit_one_module(name, &entry)
                .map_err(|error| error.with_module_source(name, entry.source.as_str()))?;
        }
        Ok(())
    }

    fn emit_one_module(&mut self, name: &str, entry: &ModuleEntry) -> Result<(), NyblError> {
        // Swap in this module's scope: prefix every user fn we
        // emit with the module slug, and replace `fn_info` with
        // the module's own fns so `is_local` / Ident / Call
        // resolution inside the module body behave correctly.
        // `current_module` also switches to the module's
        // source-level path so types declared in this module's
        // body tag their values correctly.
        let saved_prefix = std::mem::replace(&mut self.module_prefix, module_fn_prefix(name));
        let saved_fn_info = std::mem::replace(&mut self.fn_info, collect_fn_info(&entry.ast));
        let saved_module = std::mem::replace(&mut self.current_module, name.to_string());
        // Fresh type_bindings frame for this module's body —
        // bare names declared here shouldn't leak back to the
        // caller / other modules. Seeded with the builtins the
        // emitter was already carrying so `Result` / `RuntimeError`
        // stay visible.
        let mut module_frame: HashMap<String, String> = HashMap::new();
        module_frame.insert(
            String::from("Result"),
            String::from(nybl::value::BUILTIN_MODULE_PATH),
        );
        module_frame.insert(
            String::from("RuntimeError"),
            String::from(nybl::value::BUILTIN_MODULE_PATH),
        );
        let saved_bindings = std::mem::replace(&mut self.type_bindings, vec![module_frame]);
        let saved_aliases = std::mem::replace(&mut self.module_aliases, vec![HashMap::new()]);
        let saved_declaration_aliases = std::mem::replace(
            &mut self.declaration_aliases,
            module_imports_from_exports(&entry.module_alias_candidates),
        );
        // Seed this module's types too — see
        // `seed_types_for_module` for why this matters. Methods
        // inside the module need to resolve bare type names
        // before the AST walker gets to the decls. Same treatment
        // for the module's own `use` statements.
        self.seed_types_for_module(name);
        self.seed_uses(&entry.ast);
        self.seed_module_alias_candidates(&entry.module_alias_candidates);

        self.emit_top_level_fn_decls(&entry.ast)?;
        self.emit_fn_value_wrappers();
        self.emit_module_exports_struct(name, entry);
        self.emit_module_load_fn(name, entry)?;

        self.fn_info = saved_fn_info;
        self.module_prefix = saved_prefix;
        self.current_module = saved_module;
        self.type_bindings = saved_bindings;
        self.module_aliases = saved_aliases;
        self.declaration_aliases = saved_declaration_aliases;
        Ok(())
    }

    fn emit_module_exports_struct(&mut self, name: &str, entry: &ModuleEntry) {
        writeln!(self.out, "#[derive(Clone)]").unwrap();
        writeln!(
            self.out,
            "struct {type_name} {{",
            type_name = module_exports_type_name(name)
        )
        .unwrap();
        for export in &entry.effective_exports {
            let value_type = if self.opts.sandbox {
                "::std::option::Option<__NyblExport>"
            } else {
                "::nybl::value::Value"
            };
            writeln!(
                self.out,
                "    {ident}: {value_type},",
                ident = rust_user_ident(export),
            )
            .unwrap();
        }
        self.out.push_str("}\n\n");
    }

    /// Emit a `use` statement in one of four forms, loading the
    /// module once (via its `__mod_*_load` fn — which handles
    /// caching + cycle detection) and then either:
    ///
    /// - **Glob** (`use path`) — injects every public export as a
    ///   Rust local. Private names (`_`-prefixed) are filtered out.
    ///   Idempotent: re-running the same plain-glob in the same
    ///   scope is a no-op.
    /// - **Selective** (`use path.{a, b, Type}`) — injects only the
    ///   listed items as locals. Private names *are* reachable
    ///   through this form (the list is explicit, so a leading
    ///   `_` isn't meaningful here). Items may name bindings *or*
    ///   types: types are globally registered in the AOT, so they
    ///   don't need a Rust local — the emitter just validates the
    ///   name is actually exported.
    /// - **Aliased glob** (`use path as m`) — packs every public
    ///   export into a `Value::Module` and binds it under `m`.
    /// - **Aliased selective** (`use path.{a, b} as m`) — packs
    ///   only the listed items into a `Value::Module` and binds
    ///   it under `m`.
    ///
    /// Shadow conflicts on injected names are emitted as a
    /// transpile-time error for aliased forms. Flat imports remain
    /// first-win; plain globs additionally emit the same runtime
    /// warning as the walker when an available export loses a
    /// same-frame conflict.
    fn emit_use_stmt(
        &mut self,
        path: &str,
        items: &Option<Vec<String>>,
        alias: &Option<String>,
        line: u32,
    ) -> Result<(), NyblError> {
        let module_top_scope = self.is_module_top_scope();
        // Idempotency: only plain-glob re-imports are cached. The
        // other three forms can legitimately produce different
        // visible effects (different item subset, different alias
        // binding) when re-run in the same scope, so we always
        // re-emit them.
        let already_imported_here = self
            .scope_stack
            .last()
            .is_some_and(|scope| scope.plain_glob_imports.contains(path));
        if items.is_none() && alias.is_none() && already_imported_here {
            return Ok(());
        }
        let entry = self.modules.modules.get(path).cloned().ok_or_else(|| {
            NyblError::runtime(format!("Module `{path}` not found (nybl-compile)"), line)
        })?;

        // Load once, regardless of form — the exports struct is
        // what every form ultimately reads from.
        let tmp = self.fresh_tmp();
        self.line(&format!(
            "let {tmp} = {load}(ctx)?;",
            tmp = tmp,
            load = module_load_fn_name(path)
        ));

        // Selective form: validate every listed item is actually
        // reachable through this module (as a binding or a type).
        // Run this up-front so the error points at the use-site.
        if let Some(list) = items {
            for item in list {
                let is_binding = entry.effective_exports.iter().any(|e| e == item);
                let is_type = entry.effective_types.contains_key(item);
                if !is_binding && !is_type {
                    return Err(NyblError::runtime(
                        format!("Module `{path}` has no export `{item}`"),
                        line,
                    ));
                }
                if self.opts.sandbox && is_binding && !is_type {
                    self.line(&format!(
                        "if {tmp}.{ident}.is_none() {{ return Err(::nybl::error::NyblError::runtime({}, {line})); }}",
                        rust_string_literal(&format!(
                            "`{item}` isn't exported from `{path}` (selective import)"
                        )),
                        ident = rust_user_ident(item),
                    ));
                }
            }
        }

        // Decide which names to expose + in what form.
        let mut expose_bindings: Vec<String> = match items {
            Some(list) => list
                .iter()
                .filter(|n| entry.effective_exports.iter().any(|e| e == *n))
                .cloned()
                .collect(),
            None => entry
                .effective_exports
                .iter()
                .filter(|n| entry.explicit_surface || alias.is_some() || !n.starts_with('_'))
                .cloned()
                .collect(),
        };
        // A module's values and functions are one import namespace even
        // though the analysis stores their origins separately. Emit the
        // combined surface lexicographically so multi-name warning order is
        // byte-identical to the walker and VM.
        expose_bindings.sort();
        let expose_types: Vec<(String, String)> = match items {
            Some(list) => list
                .iter()
                .filter_map(|name| {
                    entry
                        .effective_types
                        .get(name)
                        .map(|origin| (name.clone(), origin.clone()))
                })
                .collect(),
            None => entry
                .effective_types
                .iter()
                .filter(|(name, _)| {
                    entry.explicit_surface || alias.is_some() || !name.starts_with('_')
                })
                .map(|(name, origin)| (name.clone(), origin.clone()))
                .collect(),
        };

        match alias {
            Some(alias_name) => {
                // Aliased: build a `Value::Module` and bind it
                // under the alias. All reads through the alias
                // (`m.helper`, `m.Entity { ... }`) resolve at
                // runtime by inspecting this value.
                let bindings_tmp = self.fresh_tmp();
                self.line(&format!(
                    "let mut {bindings_tmp}: ::std::vec::Vec<(::std::string::String, ::nybl::value::Value)> = ::std::vec::Vec::new();"
                ));
                for name in &expose_bindings {
                    let ident = rust_user_ident(name);
                    if self.opts.sandbox {
                        self.line(&format!(
                            "match {tmp}.{ident}.clone() {{ ::std::option::Option::Some(__NyblExport::Value(__value)) => {bindings_tmp}.push(({}.to_string(), __value)), ::std::option::Option::Some(__NyblExport::Function(__site)) => {bindings_tmp}.push(({}.to_string(), __nybl_function_site_value(ctx, __site, {line})?)), ::std::option::Option::None => {{}} }}",
                            rust_string_literal(name),
                            rust_string_literal(name),
                        ));
                    } else {
                        self.line(&format!(
                            "{bindings_tmp}.push(({}.to_string(), {tmp}.{ident}.clone()));",
                            rust_string_literal(name),
                        ));
                    }
                }
                let types_src: String = expose_types
                    .iter()
                    .map(|(type_name, origin)| {
                        format!(
                            "({}.to_string(), {}.to_string())",
                            rust_string_literal(type_name),
                            rust_string_literal(origin),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                if self.is_local_in_current_scope(alias_name)
                    || (self.fn_info.all_fns.contains(alias_name)
                        && !self.has_module_alias_candidate(alias_name))
                {
                    return Err(NyblError::runtime(
                        format!(
                            "Alias `{alias_name}` in `use {path} as {alias_name}` would shadow an existing binding"
                        ),
                        line,
                    ));
                }
                self.line(&format!(
                    "let mut {alias}: ::nybl::value::Value = ::nybl::value::Value::new_module_with_type_exports({path_lit}.to_string(), {bindings}, ::nybl::value::NyblTypeExports::from_origins(::std::vec![{types}]), {line})?;",
                    alias = rust_user_ident(alias_name),
                    path_lit = rust_string_literal(path),
                    bindings = bindings_tmp,
                    types = types_src,
                    line = line,
                ));
                if module_top_scope {
                    self.line(&format!(
                        "ctx.module_aliases.insert(({module}.to_string(), {alias_name}.to_string()), {alias}.clone());",
                        module = rust_string_literal(&self.current_module),
                        alias_name = rust_string_literal(alias_name),
                        alias = rust_user_ident(alias_name),
                    ));
                }
                if module_top_scope {
                    self.line(&format!(
                        "__nybl_define_binding(ctx, {}, {}, {});",
                        rust_string_literal(&self.current_module),
                        rust_string_literal(alias_name),
                        rust_user_ident(alias_name),
                    ));
                    self.bind_persistent(alias_name);
                } else {
                    self.bind_local(alias_name);
                    self.scope_stack
                        .last_mut()
                        .expect("use emission has a scope")
                        .module_alias_locals
                        .insert(alias_name.clone());
                }
                // Track the alias for compile-time resolution
                // of namespaced references (`m.Color`). Emit-
                // time resolution is sufficient here because
                // the AOT bakes module_path literals directly
                // into construction + match sites.
                self.bind_module_alias(alias_name, &expose_types);
            }
            None => {
                // Non-aliased: inject each binding as a local.
                // Types don't need a Rust local (they're
                // compile-time metadata in the AOT), but they
                // do need a `type_bindings` entry so
                // construction + pattern sites can resolve
                // the bare name to the right module path.
                for name in &expose_bindings {
                    if module_top_scope {
                        // Native named functions claim their binding at the
                        // declaration's source position. If that claim already
                        // won, the runtime rejects this flat import; mirror the
                        // same source-ordered decision in static storage
                        // resolution so later calls reach the named function.
                        if !self.opts.sandbox && self.function_claimed_in_current_scope(name) {
                            // The claim statically guarantees the shadow, so
                            // the glob warning is unconditional — mirroring
                            // the walker/VM, which warn before skipping the
                            // clashing export.
                            if items.is_none() {
                                self.line(&format!(
                                    "__nybl_warn_glob_shadow({}, {});",
                                    rust_string_literal(name),
                                    rust_string_literal(path),
                                ));
                            }
                            continue;
                        }
                        if self.opts.sandbox {
                            self.line(&format!(
                                "__nybl_import_export(ctx, {}, {}, {}, {}, {tmp}.{ident}.clone(), {}, {line})?;",
                                rust_string_literal(&self.current_module),
                                rust_string_literal(name),
                                rust_string_literal(path),
                                rust_string_literal(name),
                                items.is_none(),
                                ident = rust_user_ident(name),
                            ));
                            self.bind_optional_import(name, OptionalImportBinding::Persistent);
                        } else if entry.effective_value_exports.contains(name) {
                            let import = format!(
                                "__nybl_import_binding_alias(ctx, {}, {}, {}, {});",
                                rust_string_literal(&self.current_module),
                                rust_string_literal(name),
                                rust_string_literal(path),
                                rust_string_literal(name),
                            );
                            if items.is_none() {
                                self.line(&format!(
                                    "if __nybl_has_binding(ctx, {}, {}) {{ __nybl_warn_glob_shadow({}, {}); }} else {{ {import} }}",
                                    rust_string_literal(&self.current_module),
                                    rust_string_literal(name),
                                    rust_string_literal(name),
                                    rust_string_literal(path),
                                ));
                            } else {
                                self.line(&import);
                            }
                            self.bind_persistent(name);
                        } else {
                            let import = format!(
                                "__nybl_import_binding_value(ctx, {}, {}, {tmp}.{ident}.clone());",
                                rust_string_literal(&self.current_module),
                                rust_string_literal(name),
                                ident = rust_user_ident(name),
                            );
                            if items.is_none() {
                                self.line(&format!(
                                    "if __nybl_has_binding(ctx, {}, {}) {{ __nybl_warn_glob_shadow({}, {}); }} else {{ {import} }}",
                                    rust_string_literal(&self.current_module),
                                    rust_string_literal(name),
                                    rust_string_literal(name),
                                    rust_string_literal(path),
                                ));
                            } else {
                                self.line(&import);
                            }
                            self.bind_persistent(name);
                        }
                        if let Some(module_export) = entry.effective_module_exports.get(name) {
                            self.bind_module_export(name, module_export);
                            self.emit_module_alias_context_sync(name);
                        }
                        continue;
                    }
                    // Every non-aliased import form is first-win in
                    // the current frame. An outer-frame binding is
                    // not a clash: this new Rust block should shadow
                    // it just like the walker and VM value scopes.
                    if self.is_local_in_current_scope(name)
                        || (self.is_module_top_scope()
                            && self.fn_info.all_fns.contains(name)
                            && !self.has_module_alias_candidate(name))
                    {
                        if items.is_none() {
                            if self.opts.sandbox {
                                self.line(&format!(
                                    "if {tmp}.{ident}.is_some() {{ __nybl_warn_glob_shadow({}, {}); }}",
                                    rust_string_literal(name),
                                    rust_string_literal(path),
                                    ident = rust_user_ident(name),
                                ));
                            } else {
                                self.line(&format!(
                                    "__nybl_warn_glob_shadow({}, {});",
                                    rust_string_literal(name),
                                    rust_string_literal(path),
                                ));
                            }
                        }
                        continue;
                    }
                    if self.opts.sandbox {
                        let candidate = self.fresh_tmp();
                        self.line(&format!(
                            "let {candidate} = {tmp}.{ident}.clone();",
                            ident = rust_user_ident(name),
                        ));
                        if let Some(OptionalImportBinding::Local {
                            value,
                            function_site,
                        }) = self.optional_import_in_current_scope(name)
                        {
                            if items.is_none() {
                                self.line(&format!(
                                    "if {value}.is_some() && {candidate}.is_some() {{ __nybl_warn_glob_shadow({}, {}); }}",
                                    rust_string_literal(name),
                                    rust_string_literal(path),
                                ));
                            }
                            self.line(&format!(
                                "if {value}.is_none() {{ {function_site} = __nybl_export_function_site(&{candidate}); {value} = __nybl_export_value(ctx, {candidate}, {line})?; }}"
                            ));
                        } else {
                            let function_site = self.fresh_tmp();
                            let value = self.fresh_tmp();
                            self.line(&format!(
                                "let mut {function_site}: ::std::option::Option<usize> = __nybl_export_function_site(&{candidate});"
                            ));
                            self.line(&format!(
                                "let mut {value}: ::std::option::Option<::nybl::value::Value> = __nybl_export_value(ctx, {candidate}, {line})?;"
                            ));
                            self.bind_optional_import(
                                name,
                                OptionalImportBinding::Local {
                                    value,
                                    function_site,
                                },
                            );
                        }
                        if let Some(module_export) = entry.effective_module_exports.get(name) {
                            self.bind_module_export(name, module_export);
                        }
                        continue;
                    }
                    self.line(&format!(
                        "let mut {ident}: ::nybl::value::Value = {tmp}.{ident}.clone();",
                        ident = rust_user_ident(name),
                    ));
                    self.bind_local(name);
                    if let Some(module_export) = entry.effective_module_exports.get(name) {
                        self.bind_module_export(name, module_export);
                        self.scope_stack
                            .last_mut()
                            .expect("use emission has a scope")
                            .module_alias_locals
                            .insert(name.clone());
                        if module_top_scope {
                            self.emit_module_alias_context_sync(name);
                        }
                    }
                }
                for (type_name, origin) in &expose_types {
                    self.line(&format!(
                        "__nybl_bind_imported_type(&mut __nybl_type_bindings, {}, {});",
                        rust_string_literal(type_name),
                        rust_string_literal(origin),
                    ));
                    self.bind_imported_type(type_name, origin);
                    self.emit_type_context_publish(type_name, origin, true);
                }
                if items.is_none() {
                    self.scope_stack
                        .last_mut()
                        .expect("use statements are emitted inside a scope")
                        .plain_glob_imports
                        .insert(path.to_string());
                }
            }
        }
        Ok(())
    }

    /// Emit the load fn for one module: checks the cache, inserts
    /// the `Loading` sentinel, emits the module body (imports +
    /// user statements), then packs every effective export into
    /// the module's exports struct and caches the result.
    fn emit_module_load_fn(&mut self, name: &str, entry: &ModuleEntry) -> Result<(), NyblError> {
        let has_type_sites = self.types.module_has_type_sites(name);
        let method_slots = self.types.module_method_slots(name);
        let load = module_load_fn_name(name);
        let exports = module_exports_type_name(name);
        writeln!(
            self.out,
            "fn {load}(ctx: &mut Ctx<'_>) -> Result<{exports}, ::nybl::error::NyblError> {{",
        )
        .unwrap();
        self.indent = 1;
        self.tmp_counter = 0;

        // Cache lookup: hit a live exports? clone + return.
        // In progress? error with cycle message. Miss? mark
        // loading and fall through to evaluate the body.
        self.line(&format!(
            r#"if let Some(entry) = ctx.module_cache.get("{name}") {{"#
        ));
        self.line("    if entry.is::<__ModuleLoading>() {");
        self.line(&format!(
            "        return Err(::nybl::error::NyblError::runtime(\"Circular import: module `{name}`\", 0));"
        ));
        self.line("    }");
        self.line(&format!(
            "    if let Some(loaded) = entry.downcast_ref::<{exports}>() {{"
        ));
        self.line("        return Ok(loaded.clone());");
        self.line("    }");
        self.line("}");
        if has_type_sites {
            self.line(&format!(
                "let __saved_type_defs = __nybl_take_module_type_defs(ctx, {});",
                rust_string_literal(name)
            ));
        }
        if !method_slots.is_empty() {
            self.line(&format!(
                "let __saved_method_slots = [{}];",
                method_slots
                    .iter()
                    .map(|slot| format!("ctx.method_slots[{slot}]"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        self.line(&format!(
            "let __saved_function_sites: ::std::vec::Vec<_> = ctx.active_function_sites.iter().filter(|((module, _), _)| module == {}).map(|(key, site)| (key.clone(), *site)).collect();",
            rust_string_literal(name),
        ));
        self.line(&format!(
            "let __saved_imported_function_sites: ::std::vec::Vec<_> = ctx.module_imported_function_sites.iter().filter(|((module, _), _)| module == {}).map(|(key, site)| (key.clone(), *site)).collect();",
            rust_string_literal(name),
        ));
        if !self.opts.sandbox {
            self.line(&format!(
                "let __saved_reached_function_sites: ::std::vec::Vec<(usize, bool)> = __NYBL_FUNCTION_SITES.iter().filter(|site| site.module_path == {}).map(|site| (site.id, ctx.reached_function_sites[site.id])).collect();",
                rust_string_literal(name),
            ));
        }
        self.line(&format!(
            "ctx.active_function_sites.retain(|(module, _), _| module != {});",
            rust_string_literal(name),
        ));
        self.line(&format!(
            "ctx.module_imported_function_sites.retain(|(module, _), _| module != {});",
            rust_string_literal(name),
        ));
        self.line(&format!(
            "let __saved_value_bindings = ctx.bindings.remove({});",
            rust_string_literal(name),
        ));
        self.line(&format!(
            "let __saved_binding_origins = ctx.binding_origins.remove({});",
            rust_string_literal(name),
        ));
        self.line(&format!(
            "let __saved_binding_claims = ctx.binding_claims.remove({});",
            rust_string_literal(name),
        ));
        self.line(&format!(
            r#"ctx.module_cache.insert("{name}".to_string(), ::std::boxed::Box::new(__ModuleLoading));"#
        ));
        self.line(&format!(
            "ctx.module_aliases.retain(|(module, _), _| module != {});",
            rust_string_literal(name)
        ));
        self.line(&format!(
            "let __saved_type_bindings = ctx.module_type_bindings.get({}).cloned();",
            rust_string_literal(name)
        ));
        self.line("let mut __nybl_type_bindings = __nybl_fresh_module_type_bindings();");
        self.line(&format!(
            "__nybl_reset_type_bindings(ctx, {});",
            rust_string_literal(name)
        ));
        self.line(&format!(
            "let __load_result = (|| -> Result<{exports}, ::nybl::error::NyblError> {{"
        ));
        self.indent += 1;

        // Sandbox gets a tick at module entry too — same checkpoint
        // as any fn entry.
        self.emit_tick(0);

        // Body: emit the module's statements, skipping top-level
        // fn decls (already emitted) but handling imports /
        // lets / everything else. Track a fresh scope so the
        // emitter's Ident lookup resolves within the module.
        self.callable_mutations
            .push(callable_assignment_names(&entry.ast));
        self.push_scope();
        let saved_top_level = self.in_top_level;
        if self.opts.sandbox {
            self.line("let __nybl_module_body = (|| -> Result<(), ::nybl::error::NyblError> {");
            self.indent += 1;
            self.in_top_level = true;
        }
        let direct_function_sites = self
            .direct_function_sites
            .get(name)
            .cloned()
            .unwrap_or_default();
        let mut direct_function_index = 0;
        for stmt in &entry.ast {
            if let StmtKind::FnDecl { name, .. } = &stmt.kind {
                let site = direct_function_sites[direct_function_index];
                direct_function_index += 1;
                self.line(&format!("__nybl_activate_function(ctx, {site});"));
                self.claim_function_in_current_scope(name);
                continue;
            }
            self.emit_stmt(stmt)?;
        }
        if self.opts.sandbox {
            self.line("Ok(())");
            self.in_top_level = saved_top_level;
            self.indent -= 1;
            self.line("})();");
            self.line("__nybl_module_body?;");
        }

        // Pack every effective export. Top-level lets are Rust
        // locals at this point. Imports injected their names as
        // Rust locals too. Top-level fns need the wrapper to get
        // a `Value::Fn`.
        writeln!(self.out).unwrap();
        self.pad();
        writeln!(self.out, "let __exports = {exports} {{").unwrap();
        for export in &entry.effective_exports {
            self.pad();
            if self.opts.sandbox {
                writeln!(
                    self.out,
                    "    {ident}: match __nybl_binding_value(ctx, {module}, {name}) {{ ::std::option::Option::Some(__value) => ::std::option::Option::Some(__NyblExport::Value(__value)), ::std::option::Option::None => ctx.active_function_sites.get(&({module}.to_string(), {name}.to_string())).or_else(|| ctx.module_imported_function_sites.get(&({module}.to_string(), {name}.to_string()))).copied().map(__NyblExport::Function) }},",
                    ident = rust_user_ident(export),
                    module = rust_string_literal(name),
                    name = rust_string_literal(export),
                )
                .unwrap();
            } else if entry.own_fns.contains_key(export)
                && !entry.effective_value_exports.contains(export)
            {
                writeln!(
                    self.out,
                    "    {ident}: match __nybl_binding_value(ctx, {module}, {name}) {{ ::std::option::Option::Some(__value) => __value, ::std::option::Option::None => {wrapper}(ctx, 0)?, }},",
                    ident = rust_user_ident(export),
                    module = rust_string_literal(name),
                    name = rust_string_literal(export),
                    wrapper = self.wrapper_fn_name(export),
                )
                .unwrap();
            } else {
                writeln!(
                    self.out,
                    "    {ident}: __nybl_read_binding(ctx, {module}, {name}, 0)?,",
                    ident = rust_user_ident(export),
                    module = rust_string_literal(name),
                    name = rust_string_literal(export),
                )
                .unwrap();
            }
        }
        self.pad();
        self.out.push_str("};\n");
        self.pad();
        writeln!(
            self.out,
            r#"ctx.module_cache.insert("{name}".to_string(), ::std::boxed::Box::new(__exports.clone()));"#
        )
        .unwrap();
        self.pad();
        self.out.push_str("Ok(__exports)\n");
        self.pop_scope();
        self.callable_mutations.pop();
        self.indent -= 1;
        self.line("})();");
        self.line("if __load_result.is_err() {");
        self.indent += 1;
        if has_type_sites {
            self.line(&format!(
                "__nybl_clear_module_type_defs(ctx, {});",
                rust_string_literal(name)
            ));
            self.line(&format!(
                "__nybl_restore_module_type_defs(ctx, {}, __saved_type_defs);",
                rust_string_literal(name)
            ));
        }
        for (saved, slot) in method_slots.iter().enumerate() {
            self.line(&format!(
                "ctx.method_slots[{slot}] = __saved_method_slots[{saved}];"
            ));
        }
        self.line(&format!(
            "ctx.active_function_sites.retain(|(module, _), _| module != {});",
            rust_string_literal(name),
        ));
        self.line("ctx.active_function_sites.extend(__saved_function_sites);");
        self.line(&format!(
            "ctx.module_imported_function_sites.retain(|(module, _), _| module != {});",
            rust_string_literal(name),
        ));
        self.line("ctx.module_imported_function_sites.extend(__saved_imported_function_sites);");
        if !self.opts.sandbox {
            self.line("for (__site, __reached) in __saved_reached_function_sites { ctx.reached_function_sites[__site] = __reached; }");
        }
        self.line(&format!(
            "ctx.bindings.remove({});",
            rust_string_literal(name),
        ));
        self.line(&format!(
            "if let ::std::option::Option::Some(__saved) = __saved_value_bindings {{ ctx.bindings.insert({}.to_string(), __saved); }}",
            rust_string_literal(name),
        ));
        self.line(&format!(
            "ctx.binding_origins.remove({});",
            rust_string_literal(name),
        ));
        self.line(&format!(
            "if let ::std::option::Option::Some(__saved) = __saved_binding_origins {{ ctx.binding_origins.insert({}.to_string(), __saved); }}",
            rust_string_literal(name),
        ));
        self.line(&format!(
            "ctx.binding_claims.remove({});",
            rust_string_literal(name),
        ));
        self.line(&format!(
            "if let ::std::option::Option::Some(__saved) = __saved_binding_claims {{ ctx.binding_claims.insert({}.to_string(), __saved); }}",
            rust_string_literal(name),
        ));
        self.line(&format!(
            "ctx.module_cache.remove({});",
            rust_string_literal(name)
        ));
        self.line(&format!(
            "ctx.module_aliases.retain(|(module, _), _| module != {});",
            rust_string_literal(name)
        ));
        self.line("match __saved_type_bindings {");
        self.indent += 1;
        self.line(&format!(
            "::std::option::Option::Some(__bindings) => {{ ctx.module_type_bindings.insert({}.to_string(), __bindings); }}",
            rust_string_literal(name)
        ));
        self.line(&format!(
            "::std::option::Option::None => {{ ctx.module_type_bindings.remove({}); }}",
            rust_string_literal(name)
        ));
        self.indent -= 1;
        self.line("}");
        self.indent -= 1;
        self.line("}");
        self.line("__load_result");
        self.indent = 0;
        self.out.push_str("}\n\n");
        Ok(())
    }

    /// Emit every top-level `fn name(...) { ... }` as a module-scope
    /// Rust fn. Called before `run_program`; the top-level decls are
    /// subsequently skipped inside `emit_run_program` so they don't
    /// emit twice.
    fn emit_top_level_fn_decls(&mut self, stmts: &[Stmt]) -> Result<(), NyblError> {
        self.function_sites_by_stmt.clear();
        self.register_named_function_sites(stmts, true);
        self.emit_registered_function_bodies(stmts)
    }

    fn register_named_function_sites(&mut self, stmts: &[Stmt], direct: bool) {
        for stmt in stmts {
            match &stmt.kind {
                StmtKind::Let { value, .. } => {
                    self.register_named_function_sites_in_expr(value);
                }
                StmtKind::Assign { target, value, .. } => {
                    self.register_named_function_sites_in_target(target);
                    self.register_named_function_sites_in_expr(value);
                }
                StmtKind::FnDecl {
                    name,
                    params,
                    body,
                    visibility,
                } => {
                    let abi_eligible =
                        direct && self.current_module == nybl::value::ROOT_MODULE_PATH;
                    let site = self.register_function_site(
                        name,
                        params,
                        *visibility,
                        stmt.line,
                        abi_eligible,
                    );
                    self.function_sites_by_stmt
                        .insert(stmt as *const Stmt as usize, site);
                    if direct {
                        self.direct_function_sites
                            .entry(self.current_module.clone())
                            .or_default()
                            .push(site);
                    }
                    self.register_named_function_sites(body, false);
                }
                StmtKind::If {
                    condition,
                    body,
                    else_ifs,
                    else_body,
                } => {
                    self.register_named_function_sites_in_expr(condition);
                    self.register_named_function_sites(body, false);
                    for (condition, body) in else_ifs {
                        self.register_named_function_sites_in_expr(condition);
                        self.register_named_function_sites(body, false);
                    }
                    if let Some(body) = else_body {
                        self.register_named_function_sites(body, false);
                    }
                }
                StmtKind::While { condition, body } => {
                    self.register_named_function_sites_in_expr(condition);
                    self.register_named_function_sites(body, false);
                }
                StmtKind::Repeat { count, body } => {
                    self.register_named_function_sites_in_expr(count);
                    self.register_named_function_sites(body, false);
                }
                StmtKind::ForIn { iterable, body, .. } => {
                    self.register_named_function_sites_in_expr(iterable);
                    self.register_named_function_sites(body, false);
                }
                // Method bodies are cloned into the method-site registry. They
                // are registered from that stable slice immediately before
                // method emission so duplicate occurrences keep their IDs.
                StmtKind::MethodDecl { .. } => {}
                StmtKind::Return { value: Some(value) } | StmtKind::ExprStmt(value) => {
                    self.register_named_function_sites_in_expr(value);
                }
                _ => {}
            }
        }
    }

    fn register_named_function_sites_in_target(&mut self, target: &AssignTarget) {
        match target {
            AssignTarget::Variable(_) => {}
            AssignTarget::Index { object, index } => {
                self.register_named_function_sites_in_expr(object);
                self.register_named_function_sites_in_expr(index);
            }
            AssignTarget::Field { object, .. } => {
                self.register_named_function_sites_in_expr(object);
            }
        }
    }

    fn register_named_function_sites_in_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Int(_)
            | ExprKind::Number(_)
            | ExprKind::Str(_)
            | ExprKind::StringInterp(_)
            | ExprKind::Bool(_)
            | ExprKind::None
            | ExprKind::Ident(_) => {}
            ExprKind::BinaryOp { left, right, .. }
            | ExprKind::Index {
                object: left,
                index: right,
            } => {
                self.register_named_function_sites_in_expr(left);
                self.register_named_function_sites_in_expr(right);
            }
            ExprKind::UnaryOp { expr, .. }
            | ExprKind::Try(expr)
            | ExprKind::FieldAccess { object: expr, .. } => {
                self.register_named_function_sites_in_expr(expr);
            }
            ExprKind::Call { callee, args } => {
                self.register_named_function_sites_in_expr(callee);
                for arg in args {
                    self.register_named_function_sites_in_expr(arg);
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                self.register_named_function_sites_in_expr(object);
                for arg in args {
                    self.register_named_function_sites_in_expr(arg);
                }
            }
            ExprKind::StructConstruct { fields, .. } => {
                for (_, value) in fields {
                    self.register_named_function_sites_in_expr(value);
                }
            }
            ExprKind::EnumConstruct { payload, .. } => match payload {
                VariantPayload::Unit => {}
                VariantPayload::Tuple(values) => {
                    for value in values {
                        self.register_named_function_sites_in_expr(value);
                    }
                }
                VariantPayload::Struct(fields) => {
                    for (_, value) in fields {
                        self.register_named_function_sites_in_expr(value);
                    }
                }
            },
            ExprKind::Array(values) => {
                for value in values {
                    self.register_named_function_sites_in_expr(value);
                }
            }
            ExprKind::Dict(entries) => {
                for (_, value) in entries {
                    self.register_named_function_sites_in_expr(value);
                }
            }
            ExprKind::IfExpr {
                condition,
                then_expr,
                else_expr,
            } => {
                self.register_named_function_sites_in_expr(condition);
                self.register_named_function_sites_in_expr(then_expr);
                self.register_named_function_sites_in_expr(else_expr);
            }
            ExprKind::Lambda { body, .. } => {
                self.register_named_function_sites(body, false);
            }
            ExprKind::Match { scrutinee, arms } => {
                self.register_named_function_sites_in_expr(scrutinee);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.register_named_function_sites_in_expr(guard);
                    }
                    self.register_named_function_sites_in_expr(&arm.body);
                }
            }
        }
    }

    fn emit_registered_function_bodies(&mut self, stmts: &[Stmt]) -> Result<(), NyblError> {
        for stmt in stmts {
            match &stmt.kind {
                StmtKind::Let { value, .. } => {
                    self.emit_registered_function_bodies_in_expr(value)?;
                }
                StmtKind::Assign { target, value, .. } => {
                    self.emit_registered_function_bodies_in_target(target)?;
                    self.emit_registered_function_bodies_in_expr(value)?;
                }
                StmtKind::FnDecl {
                    name, params, body, ..
                } => {
                    let site = self.function_site_for_stmt(stmt);
                    self.emit_fn_decl_as_site(
                        &function_site_fn_name(site),
                        params,
                        body,
                        stmt.line,
                        Some((name, site)),
                    )?;
                    self.emit_registered_function_bodies(body)?;
                }
                StmtKind::If {
                    condition,
                    body,
                    else_ifs,
                    else_body,
                } => {
                    self.emit_registered_function_bodies_in_expr(condition)?;
                    self.emit_registered_function_bodies(body)?;
                    for (condition, body) in else_ifs {
                        self.emit_registered_function_bodies_in_expr(condition)?;
                        self.emit_registered_function_bodies(body)?;
                    }
                    if let Some(body) = else_body {
                        self.emit_registered_function_bodies(body)?;
                    }
                }
                StmtKind::While { condition, body } => {
                    self.emit_registered_function_bodies_in_expr(condition)?;
                    self.emit_registered_function_bodies(body)?;
                }
                StmtKind::Repeat { count, body } => {
                    self.emit_registered_function_bodies_in_expr(count)?;
                    self.emit_registered_function_bodies(body)?;
                }
                StmtKind::ForIn { iterable, body, .. } => {
                    self.emit_registered_function_bodies_in_expr(iterable)?;
                    self.emit_registered_function_bodies(body)?;
                }
                StmtKind::MethodDecl { .. } => {}
                StmtKind::Return { value: Some(value) } | StmtKind::ExprStmt(value) => {
                    self.emit_registered_function_bodies_in_expr(value)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn emit_registered_function_bodies_in_target(
        &mut self,
        target: &AssignTarget,
    ) -> Result<(), NyblError> {
        match target {
            AssignTarget::Variable(_) => {}
            AssignTarget::Index { object, index } => {
                self.emit_registered_function_bodies_in_expr(object)?;
                self.emit_registered_function_bodies_in_expr(index)?;
            }
            AssignTarget::Field { object, .. } => {
                self.emit_registered_function_bodies_in_expr(object)?;
            }
        }
        Ok(())
    }

    fn emit_registered_function_bodies_in_expr(&mut self, expr: &Expr) -> Result<(), NyblError> {
        match &expr.kind {
            ExprKind::Int(_)
            | ExprKind::Number(_)
            | ExprKind::Str(_)
            | ExprKind::StringInterp(_)
            | ExprKind::Bool(_)
            | ExprKind::None
            | ExprKind::Ident(_) => {}
            ExprKind::BinaryOp { left, right, .. }
            | ExprKind::Index {
                object: left,
                index: right,
            } => {
                self.emit_registered_function_bodies_in_expr(left)?;
                self.emit_registered_function_bodies_in_expr(right)?;
            }
            ExprKind::UnaryOp { expr, .. }
            | ExprKind::Try(expr)
            | ExprKind::FieldAccess { object: expr, .. } => {
                self.emit_registered_function_bodies_in_expr(expr)?;
            }
            ExprKind::Call { callee, args } => {
                self.emit_registered_function_bodies_in_expr(callee)?;
                for arg in args {
                    self.emit_registered_function_bodies_in_expr(arg)?;
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                self.emit_registered_function_bodies_in_expr(object)?;
                for arg in args {
                    self.emit_registered_function_bodies_in_expr(arg)?;
                }
            }
            ExprKind::StructConstruct { fields, .. } => {
                for (_, value) in fields {
                    self.emit_registered_function_bodies_in_expr(value)?;
                }
            }
            ExprKind::EnumConstruct { payload, .. } => match payload {
                VariantPayload::Unit => {}
                VariantPayload::Tuple(values) => {
                    for value in values {
                        self.emit_registered_function_bodies_in_expr(value)?;
                    }
                }
                VariantPayload::Struct(fields) => {
                    for (_, value) in fields {
                        self.emit_registered_function_bodies_in_expr(value)?;
                    }
                }
            },
            ExprKind::Array(values) => {
                for value in values {
                    self.emit_registered_function_bodies_in_expr(value)?;
                }
            }
            ExprKind::Dict(entries) => {
                for (_, value) in entries {
                    self.emit_registered_function_bodies_in_expr(value)?;
                }
            }
            ExprKind::IfExpr {
                condition,
                then_expr,
                else_expr,
            } => {
                self.emit_registered_function_bodies_in_expr(condition)?;
                self.emit_registered_function_bodies_in_expr(then_expr)?;
                self.emit_registered_function_bodies_in_expr(else_expr)?;
            }
            ExprKind::Lambda { body, .. } => {
                self.emit_registered_function_bodies(body)?;
            }
            ExprKind::Match { scrutinee, arms } => {
                self.emit_registered_function_bodies_in_expr(scrutinee)?;
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.emit_registered_function_bodies_in_expr(guard)?;
                    }
                    self.emit_registered_function_bodies_in_expr(&arm.body)?;
                }
            }
        }
        Ok(())
    }

    /// For each top-level user fn, emit a helper that resolves the declaration
    /// site activated by source execution and reifies it as a `Value::Fn`.
    /// Nested functions use the same site helper directly from identifier
    /// lowering; top-level wrappers are retained for module export assembly.
    fn emit_fn_value_wrappers(&mut self) {
        // Sort for deterministic output.
        let mut names: Vec<_> = self.fn_info.top_level_fns.iter().cloned().collect();
        names.sort();
        for name in names {
            writeln!(
                self.out,
                "fn {wrapper}(ctx: &mut Ctx<'_>, line: u32) -> Result<::nybl::value::Value, ::nybl::error::NyblError> {{",
                wrapper = self.wrapper_fn_name(&name)
            )
            .unwrap();
            writeln!(
                self.out,
                "    let site_id = __nybl_active_function_site(ctx, {module}, {name}, line)?;",
                module = rust_string_literal(&self.current_module),
                name = rust_string_literal(&name),
            )
            .unwrap();
            writeln!(
                self.out,
                "    __nybl_function_site_value(ctx, site_id, line)",
            )
            .unwrap();
            writeln!(self.out, "}}\n").unwrap();
        }
    }

    fn emit_method_sites(&mut self) -> Result<(), NyblError> {
        let sites = self.types.method_sites.clone();
        for site in &sites {
            let method_fn_name = method_site_fn_name(site.id);
            let saved_prefix =
                std::mem::replace(&mut self.module_prefix, site.module_prefix.clone());
            let saved_module_context = if site.module_path == nybl::value::ROOT_MODULE_PATH {
                None
            } else {
                let module_entry = self
                    .modules
                    .modules
                    .get(&site.module_path)
                    .expect("method's declaring module must be in the module graph")
                    .clone();
                let module_ast = module_entry.ast.clone();
                let saved_module =
                    std::mem::replace(&mut self.current_module, site.module_path.clone());
                let mut builtin_types = HashMap::new();
                for name in ["Result", "RuntimeError", "Iter"] {
                    builtin_types.insert(
                        name.to_string(),
                        nybl::value::BUILTIN_MODULE_PATH.to_string(),
                    );
                }
                let saved_type_bindings =
                    std::mem::replace(&mut self.type_bindings, vec![builtin_types]);
                let saved_module_aliases =
                    std::mem::replace(&mut self.module_aliases, vec![HashMap::new()]);
                let saved_declaration_aliases = std::mem::replace(
                    &mut self.declaration_aliases,
                    module_imports_from_exports(&module_entry.module_alias_candidates),
                );
                self.seed_types_for_module(&site.module_path);
                self.seed_uses(&module_ast);
                self.seed_module_alias_candidates(&module_entry.module_alias_candidates);
                Some((
                    saved_module,
                    saved_type_bindings,
                    saved_module_aliases,
                    saved_declaration_aliases,
                ))
            };
            let mut method_fn_info = collect_fn_info(&site.body);
            // The method's Rust symbol stays type-qualified, while calls in
            // its body resolve against the declaring module's functions.
            let module_fn_info = if site.module_path == nybl::value::ROOT_MODULE_PATH {
                self.fn_info.clone()
            } else {
                let module = self
                    .modules
                    .modules
                    .get(&site.module_path)
                    .expect("method's declaring module must be in the module graph");
                collect_fn_info(&module.ast)
            };
            method_fn_info.site_counts = module_fn_info.site_counts.clone();
            method_fn_info
                .names_with_ref_sites
                .extend(module_fn_info.names_with_ref_sites);
            method_fn_info
                .names_with_value_only_sites
                .extend(module_fn_info.names_with_value_only_sites);
            method_fn_info.all_fns.extend(module_fn_info.all_fns);
            method_fn_info
                .top_level_fns
                .extend(module_fn_info.top_level_fns);
            let saved_fn_info = std::mem::replace(&mut self.fn_info, method_fn_info);
            let saved_non_capturing_method = self.in_non_capturing_method;
            self.in_non_capturing_method = true;
            self.register_named_function_sites(&site.body, false);
            self.emit_registered_function_bodies(&site.body)?;
            self.emit_fn_decl_as(&method_fn_name, &site.params, &site.body, site.line)?;
            self.in_non_capturing_method = saved_non_capturing_method;
            self.emit_method_adapter(site);
            self.fn_info = saved_fn_info;
            self.module_prefix = saved_prefix;
            if let Some((module, type_bindings, module_aliases, declaration_aliases)) =
                saved_module_context
            {
                self.current_module = module;
                self.type_bindings = type_bindings;
                self.module_aliases = module_aliases;
                self.declaration_aliases = declaration_aliases;
            }
        }
        Ok(())
    }

    fn emit_method_adapter(&mut self, site: &MethodSite) {
        let adapter = method_site_adapter_name(site.id);
        let body = method_site_fn_name(site.id);
        let arity = site.params.len();
        let non_receiver_modes = site
            .params
            .iter()
            .skip(1)
            .map(|param| match param.mode {
                ParamMode::Value => "::nybl::parser::ParamMode::Value",
                ParamMode::Ref => "::nybl::parser::ParamMode::Ref",
                ParamMode::Rest => "::nybl::parser::ParamMode::Rest",
            })
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            self.out,
            "fn {adapter}(ctx: &mut Ctx<'_>, obj: &::nybl::value::Value, args: &[::nybl::value::Value], line: u32) -> Result<::nybl::value::Value, ::nybl::error::NyblError> {{"
        )
        .unwrap();
        writeln!(self.out, "    let __modes = &[{non_receiver_modes}]; if !::nybl::ref_params::accepts_arity(__modes, args.len()) {{ return Err(__nybl_arity_error({}, __modes, args.len(), line)); }} let args = __nybl_pack_rest_args(args.to_vec(), __modes, line, &ctx.memory)?; let _ = &args;", rust_string_literal(&format!("{}.{}", site.type_name, site.method_name))).unwrap();
        if arity == 0 {
            writeln!(self.out, "    {body}(ctx)").unwrap();
        } else if site.params[0].mode == ParamMode::Ref {
            writeln!(
                self.out,
                "    let _ = (ctx, obj, args); let mut error = ::nybl::error::NyblError::runtime(\"a `ref` method receiver must be a variable\", line); error.friendly_hint = Some(\"Store the value in a `let` binding before calling this method.\".to_string()); Err(error)"
            )
            .unwrap();
        } else {
            for index in 0..arity.saturating_sub(1) {
                writeln!(self.out, "    let mut __a{index} = args[{index}].clone();").unwrap();
            }
            let args = site
                .params
                .iter()
                .skip(1)
                .enumerate()
                .map(|(index, param)| {
                    if param.mode == ParamMode::Ref {
                        format!(", &mut __a{index}")
                    } else {
                        format!(", __a{index}")
                    }
                })
                .collect::<String>();
            writeln!(self.out, "    {body}(ctx, obj.clone(){args})").unwrap();
        }
        self.out.push_str("}\n\n");

        let outcome_adapter = method_site_outcome_adapter_name(site.id);
        writeln!(
            self.out,
            "fn {outcome_adapter}(ctx: &mut Ctx<'_>, mut obj: ::nybl::value::Value, mut args: ::std::vec::Vec<::nybl::value::Value>, line: u32) -> Result<__NyblMethodOutcome, ::nybl::error::NyblError> {{"
        )
        .unwrap();
        writeln!(self.out, "    let __modes = &[{non_receiver_modes}]; if !::nybl::ref_params::accepts_arity(__modes, args.len()) {{ return Err(__nybl_arity_error({}, __modes, args.len(), line)); }} args = __nybl_pack_rest_args(args, __modes, line, &ctx.memory)?; let _ = &args;", rust_string_literal(&format!("{}.{}", site.type_name, site.method_name))).unwrap();
        for index in 0..arity.saturating_sub(1) {
            writeln!(self.out, "    let mut __a{index} = args.remove(0);").unwrap();
        }
        let call_args = site
            .params
            .iter()
            .skip(1)
            .enumerate()
            .map(|(index, param)| {
                if param.mode == ParamMode::Ref {
                    format!(", &mut __a{index}")
                } else {
                    format!(", __a{index}.clone()")
                }
            })
            .collect::<String>();
        let final_args = (0..arity.saturating_sub(1))
            .map(|index| format!("__a{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        if arity == 0 {
            writeln!(
                self.out,
                "    let value = {body}(ctx)?;\n    Ok(__NyblMethodOutcome {{ value, receiver: obj, args: ::std::vec::Vec::new() }})"
            )
            .unwrap();
        } else {
            let receiver = if site.params[0].mode == ParamMode::Ref {
                "&mut obj"
            } else {
                "obj.clone()"
            };
            writeln!(
                self.out,
                "    let value = {body}(ctx, {receiver}{call_args})?;\n    Ok(__NyblMethodOutcome {{ value, receiver: obj, args: vec![{final_args}] }})"
            )
            .unwrap();
        }
        self.out.push_str("}\n\n");
    }

    /// Emit a runtime dispatcher that maps `(type_name,
    /// method_name)` pairs to their compiled user-method Rust fns.
    /// The method-call emitter calls this first; on `None` it
    /// falls back to built-in method dispatch.
    fn emit_method_dispatcher(&mut self) {
        self.out.push_str("\nfn __nybl_try_user_method(\n");
        self.out.push_str(
            "    ctx: &mut Ctx<'_>,\n    obj: &::nybl::value::Value,\n    method: &str,\n    args: &[::nybl::value::Value],\n    line: u32,\n) -> Result<::std::option::Option<::nybl::value::Value>, ::nybl::error::NyblError> {\n",
        );
        if self.types.method_slots.is_empty() {
            self.out.push_str(
                "    let _ = (ctx, obj, method, args, line);\n    Ok(None)\n}\n\nfn __nybl_call_preflighted_user_method(ctx: &mut Ctx<'_>, adapter: __NyblMethodFn, obj: &::nybl::value::Value, args: &[::nybl::value::Value], line: u32) -> Result<::nybl::value::Value, ::nybl::error::NyblError> { adapter(ctx, obj, args, line) }\n\n",
            );
            return;
        }
        if self.opts.sandbox {
            self.out.push_str(
                "    fn invoke(\n        ctx: &mut Ctx<'_>,\n        adapter: __NyblMethodFn,\n        obj: &::nybl::value::Value,\n        args: &[::nybl::value::Value],\n        line: u32,\n    ) -> Result<::nybl::value::Value, ::nybl::error::NyblError> {\n        __nybl_enter_aot_call(ctx, line)?;\n        let result = adapter(ctx, obj, args, line);\n        __nybl_leave_aot_call(ctx);\n        result\n    }\n",
            );
        }
        self.out
            .push_str("    let type_key: ::std::option::Option<(&str, &str)> = match obj {\n");
        self.out.push_str(
            "        ::nybl::value::Value::Struct(s) => Some((s.module_path(), s.type_name())),\n",
        );
        self.out.push_str(
            "        ::nybl::value::Value::EnumVariant(e) => Some((e.module_path(), e.type_name())),\n",
        );
        self.out.push_str("        _ => None,\n    };\n");
        self.out.push_str(
            "    let (type_mp, type_name) = match type_key { Some(k) => k, None => return Ok(None) };\n",
        );
        self.out
            .push_str("    match (type_mp, type_name, method) {\n");

        for (slot, (module_path, type_name, method_name)) in
            self.types.method_slots.iter().enumerate()
        {
            let invoke = if self.opts.sandbox {
                "invoke(ctx, adapter, obj, args, line)?"
            } else {
                "adapter(ctx, obj, args, line)?"
            };
            writeln!(
                self.out,
                "        ({mp_lit}, {type_lit}, {method_lit}) => match ctx.method_slots[{slot}] {{ Some(adapter) => Ok(Some({invoke})), None => Ok(None) }},",
                mp_lit = rust_string_literal(module_path),
                type_lit = rust_string_literal(type_name),
                method_lit = rust_string_literal(method_name),
            )
            .unwrap();
        }

        self.out.push_str("        _ => Ok(None),\n    }\n}\n\n");
        if self.opts.sandbox {
            self.out.push_str(
                "fn __nybl_call_preflighted_user_method(ctx: &mut Ctx<'_>, adapter: __NyblMethodFn, obj: &::nybl::value::Value, args: &[::nybl::value::Value], line: u32) -> Result<::nybl::value::Value, ::nybl::error::NyblError> { __nybl_enter_aot_call(ctx, line)?; let result = adapter(ctx, obj, args, line); __nybl_leave_aot_call(ctx); result }\n\n",
            );
        } else {
            self.out.push_str(
                "fn __nybl_call_preflighted_user_method(ctx: &mut Ctx<'_>, adapter: __NyblMethodFn, obj: &::nybl::value::Value, args: &[::nybl::value::Value], line: u32) -> Result<::nybl::value::Value, ::nybl::error::NyblError> { adapter(ctx, obj, args, line) }\n\n",
            );
        }
    }

    fn emit_method_ref_dispatcher(&mut self) {
        self.out.push_str(
            "fn __nybl_user_method_type_key(obj: &::nybl::value::Value) -> ::std::option::Option<(&str, &str)> {\n    match obj {\n        ::nybl::value::Value::Struct(value) => Some((value.module_path(), value.type_name())),\n        ::nybl::value::Value::EnumVariant(value) => Some((value.module_path(), value.type_name())),\n        _ => None,\n    }\n}\n\n",
        );
        self.out.push_str(
            "fn __nybl_preflight_user_method(\n    ctx: &Ctx<'_>,\n    obj: &::nybl::value::Value,\n    method: &str,\n    actual_modes: &[::nybl::parser::ParamMode],\n    receiver_is_place: bool,\n    line: u32,\n) -> Result<::std::option::Option<__NyblMethodFn>, ::nybl::error::NyblError> {\n    let Some((module_path, type_name)) = __nybl_user_method_type_key(obj) else { return Ok(None); };\n    match (module_path, type_name, method) {\n",
        );
        for (slot, (module_path, type_name, method_name)) in
            self.types.method_slots.iter().enumerate()
        {
            let sites = self
                .types
                .method_sites
                .iter()
                .filter(|site| {
                    site.module_path == *module_path
                        && site.type_name == *type_name
                        && site.method_name == *method_name
                })
                .collect::<Vec<_>>();
            writeln!(
                self.out,
                "        ({module}, {ty}, {method}) => match ctx.method_slots[{slot}] {{",
                module = rust_string_literal(module_path),
                ty = rust_string_literal(type_name),
                method = rust_string_literal(method_name),
            )
            .unwrap();
            for site in sites {
                let modes = site
                    .params
                    .iter()
                    .skip(1)
                    .map(|param| match param.mode {
                        ParamMode::Value => "::nybl::parser::ParamMode::Value",
                        ParamMode::Ref => "::nybl::parser::ParamMode::Ref",
                        ParamMode::Rest => "::nybl::parser::ParamMode::Rest",
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let receiver_check = if site
                    .params
                    .first()
                    .is_some_and(|param| param.mode == ParamMode::Ref)
                {
                    "if !receiver_is_place { let mut error = ::nybl::error::NyblError::runtime(\"a `ref` method receiver must be a variable\", line); error.friendly_hint = Some(\"Store the value in a `let` binding before calling this method.\".to_string()); return Err(error); } "
                } else {
                    ""
                };
                let full_modes = site
                    .params
                    .iter()
                    .map(|param| match param.mode {
                        ParamMode::Value => "::nybl::parser::ParamMode::Value",
                        ParamMode::Ref => "::nybl::parser::ParamMode::Ref",
                        ParamMode::Rest => "::nybl::parser::ParamMode::Rest",
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    self.out,
                    "            Some(adapter) if adapter as usize == {adapter} as *const () as usize => {{ {receiver_check}let __full_modes = &[{full_modes}]; if !::nybl::ref_params::accepts_arity(__full_modes, actual_modes.len() + 1) {{ let required = ::nybl::ref_params::required_arity(__full_modes); let message = if matches!(__full_modes.last(), Some(::nybl::parser::ParamMode::Rest)) {{ format!(\"`{{}}.{{}}` expects at least {{}} arguments (including `self`), but got {{}}\", {type_name}, {method_name}, required, actual_modes.len() + 1) }} else {{ format!(\"`{{}}.{{}}` expects {{}} argument{{}} (including `self`), but got {{}}\", {type_name}, {method_name}, required, if required == 1 {{ \"\" }} else {{ \"s\" }}, actual_modes.len() + 1) }}; return Err(::nybl::error::NyblError::runtime(message, line)); }} ::nybl::validate_call_modes({label}, &[{modes}], actual_modes, line)?; Ok(Some(adapter)) }},",
                    adapter = method_site_adapter_name(site.id),
                    label = rust_string_literal(&format!("{type_name}.{method_name}")),
                    type_name = rust_string_literal(type_name),
                    method_name = rust_string_literal(method_name),
                )
                .unwrap();
            }
            self.out
                .push_str("            _ => Ok(None),\n        },\n");
        }
        self.out.push_str("        _ => Ok(None),\n    }\n}\n\n");

        self.out.push_str(
            "fn __nybl_user_method_receiver_is_ref(adapter: __NyblMethodFn) -> bool {\n    match adapter as usize {\n",
        );
        for site in &self.types.method_sites {
            let is_ref = site
                .params
                .first()
                .is_some_and(|param| param.mode == ParamMode::Ref);
            writeln!(
                self.out,
                "        value if value == {adapter} as *const () as usize => {is_ref},",
                adapter = method_site_adapter_name(site.id),
            )
            .unwrap();
        }
        self.out.push_str("        _ => false,\n    }\n}\n\n");

        self.out.push_str(
            "fn __nybl_call_preflighted_user_method_outcome(\n    ctx: &mut Ctx<'_>,\n    adapter: __NyblMethodFn,\n    obj: ::nybl::value::Value,\n    args: ::std::vec::Vec<::nybl::value::Value>,\n    line: u32,\n) -> Result<__NyblMethodOutcome, ::nybl::error::NyblError> {\n    match adapter as usize {\n",
        );
        for site in &self.types.method_sites {
            let invoke = if self.opts.sandbox {
                format!(
                    "{{ __nybl_enter_aot_call(ctx, line)?; let result = {}(ctx, obj, args, line); __nybl_leave_aot_call(ctx); result }}",
                    method_site_outcome_adapter_name(site.id)
                )
            } else {
                format!(
                    "{}(ctx, obj, args, line)",
                    method_site_outcome_adapter_name(site.id)
                )
            };
            writeln!(
                self.out,
                "        value if value == {adapter} as *const () as usize => {invoke},",
                adapter = method_site_adapter_name(site.id),
            )
            .unwrap();
        }
        self.out.push_str(
            "        _ => unreachable!(\"preflighted user-method adapter has an outcome adapter\"),\n    }\n}\n\n",
        );
    }

    // ─── Preamble / footer ────────────────────────────────────────

    fn emit_header(&mut self) {
        self.out.push_str(HEADER);
    }

    /// Emit the runtime preamble by stitching together the
    /// variant-specific `Ctx` struct, the shared helpers, and
    /// — when sandbox mode is on — the `__nybl_tick` step /
    /// memory / on_tick checkpoint fn. Keeping the helpers in
    /// one place means every new runtime helper lands once;
    /// prior to this refactor two near-identical preamble
    /// strings drifted every time a new helper was added.
    fn emit_runtime_preamble(&mut self) {
        self.out.push_str(RUNTIME_HEADER);
        let ctx_template = if self.opts.sandbox {
            CTX_SANDBOX
        } else {
            CTX_BASE
        };
        self.out.push_str("type __NyblMethodFn = for<'a> fn(&mut Ctx<'a>, &::nybl::value::Value, &[::nybl::value::Value], u32) -> Result<::nybl::value::Value, ::nybl::error::NyblError>;\n\n");
        let method_field = if self.types.method_slots.is_empty() {
            String::new()
        } else {
            let visibility = if self.opts.sandbox { "" } else { "pub " };
            format!(
                "    {visibility}method_slots: [::std::option::Option<__NyblMethodFn>; {}],\n",
                self.types.method_slots.len(),
            )
        };
        self.out
            .push_str(&ctx_template.replace("/*__NYBL_METHOD_SLOTS__*/", &method_field));
        let shared_visibility = if self.opts.sandbox { "" } else { "pub " };
        let shared = RUNTIME_SHARED
            .replace("/*__NYBL_RUNTIME_VIS__*/", shared_visibility)
            .replace(
                "/*__NYBL_MEMORY_HELPER__*/",
                if self.opts.sandbox {
                    r#"fn __nybl_check_memory(
    ctx: &Ctx<'_>,
    line: u32,
) -> Result<(), ::nybl::error::NyblError> {
    if ctx.memory.__exceeded() {
        Err(::nybl::builtins::error_fatal_with_hint(
            line,
            "Memory limit exceeded",
            "Your code is using too much memory. Check for large strings or arrays growing in loops.",
        ))
    } else {
        Ok(())
    }
}

"#
                } else {
                    ""
                },
            )
            .replace(
                "/*__NYBL_MEMORY_CHECK__*/",
                if self.opts.sandbox {
                    "__nybl_check_memory(ctx, line)?;"
                } else {
                    ""
                },
            )
            .replace(
                "/*__NYBL_TRY_MEMORY_CHECK__*/",
                if self.opts.sandbox {
                    r#"    if wrapped.is_ok() && ctx.memory.__exceeded() {
        Err(::nybl::builtins::error_fatal_with_hint(
            line,
            "Memory limit exceeded",
            "Your code is using too much memory. Check for large strings or arrays growing in loops.",
        ))
    } else {
        wrapped
    }"#
                } else {
                    "    wrapped"
                },
            )
            .replace(
                "/*__NYBL_INVOKE_HELPER__*/",
                if self.opts.sandbox {
                    r#"fn __nybl_invoke_aot_callable(
    ctx: &mut Ctx<'_>,
    callable: ::std::rc::Rc<
        dyn for<'__a> Fn(
            &mut Ctx<'__a>,
            ::std::vec::Vec<::nybl::value::Value>,
        ) -> Result<__NyblCallOutcome, ::nybl::error::NyblError>,
    >,
    args: ::std::vec::Vec<::nybl::value::Value>,
    line: u32,
) -> Result<__NyblCallOutcome, ::nybl::error::NyblError> {
    __nybl_enter_aot_call(ctx, line)?;
    let result = callable(ctx, args);
    __nybl_leave_aot_call(ctx);
    let outcome = result?;
    __nybl_check_memory(ctx, line)?;
    Ok(outcome)
}

"#
                } else {
                    ""
                },
            )
            .replace(
                "/*__NYBL_WRAP_CTX_PARAM__*/",
                if self.opts.sandbox { "    ctx: &Ctx<'_>,\n" } else { "" },
            )
            .replace(
                "/*__NYBL_WRAP_CONSTRUCTOR__*/",
                if self.opts.sandbox {
                    "::nybl::value::NyblFn::try_new_compiled_in_module_with_origin_and_modes(\n        params,\n        param_modes,\n        captures,\n        body,\n        self_name,\n        ::std::option::Option::None,\n        ctx.function_origin.clone(),\n        opaque_body_depth,\n        line,\n    ).map(::nybl::value::Value::Fn)"
                } else {
                    "::nybl::value::NyblFn::try_new_compiled_in_module_with_origin_and_modes(\n        params,\n        param_modes,\n        captures,\n        body,\n        self_name,\n        ::std::option::Option::None,\n        ::nybl::value::NyblFnOrigin::__instance(\"aot\"),\n        opaque_body_depth,\n        line,\n    ).map(::nybl::value::Value::Fn)"
                },
            )
            .replace(
                "/*__NYBL_VALIDATE_AOT_ORIGIN__*/",
                if self.opts.sandbox {
                    "                __nybl_validate_aot_function(ctx, f, line)?;\n"
                } else {
                    ""
                },
            )
            .replace(
                "/*__NYBL_PREFLIGHT_AOT_ORIGIN__*/",
                if self.opts.sandbox {
                    "    __nybl_validate_aot_function(ctx, function, line)?;\n"
                } else {
                    "    let _ = ctx;\n"
                },
            )
            .replace(
                "/*__NYBL_VALIDATE_AOT_ARITY__*/",
                if self.opts.sandbox {
                    r#"            if !::nybl::ref_params::accepts_arity(&f.param_modes, args.len()) {
                let callable = f
                    .self_name
                    .as_ref()
                    .map_or_else(|| "lambda".to_string(), |name| format!("`{}`", name));
                return Err(__nybl_arity_error(&callable, &f.param_modes, args.len(), line));
            }
"#
                } else {
                    ""
                },
            )
            .replace(
                "/*__NYBL_VALIDATE_AOT_FUNC_ORIGIN__*/",
                if self.opts.sandbox {
                    "    __nybl_validate_aot_function(ctx, &func, line)?;\n"
                } else {
                    ""
                },
            )
            .replace(
                "/*__NYBL_CALL_DEPTH_ENTER__*/",
                if self.opts.sandbox {
                    "    __nybl_enter_aot_call(ctx, line)?;\n"
                } else {
                    ""
                },
            )
            .replace(
                "/*__NYBL_CALL_DEPTH_LEAVE__*/",
                if self.opts.sandbox {
                    "    __nybl_leave_aot_call(ctx);\n"
                } else {
                    ""
                },
            )
            .replace(
                "/*__NYBL_CALL_VALUE_INVOKE__*/",
                if self.opts.sandbox {
                    "__nybl_invoke_aot_callable(ctx, callable, args, line)"
                } else {
                    "callable(ctx, args)"
                },
            )
            .replace(
                "/*__NYBL_TRY_ATTEMPT_START__*/",
                if self.opts.sandbox {
                    "    let attempted = (|| -> Result<\n        __NyblCallOutcome,\n        ::nybl::error::NyblError,\n    > {\n"
                } else {
                    ""
                },
            )
            .replace(
                "/*__NYBL_TRY_ATTEMPT_INVOKE__*/",
                if self.opts.sandbox {
                    "        __nybl_invoke_aot_callable(ctx, callable_fn, ::std::vec::Vec::new(), line)\n"
                } else {
                    "    let attempted = callable_fn(ctx, ::std::vec::Vec::new());\n"
                },
            )
            .replace(
                "/*__NYBL_TRY_ATTEMPT_END__*/",
                if self.opts.sandbox { "    })();\n" } else { "" },
            )
            .replace(
                "/*__NYBL_RESULT_INVOKE__*/",
                if self.opts.sandbox {
                    "    let result = __nybl_invoke_aot_callable(ctx, callable_fn, ::std::vec![payload], line)?;\n"
                } else {
                    "    let result = callable_fn(ctx, ::std::vec![payload])?;\n"
                },
            );
        self.out.push_str(&shared);
        if self.opts.sandbox {
            self.out.push_str(TICK_HELPER);
        }
    }

    fn emit_function_site_catalogue(&mut self) {
        self.out
            .push_str("const __NYBL_FUNCTION_SITES: &[__NyblFunctionSite] = &[\n");
        for site in &self.functions.sites {
            let params = site
                .params
                .iter()
                .map(|param| rust_string_literal(&param.name))
                .collect::<Vec<_>>()
                .join(", ");
            let param_modes = site
                .params
                .iter()
                .map(|param| match param.mode {
                    ParamMode::Value => "::nybl::parser::ParamMode::Value",
                    ParamMode::Ref => "::nybl::parser::ParamMode::Ref",
                    ParamMode::Rest => "::nybl::parser::ParamMode::Rest",
                })
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(
                self.out,
                "    __NyblFunctionSite {{ id: {id}, module_path: {module}, name: {name}, params: &[{params}], param_modes: &[{param_modes}], is_public: {is_public}, abi_eligible: {abi_eligible}, line: {line} }},",
                id = site.id,
                module = rust_string_literal(&site.module_path),
                name = rust_string_literal(&site.name),
                is_public = site.visibility == Visibility::Public,
                abi_eligible = site.abi_eligible,
                line = site.line,
            )
            .unwrap();
        }
        self.out.push_str("];\n\n");
    }

    fn emit_function_site_dispatcher(&mut self) {
        let runtime = r#"fn __nybl_authoritative_alias_namespace(
    ctx: &Ctx<'_>,
    module_path: &str,
    alias: &str,
    line: u32,
) -> Result<::nybl::value::Value, ::nybl::error::NyblError> {
    __nybl_binding_value(ctx, module_path, alias)
        .ok_or_else(|| ::nybl::error::NyblError::runtime(
            format!("`{}` isn't a module alias in scope", alias),
            line,
        ))
}

fn __nybl_activate_function(ctx: &mut Ctx<'_>, site_id: usize) {
    let site = &__NYBL_FUNCTION_SITES[site_id];
/*__NYBL_MARK_REACHED_FUNCTION_SITE__*/
    ctx.active_function_sites.insert(
        (site.module_path.to_string(), site.name.to_string()),
        site_id,
    );
    if site.abi_eligible {
        ctx.abi_declarations.retain(|existing| {
            __NYBL_FUNCTION_SITES[*existing].name != site.name
        });
        ctx.abi_declarations.push(site_id);
    }
}

fn __nybl_active_function_site(
    ctx: &Ctx<'_>,
    module_path: &str,
    name: &str,
    line: u32,
) -> Result<usize, ::nybl::error::NyblError> {
    ctx.active_function_sites
        .get(&(module_path.to_string(), name.to_string()))
        .or_else(|| ctx.module_imported_function_sites.get(&(module_path.to_string(), name.to_string())))
        .copied()
        .ok_or_else(|| ::nybl::error::NyblError::runtime(
            ::nybl::error_messages::function_not_found(name),
            line,
        ))
}

fn __nybl_preflight_function_site_call(
    site_id: usize,
    actual_modes: &[::nybl::parser::ParamMode],
    line: u32,
) -> Result<(), ::nybl::error::NyblError> {
    let site = &__NYBL_FUNCTION_SITES[site_id];
    if !::nybl::ref_params::accepts_arity(site.param_modes, actual_modes.len()) {
        return Err(__nybl_arity_error(site.name, site.param_modes, actual_modes.len(), line));
    }
    ::nybl::validate_call_modes(site.name, site.param_modes, actual_modes, line)
}

fn __nybl_preflight_active_function_call(
    ctx: &Ctx<'_>,
    module_path: &str,
    name: &str,
    actual_modes: &[::nybl::parser::ParamMode],
    line: u32,
) -> Result<(), ::nybl::error::NyblError> {
    if __nybl_binding_value(ctx, module_path, name).is_some() {
        return Ok(());
    }
    if let ::std::option::Option::Some(site_id) = ctx
        .active_function_sites
        .get(&(module_path.to_string(), name.to_string()))
        .or_else(|| ctx.module_imported_function_sites.get(&(module_path.to_string(), name.to_string())))
        .copied()
    {
        __nybl_preflight_function_site_call(site_id, actual_modes, line)?;
    }
    Ok(())
}

fn __nybl_active_ref_function_site(
    ctx: &Ctx<'_>,
    module_path: &str,
    name: &str,
) -> ::std::option::Option<usize> {
    if __nybl_binding_value(ctx, module_path, name).is_some() {
        return None;
    }
    ctx.active_function_sites
        .get(&(module_path.to_string(), name.to_string()))
        .or_else(|| ctx.module_imported_function_sites.get(&(module_path.to_string(), name.to_string())))
        .copied()
        .filter(|site_id| {
            __NYBL_FUNCTION_SITES[*site_id]
                .param_modes
                .iter()
                .any(|mode| *mode == ::nybl::parser::ParamMode::Ref)
        })
}

fn __nybl_import_function_site(
    ctx: &mut Ctx<'_>,
    module_path: &str,
    name: &str,
    site_id: ::std::option::Option<usize>,
    line: u32,
) -> Result<(), ::nybl::error::NyblError> {
    if let ::std::option::Option::Some(site_id) = site_id {
        let key = (module_path.to_string(), name.to_string());
        if !__nybl_has_binding(ctx, module_path, name)
            && !ctx.active_function_sites.contains_key(&key)
            && !ctx.module_imported_function_sites.contains_key(&key)
        {
            let value = __nybl_function_site_value(ctx, site_id, line)?;
            __nybl_import_binding_value(ctx, module_path, name, value);
            ctx.module_imported_function_sites.insert(key, site_id);
        }
    }
    Ok(())
}

fn __nybl_import_export(
    ctx: &mut Ctx<'_>,
    module_path: &str,
    name: &str,
    origin_module_path: &str,
    origin_name: &str,
    export: ::std::option::Option<__NyblExport>,
    warn_on_shadow: bool,
    line: u32,
) -> Result<(), ::nybl::error::NyblError> {
    let key = (module_path.to_string(), name.to_string());
    let shadows_existing = __nybl_has_binding(ctx, module_path, name)
        || ctx.active_function_sites.contains_key(&key)
        || ctx.module_imported_function_sites.contains_key(&key);
    if export.is_some() && shadows_existing {
        if warn_on_shadow {
            __nybl_warn_glob_shadow(name, origin_module_path);
        }
        return Ok(());
    }
    match export {
        ::std::option::Option::Some(__NyblExport::Value(_)) => {
            __nybl_import_binding_alias(
                ctx,
                module_path,
                name,
                origin_module_path,
                origin_name,
            );
        }
        ::std::option::Option::Some(__NyblExport::Function(site_id)) => {
            let value = __nybl_function_site_value(ctx, site_id, line)?;
            __nybl_import_binding_value(ctx, module_path, name, value);
            ctx.module_imported_function_sites.insert(key, site_id);
        }
        ::std::option::Option::None => {}
    }
    Ok(())
}

fn __nybl_export_function_site(
    export: &::std::option::Option<__NyblExport>,
) -> ::std::option::Option<usize> {
    match export {
        ::std::option::Option::Some(__NyblExport::Function(site_id)) => Some(*site_id),
        _ => None,
    }
}

fn __nybl_export_value(
    ctx: &mut Ctx<'_>,
    export: ::std::option::Option<__NyblExport>,
    line: u32,
) -> Result<::std::option::Option<::nybl::value::Value>, ::nybl::error::NyblError> {
    match export {
        ::std::option::Option::Some(__NyblExport::Value(value)) => Ok(Some(value)),
        ::std::option::Option::Some(__NyblExport::Function(site_id)) => {
            __nybl_function_site_value(ctx, site_id, line).map(Some)
        }
        ::std::option::Option::None => Ok(None),
    }
}

fn __nybl_call_active_function(
    ctx: &mut Ctx<'_>,
    module_path: &str,
    name: &str,
    args: ::std::vec::Vec<::nybl::value::Value>,
    line: u32,
) -> Result<::nybl::value::Value, ::nybl::error::NyblError> {
    if let ::std::option::Option::Some(value) =
        __nybl_binding_value(ctx, module_path, name)
    {
        return __nybl_call_named_value(ctx, value, args, name, line);
    }
    let site_id = __nybl_active_function_site(ctx, module_path, name, line)?;
    __nybl_call_function_site(ctx, site_id, args, line)
}

fn __nybl_active_function_value(
    ctx: &mut Ctx<'_>,
    module_path: &str,
    name: &str,
    line: u32,
) -> Result<::nybl::value::Value, ::nybl::error::NyblError> {
    if let ::std::option::Option::Some(value) =
        __nybl_binding_value(ctx, module_path, name)
    {
        return Ok(value);
    }
    let site_id = ctx.active_function_sites
        .get(&(module_path.to_string(), name.to_string()))
        .or_else(|| ctx.module_imported_function_sites.get(&(module_path.to_string(), name.to_string())))
        .copied()
        .ok_or_else(|| ::nybl::error::NyblError::runtime(
            ::nybl::error_messages::variable_not_found(name),
            line,
        ))?;
    __nybl_function_site_value(ctx, site_id, line)
}

fn __nybl_function_site_value(
    ctx: &mut Ctx<'_>,
    site_id: usize,
    line: u32,
) -> Result<::nybl::value::Value, ::nybl::error::NyblError> {
    let site = &__NYBL_FUNCTION_SITES[site_id];
    let params = site.params.iter().map(|param| (*param).to_string()).collect();
    let param_modes = site.param_modes.to_vec();
    let callable: ::std::rc::Rc<
        dyn for<'__a> Fn(
            &mut Ctx<'__a>,
            ::std::vec::Vec<::nybl::value::Value>,
        ) -> Result<__NyblCallOutcome, ::nybl::error::NyblError>,
    > = ::std::rc::Rc::new(move |ctx, args| {
        __nybl_call_function_site_inner(ctx, site_id, args, 0)
    });
    __nybl_wrap_callable(
/*__NYBL_FUNCTION_SITE_WRAP_CTX__*/
        params,
        param_modes,
        ::std::vec::Vec::new(),
        Some(site.name.to_string()),
        0u16,
        line,
        callable,
    )
}

/*__NYBL_INSTANCE_ENTRY_POINTS__*/
"#;
        self.out.push_str(
            &runtime
                .replace(
                    "/*__NYBL_FUNCTION_SITE_WRAP_CTX__*/",
                    if self.opts.sandbox { "        ctx,\n" } else { "" },
                )
                .replace(
                    "/*__NYBL_MARK_REACHED_FUNCTION_SITE__*/",
                    if self.opts.sandbox {
                        ""
                    } else {
                        "    ctx.reached_function_sites[site_id] = true;\n"
                    },
                )
                .replace(
                    "/*__NYBL_INSTANCE_ENTRY_POINTS__*/",
                    if self.opts.sandbox {
                        r#"fn __nybl_instance_entry_points(state: &__NyblState) -> ::std::vec::Vec<::nybl::EntryPoint> {
    state
        .abi_declarations
        .iter()
        .filter_map(|site_id| {
            let site = &__NYBL_FUNCTION_SITES[*site_id];
            site.is_public.then(|| {
                let required = ::nybl::ref_params::required_arity(site.param_modes);
                if matches!(site.param_modes.last(), Some(::nybl::parser::ParamMode::Rest)) {
                    ::nybl::EntryPoint::__new_variadic(site.name.to_string(), required)
                } else {
                    ::nybl::EntryPoint::__new(site.name.to_string(), required)
                }
            })
        })
        .collect()
}

"#
                    } else {
                        ""
                    },
                ),
        );
        for site in &self.functions.sites {
            let params = site
                .params
                .iter()
                .enumerate()
                .map(|(index, param)| {
                    if param.mode == ParamMode::Ref {
                        format!("__nybl_param_{index}: &mut ::nybl::value::Value")
                    } else {
                        format!("__nybl_param_{index}: ::nybl::value::Value")
                    }
                })
                .collect::<Vec<_>>();
            let signature_args = if params.is_empty() {
                String::new()
            } else {
                format!(", {}", params.join(", "))
            };
            let call_args = site
                .params
                .iter()
                .enumerate()
                .map(|(index, _)| format!(", __nybl_param_{index}"))
                .collect::<String>();
            if self.opts.sandbox {
                writeln!(
                    self.out,
                    "fn {guarded}(ctx: &mut Ctx<'_>{signature_args}, line: u32) -> Result<::nybl::value::Value, ::nybl::error::NyblError> {{\n    __nybl_enter_aot_call(ctx, line)?;\n    let result = {body}(ctx{call_args});\n    __nybl_leave_aot_call(ctx);\n    result\n}}\n",
                    guarded = guarded_function_site_fn_name(site.id),
                    body = function_site_fn_name(site.id),
                )
                .unwrap();
            } else {
                writeln!(
                    self.out,
                    "fn {guarded}(ctx: &mut Ctx<'_>{signature_args}, _line: u32) -> Result<::nybl::value::Value, ::nybl::error::NyblError> {{\n    {body}(ctx{call_args})\n}}\n",
                    guarded = guarded_function_site_fn_name(site.id),
                    body = function_site_fn_name(site.id),
                )
                .unwrap();
            }
        }
        self.out.push_str(
            "fn __nybl_call_function_site_inner(\n    ctx: &mut Ctx<'_>,\n    site_id: usize,\n    mut args: ::std::vec::Vec<::nybl::value::Value>,\n    line: u32,\n) -> Result<__NyblCallOutcome, ::nybl::error::NyblError> {\n",
        );
        self.out
            .push_str("    let site = &__NYBL_FUNCTION_SITES[site_id];\n");
        self.out
            .push_str("    if !::nybl::ref_params::accepts_arity(site.param_modes, args.len()) { return Err(__nybl_arity_error(site.name, site.param_modes, args.len(), line)); }\n");
        self.out.push_str("    args = __nybl_pack_rest_args(args, site.param_modes, line, &ctx.memory)?;\n    let _ = &args;\n    match site_id {\n");
        for site in &self.functions.sites {
            let binds = (0..site.params.len())
                .map(|index| format!("let mut __a{index} = args.remove(0);"))
                .collect::<Vec<_>>()
                .join(" ");
            let call_args = site
                .params
                .iter()
                .enumerate()
                .map(|(index, param)| {
                    if param.mode == ParamMode::Ref {
                        format!("&mut __a{index}")
                    } else {
                        format!("__a{index}.clone()")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            let final_args = (0..site.params.len())
                .map(|index| format!("__a{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            if call_args.is_empty() {
                writeln!(
                    self.out,
                    "        {} => {{ let value = {}(ctx)?; Ok(__NyblCallOutcome {{ value, args: ::std::vec::Vec::new() }}) }},",
                    site.id,
                    function_site_fn_name(site.id),
                )
                .unwrap();
            } else {
                writeln!(
                    self.out,
                    "        {} => {{ {binds} let value = {}(ctx, {})?; Ok(__NyblCallOutcome {{ value, args: vec![{final_args}] }}) }},",
                    site.id,
                    function_site_fn_name(site.id),
                    call_args,
                )
                .unwrap();
            }
        }
        self.out.push_str(
            "        _ => Err(::nybl::error::NyblError::runtime(\"Invalid compiled function site\", line)),\n    }\n}\n\n",
        );
        if self.opts.sandbox {
            self.out.push_str(
                "fn __nybl_call_function_site(\n    ctx: &mut Ctx<'_>,\n    site_id: usize,\n    args: ::std::vec::Vec<::nybl::value::Value>,\n    line: u32,\n) -> Result<::nybl::value::Value, ::nybl::error::NyblError> {\n    __nybl_enter_aot_call(ctx, line)?;\n    let result = __nybl_call_function_site_inner(ctx, site_id, args, line).map(|outcome| outcome.value);\n    __nybl_leave_aot_call(ctx);\n    result\n}\n\n",
            );
        } else {
            self.out.push_str(
                "fn __nybl_call_function_site(\n    ctx: &mut Ctx<'_>,\n    site_id: usize,\n    args: ::std::vec::Vec<::nybl::value::Value>,\n    line: u32,\n) -> Result<::nybl::value::Value, ::nybl::error::NyblError> {\n    __nybl_call_function_site_inner(ctx, site_id, args, line).map(|outcome| outcome.value)\n}\n\n",
            );
        }
    }

    fn emit_public_entry(&mut self) {
        let entry = if self.opts.sandbox {
            PUBLIC_ENTRY_SANDBOX
        } else {
            PUBLIC_ENTRY
        };
        let method_slots = if self.types.method_slots.is_empty() {
            String::new()
        } else {
            format!(
                "        method_slots: [::std::option::Option::None; {}],\n",
                self.types.method_slots.len()
            )
        };
        self.out
            .push_str(&entry.replace("/*__NYBL_METHOD_SLOTS_INIT__*/", &method_slots));
        if self.opts.sandbox {
            self.emit_public_entry_wrappers();
        }
    }

    fn emit_public_entry_wrappers(&mut self) {
        self.out.push_str(
            "/// Hygienic convenience wrappers for potential direct-root public entries.\n/// Each wrapper delegates to `NyblInstance::call`, so runtime reachability,\n/// final visibility, and arity checks remain authoritative.\npub mod nybl_entry_points {\n",
        );
        let names = self
            .functions
            .sites
            .iter()
            .filter(|site| site.abi_eligible && site.visibility == Visibility::Public)
            .map(|site| site.name.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for name in names {
            writeln!(
                self.out,
                "    pub fn __nybl_entry_{symbol}(\n        instance: &mut super::NyblInstance,\n        args: &[::nybl::value::Value],\n        host: &mut dyn ::nybl::NyblHost,\n    ) -> Result<::nybl::value::Value, ::nybl::error::NyblError> {{\n        instance.call({name}, args, host)\n    }}",
                symbol = user_name_component(&name),
                name = rust_string_literal(&name),
            )
            .unwrap();
        }
        self.out.push_str("}\n\n");
    }

    fn emit_main(&mut self) {
        if self.opts.sandbox {
            self.out.push_str(MAIN_FN_SANDBOX);
        } else {
            self.out.push_str(MAIN_FN);
        }
    }

    fn emit_run_program(&mut self, stmts: &[Stmt]) -> Result<(), NyblError> {
        writeln!(
            self.out,
            "fn run_program(ctx: &mut Ctx<'_>) -> Result<(), ::nybl::error::NyblError> {{"
        )
        .unwrap();
        self.indent = 1;
        self.tmp_counter = 0;
        // Mark the body as top-level so `try` lowers `Err` to a
        // real runtime error rather than an impossible
        // `return Ok(value)` (run_program returns `()`).
        let saved_top_level = self.in_top_level;
        self.in_top_level = true;
        self.emit_tick(0);
        self.line("let mut __nybl_type_bindings = __nybl_fresh_module_type_bindings();");
        self.line(&format!(
            "__nybl_reset_type_bindings(ctx, {});",
            rust_string_literal(nybl::value::ROOT_MODULE_PATH),
        ));
        self.callable_mutations
            .push(callable_assignment_names(stmts));
        self.push_scope();
        // Top-level fn decls were already emitted at module scope;
        // skip them here. The scope_stack deliberately stays empty
        // for fn names — they're resolved through `fn_info`
        // (top_level_fns / all_fns), not via `is_local`.
        let direct_function_sites = self
            .direct_function_sites
            .get(nybl::value::ROOT_MODULE_PATH)
            .cloned()
            .unwrap_or_default();
        let mut direct_function_index = 0;
        for stmt in stmts {
            if let StmtKind::FnDecl { name, .. } = &stmt.kind {
                let site = direct_function_sites[direct_function_index];
                direct_function_index += 1;
                self.line(&format!("__nybl_activate_function(ctx, {site});"));
                self.claim_function_in_current_scope(name);
                continue;
            }
            self.emit_stmt(stmt)?;
        }
        self.pop_scope();
        self.callable_mutations.pop();
        self.line("Ok(())");
        self.indent = 0;
        self.out.push_str("}\n\n");
        self.in_top_level = saved_top_level;
        Ok(())
    }

    /// Emit a `__nybl_tick` call at the current indent. No-op when
    /// sandbox mode is off, so call sites don't need their own
    /// gate.
    fn emit_tick(&mut self, line: u32) {
        if self.opts.sandbox {
            self.line(&format!("__nybl_tick(ctx, {line})?;"));
        }
    }

    // ─── Indentation helpers ──────────────────────────────────────

    fn pad(&mut self) {
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
    }

    fn line(&mut self, s: &str) {
        self.pad();
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn open_block(&mut self, header: &str) {
        self.pad();
        self.out.push_str(header);
        self.out.push_str(" {\n");
        self.indent += 1;
    }

    fn close_block(&mut self) {
        self.indent -= 1;
        self.pad();
        self.out.push_str("}\n");
    }

    fn fresh_tmp(&mut self) -> String {
        let t = format!("__t{}", self.tmp_counter);
        self.tmp_counter += 1;
        t
    }

    // ─── Statements ───────────────────────────────────────────────

    fn emit_stmt(&mut self, stmt: &Stmt) -> Result<(), NyblError> {
        let line = stmt.line;
        match &stmt.kind {
            StmtKind::Let {
                name,
                value,
                is_const,
            } => {
                let rhs = self.expr_src(value)?;
                if self.is_module_top_scope() {
                    let value_tmp = self.fresh_tmp();
                    self.line(&format!("let {value_tmp} = {rhs};"));
                    self.line(&format!(
                        "__nybl_define_binding(ctx, {}, {}, {value_tmp});",
                        rust_string_literal(&self.current_module),
                        rust_string_literal(name),
                    ));
                    self.bind_persistent(name);
                    if *is_const {
                        self.mark_constant(name);
                    }
                    self.emit_module_alias_context_sync(name);
                    return Ok(());
                }
                let ident = rust_user_ident(name);
                self.line(&format!("let mut {ident}: ::nybl::value::Value = {rhs};"));
                self.bind_local(name);
                if *is_const {
                    self.mark_constant(name);
                }
                if let Some(scope) = self.scope_stack.last_mut() {
                    scope.module_alias_locals.remove(name);
                }
                self.emit_module_alias_context_sync(name);
            }

            StmtKind::Assign { target, op, value } => {
                self.emit_assign(target, op, value, line)?;
                if let AssignTarget::Variable(name) = target {
                    self.emit_module_alias_context_sync(name);
                }
            }

            StmtKind::If {
                condition,
                body,
                else_ifs,
                else_body,
            } => {
                self.emit_if_statement(condition, body, else_ifs, else_body)?;
            }

            StmtKind::While { condition, body } => {
                let cond_src = self.expr_src(condition)?;
                self.open_block(&format!("while ({cond_src}).is_truthy()"));
                self.emit_tick(line);
                self.emit_runtime_type_scope_for(body);
                self.push_scope();
                for s in body {
                    self.emit_stmt(s)?;
                }
                self.pop_scope();
                self.close_block();
            }

            StmtKind::Repeat { count, body } => {
                let count_src = self.expr_src(count)?;
                let count_tmp = self.fresh_tmp();
                let n_tmp = self.fresh_tmp();
                self.line(&format!("let {count_tmp} = {count_src};"));
                self.open_block(&format!("let {n_tmp}: i64 = match {count_tmp}"));
                self.line("::nybl::value::Value::Int(n) => n,");
                self.line("::nybl::value::Value::Number(n) => n as i64,");
                self.line(&format!(
                    "other => return Err(::nybl::error::NyblError::runtime(format!(\"repeat needs a number, but got {{}}\", other.type_name()), {line})),"
                ));
                self.indent -= 1;
                self.pad();
                self.out.push_str("};\n");
                self.open_block(&format!("for _ in 0..({n_tmp}.max(0))"));
                self.emit_tick(line);
                self.emit_runtime_type_scope_for(body);
                self.push_scope();
                for s in body {
                    self.emit_stmt(s)?;
                }
                self.pop_scope();
                self.close_block();
            }

            StmtKind::ForIn {
                var,
                iterable,
                body,
            } => {
                // Bind the iterable into a tmp before calling
                // `__nybl_iter_start`, otherwise expressions like
                // `range(5)` — which take `&mut ctx.rand_state`
                // internally — would collide with the helper's
                // own `&mut Ctx` borrow.
                let iter_src = self.expr_src(iterable)?;
                let iter_tmp = self.fresh_tmp();
                self.line(&format!(
                    "let {iter_tmp}: ::nybl::value::Value = {iter_src};"
                ));
                let state_tmp = self.fresh_tmp();
                // `__nybl_iter_start` chooses between the eager
                // fast path (Array/Str/Dict) and the protocol
                // (Value::Iter or user type with `.iter()`). The
                // corresponding `__nybl_iter_step` advances
                // whichever shape got picked, so the emitted
                // loop body stays uniform.
                self.line(&format!(
                    "let mut {state_tmp}: __NyblIterState = __nybl_iter_start(ctx, {iter_tmp}, {line})?;"
                ));
                let ident = rust_user_ident(var);
                self.open_block("loop");
                self.emit_tick(line);
                self.line(&format!(
                    "let mut {ident}: ::nybl::value::Value = match __nybl_iter_step(ctx, &mut {state_tmp}, {line})? {{ Some(__v) => __v, None => break, }};"
                ));
                self.emit_runtime_type_scope_for(body);
                self.push_scope();
                self.bind_local(var);
                for s in body {
                    self.emit_stmt(s)?;
                }
                self.pop_scope();
                self.close_block();
            }

            StmtKind::FnDecl { name, .. } => {
                let site = self.function_site_for_stmt(stmt);
                self.line(&format!("__nybl_activate_function(ctx, {site});"));
                if self.function_name_has_single_site(name) {
                    self.bind_function_site(name, site);
                }
            }

            StmtKind::Return { value } => {
                if self.in_top_level && self.opts.sandbox {
                    if let Some(value) = value {
                        let src = self.expr_src(value)?;
                        self.line(&format!("let _ = {src};"));
                    }
                    self.line("return Ok(());");
                    return Ok(());
                }
                match value {
                    Some(v) => {
                        let src = self.expr_src(v)?;
                        self.line(&format!("return Ok({src});"));
                    }
                    None => {
                        self.line("return Ok(::nybl::value::Value::None);");
                    }
                }
            }

            StmtKind::Break => self.line("break;"),
            StmtKind::Continue => self.line("continue;"),

            StmtKind::Use { path, items, alias } => {
                self.emit_use_stmt(path, items, alias, line)?;
            }

            StmtKind::StructDecl { name, fields } => {
                let site = self.type_site_counter;
                self.type_site_counter += 1;
                let descriptor = fields
                    .iter()
                    .map(|field| rust_string_literal(field))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.line(&format!(
                    "const __NYBL_STRUCT_SITE_{site}: &'static [&'static str] = &[{descriptor}];"
                ));
                let mp = self.current_module.clone();
                self.line(&format!(
                    "__nybl_register_struct(ctx, {}, {}, __NYBL_STRUCT_SITE_{site}, {line})?;",
                    rust_string_literal(&mp),
                    rust_string_literal(name),
                ));
                self.line(&format!(
                    "__nybl_bind_type(&mut __nybl_type_bindings, {}, {});",
                    rust_string_literal(name),
                    rust_string_literal(&mp),
                ));
                self.bind_type(name, &mp);
                self.emit_type_context_publish(name, &mp, false);
            }
            StmtKind::EnumDecl { name, variants } => {
                let site = self.type_site_counter;
                self.type_site_counter += 1;
                let descriptor = variants
                    .iter()
                    .map(|variant| {
                        let shape = match &variant.kind {
                            VariantKind::Unit => "__NyblDynamicVariantShape::Unit".to_string(),
                            VariantKind::Tuple(fields) => format!(
                                "__NyblDynamicVariantShape::Tuple(&[{}])",
                                fields
                                    .iter()
                                    .map(|field| rust_string_literal(field))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            VariantKind::Struct(fields) => format!(
                                "__NyblDynamicVariantShape::Struct(&[{}])",
                                fields
                                    .iter()
                                    .map(|field| rust_string_literal(field))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        };
                        format!("({}, {shape})", rust_string_literal(&variant.name))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                self.line(&format!(
                    "const __NYBL_ENUM_SITE_{site}: &'static [(&'static str, __NyblDynamicVariantShape)] = &[{descriptor}];"
                ));
                let mp = self.current_module.clone();
                self.line(&format!(
                    "__nybl_register_enum(ctx, {}, {}, __NYBL_ENUM_SITE_{site}, {line})?;",
                    rust_string_literal(&mp),
                    rust_string_literal(name),
                ));
                self.line(&format!(
                    "__nybl_bind_type(&mut __nybl_type_bindings, {}, {});",
                    rust_string_literal(name),
                    rust_string_literal(&mp),
                ));
                self.bind_type(name, &mp);
                self.emit_type_context_publish(name, &mp, false);
            }
            StmtKind::MethodDecl {
                type_name,
                method_name,
                ..
            } => {
                let mut matches = self.types.method_sites.iter().filter(|site| {
                    site.module_path == self.current_module
                        && site.type_name == *type_name
                        && site.method_name == *method_name
                        && site.line == line
                        && site.column == stmt.column
                });
                let site = matches
                    .next()
                    .expect("method declaration must have a preassigned site");
                assert!(
                    matches.next().is_none(),
                    "parsed method declaration source positions must be unique"
                );
                let site_id = site.id;
                let slot = self
                    .types
                    .method_slot(&self.current_module, type_name, method_name);
                self.line(&format!(
                    "ctx.method_slots[{slot}] = ::std::option::Option::Some({});",
                    method_site_adapter_name(site_id)
                ));
            }

            StmtKind::ExprStmt(expr) => {
                let src = self.expr_src(expr)?;
                self.line(&format!("let _ = {src};"));
            }
            StmtKind::PublicSurface { .. } => {}
        }
        Ok(())
    }

    fn emit_if_statement(
        &mut self,
        cond: &Expr,
        body: &[Stmt],
        else_ifs: &[(Expr, Vec<Stmt>)],
        else_body: &Option<Vec<Stmt>>,
    ) -> Result<(), NyblError> {
        let cond_src = self.expr_src(cond)?;
        self.open_block(&format!("if ({cond_src}).is_truthy()"));
        self.emit_runtime_type_scope_for(body);
        self.push_scope();
        for s in body {
            self.emit_stmt(s)?;
        }
        self.pop_scope();
        self.indent -= 1;
        for (elif_cond, elif_body) in else_ifs {
            let c = self.expr_src(elif_cond)?;
            self.pad();
            self.out
                .push_str(&format!("}} else if ({c}).is_truthy() {{\n"));
            self.indent += 1;
            self.emit_runtime_type_scope_for(elif_body);
            self.push_scope();
            for s in elif_body {
                self.emit_stmt(s)?;
            }
            self.pop_scope();
            self.indent -= 1;
        }
        if let Some(else_body) = else_body {
            self.pad();
            self.out.push_str("} else {\n");
            self.indent += 1;
            self.emit_runtime_type_scope_for(else_body);
            self.push_scope();
            for s in else_body {
                self.emit_stmt(s)?;
            }
            self.pop_scope();
            self.indent -= 1;
        }
        self.pad();
        self.out.push_str("}\n");
        Ok(())
    }

    fn emit_assign(
        &mut self,
        target: &AssignTarget,
        op: &AssignOp,
        value: &Expr,
        line: u32,
    ) -> Result<(), NyblError> {
        match target {
            AssignTarget::Variable(name) => {
                let ident = rust_user_ident(name);
                let rhs_src = self.expr_src(value)?;
                if !self.opts.sandbox
                    && !self.is_local(name)
                    && self.declaration_alias_overlay(name).is_some()
                {
                    let rhs_tmp = self.fresh_tmp();
                    self.line(&format!("let {rhs_tmp} = {rhs_src};"));
                    match op {
                        AssignOp::Eq => {
                            let assign = self
                                .declaration_alias_assign_src(name, &rhs_tmp, line)
                                .expect("overlay checked above");
                            self.line(&assign);
                        }
                        compound => {
                            let current_tmp = self.fresh_tmp();
                            let next_tmp = self.fresh_tmp();
                            let current = self
                                .declaration_alias_read_src(name, line)
                                .expect("overlay checked above");
                            self.line(&format!("let {current_tmp} = {current};"));
                            self.line(&format!(
                                "let {} = {}(&{}, &{}, {}, &ctx.memory)?;",
                                next_tmp,
                                compound_op_path(*compound),
                                current_tmp,
                                rhs_tmp,
                                line
                            ));
                            let assign = self
                                .declaration_alias_assign_src(name, &next_tmp, line)
                                .expect("overlay checked above");
                            self.line(&assign);
                        }
                    }
                    self.emit_module_alias_context_sync(name);
                    return Ok(());
                }
                if !self.is_local(name) {
                    let rhs_tmp = self.fresh_tmp();
                    let target_tmp = self.fresh_tmp();
                    let memory_tmp = (!matches!(op, AssignOp::Eq)).then(|| self.fresh_tmp());
                    self.line(&format!("let {rhs_tmp} = {rhs_src};"));
                    if self.opts.sandbox
                        && matches!(op, AssignOp::Eq)
                        && self.has_declaration_alias_overlay(name)
                    {
                        self.line(&format!(
                            "if !__nybl_has_binding(ctx, {}, {}) {{ return Err(::nybl::error::NyblError::runtime({}, {line})); }}",
                            rust_string_literal(&self.current_module),
                            rust_string_literal(name),
                            rust_string_literal(&format!("Variable `{name}` doesn't exist yet")),
                        ));
                    }
                    let storage =
                        self.binding_storage(name)
                            .unwrap_or_else(|| BindingStorage::Persistent {
                                module: self.current_module.clone(),
                                name: name.to_string(),
                            });
                    let target_src = self.storage_mut_src(&storage, name, line);
                    if let Some(memory_tmp) = &memory_tmp {
                        self.line(&format!("let {memory_tmp} = ctx.memory.clone();"));
                    }
                    self.line(&format!(
                        "let {target_tmp}: &mut ::nybl::value::Value = {target_src};"
                    ));
                    match op {
                        AssignOp::Eq => {
                            self.line(&format!("*{target_tmp} = {rhs_tmp};"));
                        }
                        compound => {
                            let memory_tmp =
                                memory_tmp.as_ref().expect("compound assignment has memory");
                            let current_tmp = self.fresh_tmp();
                            let next_tmp = self.fresh_tmp();
                            self.line(&format!("let {current_tmp} = {target_tmp}.clone();"));
                            self.line(&format!(
                                "let {next_tmp} = {}(&{current_tmp}, &{rhs_tmp}, {line}, &{memory_tmp})?;",
                                compound_op_path(*compound),
                            ));
                            self.line(&format!("*{target_tmp} = {next_tmp};"));
                        }
                    }
                    return Ok(());
                }
                match op {
                    AssignOp::Eq => {
                        self.line(&format!("{ident} = {rhs_src};"));
                    }
                    compound => {
                        let op_path = compound_op_path(*compound);
                        let rhs_tmp = self.fresh_tmp();
                        self.line(&format!("let {rhs_tmp} = {rhs_src};"));
                        self.line(&format!(
                            "{ident} = {op_path}(&{ident}, &{rhs_tmp}, {line}, &ctx.memory)?;"
                        ));
                    }
                }
                self.mark_local_mutated(name);
                Ok(())
            }
            AssignTarget::Index { object, index } => {
                let mut place = aot_place(object).ok_or_else(|| {
                    NyblError::runtime(
                        "Can only assign to indexed variables (like `arr[0] = val`)",
                        line,
                    )
                })?;
                place
                    .projections
                    .push(AotPlaceProjection::Index(index.clone()));
                self.emit_place_assign(&place, op, value, line)
            }
            AssignTarget::Field { object, field } => {
                let mut place = aot_place(object).ok_or_else(|| {
                    NyblError::runtime(
                        "Can only assign to fields of named variables (like `p.x = val`)",
                        line,
                    )
                })?;
                place
                    .projections
                    .push(AotPlaceProjection::Field(field.clone()));
                self.emit_place_assign(&place, op, value, line)
            }
        }
    }

    fn emit_place_assign(
        &mut self,
        place: &AotPlace,
        op: &AssignOp,
        value: &Expr,
        line: u32,
    ) -> Result<(), NyblError> {
        // Preserve assignment order: RHS, then projection expressions, then
        // the current leaf for compound operations.
        let rhs = self.expr_src(value)?;
        let rhs_tmp = self.fresh_tmp();
        self.line(&format!("let {rhs_tmp} = {rhs};"));
        let (steps, path) = self.place_steps_src(place)?;
        self.line(&steps);
        let storage =
            self.binding_storage(&place.root)
                .unwrap_or_else(|| BindingStorage::Persistent {
                    module: self.current_module.clone(),
                    name: place.root.clone(),
                });
        let root_read = self
            .declaration_alias_read_src(&place.root, line)
            .unwrap_or_else(|| self.storage_read_src(&storage, line));
        let root_write = self
            .declaration_alias_mut_src(&place.root, line)
            .unwrap_or_else(|| self.storage_mut_src(&storage, &place.root, line));
        let root = self.fresh_tmp();
        self.line(&format!("let {root} = {root_read};"));
        let replacement = match op {
            AssignOp::Eq => rhs_tmp,
            compound => {
                let current = self.fresh_tmp();
                let replacement = self.fresh_tmp();
                self.line(&format!(
                    "let {current} = __nybl_place_get(&{root}, &{path}, {line}, &ctx.memory)?;"
                ));
                self.line(&format!(
                    "let {replacement} = {}(&{current}, &{rhs_tmp}, {line}, &ctx.memory)?;",
                    compound_op_path(*compound),
                ));
                replacement
            }
        };
        let candidate = self.fresh_tmp();
        self.line(&format!(
            "let {candidate} = __nybl_place_set(&ctx.memory, {root}, &{path}, {replacement}, {line})?;"
        ));
        if self.opts.sandbox {
            self.line(&format!("__nybl_check_memory(ctx, {line})?;"));
        }
        let target = self.fresh_tmp();
        self.line(&format!(
            "let {target}: &mut ::nybl::value::Value = {root_write};"
        ));
        self.line(&format!("*{target} = {candidate};"));
        self.refresh_write_through_alias_overlay(&place.root);
        if self.is_local(&place.root) {
            self.mark_local_mutated(&place.root);
        }
        Ok(())
    }

    fn emit_fn_decl_as(
        &mut self,
        fn_name: &str,
        params: &[Parameter],
        body: &[Stmt],
        line: u32,
    ) -> Result<(), NyblError> {
        self.emit_fn_decl_as_site(fn_name, params, body, line, None)
    }

    fn emit_fn_decl_as_site(
        &mut self,
        fn_name: &str,
        params: &[Parameter],
        body: &[Stmt],
        line: u32,
        self_site: Option<(&str, usize)>,
    ) -> Result<(), NyblError> {
        let uses_runtime_type_bindings = self.statements_use_runtime_type_bindings(body);
        let param_list = params
            .iter()
            .enumerate()
            .map(|(index, param)| {
                if param.mode == ParamMode::Ref {
                    format!("__nybl_param_{index}: &mut ::nybl::value::Value")
                } else {
                    format!("__nybl_param_{index}: ::nybl::value::Value")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let sig = if params.is_empty() {
            format!(
                "fn {fn_name}(ctx: &mut Ctx<'_>) -> Result<::nybl::value::Value, ::nybl::error::NyblError>"
            )
        } else {
            format!(
                "fn {fn_name}(ctx: &mut Ctx<'_>, {param_list}) -> Result<::nybl::value::Value, ::nybl::error::NyblError>"
            )
        };
        self.open_block(&sig);
        let saved_tmp = self.tmp_counter;
        self.tmp_counter = 0;
        // Inside a user fn: the enclosing Rust fn returns
        // `Result<Value, NyblError>`, so a Nybl-level return from
        // `try` on `Err` is `return Ok(err_value)` rather than a
        // real error.
        let saved_top_level = self.in_top_level;
        self.in_top_level = false;
        let saved_callable_body = self.in_callable_body;
        self.in_callable_body = true;
        // Function-entry checkpoint — matches the plan's "step-count
        // checks at loop backedges / function entry". No-op outside
        // sandbox mode.
        self.emit_tick(line);
        if uses_runtime_type_bindings {
            self.line(&format!(
                "let mut __nybl_type_bindings = __nybl_callable_type_bindings(ctx, {});",
                rust_string_literal(&self.current_module),
            ));
        }
        // Rust item functions cannot capture locals from `run_program` or a
        // module loader. Give each alias required by this callable or one of
        // its descendant lambdas a lazy call-local binding. No context lookup
        // occurs until an executed read, so an unexecuted branch cannot fail
        // merely because its alias has not been declared yet.
        let saved_scope_stack = core::mem::take(&mut self.scope_stack);
        self.push_scope();
        // Lifted Rust functions cannot retain the outer emission scopes, but
        // ref-target validation still needs source-ordered metadata for
        // persistent constants that were visible at the declaration site.
        for scope in &saved_scope_stack {
            for name in &scope.persistent_bindings {
                self.bind_persistent(name);
                if scope.constants.contains(name) {
                    self.mark_constant(name);
                }
            }
        }
        let declaration_aliases = self.declaration_aliases_for_callable(params, body);
        let declaration_alias_overlays =
            self.emit_declaration_alias_overlays(&declaration_aliases, &HashMap::new());
        self.declaration_alias_overlays
            .push(declaration_alias_overlays);
        self.declaration_alias_write_through.push(true);
        self.callable_mutations
            .push(callable_assignment_names(body));
        // Fresh scope with the params bound; Rust-level fn scope
        // isolates outer locals anyway, but scope tracking is the
        // source of truth for lambda-capture analysis.
        self.push_scope();
        if let Some((name, site)) = self_site {
            self.bind_function_site(name, site);
        }
        for (index, p) in params.iter().enumerate() {
            self.bind_local(&p.name);
            if p.mode == ParamMode::Ref {
                self.mark_ref_parameter(&p.name);
            }
            let source = if p.mode == ParamMode::Ref {
                format!("__nybl_param_{index}.clone()")
            } else {
                format!("__nybl_param_{index}")
            };
            self.line(&format!(
                "let mut {}: ::nybl::value::Value = {};",
                rust_user_ident(&p.name),
                source
            ));
        }
        let has_refs = params.iter().any(|param| param.mode == ParamMode::Ref);
        if has_refs {
            self.line("let __nybl_call_result = (|| -> Result<::nybl::value::Value, ::nybl::error::NyblError> {");
            self.indent += 1;
        }
        for s in body {
            self.emit_stmt(s)?;
        }
        if has_refs {
            self.line("#[allow(unreachable_code)] Ok(::nybl::value::Value::None)");
            self.indent -= 1;
            self.line("})();");
            self.line("match __nybl_call_result {");
            self.indent += 1;
            self.line("Ok(__nybl_return_value) => {");
            self.indent += 1;
            self.emit_tick(line);
            for (index, param) in params.iter().enumerate() {
                if param.mode == ParamMode::Ref {
                    self.line(&format!(
                        "*__nybl_param_{index} = {}.clone();",
                        rust_user_ident(&param.name)
                    ));
                }
            }
            self.line("Ok(__nybl_return_value)");
            self.indent -= 1;
            self.line("}");
            self.line("Err(__nybl_error) => Err(__nybl_error),");
            self.indent -= 1;
            self.line("}");
        }
        self.pop_scope();
        self.pop_scope();
        self.callable_mutations.pop();
        self.declaration_alias_write_through.pop();
        self.declaration_alias_overlays.pop();
        self.scope_stack = saved_scope_stack;
        // Implicit `return none` if control falls off the end. The
        // `allow(unreachable_code)` at the top of the file silences
        // the warning for bodies that always return explicitly.
        if !has_refs {
            self.line("Ok(::nybl::value::Value::None)");
        }
        self.tmp_counter = saved_tmp;
        self.in_top_level = saved_top_level;
        self.in_callable_body = saved_callable_body;
        self.close_block();
        Ok(())
    }

    // ─── Expressions ──────────────────────────────────────────────

    /// Render `expr` as a Rust expression of type
    /// `::nybl::value::Value`. Fallible sub-expressions propagate via
    /// `?`, so the result must be used inside a function returning
    /// `Result<_, NyblError>`.
    fn expr_src(&mut self, expr: &Expr) -> Result<String, NyblError> {
        let line = expr.line;
        let s = match &expr.kind {
            ExprKind::Int(n) => format!("::nybl::value::Value::Int({n}i64)"),
            ExprKind::Number(n) => format!("::nybl::value::Value::Number({}f64)", rust_f64(*n)),
            ExprKind::Str(s) => format!(
                "::nybl::value::Value::__new_str_in({}.to_string(), &ctx.memory)",
                rust_string_literal(s)
            ),
            ExprKind::Bool(b) => {
                format!("::nybl::value::Value::Bool({b})")
            }
            ExprKind::None => "::nybl::value::Value::None".to_string(),

            ExprKind::Ident(name) => self.ident_value_src(name, line)?,

            ExprKind::StringInterp(parts) => self.string_interp_src(parts, line)?,

            ExprKind::FieldAccess { object, field } => {
                let obj_src = self.expr_src(object)?;
                let obj_tmp = self.fresh_tmp();
                format!(
                    "{{ let {tmp} = {obj}; __nybl_field_get_live(ctx, &{tmp}, {field_lit}, {line})? }}",
                    tmp = obj_tmp,
                    obj = obj_src,
                    field_lit = rust_string_literal(field),
                    line = line,
                )
            }

            ExprKind::StructConstruct {
                namespace,
                type_name,
                fields,
            } => self.struct_construct_src(namespace.as_deref(), type_name, fields, line)?,

            ExprKind::EnumConstruct {
                namespace,
                type_name,
                variant,
                payload,
            } => {
                self.enum_construct_src(namespace.as_deref(), type_name, variant, payload, line)?
            }

            ExprKind::Lambda { params, body } => self.lambda_src(params, body, line)?,

            ExprKind::BinaryOp { left, op, right } => self.binary_src(left, *op, right, line)?,

            ExprKind::UnaryOp { op, expr: inner } => {
                let inner_src = self.expr_src(inner)?;
                match op {
                    UnaryOp::Neg => {
                        format!("{{ let __v = {inner_src}; ::nybl::ops::neg(&__v, {line})? }}")
                    }
                    UnaryOp::Not => {
                        format!("{{ let __v = {inner_src}; ::nybl::ops::not(&__v) }}")
                    }
                }
            }

            ExprKind::Call { callee, args } => self.call_src(callee, args, line)?,

            ExprKind::MethodCall {
                object,
                method,
                args,
            } => self.method_call_src(object, method, args, line)?,

            ExprKind::Index { object, index } => {
                let obj_src = self.expr_src(object)?;
                let idx_src = self.expr_src(index)?;
                format!(
                    "{{ let __o = {obj_src}; let __i = {idx_src}; ::nybl::ops::index_get_in(&__o, &__i, {line}, &ctx.memory)? }}"
                )
            }

            ExprKind::Array(items) => self.array_src(items, line)?,

            ExprKind::Dict(entries) => self.dict_src(entries, line)?,

            ExprKind::IfExpr {
                condition,
                then_expr,
                else_expr,
            } => {
                let cond = self.expr_src(condition)?;
                let then_s = self.expr_src(then_expr)?;
                let else_s = self.expr_src(else_expr)?;
                format!("(if ({cond}).is_truthy() {{ {then_s} }} else {{ {else_s} }})")
            }

            ExprKind::Match { scrutinee, arms } => self.match_src(scrutinee, arms, line)?,

            ExprKind::Try(inner) => self.try_src(inner, line)?,
        };
        Ok(s)
    }

    /// Lower `try <expr>` to a Rust block expression. The shape
    /// matches the walker / VM: inspect the variant name, unwrap
    /// `Ok`, short-circuit on `Err`, raise on anything else.
    ///
    /// `Err` short-circuits differently depending on whether
    /// we're inside a user fn (in which case the enclosing Rust
    /// fn returns `Result<Value, NyblError>` and `return Ok(v)`
    /// propagates the Err variant as that fn's result) or
    /// top-level `run_program` (which returns `Result<(),
    /// NyblError>` — no way to thread a value through, so we raise
    /// a real runtime error instead, matching the walker's
    /// "try at top-level" rule).
    fn try_src(&mut self, inner: &Expr, line: u32) -> Result<String, NyblError> {
        let inner_src = self.expr_src(inner)?;
        let id = self.tmp_counter;
        self.tmp_counter += 1;
        let v_name = format!("__try_v_{id}");
        let err_arm = if self.in_top_level {
            format!(
                "return ::std::result::Result::Err(::nybl::error_messages::top_level_try_error({line}));"
            )
        } else {
            // Inside a user fn: a Nybl-level `return err` is
            // spelled `return Ok(err_value)` at the Rust level.
            format!("return ::std::result::Result::Ok({v_name}.clone());")
        };
        // We construct a block that:
        //   1. evaluates the scrutinee once into a local;
        //   2. inspects its variant;
        //   3. either yields the unwrapped value, returns early,
        //      or raises.
        //
        // The block's type is `::nybl::value::Value` — the `Ok`
        // arm's unwrapped payload, or one of two `!`-typed
        // returns. `#[allow(unreachable_code)]` guards the
        // pattern where the Rust compiler can prove the block
        // always returns (e.g. a bare `try` at the end of a fn).
        Ok(format!(
            "{{\n    \
             let {v_name}: ::nybl::value::Value = {inner_src};\n    \
             match &{v_name} {{\n        \
                 ::nybl::value::Value::EnumVariant(ev) if ev.variant() == \"Ok\" => {{\n            \
                     match ev.payload() {{\n                \
                         ::nybl::value::EnumPayload::Tuple(items) if items.len() == 1 => items[0].clone(),\n                \
                         ::nybl::value::EnumPayload::Unit => ::nybl::value::Value::None,\n                \
                         ::nybl::value::EnumPayload::Tuple(items) => {{\n                    \
                             return ::std::result::Result::Err(::nybl::error::NyblError::runtime(format!(\"try: Ok variant must carry exactly one value, got {{}}\", items.len()), {line}));\n                \
                         }}\n                \
                         ::nybl::value::EnumPayload::Struct(_) => {{\n                    \
                             return ::std::result::Result::Err(::nybl::error::NyblError::runtime(\"try: Ok variant must carry a single positional value, not named fields\", {line}));\n                \
                         }}\n            \
                     }}\n        \
                 }}\n        \
                 ::nybl::value::Value::EnumVariant(ev) if ev.variant() == \"Err\" => {{\n            \
                     {err_arm}\n        \
                 }}\n        \
                 other => {{\n            \
                     return ::std::result::Result::Err(::nybl::error::NyblError::runtime(format!(\"try expected a Result-shaped value (Ok/Err variant), got {{}}\", other.type_name()), {line}));\n        \
                 }}\n    \
             }}\n\
             }}",
        ))
    }

    /// Lower a `match` expression to a Rust block expression.
    ///
    /// Emitted shape (trimmed for clarity):
    ///
    /// ```text
    /// {
    ///   let __match_sc_N: Value = <scrutinee>;
    ///   'match_arms_N: loop {
    ///     {
    ///       let __pat: Pattern = <pattern ctor>;
    ///       let mut __bindings = Vec::new();
    ///       if nybl::pattern_matches(&__pat, &__match_sc_N, &mut __bindings) {
    ///         let <b1>: Value = __bindings.iter().rev().find(...)...;
    ///         // ...
    ///         if (<guard>).is_truthy() {
    ///           break 'match_arms_N (<body>);
    ///         }
    ///       }
    ///     }
    ///     // ... next arm ...
    ///     return Err(NyblError::runtime("No match arm matched the scrutinee", line));
    ///   }
    /// }
    /// ```
    ///
    /// The pattern itself is constructed as a `nybl::parser::Pattern`
    /// value in the emitted Rust and fed through the shared
    /// `nybl::pattern_matches` helper so walker, VM, and AOT all
    /// apply identical matching rules. Each arm pushes a fresh
    /// emitter scope so binding names are visible as Rust locals
    /// inside `expr_src` calls for guard and body.
    fn match_src(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        line: u32,
    ) -> Result<String, NyblError> {
        let scrutinee_src = self.expr_src(scrutinee)?;
        let id = self.tmp_counter;
        self.tmp_counter += 1;
        let sc_name = format!("__match_sc_{id}");
        let label = format!("match_arms_{id}");

        let mut src = String::new();
        src.push_str("{\n");
        src.push_str(&format!(
            "    let {sc_name}: ::nybl::value::Value = {scrutinee_src};\n"
        ));
        src.push_str(&format!("    '{label}: loop {{\n"));

        for arm in arms {
            let arm_line = arm.line;
            let pat_src = pattern_rust(&arm.pattern);
            // Collect every name this arm's pattern binds. We sort
            // for deterministic emission and register them as
            // locals on the emitter's scope stack so `expr_src`
            // treats them as locals (not free captures) inside the
            // guard and body.
            let names: Vec<String> = arm.pattern.binding_names().into_iter().collect();
            let namespaces: Vec<String> = arm.pattern.namespace_names().into_iter().collect();

            src.push_str("        {\n");
            src.push_str(&format!(
                "            let __pat: ::nybl::parser::Pattern = {pat_src};\n"
            ));
            src.push_str("            let mut __bindings: ::std::vec::Vec<(::std::string::String, ::nybl::value::Value)> = ::std::vec::Vec::new();\n");
            // Emit a per-site resolver closure that encodes the
            // emit-time view of type_bindings + module_aliases.
            // Patterns reaching this point use the same lexical
            // scope we're in *right now*, so statically baking
            // the mapping is both correct and efficient.
            src.push_str(
                &self.emit_resolver_closure_src(&namespaces, pattern_uses_bare_type(&arm.pattern)),
            );
            src.push_str(&format!(
                "            if ::nybl::pattern_matches_in(&__pat, &{sc_name}, &mut __bindings, &__resolver, &ctx.memory) {{\n"
            ));

            self.push_scope();
            for name in &names {
                self.bind_local(name);
                src.push_str(&format!(
                    "                let mut {}: ::nybl::value::Value = __bindings.iter().rev().find(|(k, _)| k == {}).map(|(_, v)| v.clone()).unwrap_or(::nybl::value::Value::None);\n",
                    rust_user_ident(name),
                    rust_string_literal(name)
                ));
            }

            // Rust warns if a binding name isn't used in the body
            // — a common case for wildcard-ish matches where the
            // programmer bound a name for clarity but didn't need
            // it. Prefix the lets with `#[allow(unused_variables)]`
            // scoped blocks? Simpler: emit `let _ = <name>;` to
            // silence the lint without changing semantics.
            for name in &names {
                src.push_str(&format!(
                    "                let _ = &{};\n",
                    rust_user_ident(name)
                ));
            }

            if let Some(guard) = &arm.guard {
                let guard_src = self.expr_src(guard)?;
                src.push_str(&format!(
                    "                if ({guard_src}).is_truthy() {{\n"
                ));
                let body_src = self.expr_src(&arm.body)?;
                src.push_str(&format!(
                    "                    break '{label} ({body_src});\n"
                ));
                src.push_str("                }\n");
            } else {
                let body_src = self.expr_src(&arm.body)?;
                src.push_str(&format!("                break '{label} ({body_src});\n"));
            }
            self.pop_scope();

            src.push_str("            }\n");
            src.push_str("        }\n");

            // Keep the per-arm line variable even if unused for
            // now — ready for future diagnostics that want to
            // point at a specific arm.
            let _ = arm_line;
        }

        src.push_str(&format!(
            "        #[allow(unreachable_code)]\n        return ::std::result::Result::Err(::nybl::error::NyblError::runtime(\"No match arm matched the scrutinee\", {line}));\n"
        ));
        src.push_str("    }\n");
        src.push('}');

        Ok(src)
    }

    fn binary_src(
        &mut self,
        left: &Expr,
        op: BinOp,
        right: &Expr,
        line: u32,
    ) -> Result<String, NyblError> {
        match op {
            BinOp::And => {
                let l = self.expr_src(left)?;
                let r = self.expr_src(right)?;
                Ok(format!(
                    "{{ let __l = {l}; if __l.is_truthy() {{ ::nybl::value::Value::Bool(({r}).is_truthy()) }} else {{ ::nybl::value::Value::Bool(false) }} }}"
                ))
            }
            BinOp::Or => {
                let l = self.expr_src(left)?;
                let r = self.expr_src(right)?;
                Ok(format!(
                    "{{ let __l = {l}; if __l.is_truthy() {{ ::nybl::value::Value::Bool(true) }} else {{ ::nybl::value::Value::Bool(({r}).is_truthy()) }} }}"
                ))
            }
            _ => {
                let l = self.expr_src(left)?;
                let r = self.expr_src(right)?;
                let op_path = bin_op_path(op);
                // Eq / NotEq are infallible; ops::eq / ops::not_eq
                // return Value directly. Everything else is
                // Result<Value, NyblError>. Emit the `?` only where
                // needed.
                let needs_try = !matches!(op, BinOp::Eq | BinOp::NotEq);
                let suffix_line = if matches!(op, BinOp::Eq | BinOp::NotEq) {
                    String::new()
                } else {
                    format!(", {line}, &ctx.memory")
                };
                let trailing = if needs_try { "?" } else { "" };
                Ok(format!(
                    "{{ let __l = {l}; let __r = {r}; {op_path}(&__l, &__r{suffix_line}){trailing} }}"
                ))
            }
        }
    }

    fn dynamic_value_call_src(
        &mut self,
        callee_src: String,
        args: &[CallArg],
        line: u32,
    ) -> Result<String, NyblError> {
        let callee_tmp = self.fresh_tmp();
        let modes = args
            .iter()
            .map(|arg| match arg.mode {
                ParamMode::Value => "::nybl::parser::ParamMode::Value",
                ParamMode::Ref => "::nybl::parser::ParamMode::Ref",
                ParamMode::Rest => "::nybl::parser::ParamMode::Value",
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut fences = String::new();
        let mut seen_targets = HashSet::new();
        let mut targets: Vec<Option<EmittedPlaceTarget>> = vec![None; args.len()];
        for (index, arg) in args.iter().enumerate() {
            if arg.mode != ParamMode::Ref {
                continue;
            }
            let position = index + 1;
            let Some(place) = aot_place(&arg.value) else {
                write!(
                    fences,
                    "return Err(::nybl::ref_params::invalid_ref_target({position}, {line})); "
                )
                .unwrap();
                continue;
            };
            let name = &place.root;
            if !seen_targets.insert(name.clone()) {
                write!(
                    fences,
                    "return Err(::nybl::ref_params::duplicate_ref_target({line})); "
                )
                .unwrap();
                continue;
            }
            if self.is_constant_binding(name) {
                write!(
                    fences,
                    "return Err(::nybl::error_messages::constant_mutation_error({}, {line})); ",
                    rust_string_literal(name),
                )
                .unwrap();
                continue;
            }
            if self.is_captured_binding(name) {
                write!(
                    fences,
                    "return Err(::nybl::ref_params::captured_ref_target({position}, {line})); "
                )
                .unwrap();
                continue;
            }
            let (target_preflight, target) = self.emitted_place_target(place, line);
            fences.push_str(&target_preflight);
            targets[index] = Some(target);
        }
        let mut ordinary_lets = String::new();
        let mut arg_names: Vec<Option<String>> = vec![None; args.len()];
        for (index, arg) in args.iter().enumerate() {
            if arg.mode == ParamMode::Ref {
                continue;
            }
            let src = self.expr_src(&arg.value)?;
            let tmp = self.fresh_tmp();
            write!(ordinary_lets, "let {tmp} = {src}; ").unwrap();
            arg_names[index] = Some(tmp);
        }
        let mut snapshot_lets = String::new();
        let mut resolved: Vec<Option<(String, String, String)>> = vec![None; args.len()];
        for (index, target) in targets.iter().enumerate() {
            let Some(target) = target else {
                continue;
            };
            let (projection_lets, path) = self.place_steps_src(&target.place)?;
            let root = self.fresh_tmp();
            let leaf = self.fresh_tmp();
            write!(
                snapshot_lets,
                "{projection_lets}let {root} = {read}; let {leaf} = __nybl_place_get(&{root}, &{path}, {line}, &ctx.memory)?; ",
                read = target.root_read,
            )
            .unwrap();
            arg_names[index] = Some(leaf);
            resolved[index] = Some((root, path, target.root_write.clone()));
        }
        let arg_values = arg_names
            .iter()
            .map(|name| {
                name.clone()
                    .unwrap_or_else(|| "::nybl::value::Value::None".to_string())
            })
            .collect::<Vec<_>>()
            .join(", ");
        let args_vec = if args.is_empty() {
            "::std::vec::Vec::new()".to_string()
        } else {
            format!("vec![{arg_values}]")
        };
        let outcome_tmp = self.fresh_tmp();
        let mut candidates = String::new();
        let mut commits = String::new();
        for (index, target) in resolved.iter().enumerate() {
            let Some((root, path, write_target)) = target else {
                continue;
            };
            let candidate = self.fresh_tmp();
            write!(
                candidates,
                "let {candidate} = __nybl_place_set(&ctx.memory, {root}.clone(), &{path}, {outcome_tmp}.args[{index}].clone(), {line})?; "
            )
            .unwrap();
            write!(
                commits,
                "{{ let __nybl_target: &mut ::nybl::value::Value = {write_target}; *__nybl_target = {candidate}; }} "
            )
            .unwrap();
        }
        let memory_check = if self.opts.sandbox {
            format!("__nybl_check_memory(ctx, {line})?; ")
        } else {
            String::new()
        };
        Ok(format!(
            "{{ let {callee_tmp} = {callee_src}; \
             __nybl_preflight_value_call(ctx, &{callee_tmp}, &[{modes}], {line})?; \
             {fences}{ordinary_lets}{snapshot_lets}\
             let {outcome_tmp} = __nybl_call_value(ctx, {callee_tmp}, {args_vec}, {line})?; \
             {candidates}{memory_check}{commits}{outcome_tmp}.value }}"
        ))
    }

    fn call_src(
        &mut self,
        callee: &Expr,
        args: &[CallArg],
        line: u32,
    ) -> Result<String, NyblError> {
        // Dynamic callees use the shared preflight/staging path so the callee
        // resolves exactly once before any argument side effect.
        let name = match &callee.kind {
            ExprKind::Ident(n) => n.clone(),
            _ => {
                let callee_src = self.expr_src(callee)?;
                return self.dynamic_value_call_src(callee_src, args, line);
            }
        };

        // A locally-bound Ident (e.g. `let f = fn() {...}; f(x)`)
        // becomes a value call. Matches the walker / VM rule that
        // local shadowing wins over builtin / host / named-fn.
        if self.is_local(&name) {
            let callee_src = self.ident_value_src(&name, line)?;
            return self.dynamic_value_call_src(callee_src, args, line);
        }

        let call_has_ref = args.iter().any(|arg| arg.mode == ParamMode::Ref);
        // Exact lexical function bindings, including a function's self-name,
        // must not be redirected to a later same-name declaration. Ref calls
        // still use the shared preflight/staging path so mutations commit only
        // after a successful call.
        if let Some(site) = self.local_function_site(&name) {
            let site_has_ref = self.functions.sites[site]
                .params
                .iter()
                .any(|param| param.mode == ParamMode::Ref);
            let site_has_rest = self.functions.sites[site]
                .params
                .last()
                .is_some_and(|param| param.mode == ParamMode::Rest);
            if site_has_ref || site_has_rest || call_has_ref {
                let callee_src = format!("__nybl_function_site_value(ctx, {site}, {line})?");
                return self.dynamic_value_call_src(callee_src, args, line);
            }
            return self.value_named_call_src(&name, args, line);
        }

        // A call using an explicit ref marker, or a name whose every
        // declaration is ref-aware, always resolves a callable before any
        // argument side effect.
        let declared_has_ref = self.fn_info.names_with_ref_sites.contains(&name);
        let declared_has_value_only = self.fn_info.names_with_value_only_sites.contains(&name);
        let declared_has_rest = self.functions.sites.iter().any(|site| {
            site.name == name
                && site
                    .params
                    .last()
                    .is_some_and(|param| param.mode == ParamMode::Rest)
        });
        if call_has_ref || declared_has_rest || (declared_has_ref && !declared_has_value_only) {
            if self.fn_info.all_fns.contains(&name) {
                let callee_src = format!(
                    "__nybl_active_function_value(ctx, {}, {}, {})?",
                    rust_string_literal(&self.current_module),
                    rust_string_literal(&name),
                    line,
                );
                return self.dynamic_value_call_src(callee_src, args, line);
            }
            if let Some(storage) = self.binding_storage(&name) {
                let callee_src = self.storage_read_src(&storage, line);
                return self.dynamic_value_call_src(callee_src, args, line);
            }
            let modes = args
                .iter()
                .map(|arg| match arg.mode {
                    ParamMode::Value => "::nybl::parser::ParamMode::Value",
                    ParamMode::Ref => "::nybl::parser::ParamMode::Ref",
                    ParamMode::Rest => "::nybl::parser::ParamMode::Value",
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(format!(
                "{{ ::nybl::validate_value_only_call_modes({}, &[{}], {})?; unreachable!() }}",
                rust_string_literal(&name),
                modes,
                line,
            ));
        }

        // Mixed-mode same-name declarations require a runtime decision before
        // evaluating ordinary arguments. Only the active site is
        // authoritative: a reached ref site uses transactional preflight,
        // while a reached value-only site (or no reached site) preserves
        // host-before-named-function dispatch.
        if declared_has_ref && declared_has_value_only && self.fn_info.all_fns.contains(&name) {
            let ref_call = self.dynamic_value_call_src(
                format!("__nybl_function_site_value(ctx, __nybl_site, {line})?"),
                args,
                line,
            )?;
            let value_call = self.value_named_call_src(&name, args, line)?;
            return Ok(format!(
                "{{ match __nybl_active_ref_function_site(ctx, {module}, {name}) {{ ::std::option::Option::Some(__nybl_site) => {{ {ref_call} }}, ::std::option::Option::None => {{ {value_call} }}, }} }}",
                module = rust_string_literal(&self.current_module),
                name = rust_string_literal(&name),
            ));
        }

        self.value_named_call_src(&name, args, line)
    }

    fn value_named_call_src(
        &mut self,
        name: &str,
        args: &[CallArg],
        line: u32,
    ) -> Result<String, NyblError> {
        let exact_site = self.local_function_site(name);
        if let Some(site) = exact_site
            && args.len() != self.functions.sites[site].params.len()
        {
            return Ok(format!(
                "{{ {} }}",
                self.function_site_arity_error_src(site, args.len(), line)
            ));
        }

        let actual_modes = args
            .iter()
            .map(|arg| match arg.mode {
                ParamMode::Value => "::nybl::parser::ParamMode::Value",
                ParamMode::Ref => "::nybl::parser::ParamMode::Ref",
                ParamMode::Rest => "::nybl::parser::ParamMode::Value",
            })
            .collect::<Vec<_>>()
            .join(", ");
        // Named function calls validate the selected declaration before
        // evaluating arguments. Correct-arity calls still evaluate their
        // arguments before host dispatch, preserving host precedence.
        let preflight = if exact_site.is_some() {
            String::new()
        } else if let Some(site) = self.native_static_function_site(name) {
            if args.len() == self.functions.sites[site].params.len() {
                String::new()
            } else {
                format!(
                    "if ctx.reached_function_sites[{site}] {{ {}; }} ",
                    self.function_site_arity_error_src(site, args.len(), line)
                )
            }
        } else if self.fn_info.all_fns.contains(name) {
            format!(
                "__nybl_preflight_active_function_call(ctx, {module}, {name}, &[{actual_modes}], {line})?; ",
                module = rust_string_literal(&self.current_module),
                name = rust_string_literal(name),
            )
        } else {
            String::new()
        };

        // Evaluate args into locals up-front so the resulting block
        // has a predictable evaluation order and doesn't reborrow
        // `ctx` inside nested sub-expressions.
        let mut arg_names = Vec::with_capacity(args.len());
        let mut arg_lets = String::new();
        for arg in args {
            let src = self.expr_src(arg)?;
            let tmp = self.fresh_tmp();
            write!(arg_lets, "let {tmp} = {src}; ").unwrap();
            arg_names.push(tmp);
        }

        // The invoked function's `self_name` is an exact lexical binding.
        // It must recurse to the retained declaration site directly, without
        // host interception or a later same-name redeclaration.
        if let Some(site) = exact_site {
            let call = self.exact_function_site_call_src(site, &arg_names, line);
            return Ok(format!("{{ {preflight}{arg_lets}{call} }}"));
        }

        let binding_storage = self.binding_storage(name);
        if let Some(
            storage @ (BindingStorage::RustLocal { .. } | BindingStorage::Persistent { .. }),
        ) = binding_storage.as_ref()
        {
            let callee = self.storage_read_src(storage, line);
            let args_vec = if arg_names.is_empty() {
                "::std::vec::Vec::new()".to_string()
            } else {
                format!("::std::vec![{}]", arg_names.join(", "))
            };
            return Ok(format!(
                "{{ {preflight}{arg_lets}__nybl_call_named_value(ctx, {callee}, {args_vec}, {name}, {line})? }}",
                name = rust_string_literal(name),
            ));
        }

        let mut body = match name {
            "print" => {
                let args_expr = build_arg_array(&arg_names);
                format!(
                    "{{ let __nybl_message = ::nybl::formatting::__format_values_in(&{args_expr}, \" \", {line}, &ctx.memory)?; __nybl_host_print(ctx, &__nybl_message, {line})?; ::nybl::value::Value::None }}"
                )
            }
            "range" => format!(
                "{{ let __nybl_memory = ctx.memory.clone(); ::nybl::builtins::builtin_range_in(&{}, {}, &mut ctx.rand_state, &__nybl_memory)? }}",
                build_arg_array(&arg_names),
                line
            ),
            "rand" => format!(
                "::nybl::builtins::builtin_rand(&{}, {}, &mut ctx.rand_state)?",
                build_arg_array(&arg_names),
                line
            ),
            "panic" => format!(
                "::nybl::builtins::builtin_panic(&{}, {})?",
                build_arg_array(&arg_names),
                line
            ),
            "try_call" => {
                // `try_call(f)` takes a single callable and
                // dispatches through the preamble's
                // `__nybl_try_call`. We validate the arity at
                // runtime for parity with walker / VM (which
                // both raise there too).
                if arg_names.len() != 1 {
                    format!(
                        "return Err(::nybl::error::NyblError::runtime(format!(\"`try_call` expects 1 argument, but got {{}}\", {}usize), {}))",
                        arg_names.len(),
                        line,
                    )
                } else {
                    format!("__nybl_try_call(ctx, {}, {})?", arg_names[0], line,)
                }
            }
            _ => {
                // Build a separate cloned-args slice for host.call
                // so we can still pass the originals to the user-fn
                // fallback without a double-clone. When there's no
                // user-fn fallback, the originals just drop at end
                // of block.
                let cloned_args = if arg_names.is_empty() {
                    "[]".to_string()
                } else {
                    format!(
                        "[{}]",
                        arg_names
                            .iter()
                            .map(|n| format!("{n}.clone()"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                if self.fn_info.all_fns.contains(name) {
                    let site_args = if arg_names.is_empty() {
                        "::std::vec::Vec::new()".to_string()
                    } else {
                        format!("vec![{}]", arg_names.join(", "))
                    };
                    let fallback = if let Some(site) = self.native_static_function_site(name) {
                        format!(
                            "{{ {} }}",
                            self.reached_function_site_call_src(site, &arg_names, line)
                        )
                    } else {
                        format!(
                            "__nybl_call_active_function(ctx, {}, {}, {}, {})?",
                            rust_string_literal(&self.current_module),
                            rust_string_literal(name),
                            site_args,
                            line,
                        )
                    };
                    if self.opts.sandbox {
                        format!(
                            "match __nybl_host_call(ctx, {name:?}, &{cloned_args}, {line}) {{ Some(r) => r?, None => {fallback}, }}"
                        )
                    } else {
                        format!(
                            "match ctx.host.call({name:?}, &{cloned_args}, {line}) {{ Some(r) => r?, None => {fallback}, }}"
                        )
                    }
                } else {
                    // No fn of that name in scope. Still try the
                    // host first so embedders (e.g. nybl-sys's
                    // readline / file / env builtins) keep working.
                    // Route the generated error through
                    // `nybl::error_messages::function_not_found` so
                    // the text stays in lockstep with walker / VM
                    // without any per-engine string duplication.
                    let hint_fallback = if self.opts.sandbox {
                        format!(
                            "{{ let hint = __nybl_host_function_hint(ctx); if hint.is_empty() {{ ::nybl::error::NyblError::runtime(::nybl::error_messages::function_not_found({name:?}), {line}) }} else {{ let mut e = ::nybl::error::NyblError::runtime(::nybl::error_messages::function_not_found({name:?}), {line}); e.friendly_hint = Some(hint); e }} }}"
                        )
                    } else {
                        format!(
                            "{{ let hint = ctx.host.function_hint(); if hint.is_empty() {{ ::nybl::error::NyblError::runtime(::nybl::error_messages::function_not_found({name:?}), {line}) }} else {{ let mut e = ::nybl::error::NyblError::runtime(::nybl::error_messages::function_not_found({name:?}), {line}); e.friendly_hint = Some(hint.to_string()); e }} }}"
                        )
                    };
                    if self.opts.sandbox {
                        format!(
                            "match __nybl_host_call(ctx, {name:?}, &{cloned_args}, {line}) {{ Some(r) => r?, None => match __nybl_call_active_function(ctx, {module}, {name_lit}, {args}, {line}) {{ Ok(value) => value, Err(error) if error.message == ::nybl::error_messages::function_not_found({name_lit}) => return Err({hint}), Err(error) => return Err(error), }}, }}",
                            name = name,
                            module = rust_string_literal(&self.current_module),
                            name_lit = rust_string_literal(name),
                            args = if arg_names.is_empty() {
                                "::std::vec::Vec::new()".to_string()
                            } else {
                                format!("::std::vec![{}]", arg_names.join(", "))
                            },
                            hint = hint_fallback,
                        )
                    } else {
                        format!(
                            "match ctx.host.call({name:?}, &{cloned_args}, {line}) {{ Some(r) => r?, None => return Err({hint_fallback}), }}"
                        )
                    }
                }
            }
        };

        let args_vec = if arg_names.is_empty() {
            "::std::vec::Vec::<::nybl::value::Value>::new()".to_string()
        } else {
            format!("::std::vec![{}]", arg_names.join(", "))
        };
        // The rebinding probe is only semantically meaningful for names some
        // top-level `let` or import in this module could ever shadow. Every
        // other call — builtins in import-free programs, user fns nothing
        // overlaps — dispatches directly.
        if self.call_may_be_rebound(name) {
            body = format!(
                "match __nybl_binding_value(ctx, {module}, {name}) {{ ::std::option::Option::Some(__callee) => __nybl_call_named_value(ctx, __callee, {args_vec}, {name}, {line})?, ::std::option::Option::None => {{ {body} }}, }}",
                module = rust_string_literal(&self.current_module),
                name = rust_string_literal(name),
            );
        }
        if let Some(storage @ BindingStorage::OptionalImport { .. }) = binding_storage.as_ref() {
            let optional = self
                .storage_optional_value_src(storage, line)
                .expect("optional import storage");
            body = format!(
                "match {optional} {{ ::std::option::Option::Some(__callee) => __nybl_call_named_value(ctx, __callee, {args_vec}, {name}, {line})?, ::std::option::Option::None => {{ {body} }}, }}",
                name = rust_string_literal(name),
            );
        }

        if let Some(alias) = self.declaration_alias_optional_src(name) {
            let args_vec = if arg_names.is_empty() {
                "::std::vec::Vec::<::nybl::value::Value>::new()".to_string()
            } else {
                format!("vec![{}]", arg_names.join(", "))
            };
            return Ok(format!(
                "{{ {arg_lets}match {alias} {{ ::std::option::Option::Some(__callee) => __nybl_call_named_value(ctx, __callee, {args_vec}, {name}, {line})?, ::std::option::Option::None => {{ {body} }}, }} }}",
                name = rust_string_literal(name),
            ));
        }

        Ok(format!("{{ {preflight}{arg_lets}{body} }}"))
    }

    fn array_src(&mut self, items: &[Expr], line: u32) -> Result<String, NyblError> {
        if items.is_empty() {
            return Ok(format!(
                "::nybl::value::Value::__try_new_array_in(::std::vec::Vec::new(), {line}, &ctx.memory)?"
            ));
        }
        let mut lets = String::new();
        let mut names = Vec::with_capacity(items.len());
        for item in items {
            let src = self.expr_src(item)?;
            let tmp = self.fresh_tmp();
            write!(lets, "let {tmp} = {src}; ").unwrap();
            names.push(tmp);
        }
        Ok(format!(
            "{{ {}::nybl::value::Value::__try_new_array_in(vec![{}], {}, &ctx.memory)? }}",
            lets,
            names.join(", "),
            line
        ))
    }

    fn dict_src(&mut self, entries: &[(String, Expr)], line: u32) -> Result<String, NyblError> {
        if entries.is_empty() {
            return Ok(format!(
                "::nybl::value::Value::__try_new_dict_in(::std::vec::Vec::new(), {line}, &ctx.memory)?"
            ));
        }
        let mut lets = String::new();
        let mut pairs = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            let src = self.expr_src(value)?;
            let tmp = self.fresh_tmp();
            write!(lets, "let {tmp} = {src}; ").unwrap();
            pairs.push(format!(
                "({}.to_string(), {})",
                rust_string_literal(key),
                tmp
            ));
        }
        Ok(format!(
            "{{ {}::nybl::value::Value::__try_new_dict_in(vec![{}], {}, &ctx.memory)? }}",
            lets,
            pairs.join(", "),
            line
        ))
    }

    fn struct_construct_src(
        &mut self,
        namespace: Option<&str>,
        type_name: &str,
        fields: &[(String, Expr)],
        line: u32,
    ) -> Result<String, NyblError> {
        if let Some(namespace) =
            namespace.filter(|namespace| self.is_dynamic_namespace_local(namespace))
        {
            let namespace_value = if self.is_local(namespace) {
                rust_user_ident(namespace)
            } else if let Some(value) = self.optional_module_namespace_src(namespace, line) {
                value
            } else {
                self.declaration_alias_namespace_src(namespace, line)
                    .unwrap_or(self.ident_value_src(namespace, line)?)
            };
            let module_path_src = format!(
                "__nybl_validate_namespace_type(&{}, {}, {}, {})?",
                namespace_value,
                rust_string_literal(namespace),
                rust_string_literal(type_name),
                line,
            );
            return self.runtime_struct_construct_src(&module_path_src, type_name, fields, line);
        }
        if namespace.is_none() {
            let module_path_src = format!(
                "__nybl_resolve_bare_type(&__nybl_type_bindings, {}, true, {})?",
                rust_string_literal(type_name),
                line,
            );
            return self.runtime_struct_construct_src(&module_path_src, type_name, fields, line);
        }
        let module_path = self
            .resolve_type_module(namespace, type_name)
            .ok_or_else(|| {
                NyblError::runtime(nybl::error_messages::struct_not_declared(type_name), line)
            })?;
        let ns = namespace.expect("namespaced branch");
        let namespace_value = if self.is_local(ns) {
            rust_user_ident(ns)
        } else if let Some(value) = self.declaration_alias_namespace_src(ns, line) {
            value
        } else {
            self.ident_value_src(ns, line)?
        };
        let module_path_src = format!(
            "{{ __nybl_validate_namespace_type(&{namespace_value}, {alias}, {type_name}, {line})?; {module_path}.to_string() }}",
            alias = rust_string_literal(ns),
            type_name = rust_string_literal(type_name),
            module_path = rust_string_literal(&module_path),
        );
        self.runtime_struct_construct_src(&module_path_src, type_name, fields, line)
    }

    fn runtime_struct_construct_src(
        &mut self,
        module_path_src: &str,
        type_name: &str,
        fields: &[(String, Expr)],
        line: u32,
    ) -> Result<String, NyblError> {
        let provided_names = fields
            .iter()
            .map(|(name, _)| rust_string_literal(name))
            .collect::<Vec<_>>()
            .join(", ");
        let mut lets = String::new();
        let mut provided = Vec::with_capacity(fields.len());
        for (name, expr) in fields {
            let src = self.expr_src(expr)?;
            let tmp = self.fresh_tmp();
            write!(lets, "let {tmp} = {src}; ").unwrap();
            provided.push(format!(
                "({}.to_string(), {})",
                rust_string_literal(name),
                tmp
            ));
        }
        Ok(format!(
            "{{ let __module_path = {module_path_src}; \
                let __declared_fields = __nybl_struct_fields(ctx, &__module_path, {type_name}, {line})?; \
                __nybl_validate_named_fields(__declared_fields, &[{provided_names}], {type_name}, ::std::option::Option::None, {line})?; \
                {lets}let __provided = vec![{provided}]; \
                let __ordered = __nybl_order_named_fields(__declared_fields, __provided); \
                ::nybl::value::Value::__try_new_struct_in(__module_path, {type_name}.to_string(), __ordered, {line}, &ctx.memory)? }}",
            module_path_src = module_path_src,
            type_name = rust_string_literal(type_name),
            provided_names = provided_names,
            lets = lets,
            provided = provided.join(", "),
            line = line,
        ))
    }

    fn enum_construct_src(
        &mut self,
        namespace: Option<&str>,
        type_name: &str,
        variant: &str,
        payload: &VariantPayload,
        line: u32,
    ) -> Result<String, NyblError> {
        if let Some(namespace) =
            namespace.filter(|namespace| self.is_dynamic_namespace_local(namespace))
        {
            let namespace_value = if self.is_local(namespace) {
                rust_user_ident(namespace)
            } else if let Some(value) = self.optional_module_namespace_src(namespace, line) {
                value
            } else {
                self.declaration_alias_namespace_src(namespace, line)
                    .unwrap_or(self.ident_value_src(namespace, line)?)
            };
            let module_path_src = format!(
                "__nybl_validate_namespace_type(&{}, {}, {}, {})?",
                namespace_value,
                rust_string_literal(namespace),
                rust_string_literal(type_name),
                line,
            );
            return self.runtime_enum_construct_src(
                &module_path_src,
                type_name,
                variant,
                payload,
                line,
            );
        }
        if namespace.is_none() {
            let module_path_src = format!(
                "__nybl_resolve_bare_type(&__nybl_type_bindings, {}, false, {})?",
                rust_string_literal(type_name),
                line,
            );
            return self.runtime_enum_construct_src(
                &module_path_src,
                type_name,
                variant,
                payload,
                line,
            );
        }
        let module_path = self
            .resolve_type_module(namespace, type_name)
            .ok_or_else(|| {
                NyblError::runtime(nybl::error_messages::enum_not_declared(type_name), line)
            })?;
        let ns = namespace.expect("namespaced branch");
        let namespace_value = if self.is_local(ns) {
            rust_user_ident(ns)
        } else if let Some(value) = self.declaration_alias_namespace_src(ns, line) {
            value
        } else {
            self.ident_value_src(ns, line)?
        };
        let module_path_src = format!(
            "{{ __nybl_validate_namespace_type(&{namespace_value}, {alias}, {type_name}, {line})?; {module_path}.to_string() }}",
            alias = rust_string_literal(ns),
            type_name = rust_string_literal(type_name),
            module_path = rust_string_literal(&module_path),
        );
        self.runtime_enum_construct_src(&module_path_src, type_name, variant, payload, line)
    }

    fn runtime_enum_construct_src(
        &mut self,
        module_path_src: &str,
        type_name: &str,
        variant: &str,
        payload: &VariantPayload,
        line: u32,
    ) -> Result<String, NyblError> {
        let namespace_check = format!(
            "let __module_path = {}; \
             let __variant_shape = __nybl_enum_variant_shape(ctx, &__module_path, {}, {}, {})?; ",
            module_path_src,
            rust_string_literal(type_name),
            rust_string_literal(variant),
            line,
        );
        let type_lit = rust_string_literal(type_name);
        let variant_lit = rust_string_literal(variant);
        match payload {
            VariantPayload::Unit => Ok(format!(
                "{{ {namespace_check}match __variant_shape {{ \
                    __NyblDynamicVariantShape::Unit => {{}}, \
                    __NyblDynamicVariantShape::Tuple(_) => return Err(::nybl::error::NyblError::runtime(format!(\"Variant `{{}}::{{}}` expects positional arguments `(…)`\", {type_lit}, {variant_lit}), {line})), \
                    __NyblDynamicVariantShape::Struct(_) => return Err(::nybl::error::NyblError::runtime(format!(\"Variant `{{}}::{{}}` expects named fields `{{{{ … }}}}`\", {type_lit}, {variant_lit}), {line})), \
                }} ::nybl::value::Value::__new_enum_unit_in(__module_path, {type_lit}.to_string(), {variant_lit}.to_string(), &ctx.memory) }}"
            )),
            VariantPayload::Tuple(args) => {
                let mut lets = String::new();
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    let src = self.expr_src(arg)?;
                    let tmp = self.fresh_tmp();
                    write!(lets, "let {tmp} = {src}; ").unwrap();
                    values.push(tmp);
                }
                Ok(format!(
                    "{{ {namespace_check}match __variant_shape {{ \
                        __NyblDynamicVariantShape::Unit => return Err(::nybl::error::NyblError::runtime(format!(\"Variant `{{}}::{{}}` takes no payload\", {type_lit}, {variant_lit}), {line})), \
                        __NyblDynamicVariantShape::Tuple(__fields) if __fields.len() == {actual} => {{}}, \
                        __NyblDynamicVariantShape::Tuple(__fields) => return Err(::nybl::error::NyblError::runtime(format!(\"`{{}}::{{}}` expects {{}} argument{{}}, but got {{}}\", {type_lit}, {variant_lit}, __fields.len(), if __fields.len() == 1 {{ \"\" }} else {{ \"s\" }}, {actual}), {line})), \
                        __NyblDynamicVariantShape::Struct(_) => return Err(::nybl::error::NyblError::runtime(format!(\"Variant `{{}}::{{}}` expects named fields `{{{{ … }}}}`\", {type_lit}, {variant_lit}), {line})), \
                    }} {lets}::nybl::value::Value::__try_new_enum_tuple_in(__module_path, {type_lit}.to_string(), {variant_lit}.to_string(), vec![{values}], {line}, &ctx.memory)? }}",
                    actual = args.len(),
                    values = values.join(", "),
                ))
            }
            VariantPayload::Struct(fields) => {
                let provided_names = fields
                    .iter()
                    .map(|(name, _)| rust_string_literal(name))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut lets = String::new();
                let mut provided = Vec::with_capacity(fields.len());
                for (name, expr) in fields {
                    let src = self.expr_src(expr)?;
                    let tmp = self.fresh_tmp();
                    write!(lets, "let {tmp} = {src}; ").unwrap();
                    provided.push(format!(
                        "({}.to_string(), {})",
                        rust_string_literal(name),
                        tmp
                    ));
                }
                Ok(format!(
                    "{{ {namespace_check}let __declared_fields = match __variant_shape {{ \
                        __NyblDynamicVariantShape::Unit => return Err(::nybl::error::NyblError::runtime(format!(\"Variant `{{}}::{{}}` takes no payload\", {type_lit}, {variant_lit}), {line})), \
                        __NyblDynamicVariantShape::Tuple(_) => return Err(::nybl::error::NyblError::runtime(format!(\"Variant `{{}}::{{}}` expects positional arguments `(…)`\", {type_lit}, {variant_lit}), {line})), \
                        __NyblDynamicVariantShape::Struct(__fields) => __fields, \
                    }}; __nybl_validate_named_fields(__declared_fields, &[{provided_names}], {type_lit}, ::std::option::Option::Some({variant_lit}), {line})?; \
                    {lets}let __provided = vec![{provided}]; let __ordered = __nybl_order_named_fields(__declared_fields, __provided); \
                    ::nybl::value::Value::__try_new_enum_struct_in(__module_path, {type_lit}.to_string(), {variant_lit}.to_string(), __ordered, {line}, &ctx.memory)? }}",
                    provided = provided.join(", "),
                ))
            }
        }
    }

    fn string_interp_src(&mut self, parts: &[StringPart], line: u32) -> Result<String, NyblError> {
        // Mirror the tree-walker: for each Variable part, resolve the
        // current Nybl value at the point where that part executes.
        let mut body = String::from("{ let mut __s = ::std::string::String::new(); ");
        for part in parts {
            match part {
                StringPart::Literal(s) => {
                    write!(body, "__s.push_str({}); ", rust_string_literal(s)).unwrap();
                }
                StringPart::Variable(name) => {
                    let value = self.ident_value_src(name, line)?;
                    // The cloned Value lives only for the duration
                    // of the format call; its Drop tracks the
                    // de-alloc correctly against `nybl::memory`.
                    write!(body, "__s.push_str(&format!(\"{{}}\", {value})); ").unwrap();
                }
            }
        }
        body.push_str("::nybl::value::Value::__new_str_in(__s, &ctx.memory) }");
        Ok(body)
    }

    fn nested_place_method_call_src(
        &mut self,
        receiver_place: AotPlace,
        method: &str,
        args: &[CallArg],
        line: u32,
    ) -> Result<String, NyblError> {
        let modes = args
            .iter()
            .map(|arg| match arg.mode {
                ParamMode::Value => "::nybl::parser::ParamMode::Value",
                ParamMode::Ref => "::nybl::parser::ParamMode::Ref",
                ParamMode::Rest => "::nybl::parser::ParamMode::Value",
            })
            .collect::<Vec<_>>()
            .join(", ");
        let method_lit = rust_string_literal(method);
        let (receiver_fence, receiver_target) =
            self.emitted_place_target(receiver_place.clone(), line);
        let (receiver_projection_lets, receiver_path) = self.place_steps_src(&receiver_place)?;
        let receiver_pre_root = self.fresh_tmp();
        let receiver_pre_value = self.fresh_tmp();
        let user_method = self.fresh_tmp();
        let module_callable = self.fresh_tmp();
        let receiver_mutates = self.fresh_tmp();

        let mut fences = String::new();
        let mut seen = HashSet::new();
        let mut targets: Vec<Option<EmittedPlaceTarget>> = vec![None; args.len()];
        for (index, arg) in args.iter().enumerate() {
            if arg.mode != ParamMode::Ref {
                continue;
            }
            let position = index + 1;
            let Some(place) = aot_place(&arg.value) else {
                write!(
                    fences,
                    "return Err(::nybl::ref_params::invalid_ref_target({position}, {line})); "
                )
                .unwrap();
                continue;
            };
            let name = &place.root;
            if !seen.insert(name.clone()) {
                write!(
                    fences,
                    "return Err(::nybl::ref_params::duplicate_ref_target({line})); "
                )
                .unwrap();
                continue;
            }
            if self.is_constant_binding(name) {
                write!(
                    fences,
                    "return Err(::nybl::error_messages::constant_mutation_error({}, {line})); ",
                    rust_string_literal(name),
                )
                .unwrap();
                continue;
            }
            if self.is_captured_binding(name) {
                write!(
                    fences,
                    "return Err(::nybl::ref_params::captured_ref_target({position}, {line})); "
                )
                .unwrap();
                continue;
            }
            let (preflight, target) = self.emitted_place_target(place, line);
            fences.push_str(&preflight);
            targets[index] = Some(target);
        }

        let receiver_guard = if self.is_constant_binding(&receiver_place.root) {
            format!(
                "if {receiver_mutates} {{ return Err(::nybl::error_messages::constant_mutation_error({}, {line})); }} ",
                rust_string_literal(&receiver_place.root),
            )
        } else if self.is_captured_binding(&receiver_place.root) {
            format!(
                "if {receiver_mutates} {{ return Err(::nybl::ref_params::captured_ref_target(1, {line})); }} "
            )
        } else {
            String::new()
        };
        let duplicate_receiver_guard = if seen.contains(&receiver_place.root) {
            format!(
                "if {receiver_mutates} {{ return Err(::nybl::ref_params::duplicate_ref_target({line})); }} "
            )
        } else {
            String::new()
        };

        let mut ordinary = String::new();
        let mut names: Vec<Option<String>> = vec![None; args.len()];
        for (index, arg) in args.iter().enumerate() {
            if arg.mode == ParamMode::Ref {
                continue;
            }
            let src = self.expr_src(&arg.value)?;
            let tmp = self.fresh_tmp();
            write!(ordinary, "let {tmp} = {src}; ").unwrap();
            names[index] = Some(tmp);
        }

        // Mutable/ref-self receivers snapshot after ordinary arguments; value
        // receivers retain the value produced by the callee expression.
        let receiver_root = self.fresh_tmp();
        let receiver_value = self.fresh_tmp();
        let receiver_snapshot = format!(
            "let {receiver_root} = if {receiver_mutates} {{ {read} }} else {{ {receiver_pre_root}.clone() }}; let {receiver_value} = if {receiver_mutates} {{ __nybl_place_get(&{receiver_root}, &{receiver_path}, {line}, &ctx.memory)? }} else {{ {receiver_pre_value}.clone() }}; ",
            read = receiver_target.root_read,
        );

        let mut snapshots = String::new();
        let mut resolved: Vec<Option<(String, String, String)>> = vec![None; args.len()];
        for (index, target) in targets.iter().enumerate() {
            let Some(target) = target else { continue };
            let (projection_lets, path) = self.place_steps_src(&target.place)?;
            let root = self.fresh_tmp();
            let leaf = self.fresh_tmp();
            write!(
                snapshots,
                "{projection_lets}let {root} = {read}; let {leaf} = __nybl_place_get(&{root}, &{path}, {line}, &ctx.memory)?; ",
                read = target.root_read,
            )
            .unwrap();
            names[index] = Some(leaf);
            resolved[index] = Some((root, path, target.root_write.clone()));
        }
        let values = names
            .iter()
            .map(|name| {
                name.clone()
                    .unwrap_or_else(|| "::nybl::value::Value::None".to_string())
            })
            .collect::<Vec<_>>()
            .join(", ");
        let args_vec = format!("::std::vec![{values}]");
        let outcome = self.fresh_tmp();
        let receiver_candidate = self.fresh_tmp();
        let result = self.fresh_tmp();
        let mut candidates = String::new();
        let mut commits = String::new();
        for (index, target) in resolved.iter().enumerate() {
            let Some((root, path, write_target)) = target else {
                continue;
            };
            let candidate = self.fresh_tmp();
            write!(
                candidates,
                "let {candidate} = __nybl_place_set(&ctx.memory, {root}.clone(), &{path}, __nybl_staged_args[{index}].clone(), {line})?; "
            )
            .unwrap();
            write!(
                commits,
                "{{ let __target: &mut ::nybl::value::Value = {write_target}; *__target = {candidate}; }} "
            )
            .unwrap();
        }
        let memory_check = if self.opts.sandbox {
            format!("__nybl_check_memory(ctx, {line})?; ")
        } else {
            String::new()
        };
        let builtin_mutates = methods::is_mutating_method(method);
        let builtin_arity = methods::builtin_method_arity(method).map_or(String::new(), |expected| {
            format!(
                "if {user_method}.is_none() && {module_callable}.is_none() && {}usize != {expected}usize {{ return Err(::nybl::error::NyblError::runtime({}, {line})); }} ",
                args.len(),
                rust_string_literal(&format!(
                    "`.{}()` needs {} argument{}",
                    method,
                    expected,
                    if expected == 1 { "" } else { "s" }
                )),
            )
        });

        Ok(format!(
            "{{ {receiver_fence}{receiver_projection_lets}let {receiver_pre_root} = {receiver_read}; let {receiver_pre_value} = __nybl_place_get(&{receiver_pre_root}, &{receiver_path}, {line}, &ctx.memory)?; \
             let {user_method} = __nybl_preflight_user_method(ctx, &{receiver_pre_value}, {method_lit}, &[{modes}], true, {line})?; \
             let {module_callable} = if {user_method}.is_some() {{ ::std::option::Option::None }} else {{ __nybl_preflight_module_call(ctx, &{receiver_pre_value}, {method_lit}, &[{modes}], {line})? }}; \
             if {user_method}.is_none() && {module_callable}.is_none() {{ ::nybl::validate_value_only_call_modes({method_lit}, &[{modes}], {line})?; }} \
             {builtin_arity}let {receiver_mutates} = {user_method}.is_some_and(__nybl_user_method_receiver_is_ref) || ({user_method}.is_none() && {module_callable}.is_none() && {builtin_mutates}); \
             {receiver_guard}{duplicate_receiver_guard}{fences}{ordinary}{receiver_snapshot}{snapshots}\
             let ({result}, __nybl_staged_args, __nybl_staged_receiver) = if let ::std::option::Option::Some(__nybl_method_adapter) = {user_method} {{ \
                 let {outcome} = __nybl_call_preflighted_user_method_outcome(ctx, __nybl_method_adapter, {receiver_value}.clone(), {args_vec}, {line})?; \
                 let __receiver = if __nybl_user_method_receiver_is_ref(__nybl_method_adapter) {{ ::std::option::Option::Some({outcome}.receiver) }} else {{ ::std::option::Option::None }}; \
                 ({outcome}.value, {outcome}.args, __receiver) \
             }} else if let ::std::option::Option::Some(__nybl_module_callable) = {module_callable} {{ \
                 let {outcome} = __nybl_call_value(ctx, __nybl_module_callable, {args_vec}, {line})?; ({outcome}.value, {outcome}.args, ::std::option::Option::None) \
             }} else {{ let (__value, __receiver) = __nybl_call_method(ctx, &{receiver_value}, {method_lit}, &[{values}], {line})?; (__value, ::std::vec::Vec::new(), __receiver) }}; \
             let {receiver_candidate} = match __nybl_staged_receiver {{ ::std::option::Option::Some(__value) => ::std::option::Option::Some(__nybl_place_set(&ctx.memory, {receiver_root}.clone(), &{receiver_path}, __value, {line})?), ::std::option::Option::None => ::std::option::Option::None }}; \
             {candidates}{memory_check}if let ::std::option::Option::Some(__root) = {receiver_candidate} {{ let __target: &mut ::nybl::value::Value = {receiver_write}; *__target = __root; }} {commits}{result} }}",
            receiver_read = receiver_target.root_read,
            receiver_write = receiver_target.root_write,
        ))
    }

    fn method_call_src(
        &mut self,
        object: &Expr,
        method: &str,
        args: &[CallArg],
        line: u32,
    ) -> Result<String, NyblError> {
        if let Some(place) = aot_place(object)
            && !place.projections.is_empty()
        {
            return self.nested_place_method_call_src(place, method, args, line);
        }
        if args.iter().any(|arg| arg.mode == ParamMode::Ref) {
            let object_src = self.expr_src(object)?;
            let object_tmp = self.fresh_tmp();
            let user_method = self.fresh_tmp();
            let modes = args
                .iter()
                .map(|arg| match arg.mode {
                    ParamMode::Value => "::nybl::parser::ParamMode::Value",
                    ParamMode::Ref => "::nybl::parser::ParamMode::Ref",
                    ParamMode::Rest => "::nybl::parser::ParamMode::Value",
                })
                .collect::<Vec<_>>()
                .join(", ");
            let mut fences = String::new();
            let mut seen = HashSet::new();
            let mut targets: Vec<Option<EmittedPlaceTarget>> = vec![None; args.len()];
            for (index, arg) in args.iter().enumerate() {
                if arg.mode != ParamMode::Ref {
                    continue;
                }
                let position = index + 1;
                let Some(place) = aot_place(&arg.value) else {
                    write!(
                        fences,
                        "return Err(::nybl::ref_params::invalid_ref_target({position}, {line})); "
                    )
                    .unwrap();
                    continue;
                };
                let name = &place.root;
                if !seen.insert(name.clone()) {
                    write!(
                        fences,
                        "return Err(::nybl::ref_params::duplicate_ref_target({line})); "
                    )
                    .unwrap();
                    continue;
                }
                if self.is_constant_binding(name) {
                    write!(
                        fences,
                        "return Err(::nybl::error_messages::constant_mutation_error({}, {line})); ",
                        rust_string_literal(name),
                    )
                    .unwrap();
                    continue;
                }
                if self.is_captured_binding(name) {
                    write!(
                        fences,
                        "return Err(::nybl::ref_params::captured_ref_target({position}, {line})); "
                    )
                    .unwrap();
                    continue;
                }
                let (target_preflight, target) = self.emitted_place_target(place, line);
                fences.push_str(&target_preflight);
                targets[index] = Some(target);
            }
            let mut ordinary = String::new();
            let mut names: Vec<Option<String>> = vec![None; args.len()];
            for (index, arg) in args.iter().enumerate() {
                if arg.mode == ParamMode::Ref {
                    continue;
                }
                let src = self.expr_src(&arg.value)?;
                let tmp = self.fresh_tmp();
                write!(ordinary, "let {tmp} = {src}; ").unwrap();
                names[index] = Some(tmp);
            }
            let mut snapshots = String::new();
            let mut resolved: Vec<Option<(String, String, String)>> = vec![None; args.len()];
            for (index, target) in targets.iter().enumerate() {
                let Some(target) = target else {
                    continue;
                };
                let (projection_lets, path) = self.place_steps_src(&target.place)?;
                let root = self.fresh_tmp();
                let leaf = self.fresh_tmp();
                write!(
                    snapshots,
                    "{projection_lets}let {root} = {read}; let {leaf} = __nybl_place_get(&{root}, &{path}, {line}, &ctx.memory)?; ",
                    read = target.root_read,
                )
                .unwrap();
                names[index] = Some(leaf);
                resolved[index] = Some((root, path, target.root_write.clone()));
            }
            let values = names
                .into_iter()
                .map(|name| name.unwrap_or_else(|| "::nybl::value::Value::None".to_string()))
                .collect::<Vec<_>>()
                .join(", ");
            let outcome = self.fresh_tmp();
            let module_callable = self.fresh_tmp();
            let mut candidates = String::new();
            let mut commits = String::new();
            for (index, target) in resolved.iter().enumerate() {
                let Some((root, path, write_target)) = target else {
                    continue;
                };
                let candidate = self.fresh_tmp();
                write!(
                    candidates,
                    "let {candidate} = __nybl_place_set(&ctx.memory, {root}.clone(), &{path}, __nybl_staged_args[{index}].clone(), {line})?; "
                )
                .unwrap();
                write!(
                    commits,
                    "{{ let __target: &mut ::nybl::value::Value = {write_target}; *__target = {candidate}; }} "
                )
                .unwrap();
            }
            let memory_check = if self.opts.sandbox {
                format!("__nybl_check_memory(ctx, {line})?; ")
            } else {
                String::new()
            };
            let (receiver_fence, receiver_snapshot, receiver_commit) = if let ExprKind::Ident(
                name,
            ) = &object.kind
            {
                let storage = self.binding_storage(name).or_else(|| {
                    (!self.fn_info.all_fns.contains(name)).then(|| BindingStorage::Persistent {
                        module: self.current_module.clone(),
                        name: name.clone(),
                    })
                });
                let Some(storage) = storage else {
                    return Ok(format!(
                        "{{ let {object_tmp} = {object_src}; \
                             let {user_method} = __nybl_preflight_user_method(ctx, &{object_tmp}, {method}, &[{modes}], true, {line})?; \
                             let {module_callable} = if {user_method}.is_some() {{ ::std::option::Option::None }} else {{ __nybl_preflight_module_call(ctx, &{object_tmp}, {method}, &[{modes}], {line})? }}; \
                             if {user_method}.is_none() && {module_callable}.is_none() {{ ::nybl::validate_value_only_call_modes({method}, &[{modes}], {line})?; unreachable!(); }} \
                             {fences}{ordinary}{snapshots}\
                             let (__nybl_result, __nybl_staged_args) = if let ::std::option::Option::Some(__nybl_method_adapter) = {user_method} {{ \
                                 let {outcome} = __nybl_call_preflighted_user_method_outcome(ctx, __nybl_method_adapter, {object_tmp}.clone(), vec![{values}], {line})?; ({outcome}.value, {outcome}.args) \
                             }} else {{ let {outcome} = __nybl_call_value(ctx, {module_callable}.expect(\"preflight selected a live module export\"), vec![{values}], {line})?; ({outcome}.value, {outcome}.args) }}; \
                             {candidates}{memory_check}{commits}__nybl_result }}",
                        method = rust_string_literal(method),
                    ));
                };
                let (target_preflight, target_read, target_write) =
                    self.ref_target_sources(&storage, name, line);
                let mut receiver_fence = target_preflight;
                if self.is_constant_binding(name) {
                    write!(
                            receiver_fence,
                            "if {user_method}.is_some_and(__nybl_user_method_receiver_is_ref) {{ return Err(::nybl::error_messages::constant_mutation_error({}, {line})); }} ",
                            rust_string_literal(name),
                        )
                        .unwrap();
                }
                if self.is_captured_binding(name) {
                    write!(
                            receiver_fence,
                            "if {user_method}.is_some_and(__nybl_user_method_receiver_is_ref) {{ return Err(::nybl::ref_params::captured_ref_target(1, {line})); }} "
                        )
                        .unwrap();
                }
                if seen.contains(name) {
                    write!(
                            receiver_fence,
                            "if {user_method}.is_some_and(__nybl_user_method_receiver_is_ref) {{ return Err(::nybl::ref_params::duplicate_ref_target({line})); }} "
                        )
                        .unwrap();
                }
                (
                    receiver_fence,
                    format!(
                        "let __nybl_receiver_snapshot = if {user_method}.is_some_and(__nybl_user_method_receiver_is_ref) {{ ::std::option::Option::Some({target_read}) }} else {{ ::std::option::Option::None }}; "
                    ),
                    format!(
                        "if let ::std::option::Option::Some(__nybl_staged_receiver) = __nybl_staged_receiver {{ let __target: &mut ::nybl::value::Value = {target_write}; *__target = __nybl_staged_receiver; }} "
                    ),
                )
            } else {
                (String::new(), String::new(), String::new())
            };
            let user_receiver = if matches!(&object.kind, ExprKind::Ident(_)) {
                format!("__nybl_receiver_snapshot.unwrap_or_else(|| {object_tmp}.clone())")
            } else {
                format!("{object_tmp}.clone()")
            };
            return Ok(format!(
                "{{ let {object_tmp} = {object_src}; \
                 let {user_method} = __nybl_preflight_user_method(ctx, &{object_tmp}, {method}, &[{modes}], {receiver_is_place}, {line})?; \
                 let {module_callable} = if {user_method}.is_some() {{ ::std::option::Option::None }} else {{ __nybl_preflight_module_call(ctx, &{object_tmp}, {method}, &[{modes}], {line})? }}; \
                 if {user_method}.is_none() && {module_callable}.is_none() {{ \
                     ::nybl::validate_value_only_call_modes({method}, &[{modes}], {line})?; \
                     unreachable!(); \
                 }} \
                 {receiver_fence}{fences}{ordinary}{receiver_snapshot}{snapshots}\
                 let (__nybl_result, __nybl_staged_args, __nybl_staged_receiver) = if let ::std::option::Option::Some(__nybl_method_adapter) = {user_method} {{ \
                     let {outcome} = __nybl_call_preflighted_user_method_outcome(ctx, __nybl_method_adapter, {user_receiver}, vec![{values}], {line})?; \
                     let __nybl_receiver = if __nybl_user_method_receiver_is_ref(__nybl_method_adapter) {{ ::std::option::Option::Some({outcome}.receiver) }} else {{ ::std::option::Option::None }}; \
                     ({outcome}.value, {outcome}.args, __nybl_receiver) \
                 }} else {{ \
                     let {outcome} = __nybl_call_value(ctx, {module_callable}.expect(\"preflight selected a live module export\"), vec![{values}], {line})?; \
                     ({outcome}.value, {outcome}.args, ::std::option::Option::None) \
                 }}; \
                 {candidates}{memory_check}{receiver_commit}{commits}__nybl_result }}",
                method = rust_string_literal(method),
                receiver_is_place = matches!(&object.kind, ExprKind::Ident(_)),
            ));
        }
        let mutating = methods::is_mutating_method(method);
        let named_mutating = mutating && matches!(&object.kind, ExprKind::Ident(_));
        let mut resolved_object = None;
        let mut resolved_user_method = None;
        let mut resolved_module_callee = None;
        let mut named_user_method = None;
        let mut named_user_receiver = None;
        let mut named_user_ref_target: Option<(String, String)> = None;
        let mut object_preflight = String::new();
        if !named_mutating {
            let src = self.expr_src(object)?;
            let tmp = self.fresh_tmp();
            let user_tmp = self.fresh_tmp();
            let module_tmp = self.fresh_tmp();
            let value_modes = args
                .iter()
                .map(|_| "::nybl::parser::ParamMode::Value")
                .collect::<Vec<_>>()
                .join(", ");
            write!(
                object_preflight,
                "let {tmp} = {src}; let {user_tmp} = __nybl_preflight_user_method(ctx, &{tmp}, {method}, &[{value_modes}], {receiver_is_place}, {line})?; let {module_tmp} = if {user_tmp}.is_some() {{ ::std::option::Option::None }} else {{ __nybl_preflight_module_call(ctx, &{tmp}, {method}, &[{value_modes}], {line})? }}; ",
                method = rust_string_literal(method),
                receiver_is_place = matches!(&object.kind, ExprKind::Ident(_)),
            )
            .unwrap();
            if let ExprKind::Ident(name) = &object.kind {
                let storage = self.binding_storage(name).or_else(|| {
                    (!self.fn_info.all_fns.contains(name)).then(|| BindingStorage::Persistent {
                        module: self.current_module.clone(),
                        name: name.clone(),
                    })
                });
                if let Some(storage) = storage {
                    let (target_preflight, target_read, target_write) =
                        self.ref_target_sources(&storage, name, line);
                    object_preflight.push_str(&target_preflight);
                    if self.is_constant_binding(name) {
                        write!(
                            object_preflight,
                            "if {user_tmp}.is_some_and(__nybl_user_method_receiver_is_ref) {{ return Err(::nybl::error_messages::constant_mutation_error({}, {line})); }} ",
                            rust_string_literal(name),
                        )
                        .unwrap();
                    }
                    if self.is_captured_binding(name) {
                        write!(
                            object_preflight,
                            "if {user_tmp}.is_some_and(__nybl_user_method_receiver_is_ref) {{ return Err(::nybl::ref_params::captured_ref_target(1, {line})); }} "
                        )
                        .unwrap();
                    }
                    named_user_ref_target = Some((target_read, target_write));
                }
            }
            if let Some(expected) = methods::builtin_method_arity(method) {
                write!(
                    object_preflight,
                    "if {user_tmp}.is_none() && {module_tmp}.is_none() {{ ::nybl::validate_value_only_call_modes({method}, &[{}], {line})?; if {actual}usize != {expected}usize {{ return Err(::nybl::error::NyblError::runtime({message}, {line})); }} }} ",
                    args.iter()
                        .map(|_| "::nybl::parser::ParamMode::Value")
                        .collect::<Vec<_>>()
                        .join(", "),
                    actual = args.len(),
                    method = rust_string_literal(method),
                    message = rust_string_literal(&format!(
                        "`.{}()` needs {} argument{}",
                        method,
                        expected,
                        if expected == 1 { "" } else { "s" }
                    )),
                )
                .unwrap();
            }
            if mutating
                && matches!(
                    &object.kind,
                    ExprKind::Index { .. } | ExprKind::FieldAccess { .. }
                )
            {
                write!(
                    object_preflight,
                    "::nybl::methods::reject_nested_array_mutation(&{tmp}, {}, {line})?; ",
                    rust_string_literal(method),
                )
                .unwrap();
            }
            resolved_object = Some(tmp);
            resolved_user_method = Some(user_tmp);
            resolved_module_callee = Some(module_tmp);
        }

        // Ordinary arguments are evaluated only after receiver/callable
        // preflight. A named implicit-ref receiver is the exception: its value
        // is deliberately snapshotted by the transactional helper after args.
        let mut arg_tmps = Vec::with_capacity(args.len());
        let mut arg_lets = object_preflight;
        if named_mutating {
            let ExprKind::Ident(target_name) = &object.kind else {
                unreachable!("named mutating receiver is an identifier");
            };
            let receiver = if let Some(BindingStorage::RustLocal { ident }) =
                self.binding_storage(target_name)
            {
                format!("&{ident}")
            } else {
                let storage = self.binding_storage(target_name).unwrap_or_else(|| {
                    BindingStorage::Persistent {
                        module: self.current_module.clone(),
                        name: target_name.clone(),
                    }
                });
                match storage {
                    BindingStorage::Persistent { module, name } => format!(
                        "__nybl_binding_entry(ctx, {}, {}).ok_or_else(|| ::nybl::error::NyblError::runtime(::nybl::error_messages::variable_not_found({}), {line}))?",
                        rust_string_literal(&module),
                        rust_string_literal(&name),
                        rust_string_literal(target_name),
                    ),
                    _ => {
                        let read = self.storage_read_src(&storage, line);
                        format!(
                            "&{{ let __nybl_preflight_receiver = {read}; __nybl_preflight_receiver }}"
                        )
                    }
                }
            };
            let user_tmp = self.fresh_tmp();
            let receiver_tmp = self.fresh_tmp();
            let receiver_is_array_tmp = self.fresh_tmp();
            let expected = methods::builtin_method_arity(method)
                .expect("known mutating built-in has declared arity");
            let captured_guard = if self.is_captured_binding(target_name) {
                format!("return Err(::nybl::ref_params::captured_ref_target(1, {line})); ")
            } else {
                String::new()
            };
            write!(
                arg_lets,
                "let ({user_tmp}, {receiver_tmp}, {receiver_is_array_tmp}) = {{ let __nybl_preflight_receiver: &::nybl::value::Value = {receiver}; let __nybl_method_adapter = __nybl_preflight_user_method(ctx, __nybl_preflight_receiver, {}, &[{}], true, {line})?; let __nybl_saved_receiver = if __nybl_method_adapter.is_some() {{ ::std::option::Option::Some(__nybl_preflight_receiver.clone()) }} else {{ ::std::option::Option::None }}; let __nybl_receiver_in_place = ::nybl::methods::is_mutating_collection_receiver(__nybl_preflight_receiver, {method_literal}); (__nybl_method_adapter, __nybl_saved_receiver, __nybl_receiver_in_place) }}; if {user_tmp}.is_none() && {receiver_is_array_tmp} {{ {captured_guard}::nybl::methods::reject_constant_array_mutation({}, {}, {line})?; if {actual}usize != {expected}usize {{ return Err(::nybl::error::NyblError::runtime({}, {line})); }} }} ",
                rust_string_literal(method),
                args.iter()
                    .map(|_| "::nybl::parser::ParamMode::Value")
                    .collect::<Vec<_>>()
                    .join(", "),
                rust_string_literal(target_name),
                rust_string_literal(method),
                rust_string_literal(&format!(
                    "`.{}()` needs {} argument{}",
                    method,
                    expected,
                    if expected == 1 { "" } else { "s" }
                )),
                actual = args.len(),
                method_literal = rust_string_literal(method),
            )
            .unwrap();
            let storage =
                self.binding_storage(target_name)
                    .unwrap_or_else(|| BindingStorage::Persistent {
                        module: self.current_module.clone(),
                        name: target_name.clone(),
                    });
            let (target_preflight, target_read, target_write) =
                self.ref_target_sources(&storage, target_name, line);
            arg_lets.push_str(&target_preflight);
            if self.is_constant_binding(target_name) {
                write!(
                    arg_lets,
                    "if {user_tmp}.is_some_and(__nybl_user_method_receiver_is_ref) {{ return Err(::nybl::error_messages::constant_mutation_error({}, {line})); }} ",
                    rust_string_literal(target_name),
                )
                .unwrap();
            }
            if self.is_captured_binding(target_name) {
                write!(
                    arg_lets,
                    "if {user_tmp}.is_some_and(__nybl_user_method_receiver_is_ref) {{ return Err(::nybl::ref_params::captured_ref_target(1, {line})); }} "
                )
                .unwrap();
            }
            named_user_method = Some(user_tmp);
            named_user_receiver = Some(receiver_tmp);
            named_user_ref_target = Some((target_read, target_write));
        }
        for arg in args {
            let src = self.expr_src(arg)?;
            let tmp = self.fresh_tmp();
            write!(arg_lets, "let {tmp} = {src}; ").unwrap();
            arg_tmps.push(tmp);
        }

        // Two slices of the same args: one for the user
        // dispatcher, one for the builtin fallback. Cloning lets
        // each site own its own copy and matches the walker's
        // value-semantics (args are already cloned when passed to
        // user methods).
        let args_arr = if arg_tmps.is_empty() {
            "[]".to_string()
        } else {
            format!(
                "[{}]",
                arg_tmps
                    .iter()
                    .map(|t| format!("{t}.clone()"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        // Method name goes into a Rust string literal; we also look
        // up "is this mutating?" up-front so we only emit the
        // back-assign branch when it's actually needed.
        let method_lit = rust_string_literal(method);
        let ident_target = if mutating {
            match &object.kind {
                ExprKind::Ident(n) => Some(n.clone()),
                _ => None,
            }
        } else {
            None
        };

        // A mutating method on a bare identifier is the one shape where the
        // language writes the receiver back. Arrays are owned values, so the
        // binding itself can be mutated without violating value semantics:
        // copy-on-write detaches only when an earlier assignment / argument
        // still shares the backing store. Crucially, do this only after all
        // argument temporaries above have evaluated. Besides matching the
        // walker, that makes nested calls such as `a.push(a.pop())` observe the
        // current post-argument binding.
        //
        // The receiver is dynamically typed. A struct may define a user method
        // named `push`, so other receivers still clone once and go through the
        // full user-method-before-builtin dispatcher. Only built-in Array and
        // Dict receivers whose method actually mutates that type take the
        // in-place path.
        if let Some(target_name) = ident_target {
            let user_token = named_user_method
                .clone()
                .expect("named mutating preflight token");
            let user_receiver = named_user_receiver
                .clone()
                .expect("named mutating receiver snapshot");
            let owned_args = if arg_tmps.is_empty() {
                "::std::vec::Vec::new()".to_string()
            } else {
                format!("::std::vec![{}]", arg_tmps.join(", "))
            };
            let obj_tmp = self.fresh_tmp();
            let (user_target_read, user_target_write) = named_user_ref_target
                .clone()
                .expect("named user receiver has ref target accessors");
            let user_dispatch = format!(
                "{{ let __nybl_receiver_is_ref = __nybl_user_method_receiver_is_ref(__nybl_method_adapter); \
                    let __nybl_receiver = if __nybl_receiver_is_ref {{ {user_target_read} }} else {{ {user_receiver}.as_ref().expect(\"preflighted user receiver\").clone() }}; \
                    let __nybl_outcome = __nybl_call_preflighted_user_method_outcome(ctx, __nybl_method_adapter, __nybl_receiver, {owned_args}, {line})?; \
                    if __nybl_receiver_is_ref {{ let __target: &mut ::nybl::value::Value = {user_target_write}; *__target = __nybl_outcome.receiver; }} \
                    __nybl_outcome.value }}"
            );
            let constant_guard = format!(
                "::nybl::methods::reject_constant_array_mutation({}, {}, {})?; ",
                rust_string_literal(&target_name),
                method_lit,
                line,
            );
            if !self.is_local(&target_name) {
                if let Some(overlay) = self
                    .declaration_alias_overlay(&target_name)
                    .filter(|_| !self.declaration_alias_writes_through(&target_name))
                {
                    let read = self
                        .declaration_alias_read_src(&target_name, line)
                        .expect("overlay checked above");
                    let mut body = String::new();
                    write!(
                        body,
                        "{{ {arg_lets}let __ret = if {overlay}.overlay.as_ref().is_some_and(|__nybl_overlay_value| ::nybl::methods::is_mutating_collection_receiver(__nybl_overlay_value, {method_lit})) {{ \
                            {constant_guard}::nybl::methods::transactional_collection_method_in({overlay}.overlay.as_mut().expect(\"collection overlay\"), {method_lit}, {owned_args}, {line}, &ctx.memory)? \
                        }} else if let ::std::option::Option::Some(__nybl_method_adapter) = {user_token} {{ \
                            {user_dispatch} \
                        }} else {{ \
                            let {obj_tmp} = {read}; \
                                    let (__r, __mutated) = __nybl_call_method(ctx, &{obj_tmp}, {method_lit}, &{args_arr}, {line})?; \
                                    if let Some(__new_obj) = __mutated {{ {overlay}.overlay = ::std::option::Option::Some(__new_obj); }} \
                                    __r \
                        }}; __ret }}",
                    )
                    .unwrap();
                    return Ok(body);
                }
                let storage = self.binding_storage(&target_name).unwrap_or_else(|| {
                    BindingStorage::Persistent {
                        module: self.current_module.clone(),
                        name: target_name.clone(),
                    }
                });
                let target_src = self.storage_mut_src(&storage, &target_name, line);
                let read_src = self.storage_read_src(&storage, line);
                let target_tmp = self.fresh_tmp();
                let overlay_sync = if self.declaration_alias_writes_through(&target_name) {
                    format!(
                        "{overlay}.overlay = __nybl_binding_value(ctx, {module}, {name});",
                        overlay = declaration_alias_overlay_ident(&target_name),
                        module = rust_string_literal(&self.current_module),
                        name = rust_string_literal(&target_name),
                    )
                } else {
                    String::new()
                };
                let mut body = String::new();
                write!(
                    body,
                    "{{ {arg_lets}let __nybl_memory = ctx.memory.clone(); let __nybl_receiver_in_place = {{ let {target_tmp}: &mut ::nybl::value::Value = {target_src}; ::nybl::methods::is_mutating_collection_receiver(&*{target_tmp}, {method_lit}) }}; let __ret = if __nybl_receiver_in_place {{ \
                        let {target_tmp}: &mut ::nybl::value::Value = {target_src}; {constant_guard}::nybl::methods::transactional_collection_method_in({target_tmp}, {method_lit}, {owned_args}, {line}, &__nybl_memory)? \
                    }} else if let ::std::option::Option::Some(__nybl_method_adapter) = {user_token} {{ \
                        {user_dispatch} \
                    }} else {{ \
                        let {obj_tmp} = {read_src}; \
                                let (__r, __mutated) = __nybl_call_method(ctx, &{obj_tmp}, {method_lit}, &{args_arr}, {line})?; \
                                if let Some(__new_obj) = __mutated {{ let {target_tmp}: &mut ::nybl::value::Value = {target_src}; *{target_tmp} = __new_obj; }} \
                                __r \
                    }}; {overlay_sync} __ret }}",
                )
                .unwrap();
                return Ok(body);
            }
            let target = rust_user_ident(&target_name);
            let mut body = String::new();
            write!(
                body,
                "{{ {arg_lets}let __ret = if ::nybl::methods::is_mutating_collection_receiver(&{target}, {method_lit}) {{ \
                    {constant_guard}::nybl::methods::transactional_collection_method_in(&mut {target}, {method_lit}, {owned_args}, {line}, &ctx.memory)? \
                }} else if let ::std::option::Option::Some(__nybl_method_adapter) = {user_token} {{ \
                    {user_dispatch} \
                }} else {{ \
                    let {obj_tmp} = {target}.clone(); \
                            let (__r, __mutated) = __nybl_call_method(ctx, &{obj_tmp}, {method_lit}, &{args_arr}, {line})?; \
                            if let Some(__new_obj) = __mutated {{ {target} = __new_obj; }} \
                            __r \
                }}; __ret }}",
            )
            .unwrap();
            return Ok(body);
        }

        let obj_src = match resolved_object {
            Some(tmp) => tmp,
            None => self.expr_src(object)?,
        };
        let obj_tmp = self.fresh_tmp();
        let mut body = String::new();
        write!(body, "{{ {arg_lets}let {obj_tmp} = {obj_src}; ").unwrap();
        // Try user-defined methods first — the dispatcher returns
        // `Some(Value)` when a match is found, else `None`
        // (meaning "fall through to the built-in method
        // dispatch"). Matches walker / VM precedence.
        let module_dispatch = resolved_module_callee
            .map(|callee| {
            format!(
                    "if let ::std::option::Option::Some(__nybl_module_callable) = {callee} {{ \
                        __nybl_call_value(ctx, __nybl_module_callable, ::std::vec![{}], {line})?.value \
                    }} else ",
                    arg_tmps
                        .iter()
                        .map(|tmp| format!("{tmp}.clone()"))
                        .collect::<Vec<_>>()
                        .join(", "),
            )
            })
            .unwrap_or_default();
        let user_token =
            resolved_user_method.unwrap_or_else(|| "::std::option::Option::None".to_string());
        let user_dispatch = if let Some((target_read, target_write)) = named_user_ref_target {
            let owned_args = if arg_tmps.is_empty() {
                "::std::vec::Vec::new()".to_string()
            } else {
                format!(
                    "::std::vec![{}]",
                    arg_tmps
                        .iter()
                        .map(|tmp| format!("{tmp}.clone()"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            format!(
                "{{ let __nybl_receiver_is_ref = __nybl_user_method_receiver_is_ref(__nybl_method_adapter); \
                    let __nybl_receiver = if __nybl_receiver_is_ref {{ {target_read} }} else {{ {obj_tmp}.clone() }}; \
                    let __nybl_outcome = __nybl_call_preflighted_user_method_outcome(ctx, __nybl_method_adapter, __nybl_receiver, {owned_args}, {line})?; \
                    if __nybl_receiver_is_ref {{ let __target: &mut ::nybl::value::Value = {target_write}; *__target = __nybl_outcome.receiver; }} \
                    __nybl_outcome.value }}"
            )
        } else {
            format!(
                "__nybl_call_preflighted_user_method(ctx, __nybl_method_adapter, &{obj_tmp}, &{args_arr}, {line})?"
            )
        };
        write!(
            body,
            "let __ret = {module_dispatch}{{ if let ::std::option::Option::Some(__nybl_method_adapter) = {user_token} {{ \
                {user_dispatch} \
            }} else {{ \
                    let (__r, _) = __nybl_call_method(ctx, &{obj_tmp}, {method_lit}, &{args_arr}, {line})?; __r \
            }} }}; __ret }}",
        )
        .unwrap();
        Ok(body)
    }

    /// Emit a lambda expression as an `AotClosure`-wrapped
    /// `Value::Fn`. Free variables that resolve to outer locals
    /// are cloned into the closure via `move`; anything else
    /// (top-level fns, builtins) stays reachable through the
    /// usual Call / Ident dispatch inside the body.
    fn lambda_src(
        &mut self,
        params: &[Parameter],
        body: &[Stmt],
        line: u32,
    ) -> Result<String, NyblError> {
        let uses_runtime_type_bindings = self.statements_use_runtime_type_bindings(body);
        // Free-variable analysis against the outer scope stack.
        let mut dependencies = FreeVarDependencies::default();
        let mut body_known = HashSet::new();
        for p in params {
            body_known.insert(p.name.clone());
        }
        scan_free_vars_stmts(
            body,
            &mut body_known,
            &mut dependencies,
            &self.scope_stack,
            &self.fn_info,
        );
        dependencies
            .required
            .extend(dependencies.pattern_namespaces);
        let captures_ordered: Vec<String> = dependencies.required.into_iter().collect();
        if captures_ordered
            .iter()
            .any(|name| self.is_ref_parameter(name))
        {
            return Ok(format!(
                "{{ return Err(::nybl::ref_params::ref_capture_error({line})); }}"
            ));
        }
        let capture_storages = captures_ordered
            .iter()
            .map(|name| self.binding_storage(name))
            .collect::<Vec<_>>();

        // Switch into the lambda's lexical context before emitting
        // its body: outer scope is hidden (so Ident lookups inside
        // resolve against the closure's scope, not the outer fn's
        // Rust locals), and the new scope holds both the params
        // and the names we'll re-introduce as moved captures.
        let saved_scope_stack = core::mem::take(&mut self.scope_stack);
        self.push_scope();
        for p in params {
            self.bind_local(&p.name);
            if p.mode == ParamMode::Ref {
                self.mark_ref_parameter(&p.name);
            }
        }
        for (index, cap) in captures_ordered.iter().enumerate() {
            if matches!(
                capture_storages[index],
                Some(BindingStorage::OptionalImport { .. })
            ) {
                self.bind_optional_import(
                    cap,
                    OptionalImportBinding::Local {
                        value: format!("__nybl_optional_capture_{index}"),
                        function_site: format!("__nybl_optional_capture_site_{index}"),
                    },
                );
                self.mark_captured_local(cap);
            } else {
                self.bind_local(cap);
                self.mark_captured_local(cap);
            }
        }
        let declaration_aliases = self.declaration_aliases_for_callable(params, body);
        for alias in &declaration_aliases {
            self.mark_captured_local(alias);
        }
        let mut alias_initializers = HashMap::new();
        let mut alias_captures = Vec::new();
        for (index, alias) in declaration_aliases.iter().enumerate() {
            if let Some(parent_overlay) = self.declaration_alias_overlay(alias) {
                let capture = format!("__alias_cap_{index}");
                alias_initializers.insert(alias.clone(), format!("{capture}.clone()"));
                alias_captures.push((capture, parent_overlay));
            }
        }

        // Emit body into a side buffer so we can splice it into
        // the closure literal without disturbing indentation or
        // the tmp counter. The closure expands to a Rust move
        // closure that returns `Result<Value, NyblError>`, so
        // `try` inside the body propagates via `return Ok(...)`
        // like any other user fn — never via the top-level
        // raise path.
        let saved_out = core::mem::take(&mut self.out);
        let saved_indent = self.indent;
        let saved_tmp = self.tmp_counter;
        let saved_top_level = self.in_top_level;
        let saved_callable_body = self.in_callable_body;
        self.indent = 0;
        self.tmp_counter = 0;
        self.in_top_level = false;
        self.in_callable_body = true;
        let declaration_alias_overlays =
            self.emit_declaration_alias_overlays(&declaration_aliases, &alias_initializers);
        self.declaration_alias_overlays
            .push(declaration_alias_overlays);
        self.declaration_alias_write_through.push(false);
        self.callable_mutations
            .push(callable_assignment_names(body));
        for s in body {
            self.emit_stmt(s)?;
        }
        self.callable_mutations.pop();
        self.declaration_alias_write_through.pop();
        self.declaration_alias_overlays.pop();
        let body_src = core::mem::replace(&mut self.out, saved_out);
        self.indent = saved_indent;
        self.tmp_counter = saved_tmp;
        self.in_top_level = saved_top_level;
        self.in_callable_body = saved_callable_body;

        self.pop_scope();
        self.scope_stack = saved_scope_stack;

        // Build the capture prelude (outer-side clones) and the
        // in-closure rebinding (moved values shadowed under the
        // original Nybl names so the body references them
        // naturally).
        let mut capture_prelude = String::new();
        let mut capture_moves = String::new();
        for (i, cap) in captures_ordered.iter().enumerate() {
            let optional_capture = matches!(
                capture_storages[i],
                Some(BindingStorage::OptionalImport { .. })
            );
            let capture_source = match capture_storages[i].as_ref() {
                Some(storage @ BindingStorage::OptionalImport { .. }) => self
                    .storage_optional_value_src(storage, line)
                    .expect("optional capture storage"),
                Some(storage) => self.storage_read_src(storage, line),
                None if self.is_persistent_binding(cap) => format!(
                    "__nybl_read_binding(ctx, {}, {}, {})?",
                    rust_string_literal(&self.current_module),
                    rust_string_literal(cap),
                    line,
                ),
                None => format!("{}.clone()", rust_user_ident(cap)),
            };
            writeln!(capture_prelude, "let __cap_{i} = {capture_source};",).unwrap();
            // `Fn` closures can be invoked repeatedly, so captures
            // must stay owned by the closure body. Clone per
            // invocation; the outer capture is the stable source.
            if optional_capture {
                writeln!(
                    capture_moves,
                    "let mut __nybl_optional_capture_{i}: ::std::option::Option<::nybl::value::Value> = __cap_{i}.clone(); let mut __nybl_optional_capture_site_{i}: ::std::option::Option<usize> = ::std::option::Option::None;",
                )
                .unwrap();
            } else {
                writeln!(
                    capture_moves,
                    "let mut {ident}: ::nybl::value::Value = __cap_{i}.clone();",
                    ident = rust_user_ident(cap),
                    i = i
                )
                .unwrap();
            }
        }
        for (capture, parent_overlay) in &alias_captures {
            writeln!(
                capture_prelude,
                "let {capture} = {parent_overlay}.overlay.clone();"
            )
            .unwrap();
        }
        let mut opaque_body_depth = captures_ordered.iter().enumerate().fold(
            String::from("0u16"),
            |depth, (i, _)| {
                if matches!(
                    capture_storages[i],
                    Some(BindingStorage::OptionalImport { .. })
                ) {
                    format!("{depth}.max(__cap_{i}.as_ref().map_or(0u16, ::nybl::value::Value::ownership_depth))")
                } else {
                    format!("{depth}.max(__cap_{i}.ownership_depth())")
                }
            },
        );
        for (capture, _) in &alias_captures {
            opaque_body_depth = format!(
                "{opaque_body_depth}.max({capture}.as_ref().map_or(0u16, ::nybl::value::Value::ownership_depth))"
            );
        }

        let mut param_binds = String::new();
        for (index, p) in params.iter().enumerate() {
            writeln!(
                param_binds,
                "let mut __nybl_lambda_arg_{index}: ::nybl::value::Value = args.remove(0); let mut {ident}: ::nybl::value::Value = __nybl_lambda_arg_{index}.clone();",
                ident = rust_user_ident(&p.name)
            )
            .unwrap();
        }
        let mut param_sync = String::new();
        for (index, p) in params.iter().enumerate() {
            if p.mode == ParamMode::Ref {
                writeln!(
                    param_sync,
                    "__nybl_lambda_arg_{index} = {ident}.clone();",
                    ident = rust_user_ident(&p.name),
                )
                .unwrap();
            }
        }
        let params_array = if params.is_empty() {
            "::std::vec::Vec::<String>::new()".to_string()
        } else {
            format!(
                "vec![{}]",
                params
                    .iter()
                    .map(|p| format!("\"{}\".to_string()", p.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let modes_array = if params.is_empty() {
            "::std::vec::Vec::<::nybl::parser::ParamMode>::new()".to_string()
        } else {
            format!(
                "vec![{}]",
                params
                    .iter()
                    .map(|p| match p.mode {
                        ParamMode::Value => "::nybl::parser::ParamMode::Value",
                        ParamMode::Ref => "::nybl::parser::ParamMode::Ref",
                        ParamMode::Rest => "::nybl::parser::ParamMode::Rest",
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let modes_slice = params
            .iter()
            .map(|p| match p.mode {
                ParamMode::Value => "::nybl::parser::ParamMode::Value",
                ParamMode::Ref => "::nybl::parser::ParamMode::Ref",
                ParamMode::Rest => "::nybl::parser::ParamMode::Rest",
            })
            .collect::<Vec<_>>()
            .join(", ");
        let final_args = if params.is_empty() {
            "::std::vec::Vec::new()".to_string()
        } else {
            format!(
                "vec![{}]",
                (0..params.len())
                    .map(|index| format!("__nybl_lambda_arg_{index}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        let type_bindings_init = if uses_runtime_type_bindings {
            format!(
                "let mut __nybl_type_bindings = __nybl_callable_type_bindings(ctx, {}); ",
                rust_string_literal(&self.current_module),
            )
        } else {
            String::new()
        };
        let wrap_ctx = if self.opts.sandbox { "ctx, " } else { "" };
        let memory_check = if self.opts.sandbox {
            format!("__nybl_check_memory(ctx, {line})?; ")
        } else {
            String::new()
        };

        Ok(format!(
            "{{ {capture_prelude}let __opaque_body_depth = {opaque_body_depth}; let __callable: ::std::rc::Rc<dyn for<'__a> Fn(&mut Ctx<'__a>, ::std::vec::Vec<::nybl::value::Value>) -> Result<__NyblCallOutcome, ::nybl::error::NyblError>> = ::std::rc::Rc::new(move |ctx, mut args| {{ let __modes = &[{modes_slice}]; if !::nybl::ref_params::accepts_arity(__modes, args.len()) {{ return Err(__nybl_arity_error(\"lambda\", __modes, args.len(), {line})); }} args = __nybl_pack_rest_args(args, __modes, {line}, &ctx.memory)?; let _ = &args; {type_bindings_init}{capture_moves}{param_binds}let __nybl_value_result = (|| -> Result<::nybl::value::Value, ::nybl::error::NyblError> {{ {body_src} #[allow(unreachable_code)] Ok(::nybl::value::Value::None) }})(); let value = __nybl_value_result?; {memory_check}{param_sync}Ok(__NyblCallOutcome {{ value, args: {final_args} }}) }}); __nybl_wrap_callable({wrap_ctx}{params_array}, {modes_array}, ::std::vec::Vec::new(), None, __opaque_body_depth, {line}, __callable)? }}",
        ))
    }
}

// ─── Free-variable analysis for lambdas ────────────────────────────
//
// Walks a lambda's body collecting every Ident that resolves to an
// *outer* local — i.e. something present in `outer_scopes` and not
// shadowed by a param / local inside the lambda. Top-level fns
// stay callable without capture (they're globally reachable Rust
// fns) and unknown identifiers are left for rustc to flag.

#[derive(Default)]
struct FreeVarDependencies {
    /// Value/expression references that must exist when the closure or lifted
    /// function is created.
    required: std::collections::BTreeSet<String>,
    /// Namespace references used only by patterns. Declaration aliases in
    /// this set are resolved lazily by the pattern resolver, while a lambda
    /// still promotes outer locals/params from this set into real captures.
    pattern_namespaces: std::collections::BTreeSet<String>,
    /// Nested named functions have their own declaration context and do not
    /// capture a caller's alias overlay. Nested lambdas do capture it and must
    /// continue to propagate through arbitrarily deep closure chains.
    skip_nested_named_functions: bool,
}

impl FreeVarDependencies {
    fn for_declaration_aliases() -> Self {
        Self {
            skip_nested_named_functions: true,
            ..Self::default()
        }
    }
}

fn scan_free_vars_stmts(
    stmts: &[Stmt],
    known: &mut HashSet<String>,
    free: &mut FreeVarDependencies,
    outer_scopes: &[EmissionScope],
    fn_info: &FnInfo,
) {
    for stmt in stmts {
        scan_free_vars_stmt(stmt, known, free, outer_scopes, fn_info);
    }
}

fn scan_free_vars_stmt(
    stmt: &Stmt,
    known: &mut HashSet<String>,
    free: &mut FreeVarDependencies,
    outer_scopes: &[EmissionScope],
    fn_info: &FnInfo,
) {
    match &stmt.kind {
        StmtKind::Let {
            name,
            value,
            is_const: _,
        } => {
            scan_free_vars_expr(value, known, free, outer_scopes, fn_info);
            known.insert(name.clone());
        }
        StmtKind::Assign {
            target,
            op: _,
            value,
        } => {
            match target {
                AssignTarget::Variable(n) => {
                    if !known.contains(n) && is_outer_local(n, outer_scopes) {
                        free.required.insert(n.clone());
                    }
                }
                AssignTarget::Index { object, index } => {
                    scan_free_vars_expr(object, known, free, outer_scopes, fn_info);
                    scan_free_vars_expr(index, known, free, outer_scopes, fn_info);
                }
                AssignTarget::Field { object, .. } => {
                    scan_free_vars_expr(object, known, free, outer_scopes, fn_info);
                }
            }
            scan_free_vars_expr(value, known, free, outer_scopes, fn_info);
        }
        StmtKind::If {
            condition,
            body,
            else_ifs,
            else_body,
        } => {
            scan_free_vars_expr(condition, known, free, outer_scopes, fn_info);
            let saved = known.clone();
            scan_free_vars_stmts(body, known, free, outer_scopes, fn_info);
            *known = saved.clone();
            for (c, b) in else_ifs {
                scan_free_vars_expr(c, known, free, outer_scopes, fn_info);
                scan_free_vars_stmts(b, known, free, outer_scopes, fn_info);
                *known = saved.clone();
            }
            if let Some(b) = else_body {
                scan_free_vars_stmts(b, known, free, outer_scopes, fn_info);
                *known = saved;
            }
        }
        StmtKind::While { condition, body } => {
            scan_free_vars_expr(condition, known, free, outer_scopes, fn_info);
            let saved = known.clone();
            scan_free_vars_stmts(body, known, free, outer_scopes, fn_info);
            *known = saved;
        }
        StmtKind::Repeat { count, body } => {
            scan_free_vars_expr(count, known, free, outer_scopes, fn_info);
            let saved = known.clone();
            scan_free_vars_stmts(body, known, free, outer_scopes, fn_info);
            *known = saved;
        }
        StmtKind::ForIn {
            var,
            iterable,
            body,
        } => {
            scan_free_vars_expr(iterable, known, free, outer_scopes, fn_info);
            let saved = known.clone();
            known.insert(var.clone());
            scan_free_vars_stmts(body, known, free, outer_scopes, fn_info);
            *known = saved;
        }
        StmtKind::FnDecl {
            name, params, body, ..
        } => {
            // A nested fn decl introduces `name` into the body's
            // scope (so recursive refs work) but its own body is
            // analysed with only its params in scope — matches
            // how the walker / VM treat nested fns.
            known.insert(name.clone());
            if free.skip_nested_named_functions {
                return;
            }
            let mut inner_known = HashSet::new();
            for p in params {
                inner_known.insert(p.name.clone());
            }
            inner_known.insert(name.clone());
            scan_free_vars_stmts(body, &mut inner_known, free, outer_scopes, fn_info);
        }
        StmtKind::Return { value } => {
            if let Some(v) = value {
                scan_free_vars_expr(v, known, free, outer_scopes, fn_info);
            }
        }
        StmtKind::Break | StmtKind::Continue => {}
        StmtKind::Use { alias, .. } => {
            // Import bindings are created when the lambda executes,
            // so the use site itself does not capture an outer value.
            if let Some(alias) = alias {
                known.insert(alias.clone());
            }
        }
        StmtKind::StructDecl { .. } => {
            // Struct declarations don't reference any identifiers
            // — only field names, which are lookup keys, not
            // bindings. Nothing to capture.
        }
        StmtKind::EnumDecl { .. } => {
            // Enum declarations are purely type-level — no free
            // variables. Treat identically to struct decls.
        }
        StmtKind::MethodDecl { .. } => {
            // Method declarations — the AOT rejects them at emit
            // time; no free-var analysis needed here.
        }
        StmtKind::PublicSurface { .. } => {}
        StmtKind::ExprStmt(e) => {
            scan_free_vars_expr(e, known, free, outer_scopes, fn_info);
        }
    }
}

/// Emit Rust source that constructs a `nybl::parser::Pattern`
/// value equivalent to `pat`. The emitted expression is used at
/// runtime by `nybl::pattern_matches`, so walker / VM / AOT all
/// share one matching implementation.
fn pattern_rust(pat: &nybl::parser::Pattern) -> String {
    use nybl::parser::{ArrayRest, Pattern};
    match pat {
        Pattern::Wildcard => "::nybl::parser::Pattern::Wildcard".to_string(),
        Pattern::Binding(name) => format!(
            "::nybl::parser::Pattern::Binding({}.to_string())",
            rust_string_literal(name)
        ),
        Pattern::Literal(lit) => format!(
            "::nybl::parser::Pattern::Literal({})",
            literal_pattern_rust(lit)
        ),
        Pattern::EnumVariant {
            namespace,
            type_name,
            variant,
            payload,
        } => format!(
            "::nybl::parser::Pattern::EnumVariant {{ namespace: {}, type_name: {}.to_string(), variant: {}.to_string(), payload: {} }}",
            optional_string_rust(namespace.as_deref()),
            rust_string_literal(type_name),
            rust_string_literal(variant),
            variant_payload_rust(payload),
        ),
        Pattern::Struct {
            namespace,
            type_name,
            fields,
            rest,
        } => {
            let fields_src = if fields.is_empty() {
                "::std::vec::Vec::new()".to_string()
            } else {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(k, p)| {
                        format!(
                            "({}.to_string(), {})",
                            rust_string_literal(k),
                            pattern_rust(p)
                        )
                    })
                    .collect();
                format!("::std::vec::Vec::from([{}])", parts.join(", "))
            };
            format!(
                "::nybl::parser::Pattern::Struct {{ namespace: {}, type_name: {}.to_string(), fields: {}, rest: {} }}",
                optional_string_rust(namespace.as_deref()),
                rust_string_literal(type_name),
                fields_src,
                rest,
            )
        }
        Pattern::Array { elements, rest } => {
            let elems_src = if elements.is_empty() {
                "::std::vec::Vec::new()".to_string()
            } else {
                let parts: Vec<String> = elements.iter().map(pattern_rust).collect();
                format!("::std::vec::Vec::from([{}])", parts.join(", "))
            };
            let rest_src = match rest {
                None => "::std::option::Option::None".to_string(),
                Some(ArrayRest::Ignored) => {
                    "::std::option::Option::Some(::nybl::parser::ArrayRest::Ignored)".to_string()
                }
                Some(ArrayRest::Named(n)) => format!(
                    "::std::option::Option::Some(::nybl::parser::ArrayRest::Named({}.to_string()))",
                    rust_string_literal(n)
                ),
            };
            format!("::nybl::parser::Pattern::Array {{ elements: {elems_src}, rest: {rest_src} }}",)
        }
        Pattern::Or(alts) => {
            let parts: Vec<String> = alts.iter().map(pattern_rust).collect();
            let alts_src = if parts.is_empty() {
                "::std::vec::Vec::new()".to_string()
            } else {
                format!("::std::vec::Vec::from([{}])", parts.join(", "))
            };
            format!("::nybl::parser::Pattern::Or({alts_src})")
        }
    }
}

fn literal_pattern_rust(lit: &nybl::parser::LiteralPattern) -> String {
    use nybl::parser::LiteralPattern;
    match lit {
        LiteralPattern::Int(n) => format!("::nybl::parser::LiteralPattern::Int({n}i64)"),
        LiteralPattern::Number(n) => {
            format!("::nybl::parser::LiteralPattern::Number({})", rust_f64(*n))
        }
        LiteralPattern::Str(s) => format!(
            "::nybl::parser::LiteralPattern::Str({}.to_string())",
            rust_string_literal(s)
        ),
        LiteralPattern::Bool(b) => format!("::nybl::parser::LiteralPattern::Bool({b})"),
        LiteralPattern::None => "::nybl::parser::LiteralPattern::None".to_string(),
    }
}

fn variant_payload_rust(payload: &nybl::parser::VariantPatternPayload) -> String {
    use nybl::parser::VariantPatternPayload;
    match payload {
        VariantPatternPayload::Unit => "::nybl::parser::VariantPatternPayload::Unit".to_string(),
        VariantPatternPayload::Tuple(items) => {
            let parts: Vec<String> = items.iter().map(pattern_rust).collect();
            let inner = if parts.is_empty() {
                "::std::vec::Vec::new()".to_string()
            } else {
                format!("::std::vec::Vec::from([{}])", parts.join(", "))
            };
            format!("::nybl::parser::VariantPatternPayload::Tuple({inner})")
        }
        VariantPatternPayload::Struct { fields, rest } => {
            let fields_src = if fields.is_empty() {
                "::std::vec::Vec::new()".to_string()
            } else {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(k, p)| {
                        format!(
                            "({}.to_string(), {})",
                            rust_string_literal(k),
                            pattern_rust(p)
                        )
                    })
                    .collect();
                format!("::std::vec::Vec::from([{}])", parts.join(", "))
            };
            format!(
                "::nybl::parser::VariantPatternPayload::Struct {{ fields: {fields_src}, rest: {rest} }}",
            )
        }
    }
}

fn pattern_uses_bare_type(pattern: &nybl::parser::Pattern) -> bool {
    use nybl::parser::{Pattern, VariantPatternPayload};
    match pattern {
        Pattern::EnumVariant {
            namespace, payload, ..
        } => {
            namespace.is_none()
                || match payload {
                    VariantPatternPayload::Unit => false,
                    VariantPatternPayload::Tuple(items) => items.iter().any(pattern_uses_bare_type),
                    VariantPatternPayload::Struct { fields, .. } => fields
                        .iter()
                        .any(|(_, field)| pattern_uses_bare_type(field)),
                }
        }
        Pattern::Struct {
            namespace, fields, ..
        } => {
            namespace.is_none()
                || fields
                    .iter()
                    .any(|(_, field)| pattern_uses_bare_type(field))
        }
        Pattern::Array { elements, .. } | Pattern::Or(elements) => {
            elements.iter().any(pattern_uses_bare_type)
        }
        Pattern::Wildcard | Pattern::Binding(_) | Pattern::Literal(_) => false,
    }
}

fn expr_uses_runtime_type_bindings(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Number(_)
        | ExprKind::Str(_)
        | ExprKind::Bool(_)
        | ExprKind::None
        | ExprKind::Ident(_)
        | ExprKind::StringInterp(_) => false,
        ExprKind::BinaryOp { left, right, .. } => {
            expr_uses_runtime_type_bindings(left) || expr_uses_runtime_type_bindings(right)
        }
        ExprKind::UnaryOp { expr, .. } | ExprKind::Try(expr) => {
            expr_uses_runtime_type_bindings(expr)
        }
        ExprKind::Call { callee, args } => {
            expr_uses_runtime_type_bindings(callee)
                || args
                    .iter()
                    .any(|arg| expr_uses_runtime_type_bindings(&arg.value))
        }
        ExprKind::MethodCall { object, args, .. } => {
            expr_uses_runtime_type_bindings(object)
                || args
                    .iter()
                    .any(|arg| expr_uses_runtime_type_bindings(&arg.value))
        }
        ExprKind::Index { object, index } => {
            expr_uses_runtime_type_bindings(object) || expr_uses_runtime_type_bindings(index)
        }
        ExprKind::Array(items) => items.iter().any(expr_uses_runtime_type_bindings),
        ExprKind::Dict(entries) => entries
            .iter()
            .any(|(_, value)| expr_uses_runtime_type_bindings(value)),
        ExprKind::IfExpr {
            condition,
            then_expr,
            else_expr,
        } => {
            expr_uses_runtime_type_bindings(condition)
                || expr_uses_runtime_type_bindings(then_expr)
                || expr_uses_runtime_type_bindings(else_expr)
        }
        // Nested callables initialize from their defining module when invoked;
        // their type requirements do not force an allocation in the creator.
        ExprKind::Lambda { .. } => false,
        ExprKind::FieldAccess { object, .. } => expr_uses_runtime_type_bindings(object),
        ExprKind::StructConstruct {
            namespace, fields, ..
        } => {
            namespace.is_none()
                || fields
                    .iter()
                    .any(|(_, value)| expr_uses_runtime_type_bindings(value))
        }
        ExprKind::EnumConstruct {
            namespace, payload, ..
        } => {
            namespace.is_none()
                || match payload {
                    VariantPayload::Unit => false,
                    VariantPayload::Tuple(values) => {
                        values.iter().any(expr_uses_runtime_type_bindings)
                    }
                    VariantPayload::Struct(fields) => fields
                        .iter()
                        .any(|(_, value)| expr_uses_runtime_type_bindings(value)),
                }
        }
        ExprKind::Match { scrutinee, arms } => {
            expr_uses_runtime_type_bindings(scrutinee)
                || arms.iter().any(|arm| {
                    pattern_uses_bare_type(&arm.pattern)
                        || arm
                            .guard
                            .as_ref()
                            .is_some_and(expr_uses_runtime_type_bindings)
                        || expr_uses_runtime_type_bindings(&arm.body)
                })
        }
    }
}

fn scan_free_vars_expr(
    expr: &Expr,
    known: &mut HashSet<String>,
    free: &mut FreeVarDependencies,
    outer_scopes: &[EmissionScope],
    fn_info: &FnInfo,
) {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Number(_)
        | ExprKind::Str(_)
        | ExprKind::Bool(_)
        | ExprKind::None => {}
        ExprKind::Ident(name) => {
            // If the ident resolves against the enclosing Nybl
            // scope (and isn't shadowed by a binding inside the
            // lambda), record it as a capture. Top-level fns and
            // the `self_name` of the enclosing decl don't need to
            // be captured — they stay callable directly.
            if !known.contains(name) && is_outer_local(name, outer_scopes) {
                free.required.insert(name.clone());
            }
        }
        ExprKind::StringInterp(parts) => {
            for part in parts {
                if let StringPart::Variable(name) = part
                    && !known.contains(name)
                    && is_outer_local(name, outer_scopes)
                {
                    free.required.insert(name.clone());
                }
            }
        }
        ExprKind::BinaryOp { left, right, .. } => {
            scan_free_vars_expr(left, known, free, outer_scopes, fn_info);
            scan_free_vars_expr(right, known, free, outer_scopes, fn_info);
        }
        ExprKind::UnaryOp { expr: inner, .. } => {
            scan_free_vars_expr(inner, known, free, outer_scopes, fn_info);
        }
        ExprKind::Call { callee, args } => {
            // For Ident callees, the call site either resolves to
            // a local (capture-worthy) or to a builtin/host/user
            // fn (not captured). Same logic as the Ident arm.
            scan_free_vars_expr(callee, known, free, outer_scopes, fn_info);
            for arg in args {
                scan_free_vars_expr(arg, known, free, outer_scopes, fn_info);
            }
        }
        ExprKind::MethodCall { object, args, .. } => {
            scan_free_vars_expr(object, known, free, outer_scopes, fn_info);
            for arg in args {
                scan_free_vars_expr(arg, known, free, outer_scopes, fn_info);
            }
        }
        ExprKind::Index { object, index } => {
            scan_free_vars_expr(object, known, free, outer_scopes, fn_info);
            scan_free_vars_expr(index, known, free, outer_scopes, fn_info);
        }
        ExprKind::Array(items) => {
            for item in items {
                scan_free_vars_expr(item, known, free, outer_scopes, fn_info);
            }
        }
        ExprKind::Dict(entries) => {
            for (_, v) in entries {
                scan_free_vars_expr(v, known, free, outer_scopes, fn_info);
            }
        }
        ExprKind::IfExpr {
            condition,
            then_expr,
            else_expr,
        } => {
            scan_free_vars_expr(condition, known, free, outer_scopes, fn_info);
            scan_free_vars_expr(then_expr, known, free, outer_scopes, fn_info);
            scan_free_vars_expr(else_expr, known, free, outer_scopes, fn_info);
        }
        ExprKind::Lambda { params, body } => {
            // A nested lambda's captures are *its* concern — but
            // anything it references from *our* outer scope still
            // needs to bubble up to be captured here so the inner
            // closure gets the value when it's constructed. Seed
            // the nested scan with our current bindings so a nested
            // reference to an outer-lambda param/local doesn't get
            // mistaken for a same-named binding outside both lambdas.
            let mut inner_known = known.clone();
            for p in params {
                inner_known.insert(p.name.clone());
            }
            scan_free_vars_stmts(body, &mut inner_known, free, outer_scopes, fn_info);
        }
        ExprKind::FieldAccess { object, .. } => {
            scan_free_vars_expr(object, known, free, outer_scopes, fn_info);
        }
        ExprKind::StructConstruct {
            namespace, fields, ..
        } => {
            // Namespaced construction emits a runtime validation
            // against the module Value. Record that otherwise-hidden
            // Rust closure capture so opaque ownership depth includes it.
            if let Some(name) = namespace
                && !known.contains(name)
                && is_outer_local(name, outer_scopes)
            {
                free.required.insert(name.clone());
            }
            for (_, v) in fields {
                scan_free_vars_expr(v, known, free, outer_scopes, fn_info);
            }
        }
        ExprKind::EnumConstruct {
            namespace, payload, ..
        } => {
            if let Some(name) = namespace
                && !known.contains(name)
                && is_outer_local(name, outer_scopes)
            {
                free.required.insert(name.clone());
            }
            use nybl::parser::VariantPayload;
            match payload {
                VariantPayload::Unit => {}
                VariantPayload::Tuple(args) => {
                    for a in args {
                        scan_free_vars_expr(a, known, free, outer_scopes, fn_info);
                    }
                }
                VariantPayload::Struct(fields) => {
                    for (_, v) in fields {
                        scan_free_vars_expr(v, known, free, outer_scopes, fn_info);
                    }
                }
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            // Recurse into scrutinee, and each arm treats pattern
            // bindings as locals so guard/body references don't
            // bubble up as captures.
            scan_free_vars_expr(scrutinee, known, free, outer_scopes, fn_info);
            for arm in arms {
                for namespace in arm.pattern.namespace_names() {
                    if !known.contains(&namespace) && is_outer_local(&namespace, outer_scopes) {
                        free.pattern_namespaces.insert(namespace);
                    }
                }
                let mut arm_known = known.clone();
                arm_known.extend(arm.pattern.binding_names());
                if let Some(guard) = &arm.guard {
                    scan_free_vars_expr(guard, &mut arm_known, free, outer_scopes, fn_info);
                }
                scan_free_vars_expr(&arm.body, &mut arm_known, free, outer_scopes, fn_info);
            }
        }
        ExprKind::Try(inner) => {
            scan_free_vars_expr(inner, known, free, outer_scopes, fn_info);
        }
    }
}

fn is_outer_local(name: &str, outer_scopes: &[EmissionScope]) -> bool {
    outer_scopes.iter().any(|scope| {
        scope.locals.contains(name)
            || scope.persistent_bindings.contains(name)
            || scope.optional_imports.contains_key(name)
    })
}

// ─── Free helpers ──────────────────────────────────────────────────

fn bin_op_path(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "::nybl::ops::add_in",
        BinOp::Sub => "::nybl::ops::sub_in",
        BinOp::Mul => "::nybl::ops::mul_in",
        BinOp::Div => "::nybl::ops::div_in",
        BinOp::Mod => "::nybl::ops::rem_in",
        BinOp::Eq => "::nybl::ops::eq",
        BinOp::NotEq => "::nybl::ops::not_eq",
        BinOp::Lt => "::nybl::ops::lt_in",
        BinOp::Gt => "::nybl::ops::gt_in",
        BinOp::LtEq => "::nybl::ops::lt_eq_in",
        BinOp::GtEq => "::nybl::ops::gt_eq_in",
        BinOp::And | BinOp::Or => unreachable!("short-circuit handled separately"),
    }
}

fn compound_op_path(op: AssignOp) -> &'static str {
    match op {
        AssignOp::Eq => unreachable!("caller filters out AssignOp::Eq"),
        AssignOp::AddEq => "::nybl::ops::add_in",
        AssignOp::SubEq => "::nybl::ops::sub_in",
        AssignOp::MulEq => "::nybl::ops::mul_in",
        AssignOp::DivEq => "::nybl::ops::div_in",
        AssignOp::ModEq => "::nybl::ops::rem_in",
    }
}

fn build_arg_array(arg_names: &[String]) -> String {
    if arg_names.is_empty() {
        "[]".to_string()
    } else {
        format!("[{}]", arg_names.join(", "))
    }
}

/// Encode arbitrary UTF-8 source text into an injective,
/// Rust-identifier-safe component. Hex is deliberately used instead
/// of a character substitution: `a.b`, `a_b`, and names that already
/// look mangled must remain distinct.
fn user_name_component(name: &str) -> String {
    let mut encoded = String::with_capacity(name.len() * 2);
    for byte in name.as_bytes() {
        write!(encoded, "{byte:02x}").unwrap();
    }
    encoded
}

/// The sole lowering boundary for source-level value identifiers.
/// Every Nybl binding and reference passes through this namespace, so
/// it cannot collide with emitter temporaries (`__t0`, `__l`, `ctx`,
/// ...), Rust keywords, or another source name.
fn rust_user_ident(name: &str) -> String {
    format!("__nybl_user_value_{}", user_name_component(name))
}

fn declaration_alias_overlay_ident(name: &str) -> String {
    format!("__nybl_declaration_alias_{}", user_name_component(name))
}

fn declaration_alias_pattern_snapshot_ident(name: &str) -> String {
    format!("__nybl_pattern_alias_{}", user_name_component(name))
}

fn wrapper_fn_name_with(module_prefix: &str, name: &str) -> String {
    format!(
        "__nybl_user_fn_value_{}n{}",
        module_prefix,
        user_name_component(name)
    )
}

/// Prefix user functions emitted from an imported module. The
/// leading non-hex marker keeps root, module, and method namespaces
/// structurally distinct.
fn module_fn_prefix(path: &str) -> String {
    format!("m{}_", user_name_component(path))
}

fn method_site_fn_name(site: usize) -> String {
    format!("__nybl_method_site_{site}")
}

fn method_site_adapter_name(site: usize) -> String {
    format!("__nybl_method_adapter_{site}")
}

fn method_site_outcome_adapter_name(site: usize) -> String {
    format!("__nybl_method_outcome_adapter_{site}")
}

fn function_site_fn_name(site: usize) -> String {
    format!("__nybl_function_site_{site}")
}

fn guarded_function_site_fn_name(site: usize) -> String {
    format!("__nybl_guarded_function_site_{site}")
}

/// Turn a Nybl module path into a collision-free Rust-safe slug.
fn module_slug(path: &str) -> String {
    user_name_component(path)
}

fn module_load_fn_name(path: &str) -> String {
    format!("__mod_{}_load", module_slug(path))
}

fn module_exports_type_name(path: &str) -> String {
    format!("NyblModule{}Exports", module_slug(path))
}

/// Render `f64` such that the emitted Rust preserves bit-exactness
/// for whole numbers (no trailing `.0` surprises) while falling back
/// to `{:?}` for anything non-finite or non-integral.
fn rust_f64(n: f64) -> String {
    if n.is_nan() {
        "f64::NAN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 {
            "f64::INFINITY".to_string()
        } else {
            "f64::NEG_INFINITY".to_string()
        }
    } else {
        // Rust's `{:?}` on f64 always includes a decimal point,
        // which is exactly what we want so the literal parses back
        // as `f64` and not `i32`.
        format!("{n:?}")
    }
}

/// Lower an `Option<&str>` to a Rust expression of type
/// `Option<String>`. Used when emitting `Pattern::EnumVariant` /
/// `Pattern::Struct` with their `namespace` field: runtime
/// pattern matching doesn't consult the namespace (types
/// register globally), but we still have to populate the field
/// so the struct literal type-checks.
fn optional_string_rust(s: Option<&str>) -> String {
    match s {
        Some(v) => format!(
            "::std::option::Option::Some({}.to_string())",
            rust_string_literal(v)
        ),
        None => "::std::option::Option::None".to_string(),
    }
}

/// Escape a Nybl string literal for embedding in Rust source.
fn rust_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                write!(out, "\\u{{{:x}}}", c as u32).unwrap();
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::emit;
    use crate::{Options, modules_from_map};

    fn emitted(source: &str) -> String {
        let statements = nybl::parse(source).expect("parse test program");
        emit(&statements, &Options::default()).expect("emit test program")
    }

    #[test]
    fn module_graph_parse_error_retains_transitive_source_context() {
        let root = nybl::parse("use outer").unwrap();
        let inner_source = "let okay = 1\nlet broken =";
        let options = Options {
            module_resolver: Some(modules_from_map([
                ("outer", "use inner\nlet outer = 1"),
                ("inner", inner_source),
            ])),
            ..Options::default()
        };

        let error = emit(&root, &options).unwrap_err();
        let context = error.source_context.as_ref().expect("module context");

        assert_eq!(context.module_path, "inner");
        assert_eq!(context.source.as_deref(), Some(inner_source));
        assert!(
            error
                .render("use outer")
                .contains("in module `inner` at line 2")
        );
    }

    #[test]
    fn module_graph_cycle_points_at_importing_module_source() {
        let root = nybl::parse("use a").unwrap();
        let options = Options {
            module_resolver: Some(modules_from_map([
                ("a", "use b\nlet a_value = 1"),
                ("b", "use a\nlet b_value = 2"),
            ])),
            ..Options::default()
        };

        let error = emit(&root, &options).unwrap_err();
        let rendered = error.render("use a");

        assert!(error.message.contains("Circular import"));
        assert!(rendered.contains("in module `b` at line 1"));
        assert!(rendered.contains("1 | use a"));
    }

    #[test]
    fn module_emission_error_uses_owning_module_source() {
        let root = nybl::parse("use facade").unwrap();
        let facade_source = "use dep.{missing}\nlet okay = 1";
        let options = Options {
            module_resolver: Some(modules_from_map([
                ("facade", facade_source),
                ("dep", "let present = 1"),
            ])),
            ..Options::default()
        };

        let error = emit(&root, &options).unwrap_err();
        let rendered = error.render("use facade");

        assert!(error.message.contains("no export `missing`"));
        assert!(rendered.contains("in module `facade` at line 1"));
        assert!(rendered.contains("1 | use dep.{missing}"));
    }

    #[test]
    fn mutating_ident_array_uses_in_place_dispatch() {
        let output = emitted("let a = [1]\na.push(2)");

        assert!(
            output.contains("__nybl_binding_mut_option(ctx, \"<root>\", \"a\")"),
            "missing authoritative mutable Array binding:\n{output}"
        );
        assert!(
            output.contains("::nybl::methods::transactional_collection_method_in(")
                && output.contains("\"push\""),
            "missing shared in-place collection dispatch:\n{output}"
        );
        assert!(
            output.contains("__nybl_preflight_user_method(ctx")
                && output.contains("__nybl_call_preflighted_user_method(ctx")
                && output.contains("} else { let ")
                && output.contains("= __nybl_read_binding(ctx, \"<root>\", \"a\", 2)?;"),
            "dynamic non-array fallback must retain user-method dispatch:\n{output}"
        );
    }

    #[test]
    fn nested_mutating_argument_is_emitted_before_outer_receiver_borrow() {
        let output = emitted("let a = [1, 2]\na.push(a.pop())");
        let pop = output
            .find("\"pop\", ::std::vec::Vec::new(), 2")
            .unwrap_or_else(|| panic!("inner pop fast path:\n{output}"));
        let push = output
            .rfind("\"push\", ::std::vec![")
            .unwrap_or_else(|| panic!("outer push fast path:\n{output}"));

        assert!(
            pop < push,
            "argument expression must execute before the outer receiver borrow:\n{output}"
        );
    }

    #[test]
    fn mutating_non_ident_keeps_value_dispatch_path() {
        let output = emitted("[1].push(2)");

        assert!(
            !output.contains("::nybl::methods::transactional_array_method_in(&mut"),
            "temporary receiver must not use identifier write-back fast path:\n{output}"
        );
        assert!(
            output.contains("__nybl_call_method(ctx,"),
            "temporary receiver should retain generic value dispatch:\n{output}"
        );
    }
}
