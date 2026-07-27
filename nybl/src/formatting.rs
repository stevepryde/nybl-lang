//! Fallible formatting and string materialization under a Nybl memory budget.
//!
//! The runtime uses two passes for amplified formatting: first count the exact
//! UTF-8 bytes without allocating, then reserve and stream into one buffer.
//! This keeps shared/COW values from expanding into untracked temporary
//! `String`s before the configured memory limit is known to permit the result.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{string::String, vec::Vec};

use core::fmt::{self, Write};

use crate::builtins::error_fatal_with_hint;
use crate::error::NyblError;
use crate::memory::MemoryContext;
use crate::value::Value;

const MEMORY_LIMIT_HINT: &str =
    "Your code is using too much memory. Check for large strings or arrays growing in loops.";

fn memory_limit_error(line: u32) -> NyblError {
    error_fatal_with_hint(line, "Memory limit exceeded", MEMORY_LIMIT_HINT)
}

fn checked_add(left: usize, right: usize, line: u32) -> Result<usize, NyblError> {
    left.checked_add(right)
        .ok_or_else(|| memory_limit_error(line))
}

#[doc(hidden)]
pub fn __preflight_in(
    additional_bytes: usize,
    line: u32,
    memory: &MemoryContext,
) -> Result<(), NyblError> {
    if memory.__would_exceed(additional_bytes) {
        Err(memory_limit_error(line))
    } else {
        Ok(())
    }
}

struct CountingWriter {
    bytes: usize,
}

impl CountingWriter {
    fn new() -> Self {
        Self { bytes: 0 }
    }
}

impl fmt::Write for CountingWriter {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.bytes = self.bytes.checked_add(value.len()).ok_or(fmt::Error)?;
        Ok(())
    }
}

fn try_string_with_capacity(
    capacity: usize,
    line: u32,
    memory: &MemoryContext,
) -> Result<String, NyblError> {
    __preflight_in(capacity, line, memory)?;
    let mut output = String::new();
    output
        .try_reserve_exact(capacity)
        .map_err(|_| memory_limit_error(line))?;
    // An allocator may grant more than the requested capacity. Validate the
    // actual reservation while the buffer is still empty.
    __preflight_in(output.capacity(), line, memory)?;
    Ok(output)
}

/// Format values exactly as Nybl's built-in `print` and Array `.join()` do,
/// without first building a `Vec<String>`.
#[doc(hidden)]
pub fn __format_values_in(
    values: &[Value],
    separator: &str,
    line: u32,
    memory: &MemoryContext,
) -> Result<String, NyblError> {
    let mut counter = CountingWriter::new();
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            counter
                .write_str(separator)
                .map_err(|_| memory_limit_error(line))?;
        }
        write!(&mut counter, "{value}").map_err(|_| memory_limit_error(line))?;
    }

    let mut output = try_string_with_capacity(counter.bytes, line, memory)?;
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.push_str(separator);
        }
        write!(&mut output, "{value}").map_err(|_| memory_limit_error(line))?;
    }
    debug_assert_eq!(output.len(), counter.bytes);
    Ok(output)
}

fn checked_replacement_len(
    input_len: usize,
    match_count: usize,
    old_len: usize,
    new_len: usize,
) -> Option<usize> {
    let removed = match_count.checked_mul(old_len)?;
    let retained = input_len.checked_sub(removed)?;
    retained.checked_add(match_count.checked_mul(new_len)?)
}

/// Replace non-overlapping matches after checking the exact output length.
#[doc(hidden)]
pub fn __replace_in(
    input: &str,
    old: &str,
    new: &str,
    line: u32,
    memory: &MemoryContext,
) -> Result<String, NyblError> {
    let match_count = input.match_indices(old).count();
    let output_len = checked_replacement_len(input.len(), match_count, old.len(), new.len())
        .ok_or_else(|| memory_limit_error(line))?;
    let mut output = try_string_with_capacity(output_len, line, memory)?;

    let mut copied_through = 0;
    for (index, _) in input.match_indices(old) {
        output.push_str(&input[copied_through..index]);
        output.push_str(new);
        copied_through = index + old.len();
    }
    output.push_str(&input[copied_through..]);
    debug_assert_eq!(output.len(), output_len);
    Ok(output)
}

fn ensure_within_starting_budget(
    available: Option<usize>,
    bytes: usize,
    line: u32,
) -> Result<(), NyblError> {
    if available.is_some_and(|available| bytes > available) {
        Err(memory_limit_error(line))
    } else {
        Ok(())
    }
}

