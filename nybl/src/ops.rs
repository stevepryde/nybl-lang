//! Operator primitives shared across execution engines.
//!
//! These are pure functions over `Value` — no interpreter state, no AST.
//! The tree-walking evaluator, the bytecode VM, and AOT-Rust output all
//! dispatch to these so the language-level semantics of `+`, `*`, `==`,
//! etc. live in exactly one place.
//!
//! Short-circuiting operators (`&&`, `||`) are NOT here: they depend on
//! evaluation order and are the engine's responsibility.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{format, string::ToString, vec::Vec};

use crate::builtins::{
    check_array_concat_memory_in, check_string_concat_memory_in, check_string_repeat_memory_in,
    error, error_with_hint,
};
use crate::error::NyblError;
use crate::memory::MemoryContext;
use crate::value::{Value, values_equal};

// ─── Numeric coercion helpers ──────────────────────────────────────
//
// Int↔Number interplay follows Python's rules:
// - `Int op Int` stays Int (overflow → `NyblError`).
// - `Int op Number` / `Number op Int` widens to `Number`.
// - `Number op Number` stays `Number`.
//
// Division is split: `/` always returns `Number`; `//` always
// returns `Int` via truncation toward zero. Modulo mirrors the
// operand types.

/// Promote a value to `f64` if it's a numeric type. Used for
/// cross-type widening where one side is `Number`.
fn to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(*n as f64),
        Value::Number(n) => Some(*n),
        _ => None,
    }
}

fn int_overflow(op: &str, line: u32) -> NyblError {
    error(line, format!("Integer overflow in `{op}`"))
}

pub fn add(left: &Value, right: &Value, line: u32) -> Result<Value, NyblError> {
    add_in(left, right, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn add_in(
    left: &Value,
    right: &Value,
    line: u32,
    memory: &MemoryContext,
) -> Result<Value, NyblError> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => a
            .checked_add(*b)
            .map(Value::Int)
            .ok_or_else(|| int_overflow("+", line)),
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a + b)),
        (Value::Int(a), Value::Number(b)) => Ok(Value::Number(*a as f64 + b)),
        (Value::Number(a), Value::Int(b)) => Ok(Value::Number(a + *b as f64)),
        (Value::Str(a), Value::Str(b)) => {
            check_string_concat_memory_in(a.len(), b.len(), line, memory)?;
            Ok(Value::__new_str_in(format!("{a}{b}"), memory))
        }
        (Value::Str(a), b) => {
            let b_display = format!("{b}");
            check_string_concat_memory_in(a.len(), b_display.len(), line, memory)?;
            Ok(Value::__new_str_in(format!("{a}{b_display}"), memory))
        }
        (a, Value::Str(b)) => {
            let a_display = format!("{a}");
            check_string_concat_memory_in(a_display.len(), b.len(), line, memory)?;
            Ok(Value::__new_str_in(format!("{a_display}{b}"), memory))
        }
        (Value::Array(a), Value::Array(b)) => {
            check_array_concat_memory_in(a.len(), b.len(), line, memory)?;
            let mut result = a.to_vec();
            result.extend(b.to_vec());
            Value::__try_new_array_in(result, line, memory)
        }
        _ => Err(error(
            line,
            format!("Can't add {} and {}", left.type_name(), right.type_name()),
        )),
    }
}

pub fn sub(left: &Value, right: &Value, line: u32) -> Result<Value, NyblError> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => a
            .checked_sub(*b)
            .map(Value::Int)
            .ok_or_else(|| int_overflow("-", line)),
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a - b)),
        (Value::Int(a), Value::Number(b)) => Ok(Value::Number(*a as f64 - b)),
        (Value::Number(a), Value::Int(b)) => Ok(Value::Number(a - *b as f64)),
        _ => Err(error(
            line,
            format!(
                "Can't use `-` with {} and {}",
                left.type_name(),
                right.type_name()
            ),
        )),
    }
}

#[doc(hidden)]
pub fn sub_in(
    left: &Value,
    right: &Value,
    line: u32,
    _memory: &MemoryContext,
) -> Result<Value, NyblError> {
    sub(left, right, line)
}

