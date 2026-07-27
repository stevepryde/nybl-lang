#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{format, string::String, vec::Vec};

use crate::error::NyblError;

pub(crate) const I64_MIN_MAGNITUDE: u64 = (i64::MAX as u64) + 1;
pub(crate) const I64_MIN_MAGNITUDE_TEXT: &str = "9223372036854775808";

pub(crate) fn integer_literal_out_of_range(digits: &str) -> String {
    format!("Integer literal out of range for i64: {digits}")
}

#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    Literal(String),
    Variable(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Literals
    /// Integer literal — a digit sequence with no `.` part.
    /// Lexed to `i64` at scan time; the parser maps it to
    /// `ExprKind::Int` which the engines evaluate as
    /// `Value::Int`.
    Int(i64),
    /// The sole unsigned integer magnitude that does not fit in
    /// `i64` but can form `i64::MIN` when immediately negated.
    /// The parser must consume this sentinel; it never reaches
    /// the AST or an execution engine.
    IntMinMagnitude,
    Number(f64),
    Str(String),
    StringInterp(Vec<StringPart>),
    True,
    False,
    None,

    // Identifiers & Keywords
    Ident(String),
    Let,
    Const,
    Fn,
    Pub,
    Return,
    If,
    Else,
    While,
    For,
    In,
    Repeat,
    Break,
    Continue,
    Use,
    As,
    Struct,
    Enum,
    Match,
    Try,
    Ref,

    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    BangEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    AmpAmp,
    PipePipe,
    Bang,
    Eq,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,

    // Delimiters
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    ColonColon,
    Dot,
    DotDot,
    Semicolon,
    FatArrow,
    Pipe,

    // Internal (removed after auto-semicolons)
    Newline,

    Eof,
}

// Define keyword spelling once and derive both lexer recognition and
// diagnostic metadata from it. Keeping these directions in one declaration
// prevents consumers such as the parser and public precheck API from growing
// hand-maintained keyword lists that drift from the lexer.
macro_rules! define_keywords {
    ($($spelling:literal => $variant:ident),+ $(,)?) => {
        impl Token {
            pub(crate) fn from_keyword(value: &str) -> Option<Self> {
                match value {
                    $($spelling => Some(Self::$variant),)+
                    _ => None,
                }
            }

            pub(crate) fn keyword_name(&self) -> Option<&'static str> {
                match self {
                    $(Self::$variant => Some($spelling),)+
                    _ => None,
                }
            }
        }

        #[cfg(test)]
        pub(crate) const KEYWORD_NAMES: &[&str] = &[$($spelling),+];
    };
}

define_keywords! {
    "let" => Let,
    "const" => Const,
    "fn" => Fn,
    "pub" => Pub,
    "return" => Return,
    "if" => If,
    "else" => Else,
    "while" => While,
    "for" => For,
    "in" => In,
    "repeat" => Repeat,
    "break" => Break,
    "continue" => Continue,
    "use" => Use,
    "as" => As,
    "struct" => Struct,
    "enum" => Enum,
    "match" => Match,
    "try" => Try,
    "ref" => Ref,
    "true" => True,
    "false" => False,
    "none" => None,
}

#[derive(Debug, Clone)]
pub struct SpannedToken {
    pub token: Token,
    pub line: u32,
    /// 1-indexed column where the token starts. Used by the
    /// parser and runtime to point error carets at the exact
    /// offending character rather than just the line.
    pub column: u32,
}

pub fn lex(source: &str) -> Result<Vec<SpannedToken>, NyblError> {
    let mut lexer = Lexer::new(source);
    let raw = lexer.lex_all()?;
    Ok(insert_semicolons(raw))
}

fn triggers_semicolon(token: &Token) -> bool {
    matches!(
        token,
        Token::Ident(_)
            | Token::Int(_)
            | Token::IntMinMagnitude
            | Token::Number(_)
            | Token::Str(_)
            | Token::StringInterp(_)
            | Token::True
            | Token::False
            | Token::None
            | Token::Break
            | Token::Continue
            | Token::Return
            | Token::RParen
            | Token::RBracket
            | Token::RBrace
    )
}

