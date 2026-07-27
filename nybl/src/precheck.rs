//! Lightweight reserved-binding precheck retained for embedder compatibility.
//!
//! This is intentionally narrower than [`crate::parse`]: it reports current
//! Nybl keywords used where the original public API checked named `let` and
//! `fn` bindings, but leaves all other lexical and syntax validation to the
//! parser. New integrations should generally call [`crate::parse`] directly.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::format;

use crate::error::NyblError;
use crate::lexer::{self, SpannedToken, Token};

/// Return a reserved-word diagnostic for a named `let` or `fn` binding.
///
/// The check operates on lexer tokens, so keyword-shaped text inside strings
/// and comments is ignored. Keyword recognition comes directly from the
/// lexer's canonical mapping, including `const`; there is no independent list
/// to drift as the language evolves.
///
/// General lexing and parsing failures are outside this compatibility API's
/// narrow contract and return `None`. Use [`crate::parse`] when those failures
/// should also be reported.
pub fn check(code: &str) -> Option<NyblError> {
    let tokens = lexer::lex(code).ok()?;

    tokens.windows(2).find_map(|pair| {
        let [introducer, candidate] = pair else {
            return None;
        };
        if !matches!(introducer.token, Token::Let | Token::Fn) {
            return None;
        }

        reserved_binding_error(&introducer.token, candidate)
    })
}

fn reserved_binding_error(introducer: &Token, candidate: &SpannedToken) -> Option<NyblError> {
    candidate.token.keyword_name().map(|keyword| {
        let mut error = NyblError::reserved_word(keyword, candidate.line, candidate.column);
        // Preserve the established public API's site-specific guidance even
        // though parser diagnostics use the more general shared wording.
        error.friendly_hint = Some(match introducer {
            Token::Let => format!(
                "You can't use `{keyword}` as a variable name — try something like `my_{keyword}` instead!"
            ),
            Token::Fn => format!(
                "You can't name a function `{keyword}` — try something like `do_{keyword}` instead!"
            ),
            _ => unreachable!("callers filter binding introducers"),
        });
        error
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_and_comments_do_not_create_reserved_bindings() {
        let source = r#"print("let if you want")
// fn while() is only documentation
let message = "fn const()"
print(message)"#;
        assert!(check(source).is_none());
    }

    #[test]
    fn public_check_retains_let_and_named_fn_diagnostics() {
        let let_error = check("let if = 1").expect("reserved let binding should fail");
        assert_eq!(let_error.line, Some(1));
        assert_eq!(let_error.column, Some(5));
        assert_eq!(let_error.message, "`if` is a reserved word in Nybl");
        assert_eq!(
            let_error.friendly_hint.as_deref(),
            Some("You can't use `if` as a variable name — try something like `my_if` instead!")
        );

        let fn_error = check("\nfn const() { none }")
            .expect("current const keyword should fail as a function name");
        assert_eq!(fn_error.line, Some(2));
        assert_eq!(fn_error.column, Some(4));
        assert_eq!(fn_error.message, "`const` is a reserved word in Nybl");
        assert_eq!(
            fn_error.friendly_hint.as_deref(),
            Some("You can't name a function `const` — try something like `do_const` instead!")
        );
    }

    #[test]
    fn every_current_lexer_keyword_is_reserved_as_a_let_name() {
        for &keyword in crate::lexer::KEYWORD_NAMES {
            let source = format!("let {keyword} = 1");
            let error =
                check(&source).unwrap_or_else(|| panic!("`{keyword}` was not treated as reserved"));
            assert_eq!(
                error.message,
                format!("`{keyword}` is a reserved word in Nybl")
            );
        }
    }

    #[test]
    fn valid_const_and_non_keyword_bindings_are_not_rejected() {
        assert!(check("const VALUE = 1").is_none());
        assert!(check("let async = 1\nfn import() { none }").is_none());
    }

    #[test]
    fn general_lexer_errors_remain_outside_the_compatibility_contract() {
        assert!(check("let value = @").is_none());
    }
}
