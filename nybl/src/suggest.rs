//! "Did you mean?" suggestions — used by error paths to turn
//! `Variable `lenght` not found` into a hint that points at
//! `length` (or `len`) when those exist in the surrounding
//! scope.
//!
//! Kept tiny and zero-dependency so it stays usable from
//! `no_std` builds of `nybl-lang`. The algorithm is a textbook
//! Wagner–Fischer Levenshtein with a per-target edit budget,
//! which is plenty for scope sizes we ever see in a Nybl program.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

/// Every core builtin callable by name, in a single canonical
/// list. Each engine (walker / VM / AOT) feeds this into
/// [`did_you_mean`] when a `Function \`foo\` not found` error
/// fires so a typo of `rang(5)` surfaces `range` regardless of
/// which engine produced the error. Host-provided builtins
/// stay out — they surface via each host's own
/// `function_hint()` so embedder-specific tips stay embedder-
/// owned.
pub const CORE_CALLABLE_BUILTINS: &[&str] = &["range", "rand", "print", "try_call", "panic"];

/// Methods every numeric receiver exposes. Produced as
/// candidates when a user writes `(some_int).xyz(...)` and the
/// name doesn't match — the "did you mean?" hint surfaces the
/// whole numeric surface in one shot.
pub const NUMERIC_METHODS: &[&str] = &[
    "abs", "sqrt", "sin", "cos", "tan", "floor", "ceil", "round", "exp", "log", "pow", "min",
    "max", "to_int", "to_float", // Shared methods every value supports:
    "type", "to_str", "inspect",
];

/// Find the closest match to `target` in `candidates`. Returns
/// `Some(candidate)` when one is within an acceptable edit
/// distance, or `None` when every candidate is too dissimilar
/// (so callers can skip adding a hint).
///
/// Budget rule: roughly 1 edit per 3 target characters, clamped
/// between 1 and 3 — long names get a bit more room, but a
/// 2-character target ("ln") never suggests a 5-character
/// match. Exact matches and empty candidates are skipped so a
/// suggestion is always a *different* valid name.
///
/// On ties, the first candidate seen with the shortest distance
/// wins — input order matters for deterministic output. Callers
/// that care (tests, diagnostic snapshots) should feed
/// candidates in a stable order.
pub fn closest_match<I, S>(target: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let target_len = target.chars().count();
    if target_len == 0 {
        return None;
    }
    let max_dist = (target_len / 3).clamp(1, 3);

    let mut best: Option<(usize, String)> = None;
    for cand in candidates {
        let cand_str = cand.as_ref();
        if cand_str.is_empty() || cand_str == target {
            continue;
        }
        // Cheap length-difference prune: if the candidate's
        // length differs from the target by more than our
        // budget, the full Levenshtein can't find an answer
        // small enough to consider.
        let cand_len = cand_str.chars().count();
        let len_diff = cand_len.abs_diff(target_len);
        if len_diff > max_dist {
            continue;
        }

        let d = levenshtein(target, cand_str);
        if d > max_dist {
            continue;
        }
        match &best {
            Some((best_d, _)) if d >= *best_d => {}
            _ => best = Some((d, cand_str.to_string())),
        }
    }
    best.map(|(_, s)| s)
}

/// Textbook Wagner–Fischer edit distance. Insertions, deletions,
/// and substitutions each cost 1. Public so `nybl-vm` / `nybl-sys`
/// can use it for their own error paths if they ever want to.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    // Two rolling rows — we only ever need the previous one.
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr: Vec<usize> = vec![0usize; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            // deletion, insertion, substitution
            let mut x = prev[j] + 1;
            let y = curr[j - 1] + 1;
            if y < x {
                x = y;
            }
            let z = prev[j - 1] + cost;
            if z < x {
                x = z;
            }
            curr[j] = x;
        }
        core::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Format the "did you mean?" phrase, or return `None` when
/// there's nothing close enough to suggest. Convenience wrapper
/// around [`closest_match`] so error sites don't have to repeat
/// the `format!` call.
pub fn did_you_mean<I, S>(target: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    closest_match(target, candidates).map(|m| format!("Did you mean `{m}`?"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_matches_expected_distances() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("flaw", "lawn"), 2);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("print", "prnit"), 2);
    }

    #[test]
    fn closest_finds_single_edit_typo() {
        // Classic typo — one transposition, Levenshtein 2 on a
        // 6-char target → fits the per-target budget.
        let cands = ["length", "height", "width"];
        assert_eq!(closest_match("lenght", cands), Some("length".to_string()));
    }

    #[test]
    fn closest_returns_none_when_nothing_close() {
        let cands = ["print", "range", "str"];
        assert_eq!(closest_match("xylophone", cands), None);
    }

    #[test]
    fn closest_skips_exact_matches() {
        // Target is already in the candidate list → no
        // suggestion (exact match means the name already
        // resolves).
        let cands = ["print", "range"];
        assert_eq!(closest_match("print", cands), None);
    }

    #[test]
    fn closest_skips_empty_candidates() {
        // Empty candidates shouldn't bubble up as the "closest"
        // answer.
        let cands = ["", "range"];
        assert_eq!(closest_match("rang", cands), Some("range".to_string()));
    }

    #[test]
    fn closest_short_targets_have_tight_budget() {
        // For a 1-character target, the budget is 1. "x" →
        // "xy" is 1 edit (fits), "x" → "range" is 4 (doesn't).
        let cands = ["xy", "range"];
        assert_eq!(closest_match("x", cands), Some("xy".to_string()));
    }

    #[test]
    fn closest_picks_shortest_distance_on_tie() {
        // "hi" → "ho" (1) beats "hi" → "hello" (4).
        let cands = ["hello", "ho"];
        assert_eq!(closest_match("hi", cands), Some("ho".to_string()));
    }

    #[test]
    fn closest_length_prune_skips_far_length_candidates() {
        // "ln" (2 chars) shouldn't suggest "logarithm" (9).
        let cands = ["logarithm"];
        assert_eq!(closest_match("ln", cands), None);
    }

    #[test]
    fn did_you_mean_formats_or_returns_none() {
        let cands = ["length"];
        assert_eq!(
            did_you_mean("lenght", cands),
            Some("Did you mean `length`?".to_string())
        );
        assert_eq!(did_you_mean("xyz", cands), None);
    }
}
