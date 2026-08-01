//! Shared `ref`-parameter mode checks and diagnostics.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{format, string::String, vec};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::{format, string::String, vec};

use crate::error::NyblError;
use crate::parser::ParamMode;

/// Number of positional arguments required before an optional final rest
/// parameter begins collecting values.
pub fn required_arity(modes: &[ParamMode]) -> usize {
    modes.len() - usize::from(matches!(modes.last(), Some(ParamMode::Rest)))
}

/// Whether an actual positional count satisfies fixed or final-rest metadata.
pub fn accepts_arity(modes: &[ParamMode], actual: usize) -> bool {
    let required = required_arity(modes);
    if matches!(modes.last(), Some(ParamMode::Rest)) {
        actual >= required
    } else {
        actual == required
    }
}

/// Validate engine-provided parameter metadata at a public trust boundary.
pub fn validate_parameter_modes(modes: &[ParamMode], line: u32) -> Result<(), NyblError> {
    if modes
        .iter()
        .enumerate()
        .any(|(index, mode)| *mode == ParamMode::Rest && index + 1 != modes.len())
    {
        return Err(NyblError::runtime(
            "Function rest parameter metadata must be final",
            line,
        ));
    }
    Ok(())
}

fn with_hint(mut error: NyblError, hint: impl Into<String>) -> NyblError {
    error.friendly_hint = Some(hint.into());
    error
}

/// Validate explicit argument modes against a callable's retained metadata.
/// Arity stays with the engine so existing callable-specific wording remains.
pub fn validate_call_modes(
    callable: &str,
    expected: &[ParamMode],
    actual: &[ParamMode],
    line: u32,
) -> Result<(), NyblError> {
    let fixed = required_arity(expected);
    for (index, actual) in actual.iter().enumerate() {
        let expected = if index < fixed {
            &expected[index]
        } else if matches!(expected.last(), Some(ParamMode::Rest)) {
            &ParamMode::Value
        } else {
            break;
        };
        if expected == actual {
            continue;
        }
        let position = index + 1;
        return Err(match (expected, actual) {
            (ParamMode::Ref, ParamMode::Value) => with_hint(
                NyblError::runtime(
                    format!("argument {position} to `{callable}` must be passed with `ref`"),
                    line,
                ),
                format!("Write `ref` before argument {position}."),
            ),
            (ParamMode::Value, ParamMode::Ref) => with_hint(
                NyblError::runtime(
                    format!(
                        "argument {position} to `{callable}` is a value parameter and can't use `ref`"
                    ),
                    line,
                ),
                format!("Remove `ref` from argument {position}."),
            ),
            (ParamMode::Rest, _) | (_, ParamMode::Rest) => {
                unreachable!("rest is declaration-only and normalized to value mode")
            }
            _ => unreachable!("equal modes were handled above"),
        });
    }
    Ok(())
}

/// Built-in and host calls are value-only in the initial feature.
pub fn validate_value_only_call_modes(
    callable: &str,
    actual: &[ParamMode],
    line: u32,
) -> Result<(), NyblError> {
    let expected = vec![ParamMode::Value; actual.len()];
    validate_call_modes(callable, &expected, actual, line)
}

pub fn invalid_ref_target(position: usize, line: u32) -> NyblError {
    with_hint(
        NyblError::runtime(
            format!("`ref` argument {position} must name a mutable variable"),
            line,
        ),
        "Assign the value to a `let` variable, then pass that variable with `ref`.",
    )
}

pub fn duplicate_ref_target(line: u32) -> NyblError {
    with_hint(
        NyblError::runtime(
            "the same variable can't be passed to more than one `ref` parameter",
            line,
        ),
        "Use a distinct variable for each `ref` argument.",
    )
}

pub fn captured_ref_target(position: usize, line: u32) -> NyblError {
    with_hint(
        NyblError::runtime(
            format!("`ref` argument {position} can't target a closure-captured binding"),
            line,
        ),
        "Pass the binding through an explicit `ref` parameter instead.",
    )
}

pub fn ref_capture_error(line: u32) -> NyblError {
    with_hint(
        NyblError::runtime("a `ref` parameter can't be captured by a closure", line),
        "Pass it through an explicit `ref` parameter instead.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_errors_are_actionable_and_one_based() {
        let missing = validate_call_modes(
            "swap",
            &[ParamMode::Value, ParamMode::Ref],
            &[ParamMode::Value, ParamMode::Value],
            7,
        )
        .unwrap_err();
        assert_eq!(
            missing.message,
            "argument 2 to `swap` must be passed with `ref`"
        );
        assert_eq!(
            missing.friendly_hint.as_deref(),
            Some("Write `ref` before argument 2.")
        );

        let extra = validate_value_only_call_modes("print", &[ParamMode::Ref], 8).unwrap_err();
        assert_eq!(
            extra.message,
            "argument 1 to `print` is a value parameter and can't use `ref`"
        );
        assert_eq!(
            extra.friendly_hint.as_deref(),
            Some("Remove `ref` from argument 1.")
        );
    }
}
