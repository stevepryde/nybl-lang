//! Error type for the Nybl interpreter.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{boxed::Box, format, string::String};

/// Identifies the non-root source that produced a diagnostic.
///
/// Module parsers and engines attach this only on an error path, so retaining
/// the source has no cost during successful execution. `source` is optional
/// for boundaries that know the module identity but no longer own its text;
/// rendering such a context deliberately omits a snippet instead of borrowing
/// an unrelated root-file line.
///
/// A `module_path` equal to [`crate::value::ROOT_MODULE_PATH`] is the
/// root-ownership sentinel: it marks the error as belonging to the root
/// program so an enclosing module boundary can't claim it, and rendering
/// treats it exactly like an uncontexted error (see
/// [`NyblError::with_module`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceContext {
    pub module_path: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NyblError {
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub message: String,
    pub friendly_hint: Option<String>,
    /// Non-root source identity and, when available, the source text needed
    /// for accurate snippet rendering.
    /// Boxed so the cold error path doesn't bloat `NyblError` itself:
    /// every recursive parser frame returns `Result<_, NyblError>` by
    /// value, and an inline context (~48 bytes) pushed 128-deep parses
    /// past a 2 MiB thread stack before the depth guard could fire.
    pub source_context: Option<Box<SourceContext>>,
    /// Fatal errors can't be caught by `try_call`; they always
    /// unwind to the engine boundary. This is the load-bearing
    /// property that makes `NyblLimits` a real sandbox — a
    /// script can't wrap an infinite loop in `try_call` and
    /// loop forever by swallowing the step-limit error.
    ///
    /// Non-fatal errors (the default) describe ordinary runtime
    /// problems — type mismatches, missing fields, index out of
    /// bounds, "function not found". Those can be caught.
    ///
    /// Currently set only on resource-limit errors
    /// (`Your code took too many steps`, `Memory limit
    /// exceeded`). Any new fatal case must explicitly construct
    /// `NyblError::fatal` rather than `NyblError::runtime`.
    pub is_fatal: bool,
    /// True only for the sentinel error the walker uses to
    /// unwind a `try`-driven early-return out of an enclosing
    /// fn. When set, the `message` / `line` / `friendly_hint`
    /// fields are unused — the return value lives on the
    /// evaluator's `pending_try_return` slot. `call_nybl_fn`
    /// traps errors with this flag and converts them into a
    /// normal `Signal::Return`.
    ///
    /// Always `false` outside that narrow window. Users and
    /// host code should never construct a `NyblError` with this
    /// flag set; use [`NyblError::runtime`] / [`NyblError::fatal`]
    /// for real errors.
    ///
    /// Replaces the older `"__nybl_try_return_signal__"` message
    /// sentinel — a field lookup is cheaper than a string
    /// compare, and a flag can never collide with a user
    /// message that happens to spell the same bytes.
    pub is_try_return: bool,
}

impl NyblError {
    /// Create a runtime error at the given source line.
    pub fn runtime(message: impl Into<String>, line: u32) -> Self {
        Self {
            line: Some(line),
            column: None,
            message: message.into(),
            friendly_hint: None,
            source_context: None,
            is_fatal: false,
            is_try_return: false,
        }
    }

    pub(crate) fn reserved_word(keyword: &str, line: u32, column: u32) -> Self {
        Self {
            line: Some(line),
            column: Some(column),
            message: format!("`{keyword}` is a reserved word in Nybl"),
            friendly_hint: Some(format!(
                "`{keyword}` is part of Nybl syntax and can't be used as an identifier. Choose a different name."
            )),
            source_context: None,
            is_fatal: false,
            is_try_return: false,
        }
    }

    /// Create a runtime error at the given line *and* column.
    /// Callers that have an AST node handy (`expr.line`,
    /// `expr.column`) should prefer this over
    /// [`Self::runtime`] so the error renderer can point a
    /// caret at the offending character.
    pub fn runtime_at(
        message: impl Into<String>,
        line: u32,
        column: Option<core::num::NonZeroU32>,
    ) -> Self {
        Self {
            line: Some(line),
            column: column.map(|c| c.get()),
            message: message.into(),
            friendly_hint: None,
            source_context: None,
            is_fatal: false,
            is_try_return: false,
        }
    }

