#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use crate::builtins::{error, error_with_hint};
use crate::error::NyblError;
use crate::memory::MemoryContext;
use crate::ops::{
    normalize_element_index, normalize_insert_index, normalize_slice_bound, numeric_index,
};
use crate::value::{NyblArray, NyblDict, Value, values_equal};

fn expect_method_index(name: &str, value: &Value, line: u32) -> Result<i64, NyblError> {
    numeric_index(value).ok_or_else(|| {
        error(
            line,
            format!("`{}` expects a number, but got {}", name, value.type_name()),
        )
    })
}

/// Returns (return_value, optional_mutated_object)
pub fn array_method(
    arr: &[Value],
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<(Value, Option<Value>), NyblError> {
    array_method_in(arr, method, args, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn array_method_in(
    arr: &[Value],
    method: &str,
    args: &[Value],
    line: u32,
    memory: &MemoryContext,
) -> Result<(Value, Option<Value>), NyblError> {
    match method {
        "len" => Ok((Value::Int(arr.len() as i64), None)),
        "push" => {
            if args.len() != 1 {
                return Err(error(line, ".push() needs exactly 1 argument"));
            }
            let mut new_arr = arr.to_vec();
            new_arr.push(args[0].clone());
            Ok((
                Value::None,
                Some(Value::__try_new_array_in(new_arr, line, memory)?),
            ))
        }
        "pop" => {
            let mut new_arr = arr.to_vec();
            let popped = new_arr.pop().unwrap_or(Value::None);
            Ok((
                popped,
                Some(Value::__try_new_array_in(new_arr, line, memory)?),
            ))
        }
        "has" => {
            if args.len() != 1 {
                return Err(error(line, ".has() needs exactly 1 argument"));
            }
            let found = arr.iter().any(|v| values_equal(v, &args[0]));
            Ok((Value::Bool(found), None))
        }
        "index_of" => {
            if args.len() != 1 {
                return Err(error(line, ".index_of() needs exactly 1 argument"));
            }
            let idx = arr.iter().position(|v| values_equal(v, &args[0]));
            Ok((Value::Int(idx.map_or(-1, |i| i as i64)), None))
        }
        "insert" => {
            if args.len() != 2 {
                return Err(error(line, ".insert() needs 2 arguments: index and value"));
            }
            let i = expect_method_index("insert", &args[0], line)?;
            let actual = normalize_insert_index(i, arr.len())
                .ok_or_else(|| error(line, format!("Insert index {i} is out of bounds")))?;
            let mut new_arr = arr.to_vec();
            new_arr.insert(actual, args[1].clone());
            Ok((
                Value::None,
                Some(Value::__try_new_array_in(new_arr, line, memory)?),
            ))
        }
        "remove" => {
            if args.len() != 1 {
                return Err(error(line, ".remove() needs exactly 1 argument (index)"));
            }
            let i = expect_method_index("remove", &args[0], line)?;
            let actual = normalize_element_index(i, arr.len())
                .ok_or_else(|| error(line, format!("Remove index {i} is out of bounds")))?;
            let mut new_arr = arr.to_vec();
            let removed = new_arr.remove(actual);
            Ok((
                removed,
                Some(Value::__try_new_array_in(new_arr, line, memory)?),
            ))
        }
        "slice" => {
            if args.len() != 2 {
                return Err(error(line, ".slice() needs 2 arguments: start and end"));
            }
            let start = expect_method_index("slice", &args[0], line)?;
            let end = expect_method_index("slice", &args[1], line)?;
            let end = normalize_slice_bound(end, arr.len());
            let start = normalize_slice_bound(start, arr.len()).min(end);
            let slice = arr[start..end].to_vec();
            Ok((Value::__try_new_array_in(slice, line, memory)?, None))
        }
        "truncate" => {
            if args.len() != 1 {
                return Err(error(line, ".truncate() needs exactly 1 argument (length)"));
            }
            let n = expect_method_index("truncate", &args[0], line)?;
            let keep = normalize_slice_bound(n, arr.len());
            let new_arr = arr[..keep].to_vec();
            Ok((
                Value::None,
                Some(Value::__try_new_array_in(new_arr, line, memory)?),
            ))
        }
        "clear" => {
            crate::builtins::expect_args("clear", args, 0, line)?;
            Ok((
                Value::None,
                Some(Value::__try_new_array_in(Vec::new(), line, memory)?),
            ))
        }
        "reverse" => {
            let mut new_arr = arr.to_vec();
            new_arr.reverse();
            Ok((
                Value::None,
                Some(Value::__try_new_array_in(new_arr, line, memory)?),
            ))
        }
        "sort" => {
            let mut new_arr = arr.to_vec();
            new_arr.sort_by(compare_array_values);
            Ok((
                Value::None,
                Some(Value::__try_new_array_in(new_arr, line, memory)?),
            ))
        }
        "join" => {
            if args.len() != 1 {
                return Err(error(line, ".join() needs exactly 1 argument (separator)"));
            }
            let sep = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".join() separator must be a string")),
            };
            let result = crate::formatting::__format_values_in(arr, sep, line, memory)?;
            let value = Value::__new_str_in(result, memory);
            crate::formatting::__preflight_in(0, line, memory)?;
            Ok((value, None))
        }
        "iter" => {
            crate::builtins::expect_args("iter", args, 0, line)?;
            // Snapshot the items — subsequent mutation of the
            // source array must not poke the iterator. Cheap
            // because every inner Value::Clone is either a
            // primitive copy or an Rc bump.
            Ok((
                Value::__try_new_array_iter_in(arr.to_vec(), line, memory)?,
                None,
            ))
        }
        _ => Err(error(
            line,
            format!("Array doesn't have a .{method}() method"),
        )),
    }
}

