//! Language-level builtins (`range`, `str`, `int`, `type`, `len`, ...) and
//! the shared argument-validation helpers used across the runtime.
//!
//! These are pure-data operations on `Value`. Host-backed builtins like
//! file I/O live in `nybl-sys` instead.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

use crate::error::NyblError;
use crate::memory::MemoryContext;
use crate::parser::{VariantDecl, VariantKind};
use crate::value::Value;

/// Maximum number of eagerly materialized values returned by [`builtin_range`].
pub const RANGE_MAX_ITEMS: usize = 10_000;

/// Stable fatal error text for range cardinalities above [`RANGE_MAX_ITEMS`].
pub const RANGE_LIMIT_ERROR_MESSAGE: &str = "range() would produce more than 10,000 values";

/// Actionable guidance paired with [`RANGE_LIMIT_ERROR_MESSAGE`].
pub const RANGE_LIMIT_HINT: &str =
    "Use a smaller range, a larger step, or process values in smaller chunks.";

// ─── Engine-wide builtin types ────────────────────────────────────
//
// `Result` and `RuntimeError` are pre-declared in every engine
// (walker, VM, AOT) so:
//
//   - `try` / `try_call` can construct `Result::Ok(..)` /
//     `Result::Err(RuntimeError { .. })` without requiring the
//     program to have imported `std.result` first;
//   - user programs can write `Result::Ok(..)` or match on
//     `RuntimeError { message, line }` out of the box;
//   - engine-to-engine behaviour stays in lockstep — each engine
//     seeds its type table from these same helpers, so the
//     shapes can't drift.
//
// The combinator fns (`is_ok`, `unwrap`, `map`, …) stay in
// `std.result`; only the bare type shapes live here.

/// The canonical `Result { Ok(value), Err(error) }` enum shape,
/// seeded into every engine's type registry at construction time.
pub fn builtin_result_variants() -> Vec<VariantDecl> {
    alloc_import::vec![
        VariantDecl {
            name: String::from("Ok"),
            kind: VariantKind::Tuple(alloc_import::vec![String::from("value")]),
        },
        VariantDecl {
            name: String::from("Err"),
            kind: VariantKind::Tuple(alloc_import::vec![String::from("error")]),
        },
    ]
}

/// The canonical `RuntimeError { message, line }` struct field
/// list. `try_call` produces these directly; declaring them as a
/// builtin lets user code pattern-match the same shape.
pub fn builtin_runtime_error_fields() -> Vec<String> {
    alloc_import::vec![String::from("message"), String::from("line")]
}

/// The canonical `Iter { Next(value), Done }` enum shape —
/// lazy iterators' return type from `.next()`. Seeded into every
/// engine's type registry alongside `Result` so user code can
/// pattern-match `Iter::Next(v) | Iter::Done` without importing
/// anything.
pub fn builtin_iter_variants() -> Vec<VariantDecl> {
    alloc_import::vec![
        VariantDecl {
            name: String::from("Next"),
            kind: VariantKind::Tuple(alloc_import::vec![String::from("value")]),
        },
        VariantDecl {
            name: String::from("Done"),
            kind: VariantKind::Unit,
        },
    ]
}