pub fn mul(left: &Value, right: &Value, line: u32) -> Result<Value, NyblError> {
    mul_in(left, right, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn mul_in(
    left: &Value,
    right: &Value,
    line: u32,
    memory: &MemoryContext,
) -> Result<Value, NyblError> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => a
            .checked_mul(*b)
            .map(Value::Int)
            .ok_or_else(|| int_overflow("*", line)),
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a * b)),
        (Value::Int(a), Value::Number(b)) => Ok(Value::Number(*a as f64 * b)),
        (Value::Number(a), Value::Int(b)) => Ok(Value::Number(a * *b as f64)),
        // String repeat accepts any numeric count. Integers use
        // their direct value; floats cast through `as usize`
        // after a positivity / finiteness check (unchanged from
        // the pre-phase-6 behaviour).
        (Value::Str(s), Value::Int(n)) | (Value::Int(n), Value::Str(s)) => {
            if *n < 0 {
                return Err(error(line, format!("Can't repeat a string {n} times")));
            }
            let count = *n as usize;
            check_string_repeat_memory_in(s.len(), count, line, memory)?;
            Ok(Value::__new_str_in(s.repeat(count), memory))
        }
        (Value::Str(s), Value::Number(n)) | (Value::Number(n), Value::Str(s)) => {
            let nf = *n;
            if nf < 0.0 || !nf.is_finite() {
                return Err(error(line, format!("Can't repeat a string {nf} times")));
            }
            let count = nf as usize;
            check_string_repeat_memory_in(s.len(), count, line, memory)?;
            Ok(Value::__new_str_in(s.repeat(count), memory))
        }
        _ => Err(error(
            line,
            format!(
                "Can't multiply {} and {}",
                left.type_name(),
                right.type_name()
            ),
        )),
    }
}

/// `/` always returns a `Number`, even for `Int / Int`. Matches
/// Python's `/` and sidesteps the "1 / 2 == 0" surprise.
pub fn div(left: &Value, right: &Value, line: u32) -> Result<Value, NyblError> {
    let a = to_f64(left).ok_or_else(|| {
        error(
            line,
            format!("Can't divide {} by {}", left.type_name(), right.type_name()),
        )
    })?;
    let b = to_f64(right).ok_or_else(|| {
        error(
            line,
            format!("Can't divide {} by {}", left.type_name(), right.type_name()),
        )
    })?;
    if b == 0.0 {
        return Err(error_with_hint(
            line,
            "Division by zero",
            "You can't divide by 0.",
        ));
    }
    Ok(Value::Number(a / b))
}

#[doc(hidden)]
pub fn div_in(
    left: &Value,
    right: &Value,
    line: u32,
    _memory: &MemoryContext,
) -> Result<Value, NyblError> {
    div(left, right, line)
}

pub fn rem(left: &Value, right: &Value, line: u32) -> Result<Value, NyblError> {
    match (left, right) {
        (Value::Int(_), Value::Int(b)) if *b == 0 => Err(error_with_hint(
            line,
            "Modulo by zero",
            "You can't use % with 0.",
        )),
        (Value::Int(a), Value::Int(b)) => a
            .checked_rem(*b)
            .map(Value::Int)
            .ok_or_else(|| int_overflow("%", line)),
        (Value::Number(_), Value::Number(b)) if *b == 0.0 => Err(error_with_hint(
            line,
            "Modulo by zero",
            "You can't use % with 0.",
        )),
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a % b)),
        (Value::Int(a), Value::Number(b)) => {
            if *b == 0.0 {
                return Err(error_with_hint(
                    line,
                    "Modulo by zero",
                    "You can't use % with 0.",
                ));
            }
            Ok(Value::Number((*a as f64) % b))
        }
        (Value::Number(a), Value::Int(b)) => {
            if *b == 0 {
                return Err(error_with_hint(
                    line,
                    "Modulo by zero",
                    "You can't use % with 0.",
                ));
            }
            Ok(Value::Number(a % (*b as f64)))
        }
        _ => Err(error(
            line,
            format!(
                "Can't use % with {} and {}",
                left.type_name(),
                right.type_name()
            ),
        )),
    }
}

#[doc(hidden)]
pub fn rem_in(
    left: &Value,
    right: &Value,
    line: u32,
    _memory: &MemoryContext,
) -> Result<Value, NyblError> {
    rem(left, right, line)
}

pub fn eq(left: &Value, right: &Value) -> Value {
    Value::Bool(values_equal(left, right))
}