/// Materialize String `.split()` directly into tracked values after checking
/// both substring bytes and flat-array storage/cardinality.
#[doc(hidden)]
pub fn __split_values_in(
    input: &str,
    separator: &str,
    line: u32,
    memory: &MemoryContext,
) -> Result<Vec<Value>, NyblError> {
    let mut part_count = 0usize;
    let mut payload_bytes = 0usize;
    for part in input.split(separator) {
        part_count = checked_add(part_count, 1, line)?;
        payload_bytes = checked_add(payload_bytes, part.len(), line)?;
    }

    let requested_array_bytes = Value::__flat_array_tracked_bytes(part_count, part_count)
        .ok_or_else(|| memory_limit_error(line))?;
    let requested_total = checked_add(payload_bytes, requested_array_bytes, line)?;
    __preflight_in(requested_total, line, memory)?;

    let available = memory.__available();
    let mut parts = Vec::new();
    parts
        .try_reserve_exact(part_count)
        .map_err(|_| memory_limit_error(line))?;
    let actual_array_bytes = Value::__flat_array_tracked_bytes(parts.capacity(), part_count)
        .ok_or_else(|| memory_limit_error(line))?;
    ensure_within_starting_budget(
        available,
        checked_add(payload_bytes, actual_array_bytes, line)?,
        line,
    )?;

    let mut actual_string_bytes = 0usize;
    for part in input.split(separator) {
        let mut text = String::new();
        text.try_reserve_exact(part.len())
            .map_err(|_| memory_limit_error(line))?;
        actual_string_bytes = checked_add(actual_string_bytes, text.capacity(), line)?;
        ensure_within_starting_budget(
            available,
            checked_add(actual_string_bytes, actual_array_bytes, line)?,
            line,
        )?;
        text.push_str(part);
        parts.push(Value::__new_str_in(text, memory));
    }

    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FragmentWriter {
        bytes: usize,
        max_fragment: usize,
    }

    impl fmt::Write for FragmentWriter {
        fn write_str(&mut self, value: &str) -> fmt::Result {
            self.bytes += value.len();
            self.max_fragment = self.max_fragment.max(value.len());
            Ok(())
        }
    }

    #[test]
    fn replacement_length_handles_empty_matches_and_overflow() {
        assert_eq!(checked_replacement_len(2, 3, 0, 1), Some(5));
        assert_eq!(checked_replacement_len(usize::MAX, 1, 0, 1), None);
        assert_eq!(checked_replacement_len(1, usize::MAX, 1, 2), None);
        assert!(Value::__flat_array_tracked_bytes(usize::MAX, usize::MAX).is_none());

        let mut counter = CountingWriter { bytes: usize::MAX };
        assert!(counter.write_str(" ").is_err());
    }

    #[test]
    fn empty_separator_split_cardinality_is_preflighted() {
        let memory = MemoryContext::__new(1);
        let error = __split_values_in("abc", "", 7, &memory).unwrap_err();
        assert!(error.is_fatal);
        assert_eq!(error.line, Some(7));
        assert_eq!(error.message, "Memory limit exceeded");
        assert_eq!(memory.__used(), 0);
    }

    #[test]
    fn empty_pattern_replace_matches_std_semantics() {
        let memory = MemoryContext::__new(1024);
        assert_eq!(
            __replace_in("ab", "", "-", 1, &memory).unwrap(),
            "ab".replace("", "-")
        );
    }

    #[test]
    fn bounded_formatting_preserves_nested_display_semantics() {
        let memory = MemoryContext::__new(1024);
        let nested = Value::__try_new_array_in(
            vec![Value::__new_str_in(String::from("x"), &memory)],
            1,
            &memory,
        )
        .unwrap();

        assert_eq!(
            __format_values_in(&[nested.clone(), nested], " ", 1, &memory).unwrap(),
            r#"["x"] ["x"]"#
        );
    }

    #[test]
    fn nested_shared_dag_streams_children_before_top_level_preflight() {
        let memory = MemoryContext::__new(2_000);
        let shared = Value::__new_str_in("x".repeat(128), &memory);
        let leaf = Value::__try_new_array_in(vec![shared; 4], 1, &memory).unwrap();
        let branch = Value::__try_new_array_in(vec![leaf; 4], 1, &memory).unwrap();
        let root = Value::__try_new_array_in(vec![branch; 4], 1, &memory).unwrap();
        let baseline = memory.__used();

        let mut fragments = FragmentWriter {
            bytes: 0,
            max_fragment: 0,
        };
        write!(&mut fragments, "{root}").unwrap();
        assert!(fragments.bytes > 8_000);
        assert_eq!(
            fragments.max_fragment, 128,
            "a nested child was materialized before reaching the writer"
        );

        let values = [root];
        let error = __format_values_in(&values, " ", 9, &memory).unwrap_err();
        assert!(error.is_fatal);
        assert_eq!(error.message, "Memory limit exceeded");
        assert_eq!(memory.__used(), baseline);
    }
}