fn supports_dot_continuation(token: &Token) -> bool {
    matches!(
        token,
        Token::Ident(_)
            | Token::Int(_)
            | Token::IntMinMagnitude
            | Token::Number(_)
            | Token::Str(_)
            | Token::StringInterp(_)
            | Token::True
            | Token::False
            | Token::None
            | Token::RParen
            | Token::RBracket
            | Token::RBrace
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Delimiter {
    Paren,
    Bracket,
    Brace,
}

fn insert_semicolons(raw: Vec<SpannedToken>) -> Vec<SpannedToken> {
    let mut result: Vec<SpannedToken> = Vec::new();
    let mut delimiters: Vec<Delimiter> = Vec::new();
    let mut tokens = raw.into_iter().peekable();
    while let Some(token) = tokens.next() {
        if token.token == Token::Newline {
            // Coalesce the run so next-token lookahead stays linear
            // even across large blocks of blank lines.
            while tokens
                .peek()
                .is_some_and(|next| next.token == Token::Newline)
            {
                tokens.next();
            }
            if let Some(last) = result.last() {
                // The innermost delimiter decides: a function block
                // inside an array still needs separators between its
                // statements even though the outer bracket is open.
                let inside_soft_delimiter = matches!(
                    delimiters.last(),
                    Some(Delimiter::Paren | Delimiter::Bracket)
                );
                let next = tokens.peek().map(|next| &next.token);
                let before_closing_delimiter =
                    matches!(next, Some(Token::RParen | Token::RBracket | Token::RBrace));
                // A newline before `else` is branch layout, not a statement
                // boundary. Suppressing its synthetic semicolon here keeps an
                // explicit `;` distinguishable (and therefore invalid) at the
                // same parser boundary.
                let before_else =
                    matches!(last.token, Token::RBrace) && matches!(next, Some(Token::Else));
                let before_dot_continuation =
                    matches!(next, Some(Token::Dot)) && supports_dot_continuation(&last.token);
                if !inside_soft_delimiter
                    && !before_closing_delimiter
                    && !before_else
                    && !before_dot_continuation
                    && triggers_semicolon(&last.token)
                {
                    result.push(SpannedToken {
                        token: Token::Semicolon,
                        line: token.line,
                        column: token.column,
                    });
                }
            }
        } else {
            match &token.token {
                Token::LParen => delimiters.push(Delimiter::Paren),
                Token::LBracket => delimiters.push(Delimiter::Bracket),
                Token::LBrace => delimiters.push(Delimiter::Brace),
                Token::RParen if delimiters.last() == Some(&Delimiter::Paren) => {
                    delimiters.pop();
                }
                Token::RBracket if delimiters.last() == Some(&Delimiter::Bracket) => {
                    delimiters.pop();
                }
                Token::RBrace if delimiters.last() == Some(&Delimiter::Brace) => {
                    delimiters.pop();
                }
                _ => {}
            }
            result.push(token);
        }
    }
    result
}

/// A zero-copy cursor over UTF-8 source text.
///
/// `byte_pos` is always kept on a character boundary by [`advance`], while
/// callers continue to count source columns in characters rather than bytes.
struct SourceCursor<'source> {
    source: &'source str,
    chars: core::str::CharIndices<'source>,
    current: Option<(usize, char)>,
    next: Option<(usize, char)>,
    byte_pos: usize,
}

impl<'source> SourceCursor<'source> {
    fn new(source: &'source str) -> Self {
        let mut chars = source.char_indices();
        let current = chars.next();
        let next = chars.next();
        Self {
            source,
            chars,
            current,
            next,
            byte_pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.current.map(|(_, ch)| ch)
    }

    fn peek_next(&self) -> Option<char> {
        self.next.map(|(_, ch)| ch)
    }

    fn advance(&mut self) -> Option<char> {
        let (_, ch) = self.current?;
        self.current = self.next;
        self.next = self.chars.next();
        self.byte_pos = self
            .current
            .map(|(byte_pos, _)| byte_pos)
            .unwrap_or(self.source.len());
        Some(ch)
    }
}

struct Lexer<'source> {
    cursor: SourceCursor<'source>,
    line: u32,
    /// 1-indexed column of the *next* character to consume.
    /// Reset to 1 after each newline; incremented by `advance`.
    column: u32,
}

impl<'source> Lexer<'source> {
    fn new(source: &'source str) -> Self {
        Self {
            cursor: SourceCursor::new(source),
            line: 1,
            column: 1,
        }
    }

    fn peek(&self) -> Option<char> {
        self.cursor.peek()
    }

    fn peek_next(&self) -> Option<char> {
        self.cursor.peek_next()
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.cursor.advance()?;
        if ch == '\n' {
            // The newline itself belongs to the line it
            // terminates; the next character starts at column 1
            // of the *following* line. `line` gets bumped by the
            // lexer's dispatch loop when it sees `\n`, so we
            // only reset the column here.
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(ch)
    }

    fn error(&self, message: impl Into<String>) -> NyblError {
        NyblError {
            line: Some(self.line),
            column: Some(self.column),
            message: message.into(),
            friendly_hint: None,
            source_context: None,
            is_fatal: false,
            is_try_return: false,
        }
    }

    fn error_at(&self, line: u32, column: u32, message: impl Into<String>) -> NyblError {
        NyblError {
            line: Some(line),
            column: Some(column),
            message: message.into(),
            friendly_hint: None,
            source_context: None,
            is_fatal: false,
            is_try_return: false,
        }
    }

    fn error_with_hint(&self, message: impl Into<String>, hint: impl Into<String>) -> NyblError {
        NyblError {
            line: Some(self.line),
            column: Some(self.column),
            message: message.into(),
            friendly_hint: Some(hint.into()),
            source_context: None,
            is_fatal: false,
            is_try_return: false,
        }
    }

    fn lex_all(&mut self) -> Result<Vec<SpannedToken>, NyblError> {
        let mut tokens = Vec::new();

        loop {
            // Skip whitespace (not newlines)
            while let Some(ch) = self.peek() {
                if ch == ' ' || ch == '\t' || ch == '\r' {
                    self.advance();
                } else {
                    break;
                }
            }

            let Some(ch) = self.peek() else {
                tokens.push(SpannedToken {
                    token: Token::Eof,
                    line: self.line,
                    column: self.column,
                });
                break;
            };

            // Capture the token's start position before we start
            // consuming characters — `self.line` / `self.column`
            // move as we advance.
            let line = self.line;
            let column = self.column;

            match ch {
                '\n' => {
                    self.advance();
                    self.line += 1;
                    tokens.push(SpannedToken {
                        token: Token::Newline,
                        line,
                        column,
                    });
                }

                '"' => {
                    tokens.push(SpannedToken {
                        token: self.lex_string()?,
                        line,
                        column,
                    });
                }

                '0'..='9' => {
                    tokens.push(SpannedToken {
                        token: self.lex_number(line, column)?,
                        line,
                        column,
                    });
                }

                'a'..='z' | 'A'..='Z' | '_' => {
                    tokens.push(SpannedToken {
                        token: self.lex_ident_or_keyword(),
                        line,
                        column,
                    });
                }

                '+' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::PlusEq,
                            line,
                            column,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Plus,
                            line,
                            column,
                        });
                    }
                }
                '-' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::MinusEq,
                            line,
                            column,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Minus,
                            line,
                            column,
                        });
                    }
                }
                '*' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::StarEq,
                            line,
                            column,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Star,
                            line,
                            column,
                        });
                    }
                }
                '/' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::SlashEq,
                            line,
                            column,
                        });
                    } else if self.peek() == Some('/') {
                        // `//` — line comment. Runs to end of
                        // line; no block-comment form. (There's
                        // no integer-division operator: `/`
                        // always returns a `Number` even for
                        // `Int / Int`, and users who want an
                        // integer result write `int(a / b)`.)
                        self.advance();
                        while let Some(c) = self.peek() {
                            if c == '\n' {
                                break;
                            }
                            self.advance();
                        }
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Slash,
                            line,
                            column,
                        });
                    }
                }
                '%' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::PercentEq,
                            line,
                            column,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Percent,
                            line,
                            column,
                        });
                    }
                }

                '=' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::EqEq,
                            line,
                            column,
                        });
                    } else if self.peek() == Some('>') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::FatArrow,
                            line,
                            column,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Eq,
                            line,
                            column,
                        });
                    }
                }
                '!' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::BangEq,
                            line,
                            column,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Bang,
                            line,
                            column,
                        });
                    }
                }
                '<' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::LtEq,
                            line,
                            column,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Lt,
                            line,
                            column,
                        });
                    }
                }
                '>' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::GtEq,
                            line,
                            column,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Gt,
                            line,
                            column,
                        });
                    }
                }

                '&' => {
                    self.advance();
                    if self.peek() == Some('&') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::AmpAmp,
                            line,
                            column,
                        });
                    } else {
                        return Err(
                            self.error_with_hint("Unexpected `&`", "Did you mean `&&` (and)?")
                        );
                    }
                }
                '|' => {
                    self.advance();
                    if self.peek() == Some('|') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::PipePipe,
                            line,
                            column,
                        });
                    } else {
                        // Single `|` is now the or-pattern
                        // separator inside `match` arms. Parser
                        // decides whether it's accepted in the
                        // current context.
                        tokens.push(SpannedToken {
                            token: Token::Pipe,
                            line,
                            column,
                        });
                    }
                }

                '(' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::LParen,
                        line,
                        column,
                    });
                }
                ')' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::RParen,
                        line,
                        column,
                    });
                }
                '[' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::LBracket,
                        line,
                        column,
                    });
                }
                ']' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::RBracket,
                        line,
                        column,
                    });
                }
                '{' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::LBrace,
                        line,
                        column,
                    });
                }
                '}' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::RBrace,
                        line,
                        column,
                    });
                }
                ',' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::Comma,
                        line,
                        column,
                    });
                }
                ':' => {
                    self.advance();
                    if self.peek() == Some(':') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::ColonColon,
                            line,
                            column,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Colon,
                            line,
                            column,
                        });
                    }
                }
                '.' => {
                    self.advance();
                    if self.peek() == Some('.') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::DotDot,
                            line,
                            column,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Dot,
                            line,
                            column,
                        });
                    }
                }
                ';' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::Semicolon,
                        line,
                        column,
                    });
                }

                _ => {
                    return Err(self.error(format!("I don't understand the character `{ch}`")));
                }
            }
        }

        Ok(tokens)
    }

    fn lex_number(&mut self, line: u32, column: u32) -> Result<Token, NyblError> {
        let mut s = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_digit() {
                s.push(ch);
                self.advance();
            } else {
                break;
            }
        }
        // A trailing `.<digit>` promotes to a float; `42..foo`
        // or `42.` at EOF stays an Int so chained method calls
        // (`42.str()`) and inclusive array-rest patterns still
        // parse.
        let is_float =
            if self.peek() == Some('.') && self.peek_next().is_some_and(|c| c.is_ascii_digit()) {
                s.push('.');
                self.advance();
                while let Some(ch) = self.peek() {
                    if ch.is_ascii_digit() {
                        s.push(ch);
                        self.advance();
                    } else {
                        break;
                    }
                }
                true
            } else {
                false
            };
        if is_float {
            let n: f64 = s
                .parse()
                .map_err(|_| self.error_at(line, column, format!("Invalid number: {s}")))?;
            Ok(Token::Number(n))
        } else {
            // Preserve one otherwise-unrepresentable magnitude
            // for the parser to fold only under unary `-`. Every
            // other out-of-range value still fails here rather
            // than wrapping or degrading to `f64`.
            match s.parse::<u64>() {
                Ok(n) if n <= i64::MAX as u64 => Ok(Token::Int(n as i64)),
                Ok(I64_MIN_MAGNITUDE) => Ok(Token::IntMinMagnitude),
                _ => Err(self.error_at(line, column, integer_literal_out_of_range(&s))),
            }
        }
    }

    fn lex_ident_or_keyword(&mut self) -> Token {
        let mut s = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                s.push(ch);
                self.advance();
            } else {
                break;
            }
        }
        Token::from_keyword(&s).unwrap_or(Token::Ident(s))
    }

    fn lex_string(&mut self) -> Result<Token, NyblError> {
        self.advance(); // consume opening "
        let mut parts: Vec<StringPart> = Vec::new();
        let mut current = String::new();

        loop {
            match self.peek() {
                None | Some('\n') => {
                    return Err(self.error_with_hint(
                        "This string is missing its closing `\"`",
                        "Every string needs to start and end with quotes.",
                    ));
                }
                Some('"') => {
                    self.advance();
                    break;
                }
                Some('\\') => {
                    self.advance();
                    match self.peek() {
                        Some('"') => {
                            current.push('"');
                            self.advance();
                        }
                        Some('\\') => {
                            current.push('\\');
                            self.advance();
                        }
                        Some('n') => {
                            current.push('\n');
                            self.advance();
                        }
                        Some('t') => {
                            current.push('\t');
                            self.advance();
                        }
                        Some('r') => {
                            // Windows / HTTP line endings; also
                            // needed so `std.json` can stringify
                            // and parse carriage returns inside
                            // strings.
                            current.push('\r');
                            self.advance();
                        }
                        Some('{') => {
                            current.push('{');
                            self.advance();
                        }
                        Some('}') => {
                            current.push('}');
                            self.advance();
                        }
                        Some(c) => {
                            return Err(self.error(format!("Unknown escape sequence `\\{c}`")));
                        }
                        None => {
                            return Err(self.error("Unexpected end of string after `\\`"));
                        }
                    }
                }
                Some('{')
                    if self
                        .peek_next()
                        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_') =>
                {
                    self.advance(); // consume {
                    // Read variable name
                    let mut var = String::new();
                    while let Some(ch) = self.peek() {
                        if ch.is_ascii_alphanumeric() || ch == '_' {
                            var.push(ch);
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    if self.peek() != Some('}') {
                        return Err(self.error_with_hint(
                            format!("Missing `}}` after `{{{var}`"),
                            "String interpolation needs a closing `}`, like: \"{name}\"",
                        ));
                    }
                    self.advance(); // consume }
                    if !current.is_empty() {
                        parts.push(StringPart::Literal(core::mem::take(&mut current)));
                    }
                    parts.push(StringPart::Variable(var));
                }
                Some(ch) => {
                    current.push(ch);
                    self.advance();
                }
            }
        }

        if parts.is_empty() {
            // Plain string, no interpolation
            Ok(Token::Str(current))
        } else {
            if !current.is_empty() {
                parts.push(StringPart::Literal(current));
            }
            Ok(Token::StringInterp(parts))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lex and strip Eof, returning just token variants
    fn toks(code: &str) -> Vec<Token> {
        lex(code)
            .unwrap()
            .into_iter()
            .map(|t| t.token)
            .filter(|t| !matches!(t, Token::Eof))
            .collect()
    }

    fn lex_err(code: &str) -> String {
        lex(code).unwrap_err().message
    }

    #[test]
    fn source_cursor_borrows_utf8_and_advances_on_character_boundaries() {
        let source = "é🙂x";
        let mut lexer = Lexer::new(source);

        assert_eq!(lexer.cursor.source.as_ptr(), source.as_ptr());
        assert_eq!(lexer.cursor.byte_pos, 0);

        assert_eq!(lexer.advance(), Some('é'));
        assert_eq!(lexer.cursor.byte_pos, 'é'.len_utf8());
        assert_eq!(lexer.column, 2);

        assert_eq!(lexer.advance(), Some('🙂'));
        assert_eq!(lexer.cursor.byte_pos, 'é'.len_utf8() + '🙂'.len_utf8());
        assert_eq!(lexer.column, 3);
        assert_eq!(lexer.peek(), Some('x'));
    }

    #[test]
    fn utf8_string_contents_preserve_character_based_token_columns() {
        let tokens = lex("\"é🙂\" next").expect("source should lex");
        assert!(matches!(&tokens[0].token, Token::Str(value) if value == "é🙂"));
        assert_eq!((tokens[0].line, tokens[0].column), (1, 1));
        assert!(matches!(&tokens[1].token, Token::Ident(name) if name == "next"));
        assert_eq!((tokens[1].line, tokens[1].column), (1, 6));
    }

    // ─── Numbers ───────────────────────────────────────────────────

    #[test]
    fn integer() {
        // Integer literals now lex to `Token::Int` (phase 6).
        assert_eq!(toks("42"), vec![Token::Int(42)]);
    }

    #[test]
    fn i64_min_magnitude_is_preserved_for_unary_minus_parsing() {
        assert_eq!(toks(I64_MIN_MAGNITUDE_TEXT), vec![Token::IntMinMagnitude]);
    }

    #[test]
    fn integer_beyond_i64_min_magnitude_errors_at_the_literal_start() {
        let error = lex("\n  9223372036854775809").expect_err("magnitude must be rejected");
        assert_eq!(error.line, Some(2));
        assert_eq!(error.column, Some(3));
        assert_eq!(
            error.message,
            "Integer literal out of range for i64: 9223372036854775809"
        );
    }

    #[test]
    fn float() {
        assert_eq!(toks("3.125"), vec![Token::Number(3.125)]);
    }

    #[test]
    fn leading_zero_float() {
        assert_eq!(toks("0.5"), vec![Token::Number(0.5)]);
    }

    // ─── Strings ───────────────────────────────────────────────────

    #[test]
    fn plain_string() {
        assert_eq!(toks(r#""hello""#), vec![Token::Str("hello".into())]);
    }

    #[test]
    fn escape_sequences() {
        assert_eq!(
            toks(r#""a\nb\t\\\"c""#),
            vec![Token::Str("a\nb\t\\\"c".into())]
        );
    }

    #[test]
    fn escape_sequence_cr() {
        // `\r` was added alongside `std.json` so carriage
        // returns can live in string literals (Windows line
        // endings, HTTP headers, etc.).
        assert_eq!(toks(r#""a\rb""#), vec![Token::Str("a\rb".into())]);
    }

    #[test]
    fn string_interpolation() {
        assert_eq!(
            toks(r#""hi {name}!""#),
            vec![Token::StringInterp(vec![
                StringPart::Literal("hi ".into()),
                StringPart::Variable("name".into()),
                StringPart::Literal("!".into()),
            ])]
        );
    }

    #[test]
    fn string_interpolation_multiple_vars() {
        assert_eq!(
            toks(r#""{x},{y}""#),
            vec![Token::StringInterp(vec![
                StringPart::Variable("x".into()),
                StringPart::Literal(",".into()),
                StringPart::Variable("y".into()),
            ])]
        );
    }

    #[test]
    fn unterminated_string() {
        assert!(lex_err(r#""hello"#).contains("missing its closing"));
    }

    #[test]
    fn unknown_escape() {
        assert!(lex_err(r#""hello\q""#).contains("Unknown escape"));
    }

    // ─── Keywords vs Identifiers ───────────────────────────────────

    #[test]
    fn keywords() {
        assert_eq!(
            toks("let fn return if else while for in repeat break continue true false none"),
            vec![
                Token::Let,
                Token::Fn,
                Token::Return,
                Token::If,
                Token::Else,
                Token::While,
                Token::For,
                Token::In,
                Token::Repeat,
                Token::Break,
                Token::Continue,
                Token::True,
                Token::False,
                Token::None,
            ]
        );
    }

    #[test]
    fn keyword_metadata_round_trips_the_lexer_vocabulary() {
        assert!(KEYWORD_NAMES.contains(&"const"));

        for &name in KEYWORD_NAMES {
            let token = Token::from_keyword(name).expect("declared keyword should lex");
            assert_eq!(token.keyword_name(), Some(name));
            assert_eq!(toks(name), vec![token]);
        }
    }

    #[test]
    fn identifiers() {
        assert_eq!(
            toks("foo bar_baz _x abc123"),
            vec![
                Token::Ident("foo".into()),
                Token::Ident("bar_baz".into()),
                Token::Ident("_x".into()),
                Token::Ident("abc123".into()),
            ]
        );
    }

    // ─── Operators ─────────────────────────────────────────────────

    #[test]
    fn single_char_ops() {
        assert_eq!(
            toks("+ - * / % = ! < > ( ) [ ] { } , : . ;"),
            vec![
                Token::Plus,
                Token::Minus,
                Token::Star,
                Token::Slash,
                Token::Percent,
                Token::Eq,
                Token::Bang,
                Token::Lt,
                Token::Gt,
                Token::LParen,
                Token::RParen,
                Token::LBracket,
                Token::RBracket,
                Token::LBrace,
                Token::RBrace,
                Token::Comma,
                Token::Colon,
                Token::Dot,
                Token::Semicolon,
            ]
        );
    }

    #[test]
    fn double_char_ops() {
        assert_eq!(
            toks("== != <= >= && || += -= *= /= %="),
            vec![
                Token::EqEq,
                Token::BangEq,
                Token::LtEq,
                Token::GtEq,
                Token::AmpAmp,
                Token::PipePipe,
                Token::PlusEq,
                Token::MinusEq,
                Token::StarEq,
                Token::SlashEq,
                Token::PercentEq,
            ]
        );
    }

    #[test]
    fn lone_ampersand_error() {
        assert!(lex_err("&x").contains("Unexpected `&`"));
    }

    #[test]
    fn lone_pipe_lexes_as_or_pattern_separator() {
        // `|` is now the or-pattern separator inside `match`
        // arms. It parses at the lexer level regardless of
        // context; the parser decides whether it's accepted.
        assert_eq!(toks("|"), vec![Token::Pipe]);
    }

    // ─── Comments ──────────────────────────────────────────────────

    #[test]
    fn line_comment_skipped() {
        assert_eq!(
            toks("1 // comment\n2"),
            vec![Token::Int(1), Token::Semicolon, Token::Int(2)]
        );
    }

    #[test]
    fn comment_at_end() {
        assert_eq!(toks("x // done"), vec![Token::Ident("x".into())]);
    }

    #[test]
    fn hash_is_not_a_comment() {
        // `#` used to be the line-comment token; comments are
        // now `//` and `#` is just an unrecognised character.
        assert!(lex_err("x # nope").contains("don't understand"));
    }

    // ─── Auto-semicolons ──────────────────────────────────────────

    #[test]
    fn auto_semi_after_ident() {
        assert_eq!(
            toks("x\ny"),
            vec![
                Token::Ident("x".into()),
                Token::Semicolon,
                Token::Ident("y".into()),
            ]
        );
    }

    #[test]
    fn auto_semi_after_number() {
        assert_eq!(
            toks("42\n10"),
            vec![Token::Int(42), Token::Semicolon, Token::Int(10)]
        );
    }

    #[test]
    fn auto_semi_after_rparen() {
        assert_eq!(
            toks("f()\ng()"),
            vec![
                Token::Ident("f".into()),
                Token::LParen,
                Token::RParen,
                Token::Semicolon,
                Token::Ident("g".into()),
                Token::LParen,
                Token::RParen,
            ]
        );
    }

    #[test]
    fn auto_semi_after_rbrace() {
        assert_eq!(
            toks("{\n}\nx"),
            vec![
                Token::LBrace,
                Token::RBrace,
                Token::Semicolon,
                Token::Ident("x".into()),
            ]
        );
    }

    #[test]
    fn no_semi_after_open_delim() {
        assert_eq!(toks("{\nx"), vec![Token::LBrace, Token::Ident("x".into()),]);
    }

    #[test]
    fn no_semi_after_operator() {
        assert_eq!(
            toks("x +\ny"),
            vec![
                Token::Ident("x".into()),
                Token::Plus,
                Token::Ident("y".into()),
            ]
        );
    }

    #[test]
    fn auto_semi_after_break_continue_return() {
        assert_eq!(
            toks("break\ncontinue\nreturn"),
            vec![
                Token::Break,
                Token::Semicolon,
                Token::Continue,
                Token::Semicolon,
                Token::Return,
            ]
        );
    }

    #[test]
    fn auto_semi_after_true_false_none() {
        assert_eq!(
            toks("true\nfalse\nnone"),
            vec![
                Token::True,
                Token::Semicolon,
                Token::False,
                Token::Semicolon,
                Token::None,
            ]
        );
    }

    #[test]
    fn no_auto_semi_inside_parentheses_or_brackets() {
        assert_eq!(
            toks("[\n1,\n2\n]\nf(\n3,\n4\n)"),
            vec![
                Token::LBracket,
                Token::Int(1),
                Token::Comma,
                Token::Int(2),
                Token::RBracket,
                Token::Semicolon,
                Token::Ident("f".into()),
                Token::LParen,
                Token::Int(3),
                Token::Comma,
                Token::Int(4),
                Token::RParen,
            ]
        );
    }

    #[test]
    fn braces_inside_parentheses_keep_statement_separation() {
        assert_eq!(
            toks("(fn() {\nx\ny\n})"),
            vec![
                Token::LParen,
                Token::Fn,
                Token::LParen,
                Token::RParen,
                Token::LBrace,
                Token::Ident("x".into()),
                Token::Semicolon,
                Token::Ident("y".into()),
                Token::RBrace,
                Token::RParen,
            ]
        );
    }

    #[test]
    fn braces_inside_brackets_keep_statement_separation() {
        assert_eq!(
            toks("[fn() {\nx\ny\n}]"),
            vec![
                Token::LBracket,
                Token::Fn,
                Token::LParen,
                Token::RParen,
                Token::LBrace,
                Token::Ident("x".into()),
                Token::Semicolon,
                Token::Ident("y".into()),
                Token::RBrace,
                Token::RBracket,
            ]
        );
    }

    #[test]
    fn newline_before_closing_delimiter_does_not_insert_semicolon() {
        assert_eq!(
            toks("{\nx\ny\n}"),
            vec![
                Token::LBrace,
                Token::Ident("x".into()),
                Token::Semicolon,
                Token::Ident("y".into()),
                Token::RBrace,
            ]
        );
    }

    #[test]
    fn newline_before_else_does_not_insert_semicolon() {
        assert_eq!(
            toks("if true { 1 }\nelse { 2 }"),
            vec![
                Token::If,
                Token::True,
                Token::LBrace,
                Token::Int(1),
                Token::RBrace,
                Token::Else,
                Token::LBrace,
                Token::Int(2),
                Token::RBrace,
            ]
        );

        assert_eq!(
            toks("if true { 1 };\nelse { 2 }"),
            vec![
                Token::If,
                Token::True,
                Token::LBrace,
                Token::Int(1),
                Token::RBrace,
                Token::Semicolon,
                Token::Else,
                Token::LBrace,
                Token::Int(2),
                Token::RBrace,
            ]
        );
    }

    #[test]
    fn leading_dot_continues_a_value_across_comments_and_blank_lines() {
        assert_eq!(
            toks("value\n// explain the chain\n\n.len()"),
            vec![
                Token::Ident("value".into()),
                Token::Dot,
                Token::Ident("len".into()),
                Token::LParen,
                Token::RParen,
            ]
        );
    }

    #[test]
    fn return_newline_remains_a_bare_return() {
        assert_eq!(
            toks("return\nvalue"),
            vec![
                Token::Return,
                Token::Semicolon,
                Token::Ident("value".into()),
            ]
        );
    }

    #[test]
    fn delimiter_characters_inside_strings_do_not_affect_asi() {
        assert_eq!(
            toks("\"([\"\nx\n\"{name}]\"\ny"),
            vec![
                Token::Str("([".into()),
                Token::Semicolon,
                Token::Ident("x".into()),
                Token::Semicolon,
                Token::StringInterp(vec![
                    StringPart::Variable("name".into()),
                    StringPart::Literal("]".into()),
                ]),
                Token::Semicolon,
                Token::Ident("y".into()),
            ]
        );
    }

    #[test]
    fn unmatched_closing_delimiter_does_not_corrupt_depth() {
        assert_eq!(
            toks("]\nx"),
            vec![Token::RBracket, Token::Semicolon, Token::Ident("x".into()),]
        );
    }

    // ─── Line tracking ─────────────────────────────────────────────

    #[test]
    fn line_numbers() {
        let tokens = lex("x\ny\nz").unwrap();
        let lines: Vec<u32> = tokens.iter().map(|t| t.line).collect();
        // x(L1), ;(L1), y(L2), ;(L2), z(L3), Eof(L3)
        assert_eq!(lines, vec![1, 1, 2, 2, 3, 3]);
    }

    // ─── Unknown character ─────────────────────────────────────────

    #[test]
    fn unknown_char() {
        assert!(lex_err("@").contains("don't understand"));
    }
}