    /// Create a **fatal** runtime error at the given source line.
    /// Used for resource-limit violations (`too many steps`,
    /// `Memory limit exceeded`) — see [`NyblError::is_fatal`]
    /// for why those must never be swallowed by `try_call`.
    pub fn fatal(message: impl Into<String>, line: u32) -> Self {
        Self {
            line: Some(line),
            column: None,
            message: message.into(),
            friendly_hint: None,
            source_context: None,
            is_fatal: true,
            is_try_return: false,
        }
    }

    /// Build the sentinel error the walker uses to unwind a
    /// `try`-driven early-return. Private to the crate because
    /// no one outside the walker's fn-call boundary should be
    /// constructing one of these — they'd leak a "phantom"
    /// error to user code. The return value itself travels on
    /// the evaluator's `pending_try_return` slot (see
    /// `Evaluator::eval_try`).
    pub(crate) fn try_return_signal(line: u32) -> Self {
        Self {
            line: Some(line),
            column: None,
            message: String::new(),
            friendly_hint: None,
            source_context: None,
            is_fatal: false,
            is_try_return: true,
        }
    }

    /// Attach the imported module and its source text to this diagnostic.
    ///
    /// The deepest context wins: if a transitive import already attached its
    /// own module, an outer importer must not replace it. When the existing
    /// context names the *same* module but couldn't supply its source (a
    /// runtime-error boundary attached [`Self::with_module`] first), the
    /// caller's source text is backfilled so rendering keeps its snippet.
    pub fn with_module_source(
        mut self,
        module_path: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        let module_path = module_path.into();
        match &mut self.source_context {
            None => {
                self.source_context = Some(Box::new(SourceContext {
                    module_path,
                    source: Some(source.into()),
                }));
            }
            Some(context) if context.module_path == module_path && context.source.is_none() => {
                context.source = Some(source.into());
            }
            Some(_) => {}
        }
        self
    }

    /// Attach a module identity when its source text is unavailable.
    ///
    /// [`Self::render`] will show the module path and line/column but will
    /// never render the caller-provided root source for this diagnostic.
    ///
    /// Passing [`crate::value::ROOT_MODULE_PATH`] marks the diagnostic as
    /// owned by the root program instead: rendering treats it exactly like
    /// an uncontexted error (root source, no ``in module`` label), but the
    /// deepest-context-wins rule still applies, so an enclosing module
    /// boundary can no longer claim a root-owned error. Engines use this
    /// when a root-declared fn escapes with an error while running inside
    /// a module fn (e.g. a callback).
    pub fn with_module(mut self, module_path: impl Into<String>) -> Self {
        if self.source_context.is_none() {
            self.source_context = Some(Box::new(SourceContext {
                module_path: module_path.into(),
                source: None,
            }));
        }
        self
    }

    /// The module context to use for rendering, with the root-ownership
    /// sentinel (see [`Self::with_module`]) treated as "no module".
    fn render_context(&self) -> Option<&SourceContext> {
        self.source_context
            .as_deref()
            .filter(|context| context.module_path != crate::value::ROOT_MODULE_PATH)
    }
}

impl core::fmt::Display for NyblError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(context) = self.render_context() {
            if let Some(line) = self.line {
                write!(
                    f,
                    "[module {}, line {}] {}",
                    context.module_path, line, self.message
                )
            } else {
                write!(f, "[module {}] {}", context.module_path, self.message)
            }
        } else if let Some(line) = self.line {
            write!(f, "[line {}] {}", line, self.message)
        } else {
            write!(f, "{}", self.message)
        }
    }
}

