//! Adversarial programs that try to exhaust the native stack under default
//! limits. Each must end in a clean `Err` carrying one of the expected
//! messages: the nesting budget, call depth, parse depth, or step budget.
//! Shapes sit at the parse-depth cap. Shared by the walker/VM `stack_bound`
//! test and the sandboxed AOT e2e stack test.

#![allow(dead_code)]

pub const NESTING: &str = "Too much nested code across active function calls";
pub const CALL_DEPTH: &str = "Too many nested function calls (possible infinite recursion)";
pub const PARSE_DEPTH: &str = "Code is nested too deeply";
pub const STEPS: &str = "Your code took too many steps (possible infinite loop)";

pub struct Shape {
    pub name: String,
    pub source: String,
    pub modules: Vec<(String, String)>,
    /// Messages a clean termination of this shape may carry (possibly
    /// inside a `try_call` error that was later unwrapped).
    pub expected: &'static [&'static str],
}

impl Shape {
    /// Whether `message` is a clean termination this shape allows.
    pub fn accepts(&self, message: &str) -> bool {
        self.expected
            .iter()
            .any(|expected| message.contains(expected))
    }

    fn new(name: &str, source: String, expected: &'static [&'static str]) -> Self {
        Self {
            name: name.to_string(),
            source,
            modules: Vec::new(),
            expected,
        }
    }

    fn with_modules(mut self, modules: Vec<(String, String)>) -> Self {
        self.modules = modules;
        self
    }
}

/// The deepest `generate(depth)` (at most `max`) that still parses, so every
/// shape sits right at the parse-depth cap however the parser counts.
fn deepest_parseable(max: usize, generate: impl Fn(usize) -> String) -> String {
    (1..=max)
        .rev()
        .map(&generate)
        .find(|source| nybl::parse(source).is_ok())
        .expect("a shallow variant parses")
}

/// `ifs` nested `if`-expressions around 64 nested list literals around
/// `inner`: the shape whose release-mode stack use motivated the nesting
/// budget (61 `if`s parsed before chain links counted toward the cap).
fn if_list_body(ifs: usize, inner: &str) -> String {
    let mut expr = format!("{}{inner}{}", "[".repeat(64), "]".repeat(64));
    for _ in 0..ifs {
        expr = format!("if k > 0 {{ {expr} }} else {{ 0 }}");
    }
    expr
}

/// A recursive `fn d(k)` of the if/list shape. When `k` reaches zero it
/// loads `next` (if any) from inside the recursion, so the module's load,
/// parse, and top level run on top of the deepest stack.
fn recursive_if_list(ifs: usize, next: Option<&str>) -> String {
    let base = match next {
        Some(module) => format!("if k == 0 {{\n        use {module}\n        return 0\n    }}"),
        None => "if k == 0 {\n        return 0\n    }".to_string(),
    };
    format!(
        "fn d(k) {{\n    {base}\n    return {}\n}}\n",
        if_list_body(ifs, "d(k - 1)")
    )
}

/// A recursive `fn d(k)` returning `wrap(depth, d(k - 1))` at the deepest
/// parseable `depth`, called deeper than the call-depth cap allows.
fn recursive(max: usize, wrap: impl Fn(usize, &str) -> String) -> String {
    deepest_parseable(max, |depth| {
        format!(
            "fn d(k) {{\n    if k == 0 {{\n        return 0\n    }}\n    return {}\n}}\nd(100)\n",
            wrap(depth, "d(k - 1)")
        )
    })
}

fn nest(depth: usize, inner: &str, open: &str, close: &str) -> String {
    format!("{}{inner}{}", open.repeat(depth), close.repeat(depth))
}

