mod diagnostics;
mod model;

use crate::error::{NyblError, NyblWarning};
use crate::parser::{AssignTarget, Expr, ExprKind, Parameter, Stmt, StmtKind, VariantPayload};
use diagnostics::check_match_exhaustive;
use model::*;

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::collections::{BTreeMap, BTreeSet};
#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    string::{String, ToString},
    vec::Vec,
};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn check_program<R>(stmts: &[Stmt], resolver: &mut R) -> Vec<NyblWarning>
where
    R: FnMut(&str) -> Option<Result<String, NyblError>>,
{
    let mut checker = Checker {
        resolver,
        module_cache: BTreeMap::new(),
        resolving_modules: BTreeSet::new(),
        cycle_tainted_modules: BTreeSet::new(),
    };
    checker.check_root(stmts)
}

struct Checker<'a, R> {
    resolver: &'a mut R,
    module_cache: BTreeMap<String, ModuleKnowledge>,
    resolving_modules: BTreeSet<String>,
    cycle_tainted_modules: BTreeSet<String>,
}

impl<R> Checker<'_, R>
where
    R: FnMut(&str) -> Option<Result<String, NyblError>>,
{
    fn check_root(&mut self, stmts: &[Stmt]) -> Vec<NyblWarning> {
        let module_path = crate::value::ROOT_MODULE_PATH;
        let sites = SiteCatalogue::discover(stmts, module_path);
        let callable_base = self.published_frame(stmts, module_path, &sites);
        let mut env = LexicalEnv::direct();
        let mut warnings = Vec::new();
        self.check_stmts(
            stmts,
            &mut env,
            &callable_base,
            module_path,
            &sites,
            &mut warnings,
        );
        warnings
    }

    fn published_frame(
        &mut self,
        stmts: &[Stmt],
        module_path: &str,
        sites: &SiteCatalogue,
    ) -> Frame {
        let mut env = LexicalEnv::direct();
        self.collect_published_namespace_effects(stmts, &mut env);
        let mut frame = env.frames.pop().expect("published module frame");

        let mut own_enums: BTreeMap<String, TypeBinding> = BTreeMap::new();
        let mut own_structs = BTreeSet::new();
        for stmt in stmts {
            match &stmt.kind {
                StmtKind::EnumDecl { name, variants } => {
                    let known = TypeBinding::Known(KnownEnum {
                        runtime_id: RuntimeTypeId {
                            module_path: module_path.to_string(),
                            type_name: name.clone(),
                        },
                        site: sites.enum_site(name, stmt),
                        shape: EnumShape::from_variants(variants),
                    });
                    own_enums
                        .entry(name.clone())
                        .and_modify(|current| *current = merge_possible_types(current, &known))
                        .or_insert(known);
                }
                StmtKind::StructDecl { name, .. } => {
                    own_structs.insert(name.clone());
                }
                _ => {}
            }
        }
        for name in own_structs {
            frame.types.insert(name, TypeBinding::Opaque);
        }
        for (name, binding) in own_enums {
            frame.types.insert(name, binding);
        }
        frame
    }

    fn collect_published_namespace_effects(
        &mut self,
        stmts: &[Stmt],
        env: &mut LexicalEnv,
    ) -> FlowEffects {
        let mut effects = FlowEffects::default();
        for stmt in stmts {
            match &stmt.kind {
                StmtKind::Use { path, items, alias } => {
                    self.apply_use(env, path, items.as_deref(), alias.as_deref());
                }
                StmtKind::Let { name, .. } => env.shadow_namespace_with_value(name),
                StmtKind::Assign {
                    target: AssignTarget::Variable(name),
                    ..
                } => {
                    if let Some(frame) = env.invalidate_visible_namespace(name) {
                        effects.invalidated_namespaces.insert(name.clone(), frame);
                    }
                }
                StmtKind::Assign { .. } => {}
                StmtKind::If {
                    body,
                    else_ifs,
                    else_body,
                    ..
                } => {
                    effects.merge(self.collect_published_child_effects(body, env, None));
                    for (_, body) in else_ifs {
                        effects.merge(self.collect_published_child_effects(body, env, None));
                    }
                    if let Some(body) = else_body {
                        effects.merge(self.collect_published_child_effects(body, env, None));
                    }
                    apply_inherited_effects(env, &mut effects);
                }
                StmtKind::While { body, .. } | StmtKind::Repeat { body, .. } => {
                    effects.merge(self.collect_published_child_effects(body, env, None));
                    apply_inherited_effects(env, &mut effects);
                }
                StmtKind::ForIn { var, body, .. } => {
                    effects.merge(self.collect_published_child_effects(body, env, Some(var)));
                    apply_inherited_effects(env, &mut effects);
                }
                StmtKind::FnDecl { name, .. } => {
                    env.block_future_namespace(name);
                    effects.alias_blockers.insert(name.clone());
                }
                StmtKind::MethodDecl { .. }
                | StmtKind::ExprStmt(_)
                | StmtKind::Return { .. }
                | StmtKind::EnumDecl { .. }
                | StmtKind::StructDecl { .. }
                | StmtKind::Break
                | StmtKind::Continue => {}
            }
        }
        effects
    }

    fn collect_published_child_effects(
        &mut self,
        body: &[Stmt],
        parent: &LexicalEnv,
        value_shadow: Option<&str>,
    ) -> FlowEffects {
        let mut child = parent.clone();
        child.push_frame();
        if let Some(name) = value_shadow {
            child.shadow_namespace_with_value(name);
        }
        self.collect_published_namespace_effects(body, &mut child)
    }

    fn module_knowledge(&mut self, path: &str) -> ModuleKnowledge {
        if let Some(cached) = self.module_cache.get(path) {
            return cached.clone();
        }
        if !self.resolving_modules.insert(path.to_string()) {
            self.cycle_tainted_modules
                .extend(self.resolving_modules.iter().cloned());
            return ModuleKnowledge::Opaque;
        }
        let mut knowledge = match (self.resolver)(path) {
            Some(Ok(source)) => match crate::parse(&source) {
                Ok(stmts) => {
                    let sites = SiteCatalogue::discover(&stmts, path);
                    let frame = self.published_frame(&stmts, path, &sites);
                    let value_names = frame
                        .value_shadows
                        .union(&frame.alias_blockers)
                        .cloned()
                        .collect();
                    let mut namespaces = frame.namespaces;
                    for name in &value_names {
                        namespaces.remove(name);
                    }
                    ModuleKnowledge::Known(ModuleSurface {
                        types: frame.types,
                        namespaces,
                        value_names,
                    })
                }
                Err(_) => ModuleKnowledge::Opaque,
            },
            _ => ModuleKnowledge::Opaque,
        };
        self.resolving_modules.remove(path);
        if self.cycle_tainted_modules.remove(path) {
            knowledge = ModuleKnowledge::Opaque;
        }
        self.module_cache
            .insert(path.to_string(), knowledge.clone());
        knowledge
    }

    fn apply_use_to_frame(
        &mut self,
        frame: &mut Frame,
        path: &str,
        items: Option<&[String]>,
        alias: Option<&str>,
    ) {
        let knowledge = self.module_knowledge(path);
        match (knowledge, alias) {
            (ModuleKnowledge::Known(surface), Some(alias_name)) => {
                let projected = project_surface(&surface, items, true);
                if frame.value_shadows.contains(alias_name)
                    || frame.alias_blockers.contains(alias_name)
                    || frame.namespaces.contains_key(alias_name)
                {
                    frame
                        .namespaces
                        .insert(alias_name.to_string(), NamespaceBinding::Opaque);
                } else {
                    frame
                        .namespaces
                        .insert(alias_name.to_string(), NamespaceBinding::Known(projected));
                }
            }
            (ModuleKnowledge::Opaque, Some(alias_name)) => {
                frame
                    .namespaces
                    .insert(alias_name.to_string(), NamespaceBinding::Opaque);
            }
            (ModuleKnowledge::Known(surface), None) => {
                let projected = project_surface(&surface, items, false);
                for name in projected.value_names {
                    if !frame.namespaces.contains_key(&name) {
                        frame.shadow_namespace_with_value(&name);
                    }
                }
                for (name, binding) in projected.types {
                    frame.types.entry(name).or_insert(binding);
                }
                for (name, binding) in projected.namespaces {
                    if !frame.value_shadows.contains(&name) && !frame.alias_blockers.contains(&name)
                    {
                        frame.namespaces.entry(name).or_insert(binding);
                    }
                }
            }
            (ModuleKnowledge::Opaque, None) => {
                if let Some(items) = items {
                    for item in items {
                        frame.alias_blockers.insert(item.clone());
                        frame
                            .types
                            .entry(item.clone())
                            .or_insert(TypeBinding::Opaque);
                        frame
                            .namespaces
                            .entry(item.clone())
                            .or_insert(NamespaceBinding::Opaque);
                    }
                }
            }
        }
    }

    fn apply_use(
        &mut self,
        env: &mut LexicalEnv,
        path: &str,
        items: Option<&[String]>,
        alias: Option<&str>,
    ) {
        let mut projected = Frame::default();
        self.apply_use_to_frame(&mut projected, path, items, alias);
        for (name, binding) in projected.types {
            env.bind_imported_type(&name, binding);
        }
        for name in projected
            .value_shadows
            .into_iter()
            .chain(projected.alias_blockers)
        {
            env.bind_imported_value_name(&name);
        }
        for (name, binding) in projected.namespaces {
            env.bind_imported_namespace(&name, binding);
        }
    }

    fn check_stmts(
        &mut self,
        stmts: &[Stmt],
        env: &mut LexicalEnv,
        callable_base: &Frame,
        module_path: &str,
        sites: &SiteCatalogue,
        warnings: &mut Vec<NyblWarning>,
    ) -> FlowEffects {
        let mut effects = FlowEffects::default();
        for stmt in stmts {
            effects.merge(self.check_stmt(stmt, env, callable_base, module_path, sites, warnings));
        }
        effects
    }

    fn check_stmt(
        &mut self,
        stmt: &Stmt,
        env: &mut LexicalEnv,
        callable_base: &Frame,
        module_path: &str,
        sites: &SiteCatalogue,
        warnings: &mut Vec<NyblWarning>,
    ) -> FlowEffects {
        let mut effects = FlowEffects::default();
        match &stmt.kind {
            StmtKind::Let { name, value, .. } => {
                self.check_expr(value, env, callable_base, module_path, sites, warnings);
                env.shadow_namespace_with_value(name);
            }
            StmtKind::Assign { target, value, .. } => {
                self.check_expr(value, env, callable_base, module_path, sites, warnings);
                self.check_target(target, env, callable_base, module_path, sites, warnings);
                if let AssignTarget::Variable(name) = target
                    && let Some(frame) = env.invalidate_visible_namespace(name)
                {
                    effects.invalidated_namespaces.insert(name.clone(), frame);
                }
            }
            StmtKind::ExprStmt(expr) => {
                self.check_expr(expr, env, callable_base, module_path, sites, warnings);
            }
            StmtKind::Return { value } => {
                if let Some(value) = value {
                    self.check_expr(value, env, callable_base, module_path, sites, warnings);
                }
            }
            StmtKind::If {
                condition,
                body,
                else_ifs,
                else_body,
            } => {
                self.check_expr(condition, env, callable_base, module_path, sites, warnings);
                effects.merge(self.check_child(
                    body,
                    env,
                    callable_base,
                    module_path,
                    sites,
                    warnings,
                ));
                for (condition, body) in else_ifs {
                    self.check_expr(condition, env, callable_base, module_path, sites, warnings);
                    effects.merge(self.check_child(
                        body,
                        env,
                        callable_base,
                        module_path,
                        sites,
                        warnings,
                    ));
                }
                if let Some(body) = else_body {
                    effects.merge(self.check_child(
                        body,
                        env,
                        callable_base,
                        module_path,
                        sites,
                        warnings,
                    ));
                }
                apply_inherited_effects(env, &mut effects);
            }
            StmtKind::While { condition, body } => {
                self.check_expr(condition, env, callable_base, module_path, sites, warnings);
                effects.merge(self.check_child(
                    body,
                    env,
                    callable_base,
                    module_path,
                    sites,
                    warnings,
                ));
                apply_inherited_effects(env, &mut effects);
            }
            StmtKind::Repeat { count, body } => {
                self.check_expr(count, env, callable_base, module_path, sites, warnings);
                effects.merge(self.check_child(
                    body,
                    env,
                    callable_base,
                    module_path,
                    sites,
                    warnings,
                ));
                apply_inherited_effects(env, &mut effects);
            }
            StmtKind::ForIn {
                var,
                iterable,
                body,
            } => {
                self.check_expr(iterable, env, callable_base, module_path, sites, warnings);
                let mut child = env.clone();
                child.push_frame();
                child.shadow_namespace_with_value(var);
                effects.merge(self.check_stmts(
                    body,
                    &mut child,
                    callable_base,
                    module_path,
                    sites,
                    warnings,
                ));
                apply_inherited_effects(env, &mut effects);
            }
            StmtKind::FnDecl {
                name, params, body, ..
            } => {
                self.check_callable(params, body, callable_base, module_path, sites, warnings);
                env.block_future_namespace(name);
                effects.alias_blockers.insert(name.clone());
            }
            StmtKind::MethodDecl { params, body, .. } => {
                self.check_callable(params, body, callable_base, module_path, sites, warnings);
            }
            StmtKind::Use { path, items, alias } => {
                self.apply_use(env, path, items.as_deref(), alias.as_deref());
            }
            StmtKind::EnumDecl { name, variants } => {
                env.bind_type(
                    name,
                    TypeBinding::Known(KnownEnum {
                        runtime_id: RuntimeTypeId {
                            module_path: module_path.to_string(),
                            type_name: name.clone(),
                        },
                        site: sites.enum_site(name, stmt),
                        shape: EnumShape::from_variants(variants),
                    }),
                );
            }
            StmtKind::StructDecl { name, .. } => {
                let same_identity_enum = matches!(
                    env.resolve_type(name),
                    Some(TypeBinding::Known(known))
                        if known.runtime_id.module_path == module_path
                            && known.runtime_id.type_name == *name
                );
                if !same_identity_enum {
                    env.bind_type(name, TypeBinding::Opaque);
                }
            }
            StmtKind::Break | StmtKind::Continue => {}
        }
        effects
    }

    fn check_child(
        &mut self,
        body: &[Stmt],
        parent: &LexicalEnv,
        callable_base: &Frame,
        module_path: &str,
        sites: &SiteCatalogue,
        warnings: &mut Vec<NyblWarning>,
    ) -> FlowEffects {
        let mut child = parent.clone();
        child.push_frame();
        self.check_stmts(
            body,
            &mut child,
            callable_base,
            module_path,
            sites,
            warnings,
        )
    }

    fn check_callable(
        &mut self,
        params: &[Parameter],
        body: &[Stmt],
        callable_base: &Frame,
        module_path: &str,
        sites: &SiteCatalogue,
        warnings: &mut Vec<NyblWarning>,
    ) {
        let param_names: Vec<String> = params.iter().map(|param| param.name.clone()).collect();
        let mut callable = LexicalEnv::callable(callable_base, &param_names);
        self.check_stmts(
            body,
            &mut callable,
            callable_base,
            module_path,
            sites,
            warnings,
        );
    }

    fn check_target(
        &mut self,
        target: &AssignTarget,
        env: &mut LexicalEnv,
        callable_base: &Frame,
        module_path: &str,
        sites: &SiteCatalogue,
        warnings: &mut Vec<NyblWarning>,
    ) {
        match target {
            AssignTarget::Variable(_) => {}
            AssignTarget::Index { object, index } => {
                self.check_expr(object, env, callable_base, module_path, sites, warnings);
                self.check_expr(index, env, callable_base, module_path, sites, warnings);
            }
            AssignTarget::Field { object, .. } => {
                self.check_expr(object, env, callable_base, module_path, sites, warnings);
            }
        }
    }

    fn check_expr(
        &mut self,
        expr: &Expr,
        env: &mut LexicalEnv,
        callable_base: &Frame,
        module_path: &str,
        sites: &SiteCatalogue,
        warnings: &mut Vec<NyblWarning>,
    ) {
        match &expr.kind {
            ExprKind::Match { scrutinee, arms } => {
                self.check_expr(scrutinee, env, callable_base, module_path, sites, warnings);
                for arm in arms {
                    let mut arm_env = env.clone();
                    arm_env.push_frame();
                    for name in arm.pattern.binding_names() {
                        arm_env.shadow_namespace_with_value(&name);
                    }
                    if let Some(guard) = &arm.guard {
                        self.check_expr(
                            guard,
                            &mut arm_env,
                            callable_base,
                            module_path,
                            sites,
                            warnings,
                        );
                    }
                    self.check_expr(
                        &arm.body,
                        &mut arm_env,
                        callable_base,
                        module_path,
                        sites,
                        warnings,
                    );
                }
                check_match_exhaustive(arms, env, expr.line, warnings);
            }
            ExprKind::BinaryOp { left, right, .. } => {
                self.check_expr(left, env, callable_base, module_path, sites, warnings);
                self.check_expr(right, env, callable_base, module_path, sites, warnings);
            }
            ExprKind::UnaryOp { expr, .. } | ExprKind::Try(expr) => {
                self.check_expr(expr, env, callable_base, module_path, sites, warnings);
            }
            ExprKind::Call { callee, args } => {
                self.check_expr(callee, env, callable_base, module_path, sites, warnings);
                for arg in args {
                    self.check_expr(arg, env, callable_base, module_path, sites, warnings);
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                self.check_expr(object, env, callable_base, module_path, sites, warnings);
                for arg in args {
                    self.check_expr(arg, env, callable_base, module_path, sites, warnings);
                }
            }
            ExprKind::FieldAccess { object, .. } => {
                self.check_expr(object, env, callable_base, module_path, sites, warnings);
            }
            ExprKind::StructConstruct { fields, .. } => {
                for (_, value) in fields {
                    self.check_expr(value, env, callable_base, module_path, sites, warnings);
                }
            }
            ExprKind::EnumConstruct { payload, .. } => match payload {
                VariantPayload::Unit => {}
                VariantPayload::Tuple(values) => {
                    for value in values {
                        self.check_expr(value, env, callable_base, module_path, sites, warnings);
                    }
                }
                VariantPayload::Struct(fields) => {
                    for (_, value) in fields {
                        self.check_expr(value, env, callable_base, module_path, sites, warnings);
                    }
                }
            },
            ExprKind::Index { object, index } => {
                self.check_expr(object, env, callable_base, module_path, sites, warnings);
                self.check_expr(index, env, callable_base, module_path, sites, warnings);
            }
            ExprKind::Array(values) => {
                for value in values {
                    self.check_expr(value, env, callable_base, module_path, sites, warnings);
                }
            }
            ExprKind::Dict(entries) => {
                for (_, value) in entries {
                    self.check_expr(value, env, callable_base, module_path, sites, warnings);
                }
            }
            ExprKind::IfExpr {
                condition,
                then_expr,
                else_expr,
            } => {
                self.check_expr(condition, env, callable_base, module_path, sites, warnings);
                self.check_expr(then_expr, env, callable_base, module_path, sites, warnings);
                self.check_expr(else_expr, env, callable_base, module_path, sites, warnings);
            }
            ExprKind::Lambda { params, body } => {
                self.check_callable(params, body, callable_base, module_path, sites, warnings);
            }
            ExprKind::Int(_)
            | ExprKind::Number(_)
            | ExprKind::Str(_)
            | ExprKind::StringInterp(_)
            | ExprKind::Bool(_)
            | ExprKind::None
            | ExprKind::Ident(_) => {}
        }
    }
}

fn project_surface(
    surface: &ModuleSurface,
    items: Option<&[String]>,
    include_private: bool,
) -> ModuleSurface {
    let selected = |name: &str| match items {
        Some(items) => items.iter().any(|item| item == name),
        None => include_private || !crate::naming::is_private(name),
    };
    ModuleSurface {
        types: surface
            .types
            .iter()
            .filter(|(name, _)| selected(name))
            .map(|(name, binding)| (name.clone(), binding.clone()))
            .collect(),
        namespaces: surface
            .namespaces
            .iter()
            .filter(|(name, _)| selected(name))
            .map(|(name, binding)| (name.clone(), binding.clone()))
            .collect(),
        value_names: surface
            .value_names
            .iter()
            .filter(|name| selected(name))
            .cloned()
            .collect(),
    }
}