impl NyblError {
    /// Render the error with an inline source snippet and a
    /// `^` caret under the offending position. Needs the full
    /// program source that produced the error — pass the same
    /// string you handed to `nybl::run` / `nybl::parse`.
    ///
    /// Falls back gracefully:
    /// - No line set → just the message.
    /// - Line set but out of range (e.g. source was truncated)
    ///   → message + "[line N]" without the snippet.
    /// - No column set → message + snippet, no caret.
    /// - Column set → message + snippet + caret.
    ///
    /// Appends the `friendly_hint` as a `hint:` line when
    /// present. Used by `nybl-cli` to render program failures;
    /// embedders can call it from their own error path.
    pub fn render(&self, source: &str) -> String {
        let mut out = String::new();
        let (source, module_path) = match self.render_context() {
            Some(context) => (
                context.source.as_deref(),
                Some(context.module_path.as_str()),
            ),
            None => (Some(source), None),
        };
        match self.line {
            Some(line) if line > 0 => {
                out.push_str(&format!("error: {}\n", self.message));
                let line_str = match module_path {
                    Some(path) => format!("  --> in module `{path}` at line {line}"),
                    None => format!("  --> line {line}"),
                };
                if let Some(col) = self.column {
                    out.push_str(&format!("{line_str}:{col}\n"));
                } else {
                    out.push_str(&format!("{line_str}\n"));
                }
                if let Some(src_line) =
                    source.and_then(|source| source.lines().nth((line - 1) as usize))
                {
                    let gutter_width = digits_of(line);
                    let gutter_pad = " ".repeat(gutter_width);
                    out.push_str(&format!("{gutter_pad} |\n"));
                    out.push_str(&format!("{line} | {src_line}\n"));
                    out.push_str(&format!("{gutter_pad} | "));
                    if let Some(col) = self.column {
                        // `column` is 1-indexed; characters up to
                        // `col - 1` get a padding space each.
                        let col_idx = col.saturating_sub(1) as usize;
                        let mut pad = String::new();
                        for (i, ch) in src_line.chars().enumerate() {
                            if i >= col_idx {
                                break;
                            }
                            // Preserve tab alignment so the caret
                            // lands under the right column even
                            // in tab-indented source.
                            pad.push(if ch == '\t' { '\t' } else { ' ' });
                        }
                        out.push_str(&pad);
                        out.push_str("^\n");
                    } else {
                        out.push('\n');
                    }
                }
            }
            _ => {
                out.push_str(&format!("error: {}\n", self.message));
                if let Some(path) = module_path {
                    out.push_str(&format!("  --> in module `{path}`\n"));
                }
            }
        }
        if let Some(hint) = &self.friendly_hint {
            out.push_str(&format!("hint: {hint}\n"));
        }
        out
    }
}

/// Count decimal digits in a positive integer — used for
/// gutter width in `render`.
fn digits_of(mut n: u32) -> usize {
    let mut d = 0usize;
    if n == 0 {
        return 1;
    }
    while n > 0 {
        d += 1;
        n /= 10;
    }
    d
}

/// Non-fatal diagnostic surfaced by static checks that run
/// after parsing (currently: match-exhaustiveness analysis in
/// [`crate::check`]). Shape mirrors `NyblError` so the same
/// source-snippet rendering works; the only divergence is the
/// leading header, which says `warning:` instead of `error:`.
///
/// Warnings never halt execution — they're informational. The
/// CLI prints them and then runs the program anyway. Embedders
/// that want to treat them as errors can call
/// [`NyblWarning::into_error`].
#[derive(Debug, Clone)]
pub struct NyblWarning {
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub message: String,
    pub friendly_hint: Option<String>,
}

impl NyblWarning {
    /// Convenience constructor that matches `NyblError::runtime`'s
    /// shape so check passes can build warnings at a single
    /// source line.
    pub fn at(message: impl Into<String>, line: u32) -> Self {
        Self {
            line: Some(line),
            column: None,
            message: message.into(),
            friendly_hint: None,
        }
    }