pub fn corpus() -> Vec<Shape> {
    let mut shapes = Vec::new();

    let ifs = (1..=61)
        .rev()
        .find(|ifs| nybl::parse(&recursive_if_list(*ifs, Some("lib0"))).is_ok())
        .expect("a shallow if/list shape parses");
    shapes.push(Shape::new(
        "if_list_recursion",
        format!("{}d(100)\n", recursive_if_list(ifs, None)),
        &[NESTING],
    ));

    // Shallow root recursion, then a chain of libraries each loaded from
    // the bottom of the previous recursion and recursing again from its
    // own top level, until the shared nesting budget runs out.
    let libraries = 6;
    let mut modules = Vec::new();
    for index in 0..libraries {
        let next = (index + 1 < libraries).then(|| format!("lib{}", index + 1));
        modules.push((
            format!("lib{index}"),
            format!(
                "{}let loaded = d(2)\n",
                recursive_if_list(ifs, next.as_deref())
            ),
        ));
    }
    shapes.push(
        Shape::new(
            "if_list_recursion_through_libraries",
            format!("{}d(2)\n", recursive_if_list(ifs, Some("lib0"))),
            &[NESTING],
        )
        .with_modules(modules),
    );

    // Result and enum construction costs the walker the most native stack
    // per nesting level.
    for (name, prelude, open, close) in [
        ("nested_ok", "", "Ok(", ")"),
        ("nested_err", "", "Err(", ")"),
        ("nested_enum_tuple", "enum E { V(x) }\n", "E::V(", ")"),
        (
            "nested_enum_struct",
            "enum E { V { a } }\n",
            "E::V { a: ",
            " }",
        ),
    ] {
        shapes.push(Shape::new(
            name,
            format!(
                "{prelude}{}",
                recursive(125, |depth, inner| nest(depth, inner, open, close))
            ),
            &[NESTING],
        ));
    }

    // Recursion that fills the nesting budget, then loads a module nested at
    // the parse-depth cap from its deepest frame: the module is lexed and
    // parsed on top of the fullest stack before its own nesting is charged.
    let deep_module = deepest_parseable(127, |depth| {
        format!(
            "let x = 1\nlet y = {}\nlet v = {}\n",
            (0..depth).fold("x".to_string(), |expr, _| format!(
                "match x {{ _ => {expr} }}"
            )),
            nest(64, "1", "[", "]")
        )
    });
    for width in [30, 61] {
        let recursion = |calls: usize| {
            format!(
                "fn d(k) {{\n    if k == 0 {{\n        use deep\n        return 0\n    }}\n    return {}\n}}\nd({calls})\n",
                nest(width, "d(k - 1)", "Ok(", ")")
            )
        };
        // The most calls whose frames still fit beside the root, so the
        // module load is what exceeds the budget (and the call-depth cap
        // leaves room for the load's own frame).
        let probe = nybl::parse(&recursion(0)).expect("recursion parses");
        let root = nybl::ast_depth::body_depth(&probe);
        let frame = match &probe[0].kind {
            nybl::parser::StmtKind::FnDecl { body, .. } => nybl::ast_depth::body_depth(body),
            _ => unreachable!("the program starts with `fn d`"),
        };
        let frames = ((nybl::DEFAULT_MAX_NESTING - root) / frame).min(63) as usize;
        shapes.push(
            Shape::new(
                &format!("deep_module_at_full_budget_width_{width}"),
                recursion(frames - 1),
                &[NESTING],
            )
            .with_modules(vec![("deep".to_string(), deep_module.clone())]),
        );
    }

    shapes.push(Shape::new(
        "nested_parens",
        recursive(62, |depth, inner| nest(depth, inner, "(", ")")),
        &[CALL_DEPTH],
    ));
    shapes.push(Shape::new(
        "nested_unary",
        recursive(125, |depth, inner| format!("{}{inner}", "- ".repeat(depth))),
        &[NESTING],
    ));
    shapes.push(Shape::new(
        "nested_if_exprs",
        recursive(125, |depth, inner| {
            let mut expr = inner.to_string();
            for _ in 0..depth {
                expr = format!("if k > 0 {{ {expr} }} else {{ 0 }}");
            }
            expr
        }),
        &[NESTING],
    ));
    shapes.push(Shape::new(
        "nested_matches",
        recursive(125, |depth, inner| {
            let mut expr = inner.to_string();
            for _ in 0..depth {
                expr = format!("match k {{ _ => {expr} }}");
            }
            expr
        }),
        &[NESTING],
    ));
    shapes.push(Shape::new(
        "nested_calls",
        format!(
            "fn f(x) {{ return x }}\n{}",
            recursive(125, |depth, inner| nest(depth, inner, "f(", ")"))
        ),
        &[NESTING],
    ));
    shapes.push(Shape::new(
        "nested_lists",
        recursive(64, |depth, inner| nest(depth, inner, "[", "]")),
        &[NESTING],
    ));
    shapes.push(Shape::new(
        "nested_dicts",
        recursive(64, |depth, inner| nest(depth, inner, "{ \"a\": ", " }")),
        &[NESTING],
    ));

    // Deeply nested statement blocks around the recursive call.
    shapes.push(Shape::new(
        "deep_body_recursion",
        deepest_parseable(125, |depth| {
            let mut body = "return d(k - 1)".to_string();
            for _ in 0..depth {
                body = format!("if k > 0 {{\n{body}\n}}");
            }
            format!("fn d(k) {{\n{body}\nreturn 0\n}}\nd(100)\n")
        }),
        &[NESTING],
    ));

    // Recursion that re-enters the engine through builtins and methods,
    // which add native frames the nesting budget does not see directly.
    shapes.push(Shape::new(
        "result_map_callback_recursion",
        deepest_parseable(64, |depth| {
            format!(
                "fn d(k) {{\n    if k == 0 {{\n        return 0\n    }}\n    return Ok(k).map(fn(x) {{ return {} }})\n}}\nd(100)\n",
                nest(depth, "d(x - 1)", "[", "]")
            )
        }),
        &[NESTING, CALL_DEPTH],
    ));
    shapes.push(Shape::new(
        "try_call_recursion",
        deepest_parseable(125, |depth| {
            let mut expr = "d(k - 1)".to_string();
            for _ in 0..depth {
                expr = format!("if k > 0 {{ {expr} }} else {{ 0 }}");
            }
            format!(
                "fn d(k) {{\n    if k == 0 {{\n        return Ok(0)\n    }}\n    let r = try_call(fn() {{ return {expr} }})\n    if r.is_err() {{\n        return r\n    }}\n    return r.unwrap()\n}}\nd(100).unwrap()\n"
            )
        }),
        &[NESTING, CALL_DEPTH],
    ));
    shapes.push(Shape::new(
        "method_recursion",
        deepest_parseable(64, |depth| {
            format!(
                "struct Node {{ k }}\nfn Node.d(self) {{\n    if self.k == 0 {{\n        return 0\n    }}\n    return {}\n}}\nNode {{ k: 100 }}.d()\n",
                nest(depth, "Node { k: self.k - 1 }.d()", "[", "]")
            )
        }),
        &[NESTING],
    ));
    shapes.push(Shape::new(
        "pattern_recursion",
        deepest_parseable(64, |depth| {
            format!(
                "fn d(k) {{\n    if k == 0 {{\n        return 0\n    }}\n    return match {} {{ {} => d(k - 1), _ => 0 }}\n}}\nd(100)\n",
                nest(depth, "k", "[", "]"),
                nest(depth, "_", "[", "]")
            )
        }),
        &[NESTING],
    ));

    let chain = 70;
    let modules = (0..chain)
        .map(|index| {
            let source = if index + 1 < chain {
                format!("use m{}\nlet value = {index}\n", index + 1)
            } else {
                format!("let value = {index}\n")
            };
            (format!("m{index}"), source)
        })
        .collect();
    shapes.push(
        Shape::new("module_chain_70", "use m0\n".to_string(), &[CALL_DEPTH]).with_modules(modules),
    );

    shapes.push(Shape::new(
        "plus_chain_past_parse_cap",
        format!("let x = {}\n", vec!["1"; 300].join(" + ")),
        &[PARSE_DEPTH],
    ));
    shapes.push(Shape::new(
        "index_chain_past_parse_cap",
        format!("let x = [0]\nlet y = x{}\n", "[0]".repeat(300)),
        &[PARSE_DEPTH],
    ));
    shapes.push(Shape::new(
        "method_chain_past_parse_cap",
        format!("let x = 1{}\n", ".to_str()".repeat(300)),
        &[PARSE_DEPTH],
    ));

    shapes.push(
        Shape::new(
            "module_top_level_exhausts_shared_steps",
            "use spinner\n".to_string(),
            &[STEPS],
        )
        .with_modules(vec![(
            "spinner".to_string(),
            "let n = 0\nwhile true { n += 1 }\n".to_string(),
        )]),
    );

    shapes
}