pub fn not_eq(left: &Value, right: &Value) -> Value {
    Value::Bool(!values_equal(left, right))
}

pub fn lt(left: &Value, right: &Value, line: u32) -> Result<Value, NyblError> {
    compare(left, right, |a, b| a < b, "<", line)
}

#[doc(hidden)]
pub fn lt_in(
    left: &Value,
    right: &Value,
    line: u32,
    _memory: &MemoryContext,
) -> Result<Value, NyblError> {
    lt(left, right, line)
}

pub fn gt(left: &Value, right: &Value, line: u32) -> Result<Value, NyblError> {
    compare(left, right, |a, b| a > b, ">", line)
}

#[doc(hidden)]
pub fn gt_in(
    left: &Value,
    right: &Value,
    line: u32,
    _memory: &MemoryContext,
) -> Result<Value, NyblError> {
    gt(left, right, line)
}

pub fn lt_eq(left: &Value, right: &Value, line: u32) -> Result<Value, NyblError> {
    compare(left, right, |a, b| a <= b, "<=", line)
}

#[doc(hidden)]
pub fn lt_eq_in(
    left: &Value,
    right: &Value,
    line: u32,
    _memory: &MemoryContext,
) -> Result<Value, NyblError> {
    lt_eq(left, right, line)
}

pub fn gt_eq(left: &Value, right: &Value, line: u32) -> Result<Value, NyblError> {
    compare(left, right, |a, b| a >= b, ">=", line)
}

#[doc(hidden)]
pub fn gt_eq_in(
    left: &Value,
    right: &Value,
    line: u32,
    _memory: &MemoryContext,
) -> Result<Value, NyblError> {
    gt_eq(left, right, line)
}

pub fn neg(val: &Value, line: u32) -> Result<Value, NyblError> {
    match val {
        Value::Int(n) => n
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| int_overflow("-", line)),
        Value::Number(n) => Ok(Value::Number(-n)),
        _ => Err(error(line, format!("Can't negate a {}", val.type_name()))),
    }
}

pub fn not(val: &Value) -> Value {
    Value::Bool(!val.is_truthy())
}

/// Coerce any numeric index (`Int` or `Number`) to an `i64`.
///
/// `Int` remains exact, while `Number` preserves the language's historical
/// float-to-index conversion (truncation toward zero, including Rust's
/// saturating casts for non-finite values). Method and subscript indexing both
/// use this helper so their accepted input types cannot drift apart.
pub(crate) fn numeric_index(idx: &Value) -> Option<i64> {
    match idx {
        Value::Int(n) => Some(*n),
        Value::Number(n) => Some(*n as i64),
        _ => None,
    }
}

/// Normalize an element index into `[0, len)`, counting negative indices from
/// the end. Returns `None` when the signed position lies outside the sequence.
pub(crate) fn normalize_element_index(index: i64, len: usize) -> Option<usize> {
    normalize_signed_index(index, len, false)
}

/// Normalize an insertion index into `[0, len]`. The positive `len` endpoint
/// appends; negative indices still count from the existing sequence end, so
/// `-1` inserts immediately before the final element.
pub(crate) fn normalize_insert_index(index: i64, len: usize) -> Option<usize> {
    normalize_signed_index(index, len, true)
}

/// Normalize one slice bound by counting negatives from the end and clamping
/// out-of-range positions to `[0, len]`.
pub(crate) fn normalize_slice_bound(index: i64, len: usize) -> usize {
    let position = signed_position(index, len);
    position.clamp(0, len as i128) as usize
}

fn normalize_signed_index(index: i64, len: usize, allow_end: bool) -> Option<usize> {
    let position = signed_position(index, len);
    let upper = len as i128;
    if position < 0 || position > upper || (!allow_end && position == upper) {
        None
    } else {
        Some(position as usize)
    }
}

fn signed_position(index: i64, len: usize) -> i128 {
    if index < 0 {
        len as i128 + index as i128
    } else {
        index as i128
    }
}

