//! Shared exhaustive AST traversal for declaration-site discovery.
//!
//! Runtime code generation and advisory analysis deliberately attach different
//! semantics to declaration sites, but they must agree on where those sites
//! can occur. This visitor owns that structural invariant and nothing else.

use crate::parser::{
    AssignTarget, Expr, ExprKind, Parameter, Stmt, StmtKind, VariantDecl, VariantPayload,
};

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::string::String;

/// Callback surface for every declaration statement found by
/// [`visit_declaration_sites`]. The traversal reports a declaration before
/// descending into its body.
pub trait DeclarationSiteVisitor {
    fn visit_struct(&mut self, name: &str, fields: &[String], stmt: &Stmt);

    fn visit_enum(&mut self, name: &str, variants: &[VariantDecl], stmt: &Stmt);

    fn visit_method(
        &mut self,
        type_name: &str,
        method_name: &str,
        params: &[Parameter],
        body: &[Stmt],
        stmt: &Stmt,
    );
}

/// Walk every statement, assignment-target, and expression edge in source
/// order, reporting struct, enum, and method declaration sites.
pub fn visit_declaration_sites(stmts: &[Stmt], visitor: &mut impl DeclarationSiteVisitor) {
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::StructDecl { name, fields } => {
                visitor.visit_struct(name, fields, stmt);
            }
            StmtKind::EnumDecl { name, variants } => {
                visitor.visit_enum(name, variants, stmt);
            }
            StmtKind::Let { value, .. } => visit_expr(value, visitor),
            StmtKind::Assign { target, value, .. } => {
                visit_target(target, visitor);
                visit_expr(value, visitor);
            }
            StmtKind::If {
                condition,
                body,
                else_ifs,
                else_body,
            } => {
                visit_expr(condition, visitor);
                visit_declaration_sites(body, visitor);
                for (condition, body) in else_ifs {
                    visit_expr(condition, visitor);
                    visit_declaration_sites(body, visitor);
                }
                if let Some(body) = else_body {
                    visit_declaration_sites(body, visitor);
                }
            }
            StmtKind::While { condition, body } => {
                visit_expr(condition, visitor);
                visit_declaration_sites(body, visitor);
            }
            StmtKind::Repeat { count, body } => {
                visit_expr(count, visitor);
                visit_declaration_sites(body, visitor);
            }
            StmtKind::ForIn { iterable, body, .. } => {
                visit_expr(iterable, visitor);
                visit_declaration_sites(body, visitor);
            }
            StmtKind::FnDecl { body, .. } => visit_declaration_sites(body, visitor),
            StmtKind::MethodDecl {
                type_name,
                method_name,
                params,
                body,
            } => {
                visitor.visit_method(type_name, method_name, params, body, stmt);
                visit_declaration_sites(body, visitor);
            }
            StmtKind::Return { value } => {
                if let Some(value) = value {
                    visit_expr(value, visitor);
                }
            }
            StmtKind::ExprStmt(expr) => visit_expr(expr, visitor),
            StmtKind::Break | StmtKind::Continue | StmtKind::Use { .. } => {}
        }
    }
}

fn visit_target(target: &AssignTarget, visitor: &mut impl DeclarationSiteVisitor) {
    match target {
        AssignTarget::Variable(_) => {}
        AssignTarget::Index { object, index } => {
            visit_expr(object, visitor);
            visit_expr(index, visitor);
        }
        AssignTarget::Field { object, .. } => visit_expr(object, visitor),
    }
}

fn visit_expr(expr: &Expr, visitor: &mut impl DeclarationSiteVisitor) {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Number(_)
        | ExprKind::Str(_)
        | ExprKind::StringInterp(_)
        | ExprKind::Bool(_)
        | ExprKind::None
        | ExprKind::Ident(_) => {}
        ExprKind::BinaryOp { left, right, .. } => {
            visit_expr(left, visitor);
            visit_expr(right, visitor);
        }
        ExprKind::UnaryOp { expr, .. } | ExprKind::Try(expr) => {
            visit_expr(expr, visitor);
        }
        ExprKind::Call { callee, args } => {
            visit_expr(callee, visitor);
            for arg in args {
                visit_expr(arg, visitor);
            }
        }
        ExprKind::MethodCall { object, args, .. } => {
            visit_expr(object, visitor);
            for arg in args {
                visit_expr(arg, visitor);
            }
        }
        ExprKind::FieldAccess { object, .. } => visit_expr(object, visitor),
        ExprKind::StructConstruct { fields, .. } => {
            for (_, value) in fields {
                visit_expr(value, visitor);
            }
        }
        ExprKind::EnumConstruct { payload, .. } => match payload {
            VariantPayload::Unit => {}
            VariantPayload::Tuple(values) => {
                for value in values {
                    visit_expr(value, visitor);
                }
            }
            VariantPayload::Struct(fields) => {
                for (_, value) in fields {
                    visit_expr(value, visitor);
                }
            }
        },
        ExprKind::Index { object, index } => {
            visit_expr(object, visitor);
            visit_expr(index, visitor);
        }
        ExprKind::Array(values) => {
            for value in values {
                visit_expr(value, visitor);
            }
        }
        ExprKind::Dict(entries) => {
            for (_, value) in entries {
                visit_expr(value, visitor);
            }
        }
        ExprKind::IfExpr {
            condition,
            then_expr,
            else_expr,
        } => {
            visit_expr(condition, visitor);
            visit_expr(then_expr, visitor);
            visit_expr(else_expr, visitor);
        }
        ExprKind::Lambda { body, .. } => visit_declaration_sites(body, visitor),
        ExprKind::Match { scrutinee, arms } => {
            visit_expr(scrutinee, visitor);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    visit_expr(guard, visitor);
                }
                visit_expr(&arm.body, visitor);
            }
        }
    }
}
