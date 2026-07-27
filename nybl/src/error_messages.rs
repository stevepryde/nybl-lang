//! Shared error-message format helpers.
//!
//! Walker, VM, and AOT each previously carried their own copy
//! of `format!("Variable `{}` not found", name)` and friends.
//! The strings are usually identical but the differential
//! tests compare errors across engines by `.message` text, so
//! any drift would surface as a spurious test failure — or
//! worse, in one engine and not another if a differential test
//! didn't happen to exercise the path.
//!
//! Collecting the common messages here means:
//!
//! - One edit when a message needs rewording.
//! - The function signature documents the intended arguments
//!   (you can't accidentally swap `type_name` and `field`).
//! - AOT-emitted code calls these same helpers, so the
//!   runtime-generated error text is byte-identical to the
//!   walker's without the AOT needing to paste format strings.
//!
//! Only messages with 2+ copies across engines live here. One-
//! off per-engine messages (e.g. `"VM: stack underflow"`) stay
//! with their engine — there's nothing to deduplicate.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{format, string::String};

/// Format the diagnostic for an unknown variable.
pub fn variable_not_found(name: &str) -> String {
    format!("Variable `{name}` not found")
}

/// Format the diagnostic for an unknown function.
pub fn function_not_found(name: &str) -> String {
    format!("Function `{name}` not found")
}

/// Format the diagnostic for a missing struct field.
/// Used by the walker's `FieldAccess` path, the VM's
/// `ConstructStruct` / `FieldGet` / `FieldSet`, and the AOT's
/// runtime `__nybl_field_get` helper.
pub fn struct_has_no_field(type_name: &str, field: &str) -> String {
    format!("Struct `{type_name}` has no field `{field}`")
}

/// Format the diagnostic for a missing field on a struct-shaped enum variant.
/// For struct-shaped enum-variant field reads.
pub fn variant_has_no_field(type_name: &str, variant: &str, field: &str) -> String {
    format!("Variant `{type_name}::{variant}` has no field `{field}`")
}

/// Format the diagnostic for an unknown struct type.
pub fn struct_not_declared(type_name: &str) -> String {
    format!("Struct `{type_name}` is not declared")
}

/// Format the diagnostic for an unknown enum type.
pub fn enum_not_declared(type_name: &str) -> String {
    format!("Enum `{type_name}` is not declared")
}

/// Format the diagnostic for an unknown enum variant.
pub fn enum_has_no_variant(type_name: &str, variant: &str) -> String {
    format!("Enum `{type_name}` has no variant `{variant}`")
}

/// Format an invalid field-read diagnostic. `kind` is the pretty type name
/// (`"array"`, `"int"`, etc.).
pub fn cant_read_field(field: &str, kind: &str) -> String {
    format!("Can't read field `{field}` on {kind}")
}

/// Format an invalid field-assignment diagnostic.
pub fn cant_assign_field(field: &str, kind: &str) -> String {
    format!("Can't assign to field `{field}` on {kind}")
}

/// Format the diagnostic for calling a non-function value.
pub fn cant_call_a(kind: &str) -> String {
    format!("Can't call a {kind}")
}

/// Format the diagnostic for iterating over a non-iterable value.
pub fn cant_iterate_over(kind: &str) -> String {
    format!("Can't iterate over {kind}")
}

/// Format the terminal method-dispatch diagnostic when nothing matches.
pub fn no_such_method(kind: &str, method: &str) -> String {
    format!("{kind} doesn't have a .{method}() method")
}

/// Canonical runtime warning for a name rejected by a plain-glob import.
///
/// The caller adds the leading `warning: ` when writing the diagnostic.
pub fn glob_shadow_warning(name: &str, path: &str) -> String {
    format!("`{name}` from `{path}` shadowed by an existing binding — the first definition wins")
}

/// Actionable recovery paired with the canonical constant-mutation error.
pub const CONSTANT_MUTATION_HINT: &str =
    "constants are immutable. Use `let` if you want a mutable binding.";

