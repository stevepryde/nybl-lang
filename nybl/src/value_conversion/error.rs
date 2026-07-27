#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{format, string::String, vec::Vec};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::{format, string::String, vec::Vec};

use core::fmt;

use crate::{NyblError, Value};

/// One location component inside a nested Rust ↔ Nybl conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValuePathSegment {
    /// Array position, rendered as `[index]`.
    Index(usize),
    /// Dict key, rendered as `["key"]`.
    Key(String),
    /// Canonical built-in Result payload, rendered as `<Ok>` or `<Err>`.
    ResultVariant(String),
}

/// A precise Rust ↔ Nybl conversion failure.
///
/// `expected` and `actual` describe the failed boundary. `path` is empty at
/// the root and accumulates array indices, dict keys, and Result variants as a
/// nested conversion unwinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueConversionError {
    expected: String,
    actual: String,
    path: Vec<ValuePathSegment>,
}

impl ValueConversionError {
    /// Create a conversion error at the root value.
    ///
    /// Custom [`IntoValue`](crate::IntoValue) and
    /// [`FromValue`](crate::FromValue) implementations can use this to retain
    /// the same expected/actual diagnostic shape as the built-in conversions.
    pub fn new(expected: impl Into<String>, actual: impl Into<String>) -> Self {
        Self {
            expected: expected.into(),
            actual: actual.into(),
            path: Vec::new(),
        }
    }

    pub(crate) fn type_mismatch(expected: impl Into<String>, value: &Value) -> Self {
        Self::new(expected, describe_value(value))
    }

    pub(crate) fn construction(error: NyblError) -> Self {
        Self::new("a Nybl value within runtime limits", error.message)
    }

    pub(crate) fn at_index(mut self, index: usize) -> Self {
        self.path.insert(0, ValuePathSegment::Index(index));
        self
    }

    pub(crate) fn at_key(mut self, key: impl Into<String>) -> Self {
        self.path.insert(0, ValuePathSegment::Key(key.into()));
        self
    }

    pub(crate) fn at_result_variant(mut self, variant: impl Into<String>) -> Self {
        self.path
            .insert(0, ValuePathSegment::ResultVariant(variant.into()));
        self
    }

    /// Prepend one location to the structured root-to-leaf error path.
    ///
    /// This is useful when a custom conversion delegates to another
    /// conversion for one of its fields or elements.
    pub fn at_path(mut self, segment: ValuePathSegment) -> Self {
        self.path.insert(0, segment);
        self
    }

    /// Human-readable expected Rust/Nybl shape.
    pub fn expected(&self) -> &str {
        &self.expected
    }

    /// Human-readable value or shape actually encountered.
    pub fn actual(&self) -> &str {
        &self.actual
    }

    /// Structured path from the root value to the failing child.
    pub fn path(&self) -> &[ValuePathSegment] {
        &self.path
    }
}

impl fmt::Display for ValueConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "value conversion failed at $")?;
        for segment in &self.path {
            match segment {
                ValuePathSegment::Index(index) => write!(f, "[{index}]")?,
                ValuePathSegment::Key(key) => {
                    write!(f, "[\"")?;
                    for ch in key.chars() {
                        for escaped in ch.escape_default() {
                            write!(f, "{escaped}")?;
                        }
                    }
                    write!(f, "\"]")?;
                }
                ValuePathSegment::ResultVariant(variant) => write!(f, "<{variant}>")?,
            }
        }
        write!(f, ": expected {}, got {}", self.expected, self.actual)
    }
}

impl core::error::Error for ValueConversionError {}

pub(crate) fn describe_value(value: &Value) -> String {
    match value {
        Value::Struct(value) => format!("struct {}.{}", value.module_path(), value.type_name()),
        Value::EnumVariant(value) => format!(
            "enum {}.{}::{}",
            value.module_path(),
            value.type_name(),
            value.variant()
        ),
        other => other.type_name().into(),
    }
}