    /// Attach a "hint:" line to the rendered output. Chained
    /// from the constructor so call sites stay tidy.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.friendly_hint = Some(hint.into());
        self
    }

    /// Promote the warning to a fatal [`NyblError`] with the same
    /// fields. Useful for `-Werror`-style embedders.
    pub fn into_error(self) -> NyblError {
        NyblError {
            line: self.line,
            column: self.column,
            message: self.message,
            friendly_hint: self.friendly_hint,
            source_context: None,
            is_fatal: false,
            is_try_return: false,
        }
    }

    /// Render the warning with a source snippet. Mirrors
    /// [`NyblError::render`] but leads with `warning:` rather
    /// than `error:`.
    pub fn render(&self, source: &str) -> String {
        let err = NyblError {
            line: self.line,
            column: self.column,
            message: self.message.clone(),
            friendly_hint: self.friendly_hint.clone(),
            source_context: None,
            is_fatal: false,
            is_try_return: false,
        };
        // Swap the leading `error:` for `warning:` so the
        // output is visually distinct. The rest of the caret /
        // snippet logic is identical to `NyblError::render`.
        err.render(source).replacen("error:", "warning:", 1)
    }
}

#[cfg(any(feature = "std", not(feature = "no_std")))]
impl std::error::Error for NyblError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(feature = "std", feature = "no_std"))]
    #[test]
    fn unified_std_and_no_std_features_keep_std_error_integration() {
        fn assert_std_error<T: std::error::Error>() {}
        assert_std_error::<NyblError>();
    }

    #[test]
    fn runtime_error_sets_message_and_line() {
        let err = NyblError::runtime("boom", 7);

        assert_eq!(err.message, "boom");
        assert_eq!(err.line, Some(7));
        assert_eq!(err.column, None);
        assert_eq!(err.friendly_hint, None);
        assert_eq!(err.source_context, None);
        assert!(!err.is_fatal);
    }

    #[test]
    fn fatal_error_marks_is_fatal_true() {
        let err = NyblError::fatal("step limit", 0);
        assert!(err.is_fatal);
        assert!(!NyblError::runtime("nope", 0).is_fatal);
    }

    #[test]
    fn render_without_source_falls_back_to_message_only() {
        let err = NyblError::runtime("boom", 0);
        let rendered = err.render("");
        assert!(rendered.contains("error: boom"));
    }

    #[test]
    fn render_with_line_shows_snippet() {
        let src = "let x = 1\nlet y = 2\nlet z = 3";
        let err = NyblError {
            line: Some(2),
            column: None,
            message: "something broke".into(),
            friendly_hint: None,
            source_context: None,
            is_fatal: false,

            is_try_return: false,
        };
        let rendered = err.render(src);
        assert!(rendered.contains("error: something broke"));
        assert!(rendered.contains("--> line 2"));
        assert!(rendered.contains("let y = 2"));
    }

    #[test]
    fn render_with_line_and_column_places_caret() {
        let src = "let x = 1\nlet abc = foo()\nlet z = 3";
        let err = NyblError {
            line: Some(2),
            column: Some(11),
            message: "undefined".into(),
            friendly_hint: Some("did you mean `bar`?".into()),
            source_context: None,
            is_fatal: false,

            is_try_return: false,
        };
        let rendered = err.render(src);
        assert!(rendered.contains("--> line 2:11"));
        assert!(rendered.contains("let abc = foo()"));
        // Carat at column 11 → 10 spaces of padding before `^`.
        assert!(
            rendered.contains(&format!("{}^", " ".repeat(10))),
            "rendered:\n{rendered}"
        );
        assert!(rendered.contains("hint: did you mean `bar`?"));
    }

    #[test]
    fn render_handles_out_of_range_line_gracefully() {
        let src = "let x = 1";
        let err = NyblError {
            line: Some(99),
            column: Some(3),
            message: "off the end".into(),
            friendly_hint: None,
            source_context: None,
            is_fatal: false,

            is_try_return: false,
        };
        // Shouldn't panic; just produces the header without a
        // snippet.
        let rendered = err.render(src);
        assert!(rendered.contains("--> line 99:3"));
        assert!(rendered.contains("error: off the end"));
    }

    #[test]
    fn render_preserves_tab_alignment_in_caret() {
        // Source has a leading tab. Caret padding should use a
        // tab too so it lines up under the offending char.
        let src = "\tlet x = bad_call()";
        let err = NyblError {
            line: Some(1),
            column: Some(10),
            message: "undefined".into(),
            friendly_hint: None,
            source_context: None,
            is_fatal: false,

            is_try_return: false,
        };
        let rendered = err.render(src);
        // The caret line has one tab (from column 1's tab in
        // source) plus 8 spaces for columns 2–9, then `^`.
        assert!(rendered.contains("\t        ^"), "rendered:\n{rendered}");
    }

    #[test]
    fn module_source_context_renders_its_own_snippet() {
        let root = "use bad";
        let module = "let okay = 1\nlet broken =";
        let mut err =
            NyblError::runtime_at("Expected expression", 2, core::num::NonZeroU32::new(13));
        err.friendly_hint = Some("finish the assignment".into());
        let err = err.with_module_source("bad", module);

        let rendered = err.render(root);

        assert!(rendered.contains("in module `bad` at line 2:13"));
        assert!(rendered.contains("let broken ="));
        assert!(rendered.contains("hint: finish the assignment"));
        assert!(!rendered.contains("use bad"));
    }

    #[test]
    fn snippet_free_module_context_never_uses_root_source() {
        let root = "this root line must not render";
        let err = NyblError::runtime_at("broken", 1, core::num::NonZeroU32::new(99))
            .with_module("nested.bad");

        let rendered = err.render(root);

        assert!(rendered.contains("in module `nested.bad` at line 1:99"));
        assert!(!rendered.contains(root));
        assert!(!rendered.contains('^'));
    }

    #[test]
    fn nested_module_context_preserves_deepest_identity() {
        let err = NyblError::runtime("broken", 3)
            .with_module_source("inner", "a\nb\nc")
            .with_module_source("outer", "x\ny\nz");

        let rendered = err.render("root");

        assert!(rendered.contains("in module `inner`"));
        assert!(rendered.contains("3 | c"));
        assert!(!rendered.contains("outer"));
    }

    #[test]
    fn with_module_source_backfills_snippet_for_same_module_context() {
        // A runtime-error boundary attaches the module identity
        // first (no source in hand); the load boundary that still
        // owns the text supplies it afterwards.
        let err = NyblError::runtime("broken", 2)
            .with_module("m")
            .with_module_source("m", "a\nb\nc");

        let rendered = err.render("root");

        assert!(rendered.contains("in module `m` at line 2"));
        assert!(rendered.contains("2 | b"));
    }

    #[test]
    fn with_module_source_never_backfills_across_modules() {
        let err = NyblError::runtime("broken", 1)
            .with_module("inner")
            .with_module_source("outer", "outer line");

        let rendered = err.render("root");

        assert!(rendered.contains("in module `inner` at line 1"));
        assert!(!rendered.contains("outer line"));
    }

    #[test]
    fn root_sentinel_context_renders_root_source() {
        let root = "let a = 1\nlet zero = 0\nlet b = a / zero";
        let err =
            NyblError::runtime("Division by zero", 3).with_module(crate::value::ROOT_MODULE_PATH);

        let rendered = err.render(root);

        assert!(rendered.contains("--> line 3"));
        assert!(rendered.contains("3 | let b = a / zero"));
        assert!(!rendered.contains("in module"));
        assert_eq!(format!("{err}"), "[line 3] Division by zero");
    }

    #[test]
    fn root_sentinel_blocks_outer_module_claim() {
        // A root-declared callback errors while running inside a
        // module fn: the callback's boundary marks root ownership
        // and the module boundary must not repaint it.
        let root = "line one\nline two";
        let err = NyblError::runtime("boom", 2)
            .with_module(crate::value::ROOT_MODULE_PATH)
            .with_module("m")
            .with_module_source("m", "module text");

        let rendered = err.render(root);

        assert!(rendered.contains("--> line 2"));
        assert!(rendered.contains("2 | line two"));
        assert!(!rendered.contains("in module"));
    }
}