/// Execute one of the built-in mutating array methods directly on its owned
/// receiver. The engines use this only after proving the receiver is a bare
/// identifier bound to an array; transient/dynamic receivers keep using
/// [`array_method`] and its value-producing fallback.
pub fn array_method_mut(
    arr: &mut NyblArray,
    method: &str,
    args: Vec<Value>,
    line: u32,
) -> Result<Value, NyblError> {
    array_method_mut_in(arr, method, args, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn array_method_mut_in(
    arr: &mut NyblArray,
    method: &str,
    args: Vec<Value>,
    line: u32,
    memory: &MemoryContext,
) -> Result<Value, NyblError> {
    match method {
        "push" => {
            if args.len() != 1 {
                return Err(error(line, ".push() needs exactly 1 argument"));
            }
            let value = args.into_iter().next().expect("length checked");
            arr.__try_push_in(value, line, memory)?;
            Ok(Value::None)
        }
        "pop" => Ok(arr.__pop_in(memory).unwrap_or(Value::None)),
        "insert" => {
            if args.len() != 2 {
                return Err(error(line, ".insert() needs 2 arguments: index and value"));
            }
            let mut args = args.into_iter();
            let index = args.next().expect("length checked");
            let value = args.next().expect("length checked");
            let index = expect_method_index("insert", &index, line)?;
            let actual = normalize_insert_index(index, arr.len())
                .ok_or_else(|| error(line, format!("Insert index {index} is out of bounds")))?;
            arr.__try_insert_in(actual, value, line, memory)?;
            Ok(Value::None)
        }
        "remove" => {
            if args.len() != 1 {
                return Err(error(line, ".remove() needs exactly 1 argument (index)"));
            }
            let index = args.into_iter().next().expect("length checked");
            let index = expect_method_index("remove", &index, line)?;
            let actual = normalize_element_index(index, arr.len())
                .ok_or_else(|| error(line, format!("Remove index {index} is out of bounds")))?;
            Ok(arr.__remove_in(actual, memory))
        }
        "truncate" => {
            if args.len() != 1 {
                return Err(error(line, ".truncate() needs exactly 1 argument (length)"));
            }
            let length = args.into_iter().next().expect("length checked");
            let n = expect_method_index("truncate", &length, line)?;
            let keep = normalize_slice_bound(n, arr.len());
            arr.__truncate_in(keep, memory);
            Ok(Value::None)
        }
        "clear" => {
            if !args.is_empty() {
                return Err(error(line, "`clear` takes no arguments"));
            }
            arr.__truncate_in(0, memory);
            Ok(Value::None)
        }
        "reverse" => {
            arr.__reverse_in(memory);
            Ok(Value::None)
        }
        "sort" => {
            arr.__sort_by_in(compare_array_values, memory);
            Ok(Value::None)
        }
        _ => Err(error(
            line,
            format!("Array doesn't have a .{method}() method"),
        )),
    }
}

/// Run a mutating built-in array method transactionally against a named
/// binding without introducing a second `Rc` handle.
///
/// Engines call this only after resolving a supported mutable plain-variable
/// receiver and evaluating ordinary arguments. The binding is temporarily
/// moved into an unobservable staged slot, restored on every error, and
/// replaced only after the method and pending memory accounting succeed.
pub fn transactional_array_method(
    binding: &mut Value,
    method: &str,
    args: Vec<Value>,
    line: u32,
) -> Result<Value, NyblError> {
    transactional_array_method_in(
        binding,
        method,
        args,
        line,
        &MemoryContext::__legacy_current(),
    )
}

#[doc(hidden)]
pub fn transactional_array_method_in(
    binding: &mut Value,
    method: &str,
    args: Vec<Value>,
    line: u32,
    memory: &MemoryContext,
) -> Result<Value, NyblError> {
    let mut staged = core::mem::replace(binding, Value::None);
    let Value::Array(array) = &mut staged else {
        *binding = staged;
        return Err(error(line, "mutating array receiver is no longer an array"));
    };
    let growth_index = match method {
        "push" => Some(array.len()),
        "insert" => args
            .first()
            .and_then(numeric_index)
            .and_then(|index| normalize_insert_index(index, array.len())),
        _ => None,
    };

    let result = array_method_mut_in(array, method, args, line, memory);
    let value = match result {
        Ok(value) => value,
        Err(error) => {
            *binding = staged;
            return Err(error);
        }
    };
    if memory.__exceeded() {
        if let Some(index) = growth_index {
            let Value::Array(array) = &mut staged else {
                unreachable!("staged receiver stayed an array")
            };
            if index < array.len() {
                array.__remove_in(index, memory);
            }
        }
        *binding = staged;
        return Err(crate::builtins::error_fatal_with_hint(
            line,
            "Memory limit exceeded",
            "Your code is using too much memory. Check for large strings or arrays growing in loops.",
        ));
    }
    *binding = staged;
    Ok(value)
}

fn compare_array_values(a: &Value, b: &Value) -> core::cmp::Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Number(x), Value::Number(y)) => {
            x.partial_cmp(y).unwrap_or(core::cmp::Ordering::Equal)
        }
        // Mixed numeric sort — widen through f64 so
        // `[1, 2.5, 0]` sorts in the obvious numeric order.
        (Value::Int(x), Value::Number(y)) => (*x as f64)
            .partial_cmp(y)
            .unwrap_or(core::cmp::Ordering::Equal),
        (Value::Number(x), Value::Int(y)) => x
            .partial_cmp(&(*y as f64))
            .unwrap_or(core::cmp::Ordering::Equal),
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        _ => core::cmp::Ordering::Equal,
    }
}

