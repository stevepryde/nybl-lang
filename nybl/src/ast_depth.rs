//! Static nesting depth of executable code.
//!
//! Every engine charges a callable body's static depth against
//! [`crate::NyblLimits::max_nesting`] while that body is active, so the
//! native stack an execution can occupy is bounded by limits the host chooses
//! rather than by how deeply a script nests code across recursive calls and
//! module loads. The measure is shared so the walker, VM, and AOT engines trip
//! the budget at the same call.
//!
//! Depth is `1 + max(child depth)` over statements, expressions, match arms,
//! and patterns. A nested `fn`, lambda, or method body is a leaf: it runs in
//! its own frame and is charged when it is called.

use crate::parser::{
    AssignTarget, Expr, ExprKind, MatchArm, Pattern, Stmt, StmtKind, VariantPatternPayload,
    VariantPayload,
};

/// Static nesting depth of a statement list run as one frame: a function,
/// lambda, or method body, a module top level, or a root program.
pub fn body_depth(stmts: &[Stmt]) -> u32 {
    block(stmts)
}

fn block(stmts: &[Stmt]) -> u32 {
    1 + stmts.iter().map(stmt).max().unwrap_or(0)
}

fn level(children: impl IntoIterator<Item = u32>) -> u32 {
    children.into_iter().max().unwrap_or(0).saturating_add(1)
}

fn stmt(stmt: &Stmt) -> u32 {
    match &stmt.kind {
        StmtKind::Let { value, .. } => level([expr(value)]),
        StmtKind::Assign { target, value, .. } => {
            let target = match target {
                AssignTarget::Variable(_) => 0,
                AssignTarget::Index { object, index } => level([expr(object), expr(index)]),
                AssignTarget::Field { object, .. } => level([expr(object)]),
            };
            level([target, expr(value)])
        }
        StmtKind::If {
            condition,
            body,
            else_ifs,
            else_body,
        } => level(
            [expr(condition), block(body)]
                .into_iter()
                .chain(
                    else_ifs
                        .iter()
                        .flat_map(|(condition, body)| [expr(condition), block(body)]),
                )
                .chain(else_body.as_deref().map(block)),
        ),
        StmtKind::While { condition, body } => level([expr(condition), block(body)]),
        StmtKind::Repeat { count, body } => level([expr(count), block(body)]),
        StmtKind::ForIn { iterable, body, .. } => level([expr(iterable), block(body)]),
        StmtKind::Return { value } => level(value.as_ref().map(expr)),
        StmtKind::ExprStmt(value) => level([expr(value)]),
        StmtKind::FnDecl { .. }
        | StmtKind::MethodDecl { .. }
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Use { .. }
        | StmtKind::PublicSurface { .. }
        | StmtKind::StructDecl { .. }
        | StmtKind::EnumDecl { .. } => 1,
    }
}

fn expr(value: &Expr) -> u32 {
    match &value.kind {
        ExprKind::Int(_)
        | ExprKind::Number(_)
        | ExprKind::Str(_)
        | ExprKind::StringInterp(_)
        | ExprKind::Bool(_)
        | ExprKind::None
        | ExprKind::Ident(_)
        | ExprKind::Lambda { .. } => 1,
        ExprKind::BinaryOp { left, right, .. } => level([expr(left), expr(right)]),
        ExprKind::UnaryOp { expr: inner, .. } | ExprKind::Try(inner) => level([expr(inner)]),
        ExprKind::Call { callee, args } => {
            level(core::iter::once(expr(callee)).chain(args.iter().map(|arg| expr(&arg.value))))
        }
        ExprKind::MethodCall { object, args, .. } => {
            level(core::iter::once(expr(object)).chain(args.iter().map(|arg| expr(&arg.value))))
        }
        ExprKind::FieldAccess { object, .. } => level([expr(object)]),
        ExprKind::StructConstruct { fields, .. } | ExprKind::Dict(fields) => {
            level(fields.iter().map(|(_, value)| expr(value)))
        }
        ExprKind::EnumConstruct { payload, .. } => match payload {
            VariantPayload::Unit => 1,
            VariantPayload::Tuple(values) => level(values.iter().map(expr)),
            VariantPayload::Struct(fields) => level(fields.iter().map(|(_, value)| expr(value))),
        },
        ExprKind::Index { object, index } => level([expr(object), expr(index)]),
        ExprKind::Array(values) => level(values.iter().map(expr)),
        ExprKind::IfExpr {
            condition,
            then_expr,
            else_expr,
        } => level([expr(condition), expr(then_expr), expr(else_expr)]),
        ExprKind::Match { scrutinee, arms } => {
            level(core::iter::once(expr(scrutinee)).chain(arms.iter().map(arm)))
        }
    }
}

fn arm(arm: &MatchArm) -> u32 {
    level([
        pattern(&arm.pattern),
        arm.guard.as_ref().map_or(0, expr),
        expr(&arm.body),
    ])
}

fn pattern(value: &Pattern) -> u32 {
    match value {
        Pattern::Literal(_) | Pattern::Wildcard | Pattern::Binding(_) => 1,
        Pattern::EnumVariant { payload, .. } => match payload {
            VariantPatternPayload::Unit => 1,
            VariantPatternPayload::Tuple(patterns) => level(patterns.iter().map(pattern)),
            VariantPatternPayload::Struct { fields, .. } => {
                level(fields.iter().map(|(_, value)| pattern(value)))
            }
        },
        Pattern::Struct { fields, .. } => level(fields.iter().map(|(_, value)| pattern(value))),
        Pattern::Array { elements, .. } | Pattern::Or(elements) => {
            level(elements.iter().map(pattern))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::body_depth;

    fn depth(source: &str) -> u32 {
        body_depth(&crate::parse(source).expect("source parses"))
    }

    #[test]
    fn depth_follows_the_deepest_branch() {
        // The measure is what the engines charge, so pin its shape on
        // independently countable programs: block -> stmt -> expr nodes.
        assert_eq!(depth(""), 1);
        assert_eq!(depth("1"), 3);
        assert_eq!(depth("[[1]]"), 5);
        assert_eq!(depth("1\n[[[1]]]\n2"), 6);
        assert_eq!(depth("if true { [1] }"), 6);
    }

    #[test]
    fn nested_callable_bodies_are_leaves() {
        assert_eq!(depth("fn f() { [[[[[1]]]]] }"), 2);
        assert_eq!(depth("let f = fn() { [[[[[1]]]]] }"), 3);
    }
}