/// Build `Iter::Next(value)` with the builtin module path so the
/// caller's pattern against `Iter::Next(v)` fires regardless of
/// which module the iterator's `.next()` was declared in.
pub fn make_iter_next(value: Value, line: u32) -> Result<Value, NyblError> {
    make_iter_next_in(value, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn make_iter_next_in(
    value: Value,
    line: u32,
    memory: &MemoryContext,
) -> Result<Value, NyblError> {
    let items: Vec<Value> = alloc_import::vec![value];
    Value::__try_new_enum_tuple_in(
        String::from(crate::value::BUILTIN_MODULE_PATH),
        String::from("Iter"),
        String::from("Next"),
        items,
        line,
        memory,
    )
}

/// Build the `Iter::Done` sentinel. Carries the builtin module
/// path for the same matching reason as [`make_iter_next`].
pub fn make_iter_done() -> Value {
    make_iter_done_in(&MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn make_iter_done_in(memory: &MemoryContext) -> Value {
    Value::__new_enum_unit_in(
        String::from(crate::value::BUILTIN_MODULE_PATH),
        String::from("Iter"),
        String::from("Done"),
        memory,
    )
}

// Small alias so this file compiles both under std and no_std. The
// parser module already uses `alloc::vec!` under no_std, so the
// engines follow the same convention here. Nothing clever — just a
// re-export that picks the right `vec!` macro per config.
#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc as alloc_import;
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std as alloc_import;

pub fn builtin_range(args: &[Value], line: u32, rand_state: &mut u64) -> Result<Value, NyblError> {
    builtin_range_in(args, line, rand_state, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn builtin_range_in(
    args: &[Value],
    line: u32,
    rand_state: &mut u64,
    memory: &MemoryContext,
) -> Result<Value, NyblError> {
    let _ = rand_state; // unused here, keeping signature uniform
    // `range` operates in integer space — matches Python and
    // keeps `range(5)[2]` predictable. Float args error out.
    let (start, end, step) = match args.len() {
        1 => {
            let n = expect_int("range", &args[0], line)?;
            (0i64, n, 1i64)
        }
        2 => {
            let start = expect_int("range", &args[0], line)?;
            let end = expect_int("range", &args[1], line)?;
            (start, end, if start <= end { 1 } else { -1 })
        }
        3 => {
            let start = expect_int("range", &args[0], line)?;
            let end = expect_int("range", &args[1], line)?;
            let step = expect_int("range", &args[2], line)?;
            if step == 0 {
                return Err(error(line, "range step can't be 0"));
            }
            (start, end, step)
        }
        _ => return Err(error(line, "range takes 1, 2, or 3 arguments")),
    };

    let count = range_cardinality(start, end, step);
    if count > RANGE_MAX_ITEMS as u128 {
        return Err(range_limit_error(line));
    }
    let len = usize::try_from(count).map_err(|_| range_memory_error(line))?;
    let bytes = len
        .checked_mul(core::mem::size_of::<Value>())
        .ok_or_else(|| range_memory_error(line))?;
    if memory.__would_exceed(bytes) {
        return Err(range_memory_error(line));
    }

    // Reserve before generating any values so allocation failure is reported as
    // a fatal resource error rather than aborting midway through the range.
    let mut result = Vec::new();
    result
        .try_reserve_exact(len)
        .map_err(|_| range_memory_error(line))?;
    let mut i = start;
    for index in 0..len {
        result.push(Value::Int(i));
        if index + 1 < len {
            i = i
                .checked_add(step)
                .ok_or_else(|| error(line, "range arithmetic overflow"))?;
        }
    }
    Value::__try_new_array_in(result, line, memory)
}

/// Return the exact number of values produced by a non-zero-step range.
///
/// The subtraction is performed in `i128` so opposite `i64` extremes remain
/// representable. A direction-mismatched range is empty, matching the loop
/// semantics used by the language.
fn range_cardinality(start: i64, end: i64, step: i64) -> u128 {
    let (distance, stride) = if step > 0 && start < end {
        ((end as i128 - start as i128) as u128, step as u128)
    } else if step < 0 && start > end {
        (
            (start as i128 - end as i128) as u128,
            -(step as i128) as u128,
        )
    } else {
        return 0;
    };

    // ceil(distance / stride), written this way to avoid overflowing when
    // `distance` spans the entire i64 domain.
    1 + (distance - 1) / stride
}

fn range_memory_error(line: u32) -> NyblError {
    error_fatal_with_hint(
        line,
        "Memory limit exceeded",
        "This range would create too many values.",
    )
}

fn range_limit_error(line: u32) -> NyblError {
    error_fatal_with_hint(line, RANGE_LIMIT_ERROR_MESSAGE, RANGE_LIMIT_HINT)
}

/// Convert a finite `f64` that's already integer-valued into a
/// `Value::Int` when it fits in `i64`; fall back to
/// `Value::Number` otherwise. Non-finite inputs stay as
/// `Number` (the caller's `f64::floor` / `ceil` / `round`
/// already handled `NaN` / `±inf` correctly).
pub fn finite_to_int_or_number(n: f64) -> Value {
    // `i64::MAX as f64` rounds up to 2^63, so the upper comparison must
    // be strict. Otherwise 2^63 passes the range check and Rust's saturating
    // float-to-int cast silently turns it into `i64::MAX`.
    if n.is_finite() && n >= i64::MIN as f64 && n < i64::MAX as f64 {
        Value::Int(n as i64)
    } else {
        Value::Number(n)
    }
}

/// `panic(message)` — raise a non-fatal runtime error carrying
/// `message`. Useful for stdlib helpers (`unwrap`, `expect`,
/// `assert_*`) that need to bail with a readable message from an
/// expression position where a plain `return` isn't enough.
///
/// Non-fatal, so `try_call` catches it — same contract as any
/// other runtime error the program raises.
pub fn builtin_panic(args: &[Value], line: u32) -> Result<Value, NyblError> {
    expect_args("panic", args, 1, line)?;
    let message = match &args[0] {
        Value::Str(s) => s.as_str().to_string(),
        // Non-string arguments are stringified via Display so a
        // caller that hands us a struct or int still gets a
        // useful trace — cheaper than rejecting and forcing the
        // caller to add `.to_str()`.
        other => format!("{other}"),
    };
    Err(error(line, message))
}

pub fn builtin_rand(args: &[Value], line: u32, rand_state: &mut u64) -> Result<Value, NyblError> {
    expect_args("rand", args, 1, line)?;
    let n = expect_int("rand", &args[0], line)?;
    if n <= 0 {
        return Err(error(line, "rand needs a positive number"));
    }
    // Simple PCG-style PRNG for deterministic behaviour
    *rand_state = rand_state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let value = (*rand_state >> 33) % (n as u64);
    Ok(Value::Int(value as i64))
}

// ─── Helpers (also used by evaluator / VM / AOT) ────────────────────────────

pub fn expect_args(
    name: &str,
    args: &[Value],
    expected: usize,
    line: u32,
) -> Result<(), NyblError> {
    if args.len() != expected {
        Err(error(
            line,
            format!(
                "`{}` expects {} argument{}, but got {}",
                name,
                expected,
                if expected == 1 { "" } else { "s" },
                args.len()
            ),
        ))
    } else {
        Ok(())
    }
}

pub fn expect_number(func_name: &str, val: &Value, line: u32) -> Result<f64, NyblError> {
    match val {
        Value::Int(n) => Ok(*n as f64),
        Value::Number(n) => Ok(*n),
        _ => Err(error(
            line,
            format!(
                "`{}` expects a number, but got {}",
                func_name,
                val.type_name()
            ),
        )),
    }
}

/// Like [`expect_number`] but strictly requires an `Int`. Used
/// by builtins that have to produce exact integer counts
/// (e.g. `range`, `rand`). `Number` inputs are rejected rather
/// than silently truncated.
pub fn expect_int(func_name: &str, val: &Value, line: u32) -> Result<i64, NyblError> {
    match val {
        Value::Int(n) => Ok(*n),
        _ => Err(error(
            line,
            format!(
                "`{}` expects an int, but got {}",
                func_name,
                val.type_name()
            ),
        )),
    }
}

pub fn error(line: u32, message: impl Into<String>) -> NyblError {
    NyblError {
        line: Some(line),
        column: None,
        message: message.into(),
        friendly_hint: None,
        source_context: None,
        is_fatal: false,
        is_try_return: false,
    }
}

/// Like [`error`] but takes a niche-packed column alongside
/// the line. Call sites with an `Expr` or `Stmt` in hand
/// prefer this over `error` so the rendered caret points at
/// the offending character rather than just the line start.
pub fn error_at(
    line: u32,
    column: Option<core::num::NonZeroU32>,
    message: impl Into<String>,
) -> NyblError {
    NyblError {
        line: Some(line),
        column: column.map(|c| c.get()),
        message: message.into(),
        friendly_hint: None,
        source_context: None,
        is_fatal: false,
        is_try_return: false,
    }
}

pub fn error_with_hint(
    line: u32,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> NyblError {
    NyblError {
        line: Some(line),
        column: None,
        message: message.into(),
        friendly_hint: Some(hint.into()),
        source_context: None,
        is_fatal: false,
        is_try_return: false,
    }
}

/// Column-aware variant of [`error_with_hint`]. Same hint
/// payload, plus a `column` slot so the renderer can draw the
/// caret.
pub fn error_with_hint_at(
    line: u32,
    column: Option<core::num::NonZeroU32>,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> NyblError {
    NyblError {
        line: Some(line),
        column: column.map(|c| c.get()),
        message: message.into(),
        friendly_hint: Some(hint.into()),
        source_context: None,
        is_fatal: false,
        is_try_return: false,
    }
}

/// Fatal variant of [`error_with_hint`] — `is_fatal = true`
/// blocks `try_call` from swallowing it. Used by resource-
/// limit violations (`too many steps`, `Memory limit
/// exceeded`) so a script can't wrap a step-bomb in
/// `try_call` and keep running.
pub fn error_fatal_with_hint(
    line: u32,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> NyblError {
    NyblError {
        line: Some(line),
        column: None,
        message: message.into(),
        friendly_hint: Some(hint.into()),
        source_context: None,
        is_fatal: true,
        is_try_return: false,
    }
}

/// Fatal variant of [`error`] (no hint). Same uncatchable
/// contract as [`error_fatal_with_hint`].
pub fn error_fatal(line: u32, message: impl Into<String>) -> NyblError {
    NyblError {
        line: Some(line),
        column: None,
        message: message.into(),
        friendly_hint: None,
        source_context: None,
        is_fatal: true,
        is_try_return: false,
    }
}

// ─── `try_call` result construction ────────────────────────────
//
// The `try_call(f)` builtin is Lua's `pcall` renamed — it calls
// `f` (a zero-arg callable), catches any non-fatal `NyblError`,
// and reports the outcome as a `Result::Ok(value)` or
// `Result::Err(RuntimeError { message, line })` structurally-
// shaped value. These helpers construct those values directly
// via `Value::new_enum_tuple` / `Value::new_struct` and
// therefore don't require the program to have declared
// `Result` or `RuntimeError` — they produce the same shape
// either way, so user code can pattern-match them regardless.
//
// Fatal errors (`is_fatal == true`) are deliberately *not*
// wrapped — `try_call`'s callers never see them. See
// [`NyblError::is_fatal`] for why.

/// Build the `Result::Ok(value)` variant `try_call` returns on a
/// successful call. `Result` is an engine builtin, so the value
/// carries `<builtin>` as its module path — any program that
/// matches it via `Result::Ok(v)` resolves `Result` to the same
/// builtin in its own type-binding scope.
pub fn make_try_call_ok(value: Value, line: u32) -> Result<Value, NyblError> {
    make_try_call_ok_in(value, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn make_try_call_ok_in(
    value: Value,
    line: u32,
    memory: &MemoryContext,
) -> Result<Value, NyblError> {
    let items: Vec<Value> = alloc_import::vec![value];
    Value::__try_new_enum_tuple_in(
        String::from(crate::value::BUILTIN_MODULE_PATH),
        String::from("Result"),
        String::from("Ok"),
        items,
        line,
        memory,
    )
}

/// Build the `Result::Err(RuntimeError { message, line })`
/// variant `try_call` returns on a caught non-fatal error.
/// `RuntimeError` is also a builtin — same `<builtin>` module
/// path as `Result`.
pub fn make_try_call_err(err: &NyblError) -> Value {
    make_try_call_err_in(err, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn make_try_call_err_in(err: &NyblError, memory: &MemoryContext) -> Value {
    let message = Value::__new_str_in(err.message.clone(), memory);
    // Line numbers are integers — use Int now that phase 6
    // distinguishes them from floats.
    let line = Value::Int(err.line.unwrap_or(0) as i64);
    let fields: Vec<(String, Value)> = alloc_import::vec![
        (String::from("message"), message),
        (String::from("line"), line),
    ];
    let rt_err = Value::__try_new_struct_in(
        String::from(crate::value::BUILTIN_MODULE_PATH),
        String::from("RuntimeError"),
        fields,
        0,
        memory,
    )
    .expect("builtin RuntimeError has bounded ownership depth");
    let items: Vec<Value> = alloc_import::vec![rt_err];
    Value::__try_new_enum_tuple_in(
        String::from(crate::value::BUILTIN_MODULE_PATH),
        String::from("Result"),
        String::from("Err"),
        items,
        0,
        memory,
    )
    .expect("builtin Result::Err has bounded ownership depth")
}

/// Pre-flight check for string repeat
pub fn check_string_repeat_memory(len: usize, count: usize, line: u32) -> Result<(), NyblError> {
    check_string_repeat_memory_in(len, count, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn check_string_repeat_memory_in(
    len: usize,
    count: usize,
    line: u32,
    memory: &MemoryContext,
) -> Result<(), NyblError> {
    let result_len = len.saturating_mul(count);
    if memory.__would_exceed(result_len) {
        Err(error_fatal_with_hint(
            line,
            "Memory limit exceeded",
            "This string repeat would use too much memory.",
        ))
    } else {
        Ok(())
    }
}

/// Pre-flight check for string concat
pub fn check_string_concat_memory(a_len: usize, b_len: usize, line: u32) -> Result<(), NyblError> {
    check_string_concat_memory_in(a_len, b_len, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn check_string_concat_memory_in(
    a_len: usize,
    b_len: usize,
    line: u32,
    memory: &MemoryContext,
) -> Result<(), NyblError> {
    let result_len = a_len + b_len;
    if memory.__would_exceed(result_len) {
        Err(error_fatal_with_hint(
            line,
            "Memory limit exceeded",
            "This string concatenation would use too much memory.",
        ))
    } else {
        Ok(())
    }
}

/// Pre-flight check for array concat
pub fn check_array_concat_memory(a_len: usize, b_len: usize, line: u32) -> Result<(), NyblError> {
    check_array_concat_memory_in(a_len, b_len, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn check_array_concat_memory_in(
    a_len: usize,
    b_len: usize,
    line: u32,
    memory: &MemoryContext,
) -> Result<(), NyblError> {
    let result_bytes = (a_len + b_len) * core::mem::size_of::<Value>();
    if memory.__would_exceed(result_bytes) {
        Err(error_fatal_with_hint(
            line,
            "Memory limit exceeded",
            "This array concatenation would use too much memory.",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_TO_63: f64 = 9_223_372_036_854_775_808.0;
    const LARGEST_IN_RANGE_F64: f64 = 9_223_372_036_854_774_784.0;

    fn run_range(args: &[Value]) -> Result<Value, NyblError> {
        let mut rand_state = 0;
        builtin_range_in(args, 7, &mut rand_state, &MemoryContext::__new(usize::MAX))
    }

    fn array_ints(value: &Value) -> Vec<i64> {
        let Value::Array(items) = value else {
            panic!("range did not return an array");
        };
        items
            .iter()
            .map(|item| match item {
                Value::Int(value) => *value,
                _ => panic!("range returned a non-integer item"),
            })
            .collect()
    }

    #[test]
    fn finite_integer_conversion_respects_the_exact_i64_boundary() {
        assert!(matches!(
            finite_to_int_or_number(TWO_TO_63),
            Value::Number(value) if value == TWO_TO_63
        ));
        assert!(matches!(
            finite_to_int_or_number(i64::MIN as f64),
            Value::Int(i64::MIN)
        ));
        assert!(matches!(
            finite_to_int_or_number(LARGEST_IN_RANGE_F64),
            Value::Int(value) if value == LARGEST_IN_RANGE_F64 as i64
        ));
    }

    #[test]
    fn range_cardinality_handles_directions_and_i64_extremes() {
        assert_eq!(range_cardinality(0, 10, 3), 4);
        assert_eq!(range_cardinality(10, 0, -3), 4);
        assert_eq!(range_cardinality(0, 10, -1), 0);
        assert_eq!(range_cardinality(10, 0, 1), 0);
        assert_eq!(range_cardinality(5, 5, 1), 0);
        assert_eq!(range_cardinality(i64::MIN, i64::MAX, 1), u64::MAX as u128);
        assert_eq!(range_cardinality(i64::MAX, i64::MIN, i64::MIN), 2);
    }

    #[test]
    fn range_accepts_exactly_ten_thousand_values() {
        let value = run_range(&[Value::Int(10_000)]).expect("range should fit");
        let items = array_ints(&value);
        assert_eq!(items.len(), RANGE_MAX_ITEMS);
        assert_eq!(items.last(), Some(&9_999));
    }

    #[test]
    fn range_emits_extreme_bounds_without_overflow_or_truncation() {
        let value = run_range(&[
            Value::Int(i64::MIN),
            Value::Int(i64::MAX),
            Value::Int(i64::MAX),
        ])
        .expect("small extreme range should fit");
        assert_eq!(array_ints(&value), [i64::MIN, -1, i64::MAX - 1]);

        let value = run_range(&[
            Value::Int(i64::MAX),
            Value::Int(i64::MIN),
            Value::Int(i64::MIN),
        ])
        .expect("small reverse extreme range should fit");
        assert_eq!(array_ints(&value), [i64::MAX, -1]);
    }

    #[test]
    fn oversized_range_is_a_dedicated_fatal_limit_error() {
        let err = run_range(&[Value::Int(10_001)]).expect_err("range should exceed limit");
        assert!(err.is_fatal);
        assert_eq!(err.line, Some(7));
        assert_eq!(err.message, RANGE_LIMIT_ERROR_MESSAGE);
        assert_eq!(err.friendly_hint.as_deref(), Some(RANGE_LIMIT_HINT));
    }

    #[test]
    fn range_limit_uses_exact_cardinality_for_steps_and_direction() {
        let forward = run_range(&[Value::Int(0), Value::Int(20_000), Value::Int(2)])
            .expect("10,000 stepped values should fit");
        assert_eq!(array_ints(&forward).len(), RANGE_MAX_ITEMS);

        let reverse = run_range(&[Value::Int(20_000), Value::Int(0), Value::Int(-2)])
            .expect("10,000 reverse values should fit");
        assert_eq!(array_ints(&reverse).len(), RANGE_MAX_ITEMS);

        for args in [
            [Value::Int(0), Value::Int(20_001), Value::Int(2)],
            [Value::Int(20_002), Value::Int(0), Value::Int(-2)],
        ] {
            let err = run_range(&args).expect_err("10,001 values should exceed the limit");
            assert!(err.is_fatal);
            assert_eq!(err.message, RANGE_LIMIT_ERROR_MESSAGE);
        }
    }

    #[test]
    fn configured_memory_limit_still_applies_below_range_cap() {
        let mut rand_state = 0;
        let err = builtin_range_in(
            &[Value::Int(10)],
            7,
            &mut rand_state,
            &MemoryContext::__new(64),
        )
        .expect_err("range should exceed memory limit");
        assert!(err.is_fatal);
        assert_eq!(err.message, "Memory limit exceeded");
        assert_eq!(
            err.friendly_hint.as_deref(),
            Some("This range would create too many values.")
        );
    }
}
