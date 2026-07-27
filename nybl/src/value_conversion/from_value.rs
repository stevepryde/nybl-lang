#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{collections::BTreeMap, format, string::String, vec::Vec};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::{collections::BTreeMap, format, string::String, vec::Vec};

use crate::value::{BUILTIN_MODULE_PATH, EnumPayload};
use crate::{Value, ValueConversionError};

use super::FromValue;

impl<'value> FromValue<'value> for &'value Value {
    fn from_value(value: &'value Value) -> Result<Self, ValueConversionError> {
        Ok(value)
    }
}

impl FromValue<'_> for Value {
    fn from_value(value: &Value) -> Result<Self, ValueConversionError> {
        Ok(value.clone())
    }
}

impl FromValue<'_> for () {
    fn from_value(value: &Value) -> Result<Self, ValueConversionError> {
        match value {
            Value::None => Ok(()),
            other => Err(ValueConversionError::type_mismatch("none", other)),
        }
    }
}

impl FromValue<'_> for i64 {
    fn from_value(value: &Value) -> Result<Self, ValueConversionError> {
        match value {
            Value::Int(value) => Ok(*value),
            other => Err(ValueConversionError::type_mismatch("int", other)),
        }
    }
}

macro_rules! impl_checked_int_from_value {
    ($($type:ty),+ $(,)?) => {
        $(
            impl FromValue<'_> for $type {
                fn from_value(value: &Value) -> Result<Self, ValueConversionError> {
                    let value = i64::from_value(value)?;
                    <$type>::try_from(value).map_err(|_| {
                        ValueConversionError::new(
                            concat!("int in Rust `", stringify!($type), "` range"),
                            format!("integer {value}"),
                        )
                    })
                }
            }
        )+
    };
}

impl_checked_int_from_value!(i8, i16, i32, isize, u8, u16, u32, u64, usize);

impl FromValue<'_> for i128 {
    fn from_value(value: &Value) -> Result<Self, ValueConversionError> {
        i64::from_value(value).map(i128::from)
    }
}

impl FromValue<'_> for u128 {
    fn from_value(value: &Value) -> Result<Self, ValueConversionError> {
        let value = i64::from_value(value)?;
        u128::try_from(value).map_err(|_| {
            ValueConversionError::new("a non-negative int", format!("integer {value}"))
        })
    }
}

impl FromValue<'_> for f64 {
    fn from_value(value: &Value) -> Result<Self, ValueConversionError> {
        match value {
            Value::Number(value) => Ok(*value),
            other => Err(ValueConversionError::type_mismatch("number", other)),
        }
    }
}

impl FromValue<'_> for f32 {
    fn from_value(value: &Value) -> Result<Self, ValueConversionError> {
        let value = f64::from_value(value)?;
        let finite_f32_range = f64::from(f32::MIN)..=f64::from(f32::MAX);
        if value.is_finite() && !finite_f32_range.contains(&value) {
            return Err(ValueConversionError::new(
                "number representable in Rust `f32` without overflow",
                format!("number {value}"),
            ));
        }
        Ok(value as f32)
    }
}

impl FromValue<'_> for bool {
    fn from_value(value: &Value) -> Result<Self, ValueConversionError> {
        match value {
            Value::Bool(value) => Ok(*value),
            other => Err(ValueConversionError::type_mismatch("bool", other)),
        }
    }
}

impl<'value> FromValue<'value> for &'value str {
    fn from_value(value: &'value Value) -> Result<Self, ValueConversionError> {
        match value {
            Value::Str(value) => Ok(value.as_str()),
            other => Err(ValueConversionError::type_mismatch("string", other)),
        }
    }
}

impl FromValue<'_> for String {
    fn from_value(value: &Value) -> Result<Self, ValueConversionError> {
        <&str>::from_value(value).map(String::from)
    }
}

impl<'value, T> FromValue<'value> for Vec<T>
where
    T: FromValue<'value>,
{
    fn from_value(value: &'value Value) -> Result<Self, ValueConversionError> {
        let values = match value {
            Value::Array(values) => values,
            other => return Err(ValueConversionError::type_mismatch("array", other)),
        };
        values
            .iter()
            .enumerate()
            .map(|(index, value)| T::from_value(value).map_err(|error| error.at_index(index)))
            .collect()
    }
}

impl<'value, T> FromValue<'value> for Option<T>
where
    T: FromValue<'value>,
{
    fn from_value(value: &'value Value) -> Result<Self, ValueConversionError> {
        match value {
            Value::None => Ok(None),
            other => T::from_value(other).map(Some),
        }
    }
}

impl<'value, T, E> FromValue<'value> for core::result::Result<T, E>
where
    T: FromValue<'value>,
    E: FromValue<'value>,
{
    fn from_value(value: &'value Value) -> Result<Self, ValueConversionError> {
        let variant = match value {
            Value::EnumVariant(variant)
                if variant.module_path() == BUILTIN_MODULE_PATH
                    && variant.type_name() == "Result" =>
            {
                variant
            }
            other => {
                return Err(ValueConversionError::type_mismatch(
                    "built-in Result::Ok(value) or Result::Err(error)",
                    other,
                ));
            }
        };

        let (name, payload) = match (variant.variant(), variant.payload()) {
            (name @ ("Ok" | "Err"), EnumPayload::Tuple(values)) if values.len() == 1 => {
                (name, &values[0])
            }
            (name @ ("Ok" | "Err"), payload) => {
                return Err(ValueConversionError::new(
                    format!("built-in Result::{name} with one positional value"),
                    result_payload_description(name, payload),
                )
                .at_result_variant(name));
            }
            (name, _) => {
                return Err(ValueConversionError::new(
                    "built-in Result::Ok(value) or Result::Err(error)",
                    format!("built-in Result::{name}"),
                ));
            }
        };

        match name {
            "Ok" => T::from_value(payload)
                .map(Ok)
                .map_err(|error| error.at_result_variant("Ok")),
            "Err" => E::from_value(payload)
                .map(Err)
                .map_err(|error| error.at_result_variant("Err")),
            _ => unreachable!("validated built-in Result variant"),
        }
    }
}

impl<'value, T> FromValue<'value> for BTreeMap<String, T>
where
    T: FromValue<'value>,
{
    fn from_value(value: &'value Value) -> Result<Self, ValueConversionError> {
        let entries = match value {
            Value::Dict(entries) => entries,
            other => return Err(ValueConversionError::type_mismatch("dict", other)),
        };
        let mut output = BTreeMap::new();
        for (key, value) in entries.iter() {
            if output.contains_key(key) {
                return Err(ValueConversionError::new(
                    "a dict with unique keys",
                    format!("duplicate key {key:?}"),
                )
                .at_key(key.clone()));
            }
            let value = T::from_value(value).map_err(|error| error.at_key(key.clone()))?;
            output.insert(key.clone(), value);
        }
        Ok(output)
    }
}

fn result_payload_description(name: &str, payload: &EnumPayload) -> String {
    match payload {
        EnumPayload::Unit => format!("built-in Result::{name} with unit payload"),
        EnumPayload::Tuple(values) => format!(
            "built-in Result::{name} with {} positional values",
            values.len()
        ),
        EnumPayload::Struct(fields) => {
            format!("built-in Result::{name} with {} named fields", fields.len())
        }
    }
}