pub fn string_method(
    s: &str,
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<(Value, Option<Value>), NyblError> {
    string_method_in(s, method, args, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn string_method_in(
    s: &str,
    method: &str,
    args: &[Value],
    line: u32,
    memory: &MemoryContext,
) -> Result<(Value, Option<Value>), NyblError> {
    match method {
        "len" => Ok((Value::Int(s.chars().count() as i64), None)),
        "contains" => {
            if args.len() != 1 {
                return Err(error(line, ".contains() needs 1 argument"));
            }
            let substr = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".contains() needs a string argument")),
            };
            Ok((Value::Bool(s.contains(substr)), None))
        }
        "starts_with" => {
            if args.len() != 1 {
                return Err(error(line, ".starts_with() needs 1 argument"));
            }
            let prefix = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".starts_with() needs a string")),
            };
            Ok((Value::Bool(s.starts_with(prefix)), None))
        }
        "ends_with" => {
            if args.len() != 1 {
                return Err(error(line, ".ends_with() needs 1 argument"));
            }
            let suffix = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".ends_with() needs a string")),
            };
            Ok((Value::Bool(s.ends_with(suffix)), None))
        }
        "index_of" => {
            if args.len() != 1 {
                return Err(error(line, ".index_of() needs 1 argument"));
            }
            let substr = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".index_of() needs a string")),
            };
            let idx = s.find(substr).map_or(-1, |i| i as i64);
            Ok((Value::Int(idx), None))
        }
        "split" => {
            if args.len() != 1 {
                return Err(error(line, ".split() needs 1 argument"));
            }
            let sep = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".split() needs a string")),
            };
            let parts = crate::formatting::__split_values_in(s, sep, line, memory)?;
            let value = Value::__try_new_array_in(parts, line, memory)?;
            crate::formatting::__preflight_in(0, line, memory)?;
            Ok((value, None))
        }
        "replace" => {
            if args.len() != 2 {
                return Err(error(line, ".replace() needs 2 arguments: old and new"));
            }
            let old = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".replace() arguments must be strings")),
            };
            let new = match &args[1] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".replace() arguments must be strings")),
            };
            let result = crate::formatting::__replace_in(s, old, new, line, memory)?;
            let value = Value::__new_str_in(result, memory);
            crate::formatting::__preflight_in(0, line, memory)?;
            Ok((value, None))
        }
        "upper" => {
            let result = s.to_uppercase();
            Ok((Value::__new_str_in(result, memory), None))
        }
        "lower" => {
            let result = s.to_lowercase();
            Ok((Value::__new_str_in(result, memory), None))
        }
        "trim" => {
            let result = s.trim().to_string();
            Ok((Value::__new_str_in(result, memory), None))
        }
        "slice" => {
            if args.len() != 2 {
                return Err(error(line, ".slice() needs 2 arguments: start and end"));
            }
            let start = expect_method_index("slice", &args[0], line)?;
            let chars: Vec<char> = s.chars().collect();
            let end = expect_method_index("slice", &args[1], line)?;
            let end = normalize_slice_bound(end, chars.len());
            let start = normalize_slice_bound(start, chars.len()).min(end);
            let result: String = chars[start..end].iter().collect();
            Ok((Value::__new_str_in(result, memory), None))
        }
        "to_int" => {
            if !args.is_empty() {
                return Err(error(line, ".to_int() takes no arguments"));
            }
            // Integer-first parse so `"42".to_int()` stays an
            // `Int`. Fall back to float-then-truncate for
            // decimal-shaped strings, matching the old
            // `"3.7".to_int()` behaviour.
            if let Ok(n) = s.parse::<i64>() {
                return Ok((Value::Int(n), None));
            }
            let n: f64 = s
                .parse()
                .map_err(|_| error(line, format!("Can't convert \"{s}\" to a number")))?;
            Ok((Value::Int(n as i64), None))
        }
        "to_float" => {
            if !args.is_empty() {
                return Err(error(line, ".to_float() takes no arguments"));
            }
            let n: f64 = s
                .parse()
                .map_err(|_| error(line, format!("Can't convert \"{s}\" to a number")))?;
            Ok((Value::Number(n), None))
        }
        "iter" => {
            crate::builtins::expect_args("iter", args, 0, line)?;
            let chars: Vec<char> = s.chars().collect();
            Ok((Value::__new_string_iter_in(chars, memory), None))
        }
        _ => Err(error(
            line,
            format!("String doesn't have a .{method}() method"),
        )),
    }
}

// Keep the array/string normalization tests next to those method families;
// the remaining method families continue below in the same runtime module.
#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    const TWO_TO_63: f64 = 9_223_372_036_854_775_808.0;
    const LARGEST_IN_RANGE_F64: f64 = 9_223_372_036_854_774_784.0;

    fn ints(value: &Value) -> Vec<i64> {
        let Value::Array(items) = value else {
            panic!("expected an array");
        };
        items
            .iter()
            .map(|item| match item {
                Value::Int(value) => *value,
                _ => panic!("expected an integer item"),
            })
            .collect()
    }

    fn updated_array(result: (Value, Option<Value>)) -> Value {
        result.1.expect("method did not return an array update")
    }

    #[test]
    fn remove_uses_negative_element_indices() {
        let source = [
            Value::Int(10),
            Value::Int(20),
            Value::Int(30),
            Value::Int(40),
        ];
        let (removed, update) = array_method(&source, "remove", &[Value::Int(-1)], 9)
            .expect("negative remove should succeed");
        let Value::Int(removed) = removed else {
            panic!("remove did not return an integer");
        };
        assert_eq!(removed, 40);
        assert_eq!(
            ints(&update.expect("remove did not update the array")),
            [10, 20, 30]
        );
    }

    #[test]
    fn insert_supports_signed_positions_and_the_positive_endpoint() {
        let source = [Value::Int(10), Value::Int(20), Value::Int(30)];
        for (index, expected) in [
            (-1, &[10, 20, 99, 30][..]),
            (-3, &[99, 10, 20, 30][..]),
            (3, &[10, 20, 30, 99][..]),
        ] {
            let result = array_method(&source, "insert", &[Value::Int(index), Value::Int(99)], 9)
                .expect("valid insertion index should succeed");
            assert_eq!(ints(&updated_array(result)), expected);
        }

        for index in [-4, 4] {
            let err = array_method(&source, "insert", &[Value::Int(index), Value::Int(99)], 9)
                .expect_err("out-of-range insertion should fail");
            assert_eq!(
                err.message,
                format!("Insert index {index} is out of bounds")
            );
        }
    }

    #[test]
    fn slice_translates_negative_bounds_then_clamps() {
        let source = [
            Value::Int(10),
            Value::Int(20),
            Value::Int(30),
            Value::Int(40),
        ];
        for (start, end, expected) in [
            (-2, 4, &[30, 40][..]),
            (0, -1, &[10, 20, 30][..]),
            (-100, 100, &[10, 20, 30, 40][..]),
            (3, 1, &[][..]),
        ] {
            let (slice, update) =
                array_method(&source, "slice", &[Value::Int(start), Value::Int(end)], 9)
                    .expect("slice should normalize its bounds");
            assert!(update.is_none());
            assert_eq!(ints(&slice), expected);
        }
    }

    #[test]
    fn string_slice_counts_unicode_characters_from_the_end() {
        let (slice, update) = string_method("a🦀éz", "slice", &[Value::Int(-3), Value::Int(-1)], 9)
            .expect("unicode slice should succeed");
        assert!(update.is_none());
        let Value::Str(ref slice) = slice else {
            panic!("slice did not return a string");
        };
        assert_eq!(slice.as_str(), "🦀é");
    }

    #[test]
    fn method_indices_preserve_subscript_numeric_coercion() {
        let source = [Value::Int(10), Value::Int(20), Value::Int(30)];
        let (removed, update) = array_method(&source, "remove", &[Value::Number(-1.9)], 9)
            .expect("fractional numeric index should truncate like a subscript");
        let Value::Int(removed) = removed else {
            panic!("remove did not return an integer");
        };
        assert_eq!(removed, 30);
        assert_eq!(
            ints(&update.expect("remove did not update the array")),
            [10, 20]
        );

        let err = array_method(&source, "remove", &[Value::Int(i64::MIN)], 9)
            .expect_err("extreme negative index should fail");
        assert_eq!(
            err.message,
            format!("Remove index {} is out of bounds", i64::MIN)
        );
    }

    #[test]
    fn rounding_methods_preserve_values_at_i64_boundaries() {
        for method in ["floor", "ceil", "round"] {
            let (above, update) =
                numeric_method(&Value::Number(TWO_TO_63), method, &[], 9).unwrap();
            assert!(update.is_none());
            assert!(
                matches!(above, Value::Number(value) if value == TWO_TO_63),
                ".{method}() converted 2^63 to an Int"
            );

            let (minimum, update) =
                numeric_method(&Value::Number(i64::MIN as f64), method, &[], 9).unwrap();
            assert!(update.is_none());
            assert!(
                matches!(minimum, Value::Int(i64::MIN)),
                ".{method}() did not preserve i64::MIN"
            );

            let (largest, update) =
                numeric_method(&Value::Number(LARGEST_IN_RANGE_F64), method, &[], 9).unwrap();
            assert!(update.is_none());
            assert!(
                matches!(largest, Value::Int(value) if value == LARGEST_IN_RANGE_F64 as i64),
                ".{method}() did not convert the largest in-range f64"
            );
        }
    }
}