pub fn index_get(obj: &Value, idx: &Value, line: u32) -> Result<Value, NyblError> {
    index_get_in(obj, idx, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn index_get_in(
    obj: &Value,
    idx: &Value,
    line: u32,
    memory: &MemoryContext,
) -> Result<Value, NyblError> {
    match obj {
        Value::Array(arr) => {
            let i = numeric_index(idx).ok_or_else(|| {
                error(
                    line,
                    format!("Can't index {} with {}", obj.type_name(), idx.type_name()),
                )
            })?;
            let actual = normalize_element_index(i, arr.len());
            actual
                .and_then(|index| arr.get(index))
                .cloned()
                .ok_or_else(|| {
                    error(
                        line,
                        format!(
                            "Index {} is out of bounds (array has {} items)",
                            i,
                            arr.len()
                        ),
                    )
                })
        }
        Value::Str(s) => {
            let i = numeric_index(idx).ok_or_else(|| {
                error(
                    line,
                    format!("Can't index {} with {}", obj.type_name(), idx.type_name()),
                )
            })?;
            let chars: Vec<char> = s.chars().collect();
            normalize_element_index(i, chars.len())
                .and_then(|index| chars.get(index))
                .map(|c| Value::__new_str_in(c.to_string(), memory))
                .ok_or_else(|| {
                    error(
                        line,
                        format!(
                            "Index {} is out of bounds (string has {} characters)",
                            i,
                            chars.len()
                        ),
                    )
                })
        }
        Value::Dict(entries) => match idx {
            // Missing keys return `none` — matches Python / JS /
            // Lua convention and lines up with the language's
            // "any variable can be `none`" story. Callers who
            // need "present vs absent" disambiguation use
            // `d.has(key)` explicitly, or `d[key].is_some()` to
            // reach the same check via method.
            Value::Str(key) => Ok(entries
                .iter()
                .find(|(k, _)| k.as_str() == key.as_str())
                .map(|(_, v)| v.clone())
                .unwrap_or(Value::None)),
            _ => Err(error(
                line,
                format!("Can't index {} with {}", obj.type_name(), idx.type_name()),
            )),
        },
        _ => Err(error(
            line,
            format!("Can't index {} with {}", obj.type_name(), idx.type_name()),
        )),
    }
}

pub fn index_set(obj: &mut Value, idx: &Value, val: Value, line: u32) -> Result<(), NyblError> {
    index_set_in(obj, idx, val, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn index_set_in(
    obj: &mut Value,
    idx: &Value,
    val: Value,
    line: u32,
    memory: &MemoryContext,
) -> Result<(), NyblError> {
    match obj {
        Value::Array(arr) => {
            let i = numeric_index(idx)
                .ok_or_else(|| error(line, "Can't set index with these types"))?;
            let len = arr.len();
            let actual = normalize_element_index(i, len).ok_or_else(|| {
                error(
                    line,
                    format!("Index {i} is out of bounds (array has {len} items)"),
                )
            })?;
            arr.__try_set_in(actual, val, line, memory)
        }
        Value::Dict(entries) => match idx {
            Value::Str(key) => entries.__try_set_key_in(key, val, line, memory),
            _ => Err(error(line, "Can't set index with these types")),
        },
        _ => Err(error(line, "Can't set index with these types")),
    }
}

// ─── Internal helpers ───────────────────────────────────────────────────────

fn compare(
    left: &Value,
    right: &Value,
    f: impl Fn(f64, f64) -> bool,
    op_str: &str,
    line: u32,
) -> Result<Value, NyblError> {
    match (left, right) {
        // Int / Int uses exact integer comparison so magnitudes
        // beyond f64's 2^53 precision still compare correctly.
        (Value::Int(a), Value::Int(b)) => {
            let result = match op_str {
                "<" => a < b,
                ">" => a > b,
                "<=" => a <= b,
                _ => a >= b,
            };
            Ok(Value::Bool(result))
        }
        (Value::Number(a), Value::Number(b)) => Ok(Value::Bool(f(*a, *b))),
        // Cross-type numeric comparison widens through `f64`.
        (Value::Int(a), Value::Number(b)) => Ok(Value::Bool(f(*a as f64, *b))),
        (Value::Number(a), Value::Int(b)) => Ok(Value::Bool(f(*a, *b as f64))),
        (Value::Str(a), Value::Str(b)) => {
            let result = match op_str {
                "<" => a < b,
                ">" => a > b,
                "<=" => a <= b,
                _ => a >= b,
            };
            Ok(Value::Bool(result))
        }
        _ => Err(error(
            line,
            format!(
                "Can't compare {} and {} with `{}`",
                left.type_name(),
                right.type_name(),
                op_str
            ),
        )),
    }
}
