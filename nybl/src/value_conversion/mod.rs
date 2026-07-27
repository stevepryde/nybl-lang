//! Rust ↔ [`Value`] conversions for embedders.
//!
//! Scalar Rust values that map to Nybl without validation implement standard
//! [`From`]. Recursive values use [`IntoValue`] because constructing a Nybl
//! array, dict, or `Result` can exceed the runtime's value-depth invariant.
//! Reverse conversion is borrowed-first: [`FromValue`] accepts `&Value`, which
//! matches the `&[Value]` received by [`crate::NyblHost`] and permits zero-copy
//! extraction such as `&str`.
//!
//! `Number` to `f32` extraction uses range-checked IEEE-754 narrowing: finite
//! values outside `f32`'s finite range return [`ValueConversionError`] instead
//! of becoming infinity. In-range values may round to the nearest `f32`, while
//! `NaN` and positive or negative infinity are preserved.
//!
//! ```
//! use std::collections::BTreeMap;
//! use nybl::{FromValue, IntoValue, Value, nybl_value};
//!
//! # fn example() -> Result<(), nybl::ValueConversionError> {
//! let request = nybl_value!({
//!     "name": "Ada",
//!     "scores": [10, 20, 30],
//!     "active": true,
//! })?;
//!
//! let fields: BTreeMap<String, Value> = request.to_rust()?;
//! let name: &str = fields["name"].to_rust()?;
//! let scores: Vec<i64> = FromValue::from_value(&fields["scores"])?;
//! assert_eq!(name, "Ada");
//! assert_eq!(scores, vec![10, 20, 30]);
//!
//! let response = Ok::<_, &str>(scores).into_value()?;
//! let round_trip: Result<Vec<i64>, &str> = response.to_rust()?;
//! assert_eq!(round_trip.unwrap(), vec![10, 20, 30]);
//! # Ok(())
//! # }
//! # example().unwrap();
//! ```

mod error;
mod from_value;
mod into_value;
mod macros;

pub use error::{ValueConversionError, ValuePathSegment};

use crate::Value;

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::string::String;
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::string::String;

/// Fallibly convert an owned Rust value into a Nybl [`Value`].
///
/// This local trait deliberately has explicit implementations instead of a
/// blanket `T: Into<Value>` implementation. That keeps generic `Vec`,
/// `Option`, `Result`, and map conversions coherent and lets downstream crates
/// implement `IntoValue` for their own local types.
pub trait IntoValue {
    fn into_value(self) -> Result<Value, ValueConversionError>;
}

/// Extract a Rust value from a borrowed Nybl [`Value`].
///
/// The lifetime permits zero-copy targets (`&str`, `&Value`) while the same
/// trait also supports owned targets such as `String`, `Vec<T>`, and
/// `BTreeMap<String, T>`.
pub trait FromValue<'value>: Sized {
    fn from_value(value: &'value Value) -> Result<Self, ValueConversionError>;
}

impl Value {
    /// Extract a Rust representation through [`FromValue`].
    ///
    /// ```
    /// use nybl::Value;
    ///
    /// let value = Value::from("hello");
    /// let borrowed: &str = value.to_rust().unwrap();
    /// let owned: String = value.to_rust().unwrap();
    /// assert_eq!(borrowed, owned);
    /// ```
    pub fn to_rust<'value, T>(&'value self) -> Result<T, ValueConversionError>
    where
        T: FromValue<'value>,
    {
        T::from_value(self)
    }
}

#[doc(hidden)]
pub fn __array_from_results<I>(values: I) -> Result<Value, ValueConversionError>
where
    I: IntoIterator<Item = Result<Value, ValueConversionError>>,
{
    into_value::array_from_results(values)
}

#[doc(hidden)]
pub fn __dict_from_results<I, K>(entries: I) -> Result<Value, ValueConversionError>
where
    I: IntoIterator<Item = (K, Result<Value, ValueConversionError>)>,
    K: Into<String>,
{
    into_value::dict_from_results(entries)
}