/// Construct the canonical error for an assignment or built-in mutation
/// rooted at a constant binding.
pub fn constant_mutation_error(name: &str, line: u32) -> crate::error::NyblError {
    let mut error = crate::error::NyblError::runtime(
        format!("can't reassign `{name}` — it's a constant"),
        line,
    );
    error.friendly_hint = Some(String::from(CONSTANT_MUTATION_HINT));
    error
}

/// Exact diagnostic contract for a mutating built-in array method
/// whose receiver is an index or field place that cannot yet be
/// written back.
pub const NESTED_MUTATION_ERROR_MESSAGE: &str =
    "can't mutate through an index or field receiver yet";

/// Actionable recovery paired with
/// [`NESTED_MUTATION_ERROR_MESSAGE`].
pub const NESTED_MUTATION_HINT: &str =
    "Assign the value to a variable, mutate that variable, then assign it back.";

/// Exact diagnostic contract for `try` encountering an `Err` outside a
/// function. All execution engines construct this through
/// [`top_level_try_error`] so both the message and actionable hint stay in
/// sync.
pub const TOP_LEVEL_TRY_ERROR_MESSAGE: &str = "try encountered Err at top-level";
pub const TOP_LEVEL_TRY_HINT: &str =
    "Wrap the calling code in a fn, or use `match` to handle both arms explicitly.";

/// Construct the canonical top-level `try` runtime error at `line`.
pub fn top_level_try_error(line: u32) -> crate::error::NyblError {
    let mut error = crate::error::NyblError::runtime(TOP_LEVEL_TRY_ERROR_MESSAGE, line);
    error.friendly_hint = Some(String::from(TOP_LEVEL_TRY_HINT));
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_not_found_format() {
        assert_eq!(variable_not_found("x"), "Variable `x` not found");
    }

    #[test]
    fn struct_has_no_field_matches_legacy_format() {
        // The legacy `format!` spelled this with backticks
        // around both `type_name` and `field`. Locking it down
        // here so a rewrite can't drift without updating this
        // test too.
        assert_eq!(
            struct_has_no_field("Point", "z"),
            "Struct `Point` has no field `z`"
        );
    }

    #[test]
    fn variant_has_no_field_uses_double_colon() {
        assert_eq!(
            variant_has_no_field("Shape", "Rect", "r"),
            "Variant `Shape::Rect` has no field `r`"
        );
    }

    #[test]
    fn cant_read_field_formats_without_backticks_around_kind() {
        // The kind (type name) is a user-facing noun like
        // "array" — no backticks around it, matching the
        // legacy walker phrasing.
        assert_eq!(
            cant_read_field("x", "array"),
            "Can't read field `x` on array"
        );
    }

    #[test]
    fn nested_array_mutation_explains_the_workaround() {
        assert_eq!(
            NESTED_MUTATION_ERROR_MESSAGE,
            "can't mutate through an index or field receiver yet"
        );
        assert_eq!(
            NESTED_MUTATION_HINT,
            "Assign the value to a variable, mutate that variable, then assign it back."
        );
    }

    #[test]
    fn glob_shadow_warning_is_the_cross_engine_contract() {
        assert_eq!(
            glob_shadow_warning("alpha", "second"),
            "`alpha` from `second` shadowed by an existing binding — the first definition wins"
        );
    }

    #[test]
    fn constant_mutation_error_keeps_message_hint_and_line_together() {
        let error = constant_mutation_error("VALUES", 12);
        assert_eq!(error.line, Some(12));
        assert_eq!(error.message, "can't reassign `VALUES` — it's a constant");
        assert_eq!(error.friendly_hint.as_deref(), Some(CONSTANT_MUTATION_HINT));
    }

    #[test]
    fn top_level_try_error_keeps_message_hint_and_line_together() {
        let error = top_level_try_error(17);
        assert_eq!(error.line, Some(17));
        assert_eq!(error.message, TOP_LEVEL_TRY_ERROR_MESSAGE);
        assert_eq!(error.friendly_hint.as_deref(), Some(TOP_LEVEL_TRY_HINT));
    }
}
