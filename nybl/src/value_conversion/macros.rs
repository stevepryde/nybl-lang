/// Construct a tracked, depth-checked Nybl value with JSON-like syntax.
///
/// The macro returns `Result<Value, ValueConversionError>` so value-depth and
/// tracked-construction failures remain explicit. Dict keys must be string
/// literals; dict insertion order follows the source literal.
///
/// ```
/// use nybl::{Value, nybl_value};
///
/// let value = nybl_value!({
///     "name": "Ada",
///     "stats": { "hp": 100, "mp": 40 },
///     "tags": ["engineer", "mathematician"],
///     "nickname": none,
/// }).unwrap();
///
/// assert!(matches!(value, Value::Dict(_)));
/// ```
#[macro_export]
macro_rules! nybl_value {
    (none) => {
        ::core::result::Result::Ok::<$crate::Value, $crate::ValueConversionError>(
            $crate::Value::None,
        )
    };
    ([ $($tokens:tt)* ]) => {
        $crate::__nybl_value_array!([] $($tokens)*)
    };
    ({ $($tokens:tt)* }) => {
        $crate::__nybl_value_dict!([] $($tokens)*)
    };
    ($value:expr) => {
        $crate::IntoValue::into_value($value)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __nybl_value_array {
    ([$($values:expr,)*]) => {
        $crate::value_conversion::__array_from_results([$($values,)*])
    };

    ([$($values:expr,)*] none, $($rest:tt)*) => {
        $crate::__nybl_value_array!([$($values,)* $crate::nybl_value!(none),] $($rest)*)
    };
    ([$($values:expr,)*] [$($nested:tt)*], $($rest:tt)*) => {
        $crate::__nybl_value_array!([$($values,)* $crate::nybl_value!([$($nested)*]),] $($rest)*)
    };
    ([$($values:expr,)*] {$($nested:tt)*}, $($rest:tt)*) => {
        $crate::__nybl_value_array!([$($values,)* $crate::nybl_value!({$($nested)*}),] $($rest)*)
    };
    ([$($values:expr,)*] none) => {
        $crate::__nybl_value_array!([$($values,)* $crate::nybl_value!(none),])
    };
    ([$($values:expr,)*] [$($nested:tt)*]) => {
        $crate::__nybl_value_array!([$($values,)* $crate::nybl_value!([$($nested)*]),])
    };
    ([$($values:expr,)*] {$($nested:tt)*}) => {
        $crate::__nybl_value_array!([$($values,)* $crate::nybl_value!({$($nested)*}),])
    };
    ([$($values:expr,)*] $value:expr, $($rest:tt)*) => {
        $crate::__nybl_value_array!([$($values,)* $crate::nybl_value!($value),] $($rest)*)
    };
    ([$($values:expr,)*] $value:expr) => {
        $crate::__nybl_value_array!([$($values,)* $crate::nybl_value!($value),])
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __nybl_value_dict {
    ([$($entries:expr,)*]) => {
        $crate::value_conversion::__dict_from_results(
            [$($entries,)*] as [
                (&'static str, ::core::result::Result<$crate::Value, $crate::ValueConversionError>);
                $crate::__nybl_value_dict!(@count $($entries,)*)
            ]
        )
    };

    (@count) => { 0usize };
    (@count $head:expr, $($tail:expr,)*) => {
        1usize + $crate::__nybl_value_dict!(@count $($tail,)*)
    };

    ([$($entries:expr,)*] $key:literal : none, $($rest:tt)*) => {
        $crate::__nybl_value_dict!([$($entries,)* ($key, $crate::nybl_value!(none)),] $($rest)*)
    };
    ([$($entries:expr,)*] $key:literal : [$($nested:tt)*], $($rest:tt)*) => {
        $crate::__nybl_value_dict!([$($entries,)* ($key, $crate::nybl_value!([$($nested)*])),] $($rest)*)
    };
    ([$($entries:expr,)*] $key:literal : {$($nested:tt)*}, $($rest:tt)*) => {
        $crate::__nybl_value_dict!([$($entries,)* ($key, $crate::nybl_value!({$($nested)*})),] $($rest)*)
    };
    ([$($entries:expr,)*] $key:literal : none) => {
        $crate::__nybl_value_dict!([$($entries,)* ($key, $crate::nybl_value!(none)),])
    };
    ([$($entries:expr,)*] $key:literal : [$($nested:tt)*]) => {
        $crate::__nybl_value_dict!([$($entries,)* ($key, $crate::nybl_value!([$($nested)*])),])
    };
    ([$($entries:expr,)*] $key:literal : {$($nested:tt)*}) => {
        $crate::__nybl_value_dict!([$($entries,)* ($key, $crate::nybl_value!({$($nested)*})),])
    };
    ([$($entries:expr,)*] $key:literal : $value:expr, $($rest:tt)*) => {
        $crate::__nybl_value_dict!([$($entries,)* ($key, $crate::nybl_value!($value)),] $($rest)*)
    };
    ([$($entries:expr,)*] $key:literal : $value:expr) => {
        $crate::__nybl_value_dict!([$($entries,)* ($key, $crate::nybl_value!($value)),])
    };
}