pub fn dict_method(
    entries: &[(String, Value)],
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<(Value, Option<Value>), NyblError> {
    dict_method_in(
        entries,
        method,
        args,
        line,
        &MemoryContext::__legacy_current(),
    )
}

#[doc(hidden)]
pub fn dict_method_in(
    entries: &[(String, Value)],
    method: &str,
    args: &[Value],
    line: u32,
    memory: &MemoryContext,
) -> Result<(Value, Option<Value>), NyblError> {
    match method {
        "len" => Ok((Value::Int(entries.len() as i64), None)),
        "keys" => {
            let keys: Vec<Value> = entries
                .iter()
                .map(|(k, _)| Value::__new_str_in(k.clone(), memory))
                .collect();
            Ok((Value::__try_new_array_in(keys, line, memory)?, None))
        }
        "values" => {
            let vals: Vec<Value> = entries.iter().map(|(_, v)| v.clone()).collect();
            Ok((Value::__try_new_array_in(vals, line, memory)?, None))
        }
        "has" => {
            if args.len() != 1 {
                return Err(error(line, ".has() needs 1 argument"));
            }
            let key = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".has() needs a string key")),
            };
            Ok((Value::Bool(entries.iter().any(|(k, _)| k == key)), None))
        }
        "iter" => {
            crate::builtins::expect_args("iter", args, 0, line)?;
            // Matches `for k in dict` semantics: iterate keys
            // in declaration order.
            let keys: Vec<String> = entries.iter().map(|(k, _)| k.clone()).collect();
            Ok((Value::__new_dict_iter_in(keys, memory), None))
        }
        "remove" => {
            let key = expect_dict_remove_key(args, line)?;
            let Some(index) = entries.iter().position(|(k, _)| k == key) else {
                return Ok((Value::None, None));
            };
            let mut new_entries = entries.to_vec();
            let (_, removed) = new_entries.remove(index);
            Ok((
                removed,
                Some(Value::__try_new_dict_in(new_entries, line, memory)?),
            ))
        }
        "clear" => {
            crate::builtins::expect_args("clear", args, 0, line)?;
            Ok((
                Value::None,
                Some(Value::__try_new_dict_in(Vec::new(), line, memory)?),
            ))
        }
        _ => Err(error(
            line,
            format!("Dict doesn't have a .{method}() method"),
        )),
    }
}

fn expect_dict_remove_key(args: &[Value], line: u32) -> Result<&str, NyblError> {
    if args.len() != 1 {
        return Err(error(line, ".remove() needs 1 argument (key)"));
    }
    match &args[0] {
        Value::Str(s) => Ok(s.as_str()),
        _ => Err(error(line, ".remove() needs a string key")),
    }
}

