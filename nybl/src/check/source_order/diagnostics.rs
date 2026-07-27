use super::{KnownEnum, LexicalEnv, TypeBinding};
use crate::error::NyblWarning;
use crate::parser::{MatchArm, Pattern};

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{format, string::String, vec::Vec};

pub(super) fn check_match_exhaustive(
    arms: &[MatchArm],
    env: &LexicalEnv,
    match_line: u32,
    warnings: &mut Vec<NyblWarning>,
) {
    for arm in arms {
        if arm.guard.is_none() && is_catch_all(&arm.pattern) {
            return;
        }
    }

    let mut target: Option<(KnownEnum, String)> = None;
    let mut covered = Vec::new();
    for arm in arms {
        if !gather_variants(
            &arm.pattern,
            env,
            &mut target,
            &mut covered,
            arm.guard.is_none(),
        ) {
            return;
        }
    }
    let Some((known, display_name)) = target else {
        return;
    };
    let missing: Vec<&str> = known
        .shape
        .variants
        .iter()
        .filter(|variant| !covered.iter().any(|name| name == &variant.name))
        .map(|variant| variant.name.as_str())
        .collect();
    if missing.is_empty() {
        return;
    }
    let list = missing.join(", ");
    let msg = format!(
        "non-exhaustive `match` on `{}`: missing {}",
        display_name,
        missing
            .iter()
            .map(|variant| format!("`{display_name}::{variant}`"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    let hint = format!("add an arm for each missing variant, or a `_` catch-all. Missing: {list}");
    warnings.push(NyblWarning::at(msg, match_line).with_hint(hint));
}

fn is_catch_all(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Wildcard | Pattern::Binding(_) => true,
        Pattern::Or(alternatives) => alternatives.iter().any(is_catch_all),
        _ => false,
    }
}

fn gather_variants(
    pattern: &Pattern,
    env: &LexicalEnv,
    target: &mut Option<(KnownEnum, String)>,
    covered: &mut Vec<String>,
    contributes: bool,
) -> bool {
    match pattern {
        Pattern::Wildcard | Pattern::Binding(_) => true,
        Pattern::EnumVariant {
            namespace,
            type_name,
            variant,
            ..
        } => {
            let binding = match namespace {
                Some(namespace) => env.resolve_namespace_type(namespace, type_name),
                None => env.resolve_type(type_name),
            };
            let Some(TypeBinding::Known(known)) = binding else {
                return false;
            };
            match target {
                None => *target = Some((known.clone(), type_name.clone())),
                Some((existing, _))
                    if existing.runtime_id == known.runtime_id && existing.shape == known.shape => {
                }
                _ => return false,
            }
            if contributes {
                covered.push(variant.clone());
            }
            true
        }
        Pattern::Or(alternatives) => alternatives
            .iter()
            .all(|alternative| gather_variants(alternative, env, target, covered, contributes)),
        _ => false,
    }
}
