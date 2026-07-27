//! Static advisory checks that run after parsing and before execution.
//!
//! Match exhaustiveness is deliberately conservative. The checker follows
//! source-ordered lexical type bindings and only warns when every relevant arm
//! resolves to one proven runtime enum identity and one runtime-equivalent
//! ordered shape. Ambiguous imports, control-flow alternatives, and value-
//! shadowed namespaces suppress the advisory rather than risking a false
//! diagnostic.

mod source_order;

use crate::error::NyblWarning;
use crate::parser::Stmt;

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{string::String, vec::Vec};

/// Run static checks without module source. Imported types remain opaque.
pub fn check_program(stmts: &[Stmt]) -> Vec<NyblWarning> {
    let mut resolver = |_path: &str| None;
    source_order::check_program(stmts, &mut resolver)
}

/// Run static checks with an advisory module resolver.
///
/// Resolver, parse, and cycle failures are treated as opaque module surfaces;
/// they never turn a warning pass into a hard error.
pub fn check_program_with_resolver<R>(stmts: &[Stmt], resolver: &mut R) -> Vec<NyblWarning>
where
    R: FnMut(&str) -> Option<Result<String, crate::error::NyblError>>,
{
    source_order::check_program(stmts, resolver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn warnings(source: &str) -> Vec<NyblWarning> {
        let stmts = parse(source).unwrap();
        check_program(&stmts)
    }

    #[test]
    fn exhaustive_match_produces_no_warning() {
        let src = r#"enum Shape { Circle(r), Square(s) }
fn area(s) {
    return match s {
        Shape::Circle(r) => r * r,
        Shape::Square(s) => s * s,
    }
}"#;
        assert!(warnings(src).is_empty());
    }

    #[test]
    fn wildcard_arm_counts_as_exhaustive() {
        let src = r#"enum Shape { Circle(r), Square(s), Triangle }
let s = Shape::Circle(5)
let _ = match s {
    Shape::Circle(r) => r,
    _ => 0,
}"#;
        assert!(warnings(src).is_empty());
    }

    #[test]
    fn bare_binding_arm_counts_as_exhaustive() {
        let src = r#"enum Shape { Circle(r), Square(s) }
let s = Shape::Circle(5)
let _ = match s {
    Shape::Circle(r) => r,
    other => 0,
}"#;
        assert!(warnings(src).is_empty());
    }

    #[test]
    fn missing_variant_warns() {
        let src = r#"enum Shape { Circle(r), Square(s), Triangle }
let s = Shape::Circle(5)
let _ = match s {
    Shape::Circle(r) => r,
    Shape::Square(s) => s,
}"#;
        let ws = warnings(src);
        assert_eq!(ws.len(), 1, "expected exactly one warning, got {ws:?}");
        assert!(
            ws[0].message.contains("non-exhaustive"),
            "msg: {}",
            ws[0].message
        );
        assert!(
            ws[0].message.contains("`Shape::Triangle`"),
            "msg: {}",
            ws[0].message
        );
    }

    #[test]
    fn guarded_arm_does_not_count_toward_coverage() {
        let src = r#"enum Light { Red, Green }
let l = Light::Red
let _ = match l {
    Light::Red if true => "stop",
    Light::Green => "go",
}"#;
        let ws = warnings(src);
        assert_eq!(ws.len(), 1, "expected a warning, got {ws:?}");
        assert!(ws[0].message.contains("`Light::Red`"));
    }

    #[test]
    fn or_pattern_covers_multiple_variants() {
        let src = r#"enum E { A, B, C }
let e = E::A
let _ = match e {
    E::A | E::B => 1,
    E::C => 2,
}"#;
        assert!(warnings(src).is_empty());
    }

    #[test]
    fn heterogeneous_match_skips_check() {
        // A match that mixes a literal and an enum variant
        // can't be exhaustiveness-checked by this pass —
        // returning zero warnings is the correct pragmatic
        // answer.
        let src = r#"enum Tag { A, B }
let _ = match 1 {
    1 => "one",
    2 => "two",
}"#;
        // This happens to include no enum at all — still zero
        // warnings. The test locks the "no false positives on
        // literal matches" invariant.
        assert!(warnings(src).is_empty());
    }

    #[test]
    fn unknown_enum_bails_rather_than_warning() {
        // `FromAnotherModule` isn't declared here; the check
        // shouldn't warn (we can't verify coverage).
        let src = r#"fn handle(x) {
    return match x {
        FromAnotherModule::A => 1,
        FromAnotherModule::B => 2,
    }
}"#;
        assert!(warnings(src).is_empty());
    }

    #[test]
    fn exhaustive_or_pattern_allows_reordered_bindings() {
        let src = r#"enum Pair { Forward(left, right), Reverse(left, right) }
fn sum(pair) {
    return match pair {
        Pair::Forward(left, right) | Pair::Reverse(right, left) => left + right,
    }
}"#;
        assert!(warnings(src).is_empty());
    }

    #[test]
    fn inconsistent_or_pattern_is_rejected_before_advisory_checks() {
        let error = parse("let value = match 1 { 1 | y => y, _ => 0 }")
            .expect_err("the parser must keep an invalid pattern out of the checker");
        assert!(error.message.contains("alternative 2"));
        assert_eq!(error.line, Some(1));
        assert!(error.friendly_hint.is_some());
    }

    #[test]
    fn warning_carries_match_line() {
        let src = r#"enum E { A, B }
let _ = match E::A {
    E::A => 1,
}"#;
        let ws = warnings(src);
        assert_eq!(ws.len(), 1);
        // The `match` keyword sits on line 2 of the source.
        assert_eq!(ws[0].line, Some(2));
    }

    #[test]
    fn match_inside_fn_body_is_checked() {
        let src = r#"enum E { A, B, C }
fn pick(e) {
    return match e {
        E::A => 1,
        E::B => 2,
    }
}"#;
        let ws = warnings(src);
        assert_eq!(ws.len(), 1);
        assert!(ws[0].message.contains("`E::C`"));
    }

    #[test]
    fn match_inside_if_branch_is_checked() {
        let src = r#"enum E { A, B, C }
let e = E::A
if true {
    let _ = match e {
        E::A => 1,
        E::B => 2,
    }
}"#;
        let ws = warnings(src);
        assert_eq!(ws.len(), 1);
        assert!(ws[0].message.contains("`E::C`"));
    }

    // ─── Cross-import exhaustiveness ──────────────────────────────

    /// Helper that wires a tiny `(&str, &str)` module map into
    /// the resolver closure shape.
    fn warnings_with_modules(source: &str, modules: &[(&str, &str)]) -> Vec<NyblWarning> {
        let stmts = parse(source).unwrap();
        let mut resolver = |name: &str| -> Option<Result<String, crate::error::NyblError>> {
            modules
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, src)| Ok(String::from(*src)))
        };
        check_program_with_resolver(&stmts, &mut resolver)
    }

    #[test]
    fn imported_enum_missing_variant_warns_via_resolver() {
        // `use` brings `Shape` in from the `geom` module; the
        // match under-covers (`Triangle` unhandled), so the
        // checker — now that it follows the import — fires a
        // warning naming the missing variant.
        let ws = warnings_with_modules(
            r#"use geom
let s = Shape::Circle(5)
let _ = match s {
    Shape::Circle(r) => r,
    Shape::Square(s) => s,
}"#,
            &[("geom", "enum Shape { Circle(r), Square(s), Triangle }")],
        );
        assert_eq!(ws.len(), 1);
        assert!(
            ws[0].message.contains("`Shape::Triangle`"),
            "got: {}",
            ws[0].message
        );
    }

    #[test]
    fn imported_enum_exhaustive_match_produces_no_warning_via_resolver() {
        let ws = warnings_with_modules(
            r#"use geom
let s = Shape::Circle(5)
let _ = match s {
    Shape::Circle(r) => r,
    Shape::Square(s) => s,
    Shape::Triangle => 0,
}"#,
            &[("geom", "enum Shape { Circle(r), Square(s), Triangle }")],
        );
        assert!(
            ws.is_empty(),
            "expected no warnings when all variants covered, got: {ws:?}"
        );
    }

    #[test]
    fn transitive_imported_enum_is_picked_up() {
        // `a` re-exports via `use b`; the root's match over
        // the enum declared in `b` should still be checkable.
        let ws = warnings_with_modules(
            r#"use a
let c = Color::Red
let _ = match c {
    Color::Red => "r",
    Color::Blue => "b",
}"#,
            &[("a", "use b"), ("b", "enum Color { Red, Blue, Green }")],
        );
        assert_eq!(ws.len(), 1);
        assert!(
            ws[0].message.contains("`Color::Green`"),
            "got: {}",
            ws[0].message
        );
    }

    #[test]
    fn unresolvable_module_is_silently_skipped() {
        // Resolver returns None → the checker treats the
        // enum as opaque and suppresses the warning rather
        // than raising a hard error. Matches the advisory
        // nature of `check_program`.
        let ws = warnings_with_modules(
            r#"use missing
let c = Color::Red
let _ = match c {
    Color::Red => 1,
}"#,
            &[],
        );
        assert!(ws.is_empty());
    }

    #[test]
    fn root_enum_shadows_imported_same_name() {
        // If the root program declares `Color` too, the
        // root's definition wins (first-write-wins). The
        // imported enum's variants don't leak into the
        // check.
        let ws = warnings_with_modules(
            r#"use paint
enum Color { Red, Blue }
let c = Color::Red
let _ = match c {
    Color::Red => 1,
    Color::Blue => 2,
}"#,
            &[("paint", "enum Color { Red, Green, Yellow }")],
        );
        assert!(
            ws.is_empty(),
            "expected no warning: root's Color is fully covered, got: {ws:?}"
        );
    }

    #[test]
    fn sibling_branch_enum_sites_do_not_cross_contaminate() {
        let ws = warnings(
            r#"if true {
    enum Choice { Left }
    let _ = match Choice::Left { Choice::Left => 1 }
} else {
    enum Choice { Right }
}"#,
        );
        assert!(ws.is_empty(), "sibling declaration leaked: {ws:?}");

        let ws = warnings(
            r#"if true {
    enum Choice { Left, Extra }
    let _ = match Choice::Left { Choice::Left => 1 }
} else if false {
    enum Choice { Left }
}"#,
        );
        assert_eq!(ws.len(), 1, "later sibling hid a missing variant: {ws:?}");
        assert!(ws[0].message.contains("`Choice::Extra`"));
    }

    #[test]
    fn branch_and_loop_declarations_are_lexically_scoped() {
        let ws = warnings(
            r#"if true { enum BranchOnly { A, B } }
while false { enum LoopOnly { A, B } }
let _ = match none { BranchOnly::A => 1 }
let _ = match none { LoopOnly::A => 1 }"#,
        );
        assert!(
            ws.is_empty(),
            "nested declarations escaped their frames: {ws:?}"
        );

        let ws = warnings(
            r#"repeat 1 {
    enum Local { A, B }
    let _ = match Local::A { Local::A => 1 }
}"#,
        );
        assert_eq!(ws.len(), 1);
        assert!(ws[0].message.contains("`Local::B`"));
    }

    #[test]
    fn declaration_is_not_visible_before_its_statement() {
        let ws = warnings(
            r#"let _ = match none { Later::A => 1 }
enum Later { A, B }"#,
        );
        assert!(ws.is_empty(), "future declaration leaked backwards: {ws:?}");
    }

    #[test]
    fn callable_local_declarations_do_not_leak_to_siblings() {
        let ws = warnings(
            r#"fn first(x) {
    enum Private { A, B }
    return match x { Private::A => 1 }
}
fn second(x) { return match x { Private::A => 1 } }
let f = fn(x) {
    enum LambdaPrivate { A, B }
    return match x { LambdaPrivate::A => 1 }
}
let g = fn(x) { return match x { LambdaPrivate::A => 1 } }"#,
        );
        assert_eq!(
            ws.len(),
            2,
            "only the two declaring callables should warn: {ws:?}"
        );
        assert!(
            ws.iter()
                .any(|warning| warning.message.contains("`Private::B`"))
        );
        assert!(
            ws.iter()
                .any(|warning| warning.message.contains("`LambdaPrivate::B`"))
        );
    }

    #[test]
    fn callable_base_requires_runtime_equivalent_ordered_shapes() {
        let equivalent = warnings(
            r#"enum E { Pair(left, right), End }
enum E { Pair(x, y), End }
fn f(value) { return match value { E::Pair(a, b) => a } }"#,
        );
        assert_eq!(
            equivalent.len(),
            1,
            "tuple field names are not runtime shape: {equivalent:?}"
        );
        assert!(equivalent[0].message.contains("`E::End`"));

        for source in [
            r#"enum E { A, B }
enum E { B, A }
fn f(value) { return match value { E::A => 1 } }"#,
            r#"enum E { Pair(a, b), End }
enum E { Pair(a), End }
fn f(value) { return match value { E::Pair(a) => a } }"#,
            r#"enum E { Item { left, right }, End }
enum E { Item { right, left }, End }
fn f(value) { return match value { E::Item { left, right } => left } }"#,
        ] {
            assert!(
                warnings(source).is_empty(),
                "non-equivalent shape was treated as known"
            );
        }
    }

    #[test]
    fn catch_all_or_alternative_makes_the_arm_exhaustive() {
        let ws = warnings(
            r#"enum E { A, B }
let _ = match E::A { _ | E::A => 1 }"#,
        );
        assert!(
            ws.is_empty(),
            "an OR catch-all must use any-alternative semantics: {ws:?}"
        );
    }

    #[test]
    fn alias_write_effects_respect_frame_provenance() {
        let modules = &[("types", "enum E { A, B }")];
        let inherited = warnings_with_modules(
            r#"use types as api
if true { api = 1 }
let _ = match none { api.E::A => 1 }"#,
            modules,
        );
        assert!(
            inherited.is_empty(),
            "conditional outer write must poison the alias"
        );

        let child_local = warnings_with_modules(
            r#"use types as api
if true {
    use types as api
    api = 1
}
let _ = match none { api.E::A => 1 }"#,
            modules,
        );
        assert_eq!(
            child_local.len(),
            1,
            "child-local write poisoned the outer alias"
        );
        assert!(child_local[0].message.contains("`E::B`"));
    }

    #[test]
    fn function_declaration_does_not_shadow_an_existing_module_alias() {
        let ws = warnings_with_modules(
            r#"use types as api
fn api() { return 1 }
let _ = match none { api.E::A => 1 }"#,
            &[("types", "enum E { A, B }")],
        );
        assert_eq!(
            ws.len(),
            1,
            "function registry entry incorrectly hid the alias: {ws:?}"
        );
        assert!(ws[0].message.contains("`E::B`"));
    }

    #[test]
    fn earlier_function_declaration_blocks_a_later_module_alias() {
        let ws = warnings_with_modules(
            r#"fn api() { return 1 }
use types as api
let _ = match none { api.E::A => 1 }"#,
            &[("types", "enum E { A, B }")],
        );
        assert!(
            ws.is_empty(),
            "checker trusted an alias the runtime rejects: {ws:?}"
        );
    }

    #[test]
    fn imported_value_and_function_names_block_later_aliases() {
        let modules = &[
            ("values", "let api = 1"),
            ("functions", "fn api() { return 1 }"),
            ("facade", "use values.{api}"),
            ("types", "enum E { A, B }"),
        ];
        for source in [
            "use values\nuse types as api\nlet _ = match none { api.E::A => 1 }",
            "use values.{api}\nuse types as api\nlet _ = match none { api.E::A => 1 }",
            "use functions\nuse types as api\nlet _ = match none { api.E::A => 1 }",
            "use functions.{api}\nuse types as api\nlet _ = match none { api.E::A => 1 }",
            "use facade\nuse types as api\nlet _ = match none { api.E::A => 1 }",
        ] {
            assert!(
                warnings_with_modules(source, modules).is_empty(),
                "imported runtime value did not block a later alias for {source:?}"
            );
        }
    }

    #[test]
    fn exported_function_wins_over_same_named_module_value_at_boundary() {
        let ws = warnings_with_modules(
            "use bridge\nlet _ = match none { api.E::A => 1 }",
            &[
                ("types", "enum E { A, B }"),
                (
                    "bridge",
                    "use types as api\nfn api() { return 1 }\nlet _ = match none { api.E::A => 1 }",
                ),
            ],
        );
        assert!(
            ws.is_empty(),
            "module boundary exported a shadowed namespace instead of the function: {ws:?}"
        );
    }

    #[test]
    fn imported_values_shadow_outer_aliases_but_not_same_frame_winners() {
        let modules = &[
            ("types", "enum E { A, B }"),
            ("values", "let api = 1"),
            ("functions", "fn api() { return 1 }"),
        ];
        for imported in ["values", "functions"] {
            let nested = format!(
                "use types as api\nif true {{ use {imported}\nlet _ = match none {{ api.E::A => 1 }} }}"
            );
            assert!(
                warnings_with_modules(&nested, modules).is_empty(),
                "imported value failed to shadow outer alias for {imported}"
            );

            let same_frame =
                format!("use types as api\nuse {imported}\nlet _ = match none {{ api.E::A => 1 }}");
            let ws = warnings_with_modules(&same_frame, modules);
            assert_eq!(
                ws.len(),
                1,
                "same-frame first winner was not preserved: {ws:?}"
            );
            assert!(ws[0].message.contains("`E::B`"));
        }
    }

    #[test]
    fn callable_base_sees_conditional_outer_alias_writes() {
        let ws = warnings_with_modules(
            r#"use types as api
if true { api = 1 }
fn f(value) { return match value { api.E::A => 1 } }"#,
            &[("types", "enum E { A, B }")],
        );
        assert!(
            ws.is_empty(),
            "callable base ignored a possible outer alias write: {ws:?}"
        );
    }

    #[test]
    fn import_alias_shadowing_and_assignment_are_opaque() {
        let modules = &[("types", "enum E { A, B }")];
        for source in [
            r#"use types as api
let api = 1
let _ = match none { api.E::A => 1 }"#,
            r#"use types as api
api = 1
let _ = match none { api.E::A => 1 }"#,
            r#"use types as api
fn f(api) { return match none { api.E::A => 1 } }"#,
            r#"use types as api
for api in [1] { let _ = match none { api.E::A => 1 } }"#,
        ] {
            assert!(warnings_with_modules(source, modules).is_empty());
        }
    }

    #[test]
    fn direct_selective_aliased_and_transitive_imports_preserve_origin() {
        let modules = &[("types", "enum E { A, B }"), ("bridge", "use types.{E}")];
        for source in [
            "use types\nlet _ = match none { E::A => 1 }",
            "use types.{E}\nlet _ = match none { E::A => 1 }",
            "use types.{E} as api\nlet _ = match none { api.E::A => 1 }",
            "use bridge\nlet _ = match none { E::A => 1 }",
        ] {
            let ws = warnings_with_modules(source, modules);
            assert_eq!(ws.len(), 1, "import origin was lost for {source:?}: {ws:?}");
            assert!(ws[0].message.contains("`E::B`"));
        }
    }

    #[test]
    fn first_import_wins_and_struct_surface_blocks_later_enum() {
        let modules = &[
            ("left", "enum E { A, B }"),
            ("right", "enum E { A, C }"),
            ("record", "struct E { value }"),
        ];
        let first_enum = warnings_with_modules(
            "use left\nuse right\nlet _ = match none { E::A => 1 }",
            modules,
        );
        assert_eq!(first_enum.len(), 1);
        assert!(first_enum[0].message.contains("`E::B`"));
        assert!(!first_enum[0].message.contains("`E::C`"));

        let struct_first = warnings_with_modules(
            "use record\nuse left\nlet _ = match none { E::A => 1 }",
            modules,
        );
        assert!(
            struct_first.is_empty(),
            "later enum bypassed first imported type"
        );
    }

    #[test]
    fn resolver_cycles_are_opaque_through_the_whole_active_chain() {
        let ws = warnings_with_modules(
            "use a as api\nlet _ = match none { api.E::A => 1 }",
            &[("a", "use b"), ("b", "enum E { A, B }\nuse a")],
        );
        assert!(
            ws.is_empty(),
            "partial surface escaped a circular import: {ws:?}"
        );
    }

    #[test]
    fn branches_are_analyzed_without_constant_folding_and_restore_outer_types() {
        let ws = warnings(
            r#"enum E { Outer, Missing }
if false {
    enum E { Inner, BranchMissing }
    let _ = match none { E::Inner => 1 }
} else {
    enum E { ElseOnly }
    let _ = match none { E::ElseOnly => 1 }
}
let _ = match none { E::Outer => 1 }"#,
        );
        assert_eq!(
            ws.len(),
            2,
            "all arms plus restored outer type must be analyzed: {ws:?}"
        );
        assert!(ws[0].message.contains("`E::BranchMissing`"));
        assert!(ws[1].message.contains("`E::Missing`"));
    }

    #[test]
    fn callable_local_exact_type_overrides_ambiguous_published_shape() {
        let ws = warnings(
            r#"enum E { A, B }
enum E { A, C }
fn f(value) {
    enum E { A, LocalMissing }
    return match value { E::A => 1 }
}"#,
        );
        assert_eq!(ws.len(), 1);
        assert!(ws[0].message.contains("`E::LocalMissing`"));
    }

    #[test]
    fn callable_scope_rules_cover_lambdas_and_methods() {
        let ws = warnings(
            r#"if true {
    enum BlockOnly { A, B }
    let f = fn(value) { return match value { BlockOnly::A => 1 } }
}
fn Holder.inspect(self, value) {
    enum MethodOnly { A, B }
    return match value { MethodOnly::A => 1 }
}"#,
        );
        assert_eq!(
            ws.len(),
            1,
            "lambda captured block-local type or method local was missed: {ws:?}"
        );
        assert!(ws[0].message.contains("`MethodOnly::B`"));
    }

    #[test]
    fn same_module_struct_and_enum_coexist_without_erasing_enum_knowledge() {
        for source in [
            "struct E { value }\nenum E { A, B }\nlet _ = match none { E::A => 1 }",
            "enum E { A, B }\nstruct E { value }\nlet _ = match none { E::A => 1 }",
        ] {
            let ws = warnings(source);
            assert_eq!(
                ws.len(),
                1,
                "struct declaration erased same-module enum: {ws:?}"
            );
            assert!(ws[0].message.contains("`E::B`"));
        }
    }

    #[test]
    fn match_pattern_binding_shadows_module_alias_in_guard_and_body() {
        let ws = warnings_with_modules(
            r#"use types as api
let _ = match 1 {
    api if match none { api.E::A => true } => match none { api.E::A => 1 },
}
let _ = match none { api.E::A => 1 }"#,
            &[("types", "enum E { A, B }")],
        );
        assert_eq!(
            ws.len(),
            1,
            "pattern binding did not shadow alias only within its arm: {ws:?}"
        );
        assert!(ws[0].message.contains("`E::B`"));
    }

    #[test]
    fn selective_and_distinct_alias_imports_are_origin_aware() {
        let modules = &[
            ("left", "enum E { A, LeftMissing }\nlet helper = 1"),
            ("right", "enum E { A, RightMissing }"),
        ];
        assert!(
            warnings_with_modules(
                "use left.{helper}\nlet _ = match none { E::A => 1 }",
                modules,
            )
            .is_empty(),
            "an excluded enum entered a selective surface"
        );

        let ws = warnings_with_modules(
            r#"use left as l
use right as r
let _ = match none { l.E::A => 1 }
let _ = match none { r.E::A => 1 }
let _ = match none { l.E::A => 1, r.E::A => 2 }"#,
            modules,
        );
        assert_eq!(
            ws.len(),
            2,
            "mixed runtime identities should suppress only their match: {ws:?}"
        );
        assert!(ws[0].message.contains("`E::LeftMissing`"));
        assert!(ws[1].message.contains("`E::RightMissing`"));
    }

    #[test]
    fn transitive_aliased_reexport_keeps_leaf_runtime_origin() {
        let ws = warnings_with_modules(
            "use bridge as api\nlet _ = match none { api.E::A => 1 }",
            &[("leaf", "enum E { A, B }"), ("bridge", "use leaf.{E}")],
        );
        assert_eq!(
            ws.len(),
            1,
            "leaf origin was lost through aliased re-export: {ws:?}"
        );
        assert!(ws[0].message.contains("`E::B`"));
    }

    #[test]
    fn nested_matches_are_postorder_and_missing_variants_keep_declaration_order() {
        let ws = warnings(
            r#"enum Inner { A, Z, B }
enum Outer { A, B }
let _ = match match none { Inner::A => Outer::A } { Outer::A => 1 }"#,
        );
        assert_eq!(ws.len(), 2);
        assert!(ws[0].message.contains("`Inner::Z`, `Inner::B`"));
        assert!(ws[1].message.contains("`Outer::B`"));
    }

    #[test]
    fn matches_in_assignment_targets_and_payload_edges_are_visited() {
        let ws = warnings(
            r#"enum E { A, B }
struct Box { value }
enum Wrap { Value(value) }
let values = [0]
values[match none { E::A => 0 }] = Box {
    value: Wrap::Value(try (match none { E::A => 1 })),
}"#,
        );
        assert_eq!(
            ws.len(),
            2,
            "an assignment-target or payload/try edge was skipped: {ws:?}"
        );
        assert!(ws.iter().all(|warning| warning.message.contains("`E::B`")));
    }

    #[test]
    fn reversed_branch_order_keeps_each_declaration_site_exact() {
        let ws = warnings(
            r#"if false {
    enum Choice { Wrong, Extra }
} else if true {
    enum Choice { Right }
    let _ = match none { Choice::Right => 1 }
}"#,
        );
        assert!(
            ws.is_empty(),
            "earlier sibling contaminated the later exact site: {ws:?}"
        );
    }

    #[test]
    fn every_loop_form_is_analyzed_without_iteration_folding() {
        let ws = warnings(
            r#"while false {
    enum W { A, B }
    let _ = match none { W::A => 1 }
}
repeat 0 {
    enum R { A, B }
    let _ = match none { R::A => 1 }
}
for item in [] {
    enum F { A, B }
    let _ = match none { F::A => item }
}"#,
        );
        assert_eq!(ws.len(), 3, "a loop form was folded or skipped: {ws:?}");
        assert!(ws[0].message.contains("`W::B`"));
        assert!(ws[1].message.contains("`R::B`"));
        assert!(ws[2].message.contains("`F::B`"));
    }

    #[test]
    fn local_struct_overwrites_imported_enum_in_callable_base() {
        let ws = warnings_with_modules(
            r#"use dep
struct E { value }
fn f(input) { return match input { E::A => 1 } }"#,
            &[("dep", "enum E { A, B }")],
        );
        assert!(
            ws.is_empty(),
            "imported enum survived a local struct binding: {ws:?}"
        );
    }
}
