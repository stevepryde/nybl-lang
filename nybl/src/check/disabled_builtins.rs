//! Load-time detection of calls to host-disabled engine builtins.
//!
//! Hosts can forbid specific engine builtins per run via
//! [`crate::NyblLimits::disabled_builtins`] (e.g. a deterministic
//! simulation host disabling `rand` so all randomness flows through its
//! own seeded RNG). This pass flags *definite* references to a disabled
//! builtin before any program statement executes; every engine also
//! carries a runtime backstop at its builtin dispatch site so a
//! reference this pass cannot prove statically still fails — fatally and
//! uncatchably — the moment it would actually invoke the builtin.
//!
//! # What counts as a builtin reference
//!
//! Engine builtins are reachable only as direct calls `name(...)` — they
//! are not first-class values. A direct call to a builtin name is
//! treated as a builtin reference unless the program can bind that name
//! to a value: a `let`/`const` of the name, a function/method/lambda
//! parameter, a `for`-loop variable, a match-arm binding, a selective
//! import (`use m.{name}`), a module alias (`use m as name`), or any
//! glob import (`use m` — its exports are unknowable statically). All
//! engines dispatch a lexically bound value before consulting builtins,
//! so a bound name never reaches the builtin.
//!
//! The suppression is deliberately file-coarse (a binding anywhere in
//! the AST suppresses the name everywhere in that AST) rather than
//! scope-precise. Precision would require replicating each engine's
//! capture and visibility rules; coarseness only delays the diagnostic
//! from load time to the runtime backstop, and both fire the same fatal
//! error. Note that a `fn name(...)` *declaration* does not suppress the
//! check: engines do not agree today on whether a user `fn` of a builtin
//! name shadows the builtin for direct calls (walker and AOT dispatch
//! the builtin; the VM dispatches the user fn), so the only
//! parity-preserving choice is to reject the program up front.
//!
//! # Split for compiled artifacts
//!
//! Collection ([`collect_builtin_usage`]) depends only on the AST, while
//! checking ([`check_disabled_builtins`]) depends only on the resulting
//! usage map. A compiled artifact can therefore store the map once and
//! re-run just the cheap set intersection at instantiate time.

use crate::error::NyblError;
use crate::parser::{AssignTarget, Expr, ExprKind, Parameter, Stmt, StmtKind, VariantPayload};

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    collections::{BTreeMap, BTreeSet},
    format,
    string::{String, ToString},
};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::collections::{BTreeMap, BTreeSet};

/// The complete set of engine builtins. Must stay in sync with the
/// builtin dispatch arms in the walker (`Evaluator::call_function`), the
/// VM (`Vm::call` / `Vm::invoke_named_fallback`), and the AOT emitter
/// (`value_named_call_src`).
pub const ENGINE_BUILTINS: [&str; 5] = ["range", "rand", "print", "try_call", "panic"];

/// Build the error every engine raises for a disabled builtin, at load
/// time and at the runtime backstop alike. Fatal so `try_call` can
/// never swallow it — a disabled builtin is a programming error the
/// host has ruled out, not a recoverable condition.
pub fn disabled_builtin_error(name: &str, line: u32) -> NyblError {
    let mut error = NyblError::fatal(format!("builtin `{name}` is disabled by the host"), line);
    error.friendly_hint = Some(format!(
        "The host running this program has disabled the `{name}` builtin. Use the alternative the host provides instead."
    ));
    error
}

/// Collect every engine-builtin name the program can only satisfy via
/// the builtin, mapped to the source line of its first direct call.
///
/// A name is omitted when the AST contains any value binding of that
/// name (see the module docs for the exact binding forms) — such
/// programs are left to the runtime backstop.
pub fn collect_builtin_usage(stmts: &[Stmt]) -> BTreeMap<String, u32> {
    let mut collector = UsageCollector::default();
    collector.walk_stmts(stmts);
    if collector.glob_import {
        // A glob import can inject any exported name unqualified, so
        // every builtin name is potentially bound.
        return BTreeMap::new();
    }
    collector
        .first_call_lines
        .into_iter()
        .filter(|(name, _)| !collector.bound_names.contains(name))
        .collect()
}