/// Execute one of the built-in mutating dict methods directly on its owned
/// receiver. Mirrors [`array_method_mut`]: engines call this only after
/// proving the receiver is a write-back place bound to a dict.
pub fn dict_method_mut(
    dict: &mut NyblDict,
    method: &str,
    args: Vec<Value>,
    line: u32,
) -> Result<Value, NyblError> {
    dict_method_mut_in(dict, method, args, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn dict_method_mut_in(
    dict: &mut NyblDict,
    method: &str,
    args: Vec<Value>,
    line: u32,
    memory: &MemoryContext,
) -> Result<Value, NyblError> {
    match method {
        "remove" => {
            let key = expect_dict_remove_key(&args, line)?;
            // Absent keys return `none`, matching missing-key reads.
            Ok(dict.__remove_key_in(key, memory).unwrap_or(Value::None))
        }
        "clear" => {
            if !args.is_empty() {
                return Err(error(line, "`clear` takes no arguments"));
            }
            dict.__clear_in(memory);
            Ok(Value::None)
        }
        _ => Err(error(
            line,
            format!("Dict doesn't have a .{method}() method"),
        )),
    }
}

/// Run a mutating built-in dict method transactionally against a named
/// binding, mirroring [`transactional_array_method`]. Dict mutation only
/// removes entries, so the sole way memory can overrun is the CoW detach of
/// a shared receiver — checked after the call like the array path.
pub fn transactional_dict_method(
    binding: &mut Value,
    method: &str,
    args: Vec<Value>,
    line: u32,
) -> Result<Value, NyblError> {
    transactional_dict_method_in(binding, method, args, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn transactional_dict_method_in(
    binding: &mut Value,
    method: &str,
    args: Vec<Value>,
    line: u32,
    memory: &MemoryContext,
) -> Result<Value, NyblError> {
    let mut staged = core::mem::replace(binding, Value::None);
    let Value::Dict(dict) = &mut staged else {
        *binding = staged;
        return Err(error(line, "mutating dict receiver is no longer a dict"));
    };
    let result = dict_method_mut_in(dict, method, args, line, memory);
    let value = match result {
        Ok(value) => value,
        Err(error) => {
            *binding = staged;
            return Err(error);
        }
    };
    *binding = staged;
    if memory.__exceeded() {
        return Err(crate::builtins::error_fatal_with_hint(
            line,
            "Memory limit exceeded",
            "Your code is using too much memory. Check for large strings or arrays growing in loops.",
        ));
    }
    Ok(value)
}

/// Route a mutating built-in collection method to the matching transactional
/// helper by the binding's live type. Engines call this after a receiver
/// guard proved a mutating collection method at preflight; re-deriving the
/// type here keeps the call correct even if argument side effects rebound
/// the receiver between preflight and execution.
pub fn transactional_collection_method(
    binding: &mut Value,
    method: &str,
    args: Vec<Value>,
    line: u32,
) -> Result<Value, NyblError> {
    transactional_collection_method_in(
        binding,
        method,
        args,
        line,
        &MemoryContext::__legacy_current(),
    )
}

#[doc(hidden)]
pub fn transactional_collection_method_in(
    binding: &mut Value,
    method: &str,
    args: Vec<Value>,
    line: u32,
    memory: &MemoryContext,
) -> Result<Value, NyblError> {
    match binding {
        Value::Array(_) if is_mutating_method(method) => {
            transactional_array_method_in(binding, method, args, line, memory)
        }
        Value::Dict(_) if is_mutating_dict_method(method) => {
            transactional_dict_method_in(binding, method, args, line, memory)
        }
        Value::Dict(_) => Err(error(
            line,
            format!("Dict doesn't have a .{method}() method"),
        )),
        // Keeps the canonical receiver-changed diagnostic for arrays.
        _ => transactional_array_method_in(binding, method, args, line, memory),
    }
}

/// Methods every value understands: introspection + stringification.
/// Dispatched from `call_method` *before* the type-specific method
/// tables so `x.type()`, `x.to_str()`, and `x.inspect()` work
/// uniformly across every `Value` shape.
///
/// Returns `Ok(Some(result))` when the method name matched,
/// `Ok(None)` when it didn't (so the caller falls through to
/// the type-specific dispatcher). `Err` on arg-count mismatches.
pub fn common_method(
    receiver: &Value,
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<Option<(Value, Option<Value>)>, NyblError> {
    common_method_in(
        receiver,
        method,
        args,
        line,
        &MemoryContext::__legacy_current(),
    )
}

#[doc(hidden)]
pub fn common_method_in(
    receiver: &Value,
    method: &str,
    args: &[Value],
    line: u32,
    memory: &MemoryContext,
) -> Result<Option<(Value, Option<Value>)>, NyblError> {
    match method {
        "type" => {
            crate::builtins::expect_args("type", args, 0, line)?;
            Ok(Some((
                Value::__new_str_in(receiver.type_name().to_string(), memory),
                None,
            )))
        }
        "to_str" => {
            crate::builtins::expect_args("to_str", args, 0, line)?;
            Ok(Some((
                Value::__new_str_in(format!("{receiver}"), memory),
                None,
            )))
        }
        "inspect" => {
            crate::builtins::expect_args("inspect", args, 0, line)?;
            Ok(Some((
                Value::__new_str_in(receiver.inspect(), memory),
                None,
            )))
        }
        // `.is_none()` / `.is_some()` — dynamic-typing's answer
        // to `Option`. Any variable can hold `none` (missing
        // dict key, explicit `return none`, unset optional
        // field), so the check works on every value rather than
        // only on an Option-typed receiver. Equivalent to
        // `x == none` / `x != none` but reads more intention-
        // ally at the call site and composes with method chains.
        "is_none" => {
            crate::builtins::expect_args("is_none", args, 0, line)?;
            Ok(Some((Value::Bool(matches!(receiver, Value::None)), None)))
        }
        "is_some" => {
            crate::builtins::expect_args("is_some", args, 0, line)?;
            Ok(Some((Value::Bool(!matches!(receiver, Value::None)), None)))
        }
        _ => Ok(None),
    }
}

/// Method dispatch for `Int` and `Number` receivers. Covers the
/// math operations that used to be global builtins (`abs`,
/// `sqrt`, `sin`, …) plus numeric coercions (`to_int`,
/// `to_float`). Returns `Err` on argument errors or unknown
/// method names — no fall-through.
pub fn numeric_method(
    receiver: &Value,
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<(Value, Option<Value>), NyblError> {
    use crate::builtins::{expect_args, finite_to_int_or_number};
    match method {
        // Preserves type: Int stays Int, Number stays Number.
        "abs" => {
            expect_args("abs", args, 0, line)?;
            match receiver {
                Value::Int(n) => n
                    .checked_abs()
                    .map(Value::Int)
                    .map(|v| (v, None))
                    .ok_or_else(|| error(line, "Integer overflow in `.abs()`")),
                Value::Number(n) => Ok((Value::Number(n.abs()), None)),
                _ => unreachable!("numeric_method called on non-numeric receiver"),
            }
        }
        // Square / trig / exp / log: always return `Number`.
        "sqrt" => unary_number(receiver, args, line, "sqrt", crate::math::sqrt),
        "sin" => unary_number(receiver, args, line, "sin", crate::math::sin),
        "cos" => unary_number(receiver, args, line, "cos", crate::math::cos),
        "tan" => unary_number(receiver, args, line, "tan", crate::math::tan),
        "exp" => unary_number(receiver, args, line, "exp", crate::math::exp),
        "log" => unary_number(receiver, args, line, "log", crate::math::ln),
        // Round-to-integer: return Int when the result fits i64,
        // Number otherwise.
        "floor" => unary_round(receiver, args, line, "floor", crate::math::floor),
        "ceil" => unary_round(receiver, args, line, "ceil", crate::math::ceil),
        "round" => unary_round(receiver, args, line, "round", crate::math::round),
        "pow" => {
            expect_args("pow", args, 1, line)?;
            let base = to_f64_or_error(receiver, "pow", line)?;
            let exp = to_f64_or_error(&args[0], "pow", line)?;
            Ok((Value::Number(crate::math::powf(base, exp)), None))
        }
        // Binary pick-min / pick-max. Preserves type when both
        // sides match; widens to Number on mixed operands —
        // same rule the old `a.min(b)` / `a.max(b)` builtins
        // used.
        "min" => pair_pick(receiver, args, line, "min", true),
        "max" => pair_pick(receiver, args, line, "max", false),
        // Explicit numeric coercion. `int` ↔ `int` is a no-op,
        // `number` → `int` truncates toward zero.
        "to_int" => {
            expect_args("to_int", args, 0, line)?;
            match receiver {
                Value::Int(n) => Ok((Value::Int(*n), None)),
                Value::Number(n) => Ok((Value::Int(*n as i64), None)),
                _ => unreachable!(),
            }
        }
        "to_float" => {
            expect_args("to_float", args, 0, line)?;
            match receiver {
                Value::Int(n) => Ok((Value::Number(*n as f64), None)),
                Value::Number(n) => Ok((Value::Number(*n), None)),
                _ => unreachable!(),
            }
        }
        _ => {
            let _ = finite_to_int_or_number;
            Err(error(
                line,
                crate::error_messages::no_such_method(receiver.type_name(), method),
            ))
        }
    }
}

/// Method dispatch for `Bool`. Only the numeric coercions
/// (`true.to_int()` → `1`, etc.); `type` / `to_str` / `inspect`
/// go through `common_method` before this is called.
pub fn bool_method(
    receiver: &Value,
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<(Value, Option<Value>), NyblError> {
    use crate::builtins::expect_args;
    let b = match receiver {
        Value::Bool(b) => *b,
        _ => unreachable!("bool_method called on non-bool receiver"),
    };
    match method {
        "to_int" => {
            expect_args("to_int", args, 0, line)?;
            Ok((Value::Int(if b { 1 } else { 0 }), None))
        }
        "to_float" => {
            expect_args("to_float", args, 0, line)?;
            Ok((Value::Number(if b { 1.0 } else { 0.0 }), None))
        }
        _ => Err(error(
            line,
            crate::error_messages::no_such_method("bool", method),
        )),
    }
}

// ─── numeric_method helpers ────────────────────────────────────

/// Coerce an `Int` / `Number` receiver to `f64`. Anything else
/// is a programmer error — `numeric_method` only reaches this
/// helper for numeric receivers.
fn to_f64_or_error(v: &Value, method: &str, line: u32) -> Result<f64, NyblError> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Number(n) => Ok(*n),
        other => Err(error(
            line,
            format!("`.{}` expects a number, got {}", method, other.type_name()),
        )),
    }
}

/// Shared implementation for trig / exp / log — zero-arg
/// methods that always return a `Number`.
fn unary_number(
    receiver: &Value,
    args: &[Value],
    line: u32,
    method: &str,
    op: fn(f64) -> f64,
) -> Result<(Value, Option<Value>), NyblError> {
    crate::builtins::expect_args(method, args, 0, line)?;
    let x = to_f64_or_error(receiver, method, line)?;
    Ok((Value::Number(op(x)), None))
}

/// Shared implementation for `floor` / `ceil` / `round`. Return
/// type mirrors the stdlib: `Int` when the rounded value fits
/// in `i64`, `Number` otherwise.
fn unary_round(
    receiver: &Value,
    args: &[Value],
    line: u32,
    method: &str,
    op: fn(f64) -> f64,
) -> Result<(Value, Option<Value>), NyblError> {
    use crate::builtins::{expect_args, finite_to_int_or_number};
    expect_args(method, args, 0, line)?;
    match receiver {
        Value::Int(n) => Ok((Value::Int(*n), None)),
        Value::Number(n) => Ok((finite_to_int_or_number(op(*n)), None)),
        _ => unreachable!("unary_round called on non-numeric receiver"),
    }
}

/// `.min(other)` / `.max(other)` — preserves numeric type
/// when both operands match, widens to Number on mixed shape.
fn pair_pick(
    receiver: &Value,
    args: &[Value],
    line: u32,
    method: &str,
    pick_smaller: bool,
) -> Result<(Value, Option<Value>), NyblError> {
    use crate::builtins::expect_args;
    expect_args(method, args, 1, line)?;
    match (receiver, &args[0]) {
        (Value::Int(a), Value::Int(b)) => {
            let pick = if pick_smaller {
                (*a).min(*b)
            } else {
                (*a).max(*b)
            };
            Ok((Value::Int(pick), None))
        }
        (Value::Number(a), Value::Number(b)) => {
            let pick = if pick_smaller { a.min(*b) } else { a.max(*b) };
            Ok((Value::Number(pick), None))
        }
        (Value::Int(a), Value::Number(b)) => {
            let af = *a as f64;
            let pick = if pick_smaller { af.min(*b) } else { af.max(*b) };
            Ok((Value::Number(pick), None))
        }
        (Value::Number(a), Value::Int(b)) => {
            let bf = *b as f64;
            let pick = if pick_smaller { a.min(bf) } else { a.max(bf) };
            Ok((Value::Number(pick), None))
        }
        (_, other) => Err(error(
            line,
            format!(
                "`.{}({})` expects a number, got {}",
                method,
                other.type_name(),
                other.type_name()
            ),
        )),
    }
}

pub fn is_mutating_method(method: &str) -> bool {
    matches!(
        method,
        "push" | "pop" | "insert" | "remove" | "reverse" | "sort" | "truncate" | "clear"
    )
}

/// The built-in dict methods that mutate their receiver. Kept separate from
/// [`is_mutating_method`] because engines gate mutation dispatch on receiver
/// type: `remove` mutates both arrays (by index) and dicts (by key), while
/// the rest of the array set stays read-only-or-missing on dicts.
pub fn is_mutating_dict_method(method: &str) -> bool {
    matches!(method, "remove" | "clear")
}

/// True when `receiver` is a built-in collection and `method` is one of its
/// mutating built-ins — the shared preflight guard for the engines' in-place
/// dispatch fast paths.
pub fn is_mutating_collection_receiver(receiver: &Value, method: &str) -> bool {
    match receiver {
        Value::Array(_) => is_mutating_method(method),
        Value::Dict(_) => is_mutating_dict_method(method),
        _ => false,
    }
}

/// Positional arity for every built-in method name. Engines use this during
/// call preflight so a bad arity prevents argument-expression side effects.
/// User-defined methods must be resolved first because they may deliberately
/// reuse one of these names with a different signature.
pub fn builtin_method_arity(method: &str) -> Option<usize> {
    match method {
        "len" | "pop" | "reverse" | "sort" | "clear" | "iter" | "keys" | "values" | "upper"
        | "lower" | "trim" | "to_int" | "to_float" | "type" | "to_str" | "inspect" | "is_none"
        | "is_some" | "abs" | "sqrt" | "sin" | "cos" | "tan" | "exp" | "log" | "floor" | "ceil"
        | "round" | "is_ok" | "is_err" | "unwrap" | "next" => Some(0),
        "push" | "has" | "index_of" | "remove" | "truncate" | "join" | "contains"
        | "starts_with" | "ends_with" | "split" | "pow" | "min" | "max" | "expect"
        | "unwrap_or" | "map" | "map_err" | "and_then" => Some(1),
        "insert" | "slice" | "replace" => Some(2),
        _ => None,
    }
}

/// Reject a built-in array mutation after dispatch has proven that the named
/// receiver is an array rather than a user-defined value with a colliding
/// method name.
///
/// Callers deliberately invoke this only on the built-in Array path. That
/// preserves ordinary user methods named `push`, `pop`, and so on, plus the
/// usual method-not-found diagnostics for non-array values.
pub fn reject_constant_array_mutation(
    receiver_name: &str,
    method: &str,
    line: u32,
) -> Result<(), NyblError> {
    if crate::naming::is_constant_name(receiver_name) && is_mutating_method(method) {
        return Err(crate::error_messages::constant_mutation_error(
            receiver_name,
            line,
        ));
    }
    Ok(())
}

/// Reject a built-in array or dict mutation whose receiver was evaluated
/// from an index or field access. Those syntactic forms are not yet
/// write-back places, so accepting the call would mutate a detached
/// value and silently discard the result.
///
/// Call this only after user-defined method dispatch. A struct or
/// enum method named `push`, `pop`, etc. remains a regular dynamic
/// method call; the restriction is specifically the combination of
/// a nested-place syntax form and a built-in collection receiver type.
pub fn reject_nested_array_mutation(
    receiver: &Value,
    method: &str,
    line: u32,
) -> Result<(), NyblError> {
    if is_mutating_collection_receiver(receiver, method) {
        return Err(error_with_hint(
            line,
            crate::error_messages::NESTED_MUTATION_ERROR_MESSAGE,
            crate::error_messages::NESTED_MUTATION_HINT,
        ));
    }
    Ok(())
}

// ─── Result methods ────────────────────────────────────────────
//
// `Result` is a built-in enum (see `builtins::builtin_result_variants`
// seeded into every engine's type table). Its combinators live
// here so `r.is_ok()`, `r.unwrap()`, etc. are always available
// without `use std.result`. Callable-taking variants (`map`,
// `map_err`, `and_then`) need the evaluator's call primitive and
// therefore live in each engine's MethodCall dispatch alongside
// the user-method table, not here — see [`is_result_callable_method`].

/// True when `receiver` is a `Value::EnumVariant` whose type
/// identity is the built-in `Result`. Shared by each engine
/// so a user enum named `Result` in some module can't
/// accidentally steal the built-in method dispatch.
pub fn is_builtin_result(receiver: &Value) -> bool {
    match receiver {
        Value::EnumVariant(e) => {
            e.module_path() == crate::value::BUILTIN_MODULE_PATH && e.type_name() == "Result"
        }
        _ => false,
    }
}

/// Identifies `map` / `map_err` / `and_then` as the
/// callable-taking Result methods each engine handles inline.
/// Returns `None` for anything else so the caller can fall
/// through to [`result_method`] or the standard dispatcher.
pub enum ResultCallableKind {
    /// `r.map(f)` — `Ok(v)` becomes `Ok(f(v))`, `Err(e)` passes.
    Map,
    /// `r.map_err(f)` — `Err(e)` becomes `Err(f(e))`, `Ok(v)` passes.
    MapErr,
    /// `r.and_then(f)` — `Ok(v)` becomes `f(v)` (expected to
    /// return a Result), `Err(e)` passes.
    AndThen,
}

pub fn is_result_callable_method(method: &str) -> Option<ResultCallableKind> {
    match method {
        "map" => Some(ResultCallableKind::Map),
        "map_err" => Some(ResultCallableKind::MapErr),
        "and_then" => Some(ResultCallableKind::AndThen),
        _ => None,
    }
}

/// Pure Result method dispatch. Handles `is_ok`, `is_err`,
/// `unwrap`, `expect`, `unwrap_or` — the combinators whose
/// implementation doesn't need to invoke a user callable.
///
/// Returns `Ok(Some(value))` when the method name matched,
/// `Ok(None)` when it didn't (so the caller falls through to
/// the callable-taking Result methods, then to the standard
/// dispatcher). Assumes `receiver` is already known to be a
/// built-in Result — callers check [`is_builtin_result`] first.
pub fn result_method(
    receiver: &Value,
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<Option<Value>, NyblError> {
    use crate::builtins::expect_args;
    let e = match receiver {
        Value::EnumVariant(e) => e,
        _ => return Ok(None),
    };
    let ok_payload = || -> Option<Value> {
        if e.variant() != "Ok" {
            return None;
        }
        match e.payload() {
            crate::value::EnumPayload::Tuple(items) if items.len() == 1 => Some(items[0].clone()),
            _ => None,
        }
    };
    let err_payload = || -> Option<Value> {
        if e.variant() != "Err" {
            return None;
        }
        match e.payload() {
            crate::value::EnumPayload::Tuple(items) if items.len() == 1 => Some(items[0].clone()),
            _ => None,
        }
    };
    match method {
        "is_ok" => {
            expect_args("is_ok", args, 0, line)?;
            Ok(Some(Value::Bool(e.variant() == "Ok")))
        }
        "is_err" => {
            expect_args("is_err", args, 0, line)?;
            Ok(Some(Value::Bool(e.variant() == "Err")))
        }
        "unwrap" => {
            expect_args("unwrap", args, 0, line)?;
            if let Some(v) = ok_payload() {
                return Ok(Some(v));
            }
            // Err case — construct the same message std.result
            // used to emit so migrated programs keep their
            // crash-trace text stable.
            let detail = match err_payload() {
                Some(payload) => format!("unwrap on Err: {}", payload.inspect()),
                None => String::from("unwrap on Err"),
            };
            Err(error(line, detail))
        }
        "expect" => {
            expect_args("expect", args, 1, line)?;
            if let Some(v) = ok_payload() {
                return Ok(Some(v));
            }
            let message = match &args[0] {
                Value::Str(s) => s.as_str().to_string(),
                other => format!("{other}"),
            };
            Err(error(line, message))
        }
        "unwrap_or" => {
            expect_args("unwrap_or", args, 1, line)?;
            if let Some(v) = ok_payload() {
                return Ok(Some(v));
            }
            Ok(Some(args[0].clone()))
        }
        _ => Ok(None),
    }
}

/// Build a `Result::Ok(value)` with the builtin's module path so
/// pattern matches against `Result::Ok(_)` fire regardless of
/// which module the receiver came from. Mirror of the helper in
/// `builtins` but specialised to the "wrap a value" case each
/// engine's callable dispatch uses.
pub fn make_result_ok(value: Value, line: u32) -> Result<Value, NyblError> {
    make_result_ok_in(value, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn make_result_ok_in(
    value: Value,
    line: u32,
    memory: &MemoryContext,
) -> Result<Value, NyblError> {
    Value::__try_new_enum_tuple_in(
        String::from(crate::value::BUILTIN_MODULE_PATH),
        String::from("Result"),
        String::from("Ok"),
        alloc_vec_of(value),
        line,
        memory,
    )
}

/// Same as [`make_result_ok`] but for `Err`.
pub fn make_result_err(value: Value, line: u32) -> Result<Value, NyblError> {
    make_result_err_in(value, line, &MemoryContext::__legacy_current())
}

#[doc(hidden)]
pub fn make_result_err_in(
    value: Value,
    line: u32,
    memory: &MemoryContext,
) -> Result<Value, NyblError> {
    Value::__try_new_enum_tuple_in(
        String::from(crate::value::BUILTIN_MODULE_PATH),
        String::from("Result"),
        String::from("Err"),
        alloc_vec_of(value),
        line,
        memory,
    )
}

fn alloc_vec_of(value: Value) -> Vec<Value> {
    vec![value]
}

// ─── Iterator methods ──────────────────────────────────────────
//
// `Value::Iter` receivers get two methods:
//  - `.next()` advances the cursor and returns
//    `Iter::Next(value)` or `Iter::Done` — the shape the `for`
//    loop (and user code) pattern-matches on.
//  - `.iter()` returns the receiver itself. Makes iterators
//    idempotently iterable, matching Python's iterator protocol
//    (an iterator's `__iter__` returns `self`), so `for x in
//    arr.iter()` works without a special case.
//
// User-defined iterators are just ordinary struct values with
// their own `.next()` method — they don't go through this
// dispatcher. The `for` loop treats them uniformly via the
// protocol: call `.iter()` to get an iterator, call `.next()`
// until `Iter::Done`.

pub fn iter_method(
    receiver: &Value,
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<(Value, Option<Value>), NyblError> {
    iter_method_in(
        receiver,
        method,
        args,
        line,
        &MemoryContext::__legacy_current(),
    )
}

#[doc(hidden)]
pub fn iter_method_in(
    receiver: &Value,
    method: &str,
    args: &[Value],
    line: u32,
    memory: &MemoryContext,
) -> Result<(Value, Option<Value>), NyblError> {
    use crate::builtins::{expect_args, make_iter_done_in, make_iter_next_in};
    let cell = match receiver {
        Value::Iter(cell) => cell,
        _ => unreachable!("iter_method called on non-iterator receiver"),
    };
    match method {
        "next" => {
            expect_args("next", args, 0, line)?;
            let mut inner = cell.borrow_mut();
            match inner.__next_in(memory) {
                Some(v) => Ok((make_iter_next_in(v, line, memory)?, None)),
                None => Ok((make_iter_done_in(memory), None)),
            }
        }
        "iter" => {
            expect_args("iter", args, 0, line)?;
            // Iterators are their own iterator — clone the Rc
            // so callers share the cursor.
            Ok((receiver.clone(), None))
        }
        _ => Err(error(
            line,
            crate::error_messages::no_such_method("iter", method),
        )),
    }
}

#[cfg(test)]
mod memory_preflight_tests {
    use super::*;

    fn assert_memory_limit(error: NyblError) {
        assert!(error.is_fatal);
        assert_eq!(error.message, "Memory limit exceeded");
    }

    #[test]
    fn amplified_join_fails_without_charging_a_partial_result() {
        let memory = MemoryContext::__new(1_200);
        let shared = Value::__new_str_in("x".repeat(256), &memory);
        let values = vec![shared; 8];
        let separator = Value::__new_str_in(String::new(), &memory);
        let baseline = memory.__used();

        let error = array_method_in(&values, "join", &[separator], 3, &memory).unwrap_err();

        assert_memory_limit(error);
        assert_eq!(memory.__used(), baseline);
    }

    #[test]
    fn amplified_replace_fails_without_charging_a_partial_result() {
        let memory = MemoryContext::__new(1_200);
        let input = Value::__new_str_in("x".repeat(256), &memory);
        let old = Value::__new_str_in(String::from("x"), &memory);
        let new = Value::__new_str_in(String::from("abcdefgh"), &memory);
        let args = [old, new];
        let baseline = memory.__used();
        let Value::Str(input) = &input else {
            unreachable!("constructed a string")
        };

        let error = string_method_in(input.as_str(), "replace", &args, 4, &memory).unwrap_err();

        assert_memory_limit(error);
        assert_eq!(memory.__used(), baseline);
    }

    #[test]
    fn high_cardinality_split_fails_without_charging_partial_parts() {
        let memory = MemoryContext::__new(1_200);
        let input = Value::__new_str_in("x,".repeat(128), &memory);
        let separator = Value::__new_str_in(String::from(","), &memory);
        let args = [separator];
        let baseline = memory.__used();
        let Value::Str(input) = &input else {
            unreachable!("constructed a string")
        };

        let error = string_method_in(input.as_str(), "split", &args, 5, &memory).unwrap_err();

        assert_memory_limit(error);
        assert_eq!(memory.__used(), baseline);
    }
}
