#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{collections::BTreeMap, format, string::String, vec::Vec};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::{collections::BTreeMap, format, string::String, vec::Vec};

use crate::value::BUILTIN_MODULE_PATH;
use crate::{Value, ValueConversionError};

use super::IntoValue;

impl From<()> for Value {
    fn from(_: ()) -> Self {
        Value::None
    }
}

macro_rules! impl_lossless_int_from {
    ($($type:ty),+ $(,)?) => {
        $(
            impl From<$type> for Value {
                fn from(value: $type) -> Self {
                    Value::Int(value as i64)
                }
            }
        )+
    };
}

impl_lossless_int_from!(i8, i16, i32, i64, isize, u8, u16, u32);

impl From<f32> for Value {
    fn from(value: f32) -> Self {
        Value::Number(f64::from(value))
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Value::Number(value)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::new_str(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::new_str(value.into())
    }
}

impl From<&String> for Value {
    fn from(value: &String) -> Self {
        Value::from(value.as_str())
    }
}

impl<T> From<Option<T>> for Value
where
    T: Into<Value>,
{
    fn from(value: Option<T>) -> Self {
        match value {
            Some(value) => value.into(),
            None => Value::None,
        }
    }
}

macro_rules! impl_infallible_into_value {
    ($($type:ty),+ $(,)?) => {
        $(
            impl IntoValue for $type {
                fn into_value(self) -> Result<Value, ValueConversionError> {
                    Ok(Value::from(self))
                }
            }
        )+
    };
}

impl_infallible_into_value!(
    (),
    i8,
    i16,
    i32,
    i64,
    isize,
    u8,
    u16,
    u32,
    f32,
    f64,
    bool,
    String
);

impl IntoValue for &str {
    fn into_value(self) -> Result<Value, ValueConversionError> {
        Ok(Value::from(self))
    }
}

impl IntoValue for &String {
    fn into_value(self) -> Result<Value, ValueConversionError> {
        Ok(Value::from(self))
    }
}

impl IntoValue for Value {
    fn into_value(self) -> Result<Value, ValueConversionError> {
        Ok(self)
    }
}

impl IntoValue for &Value {
    fn into_value(self) -> Result<Value, ValueConversionError> {
        Ok(self.clone())
    }
}

macro_rules! impl_checked_int_into_value {
    ($($type:ty),+ $(,)?) => {
        $(
            impl IntoValue for $type {
                fn into_value(self) -> Result<Value, ValueConversionError> {
                    i64::try_from(self).map(Value::Int).map_err(|_| {
                        ValueConversionError::new(
                            "an integer in Nybl's i64 range",
                            format!("integer {self}"),
                        )
                    })
                }
            }
        )+
    };
}

impl_checked_int_into_value!(i128, u64, u128, usize);

impl<T> IntoValue for Vec<T>
where
    T: IntoValue,
{
    fn into_value(self) -> Result<Value, ValueConversionError> {
        array_from_results(self.into_iter().map(IntoValue::into_value))
    }
}

impl<T, const N: usize> IntoValue for [T; N]
where
    T: IntoValue,
{
    fn into_value(self) -> Result<Value, ValueConversionError> {
        array_from_results(self.into_iter().map(IntoValue::into_value))
    }
}

impl<T> IntoValue for Option<T>
where
    T: IntoValue,
{
    fn into_value(self) -> Result<Value, ValueConversionError> {
        match self {
            Some(value) => value.into_value(),
            None => Ok(Value::None),
        }
    }
}

impl<T, E> IntoValue for core::result::Result<T, E>
where
    T: IntoValue,
    E: IntoValue,
{
    fn into_value(self) -> Result<Value, ValueConversionError> {
        let (variant, value) = match self {
            Ok(value) => (
                "Ok",
                value
                    .into_value()
                    .map_err(|error| error.at_result_variant("Ok"))?,
            ),
            Err(error) => (
                "Err",
                error
                    .into_value()
                    .map_err(|error| error.at_result_variant("Err"))?,
            ),
        };
        Value::try_new_enum_tuple(
            String::from(BUILTIN_MODULE_PATH),
            String::from("Result"),
            String::from(variant),
            Vec::from([value]),
            0,
        )
        .map_err(ValueConversionError::construction)
        .map_err(|error| error.at_result_variant(variant))
    }
}

impl<K, V> IntoValue for BTreeMap<K, V>
where
    K: Ord + Into<String>,
    V: IntoValue,
{
    fn into_value(self) -> Result<Value, ValueConversionError> {
        let entries = self
            .into_iter()
            .map(|(key, value)| (key.into(), value.into_value()));
        dict_from_results(entries)
    }
}

pub(super) fn array_from_results<I>(values: I) -> Result<Value, ValueConversionError>
where
    I: IntoIterator<Item = Result<Value, ValueConversionError>>,
{
    let values = values
        .into_iter()
        .enumerate()
        .map(|(index, value)| value.map_err(|error| error.at_index(index)))
        .collect::<Result<Vec<_>, _>>()?;
    Value::try_new_array(values, 0).map_err(ValueConversionError::construction)
}

pub(super) fn dict_from_results<I, K>(entries: I) -> Result<Value, ValueConversionError>
where
    I: IntoIterator<Item = (K, Result<Value, ValueConversionError>)>,
    K: Into<String>,
{
    let mut values = Vec::new();
    for (key, value) in entries {
        let key = key.into();
        if values.iter().any(|(existing, _)| existing == &key) {
            return Err(ValueConversionError::new(
                "a dict with unique keys",
                format!("duplicate key {key:?}"),
            )
            .at_key(key));
        }
        let value = value.map_err(|error| error.at_key(key.clone()))?;
        values.push((key, value));
    }
    Value::try_new_dict(values, 0).map_err(ValueConversionError::construction)
}

#[cfg(all(test, any(feature = "std", not(feature = "no_std"))))]
mod tests {
    use crate::memory::{nybl_memory_init, nybl_memory_used};

    use super::*;

    #[test]
    fn converted_containers_keep_tracked_cow_allocation_semantics() {
        nybl_memory_init(usize::MAX);

        let value = vec![1_i64, 2, 3].into_value().unwrap();
        let used = nybl_memory_used();
        assert!(used > 0);

        let shared = value.clone();
        assert_eq!(nybl_memory_used(), used, "Value clone must remain O(1)");
        drop(value);
        assert_eq!(nybl_memory_used(), used);
        drop(shared);
        assert_eq!(nybl_memory_used(), 0);
    }
}