/// Reject the usage map against a host deny set. On a hit, the error
/// names the builtin and points at its first unshadowed call.
///
/// Unknown names in `disabled` are allowed — they simply never match,
/// which keeps older programs loadable under hosts that disable
/// builtins added by newer engines.
pub fn check_disabled_builtins(
    usage: &BTreeMap<String, u32>,
    disabled: &BTreeSet<String>,
) -> Result<(), NyblError> {
    if disabled.is_empty() {
        return Ok(());
    }
    // Report the violation earliest in the source, not first
    // alphabetically, so the diagnostic matches reading order.
    let violation = usage
        .iter()
        .filter(|(name, _)| disabled.contains(*name))
        .min_by_key(|(name, line)| (**line, (*name).clone()));
    match violation {
        Some((name, line)) => Err(disabled_builtin_error(name, *line)),
        None => Ok(()),
    }
}

/// Convenience used by every engine load path: collect and check in one
/// call, skipping the AST walk entirely when the deny set is empty.
pub fn enforce_disabled_builtins(
    stmts: &[Stmt],
    disabled: &BTreeSet<String>,
) -> Result<(), NyblError> {
    if disabled.is_empty() {
        return Ok(());
    }
    check_disabled_builtins(&collect_builtin_usage(stmts), disabled)
}

#[derive(Default)]
struct UsageCollector {
    first_call_lines: BTreeMap<String, u32>,
    bound_names: BTreeSet<String>,
    glob_import: bool,
}

impl UsageCollector {
    fn bind(&mut self, name: &str) {
        self.bound_names.insert(name.to_string());
    }

    fn bind_params(&mut self, params: &[Parameter]) {
        for param in params {
            self.bind(&param.name);
        }
    }

    fn record_call(&mut self, name: &str, line: u32) {
        if ENGINE_BUILTINS.contains(&name) {
            self.first_call_lines
                .entry(name.to_string())
                .or_insert(line);
        }
    }

    fn walk_stmts(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            match &stmt.kind {
                StmtKind::Let { name, value, .. } => {
                    self.walk_expr(value);
                    self.bind(name);
                }
                StmtKind::Assign { target, value, .. } => {
                    match target {
                        AssignTarget::Variable(_) => {}
                        AssignTarget::Index { object, index } => {
                            self.walk_expr(object);
                            self.walk_expr(index);
                        }
                        AssignTarget::Field { object, .. } => self.walk_expr(object),
                    }
                    self.walk_expr(value);
                }
                StmtKind::If {
                    condition,
                    body,
                    else_ifs,
                    else_body,
                } => {
                    self.walk_expr(condition);
                    self.walk_stmts(body);
                    for (condition, body) in else_ifs {
                        self.walk_expr(condition);
                        self.walk_stmts(body);
                    }
                    if let Some(body) = else_body {
                        self.walk_stmts(body);
                    }
                }
                StmtKind::While { condition, body } => {
                    self.walk_expr(condition);
                    self.walk_stmts(body);
                }
                StmtKind::Repeat { count, body } => {
                    self.walk_expr(count);
                    self.walk_stmts(body);
                }
                StmtKind::ForIn {
                    var,
                    iterable,
                    body,
                } => {
                    self.bind(var);
                    self.walk_expr(iterable);
                    self.walk_stmts(body);
                }
                // The declared fn *name* is deliberately not a binding —
                // see the module docs.
                StmtKind::FnDecl { params, body, .. }
                | StmtKind::MethodDecl { params, body, .. } => {
                    self.bind_params(params);
                    self.walk_stmts(body);
                }
                StmtKind::Return { value } => {
                    if let Some(value) = value {
                        self.walk_expr(value);
                    }
                }
                StmtKind::Use { items, alias, .. } => match (items, alias) {
                    (Some(items), None) => {
                        for item in items {
                            self.bind(item);
                        }
                    }
                    (_, Some(alias)) => self.bind(alias),
                    (None, None) => self.glob_import = true,
                },
                StmtKind::ExprStmt(expr) => self.walk_expr(expr),
                StmtKind::Break
                | StmtKind::Continue
                | StmtKind::PublicSurface { .. }
                | StmtKind::StructDecl { .. }
                | StmtKind::EnumDecl { .. } => {}
            }
        }
    }

    fn walk_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Int(_)
            | ExprKind::Number(_)
            | ExprKind::Str(_)
            | ExprKind::StringInterp(_)
            | ExprKind::Bool(_)
            | ExprKind::None
            | ExprKind::Ident(_) => {}
            ExprKind::BinaryOp { left, right, .. } => {
                self.walk_expr(left);
                self.walk_expr(right);
            }
            ExprKind::UnaryOp { expr, .. } | ExprKind::Try(expr) => self.walk_expr(expr),
            ExprKind::Call { callee, args } => {
                if let ExprKind::Ident(name) = &callee.kind {
                    self.record_call(name, callee.line);
                } else {
                    self.walk_expr(callee);
                }
                for arg in args {
                    self.walk_expr(&arg.value);
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                self.walk_expr(object);
                for arg in args {
                    self.walk_expr(&arg.value);
                }
            }
            ExprKind::FieldAccess { object, .. } => self.walk_expr(object),
            ExprKind::StructConstruct { fields, .. } => {
                for (_, value) in fields {
                    self.walk_expr(value);
                }
            }
            ExprKind::EnumConstruct { payload, .. } => match payload {
                VariantPayload::Unit => {}
                VariantPayload::Tuple(values) => {
                    for value in values {
                        self.walk_expr(value);
                    }
                }
                VariantPayload::Struct(fields) => {
                    for (_, value) in fields {
                        self.walk_expr(value);
                    }
                }
            },
            ExprKind::Index { object, index } => {
                self.walk_expr(object);
                self.walk_expr(index);
            }
            ExprKind::Array(values) => {
                for value in values {
                    self.walk_expr(value);
                }
            }
            ExprKind::Dict(entries) => {
                for (_, value) in entries {
                    self.walk_expr(value);
                }
            }
            ExprKind::IfExpr {
                condition,
                then_expr,
                else_expr,
            } => {
                self.walk_expr(condition);
                self.walk_expr(then_expr);
                self.walk_expr(else_expr);
            }
            ExprKind::Lambda { params, body } => {
                self.bind_params(params);
                self.walk_stmts(body);
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_expr(scrutinee);
                for arm in arms {
                    for name in arm.pattern.binding_names() {
                        self.bind(&name);
                    }
                    if let Some(guard) = &arm.guard {
                        self.walk_expr(guard);
                    }
                    self.walk_expr(&arm.body);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn usage(source: &str) -> BTreeMap<String, u32> {
        collect_builtin_usage(&parse(source).unwrap())
    }

    #[test]
    fn direct_calls_report_first_line_per_builtin() {
        let map = usage("print(\"a\")\nlet r = rand(5)\nprint(rand(6))");
        assert_eq!(map.get("print"), Some(&1));
        assert_eq!(map.get("rand"), Some(&2));
        assert_eq!(map.get("range"), None);
    }

    #[test]
    fn value_bindings_suppress_the_name_but_not_others() {
        // A let, a lambda param, a for-var, and a match binding each
        // suppress their own name; `print` stays flagged.
        for source in [
            "let rand = fn(n) { return n }\nprint(rand(1))",
            "let f = fn(rand) { return rand(1) }\nprint(f(2))",
            "for rand in [1] { print(rand(1)) }",
            "let _ = match 1 { rand => rand(1) }\nprint(2)",
        ] {
            let map = usage(source);
            assert_eq!(map.get("rand"), None, "source: {source}");
            assert!(map.contains_key("print"), "source: {source}");
        }
    }

    #[test]
    fn fn_declarations_do_not_suppress() {
        // Engines disagree on user-fn-vs-builtin priority for direct
        // calls, so a `fn rand` must still flag the call.
        let map = usage("fn rand(n) { return n }\nlet x = rand(1)");
        assert_eq!(map.get("rand"), Some(&2));
    }

    #[test]
    fn imports_suppress_selective_alias_and_glob_forms() {
        // Selective import binds exactly the listed names.
        let map = usage("use m.{rand}\nlet x = rand(1)\nprint(x)");
        assert_eq!(map.get("rand"), None);
        assert!(map.contains_key("print"));

        // Module alias binds the alias name.
        let map = usage("use m as rand\nlet x = rand(1)\nprint(x)");
        assert_eq!(map.get("rand"), None);

        // Glob imports can bind anything, so everything is suppressed.
        assert!(usage("use m\nlet x = rand(1)\nprint(x)").is_empty());
    }

    #[test]
    fn checker_reports_earliest_violation_and_ignores_unknown_names() {
        let disabled: BTreeSet<String> = [
            "rand".to_string(),
            "print".to_string(),
            "no_such".to_string(),
        ]
        .into_iter()
        .collect();
        let error = check_disabled_builtins(&usage("print(1)\nlet x = rand(2)"), &disabled)
            .expect_err("both builtins are disabled");
        assert_eq!(error.message, "builtin `print` is disabled by the host");
        assert_eq!(error.line, Some(1));
        assert!(error.is_fatal);

        let only_unknown: BTreeSet<String> = ["no_such".to_string()].into_iter().collect();
        check_disabled_builtins(&usage("print(1)"), &only_unknown)
            .expect("unknown deny-list names never match");
    }
}
