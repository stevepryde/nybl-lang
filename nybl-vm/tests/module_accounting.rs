//! Walker/VM parity for resource accounting shared across calls and module
//! loads: the nesting budget, call depth through `use`, the step budget a
//! module top level shares with its importer, and the per-token load cost
//! (lazy and precompiled VM modules alike).
//!
//! Deep tree-walker recursion needs the stack RUN-002 documents (debug
//! frames are large), so each test body runs on such a thread.

use std::collections::BTreeMap;

use nybl::{NyblError, NyblHost, NyblLimits, Value};

const NESTING: &str = "Too much nested code across active function calls";
const CALL_DEPTH: &str = "Too many nested function calls (possible infinite recursion)";
const STEPS: &str = "Your code took too many steps (possible infinite loop)";

struct Host {
    modules: BTreeMap<String, String>,
    prints: Vec<String>,
}

impl Host {
    fn new(modules: &[(&str, &str)]) -> Self {
        Self {
            modules: modules
                .iter()
                .map(|(name, source)| (name.to_string(), source.to_string()))
                .collect(),
            prints: Vec::new(),
        }
    }
}

impl NyblHost for Host {
    fn call(&mut self, _: &str, _: &[Value], _: u32) -> Option<Result<Value, NyblError>> {
        None
    }

    fn on_print(&mut self, message: &str) {
        self.prints.push(message.to_string());
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        self.modules.get(name).cloned().map(Ok)
    }
}

/// Run `test` on a thread with the documented stack for this profile (see
/// `stack_bound.rs`).
fn on_documented_stack(test: impl FnOnce() + Send + 'static) {
    let size = if cfg!(debug_assertions) {
        128 * 1024 * 1024
    } else {
        10 * 1024 * 1024
    };
    std::thread::Builder::new()
        .stack_size(size)
        .spawn(test)
        .expect("spawn test thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

#[derive(Debug, PartialEq)]
struct Outcome {
    prints: Vec<String>,
    error: Option<(String, Option<u32>, bool)>,
}

fn outcome(result: Result<(), NyblError>, host: Host) -> Outcome {
    Outcome {
        prints: host.prints,
        error: result
            .err()
            .map(|error| (error.message, error.line, error.is_fatal)),
    }
}

fn walker(source: &str, modules: &[(&str, &str)], limits: &NyblLimits) -> Outcome {
    let mut host = Host::new(modules);
    let result = nybl::run(source, &mut host, limits);
    outcome(result, host)
}

fn vm(source: &str, modules: &[(&str, &str)], limits: &NyblLimits) -> Outcome {
    let mut host = Host::new(modules);
    let result = nybl_vm::run(source, &mut host, limits);
    outcome(result, host)
}

/// Both engines must agree exactly (prints, message, line, fatality).
#[track_caller]
fn both(source: &str, modules: &[(&str, &str)], limits: &NyblLimits) -> Outcome {
    let walker = walker(source, modules, limits);
    assert_eq!(
        walker,
        vm(source, modules, limits),
        "engines diverged on:\n{source}"
    );
    walker
}

#[track_caller]
fn error_message(outcome: &Outcome) -> &str {
    &outcome.error.as_ref().expect("program should fail").0
}

/// A recursive function whose body nests `depth` list literals.
fn deep_body_recursion(depth: usize) -> String {
    format!(
        "fn d(k) {{\n    print(k)\n    return {}d(k + 1){}\n}}\nd(1)\n",
        "[".repeat(depth),
        "]".repeat(depth)
    )
}

#[test]
fn deep_recursion_trips_the_nesting_budget_at_the_same_call() {
    on_documented_stack(|| {
        let outcome = both(&deep_body_recursion(60), &[], &NyblLimits::standard());
        assert_eq!(error_message(&outcome), NESTING);
        let (_, line, fatal) = outcome.error.as_ref().unwrap();
        assert_eq!(*line, Some(3), "the trip is reported at the recursive call");
        assert!(!fatal, "the nesting budget is an ordinary error");
        // The root charges 4 levels and each call its body's 65 against the
        // default 2048, so the 32nd call is the first that no longer fits
        // (4 + 31 * 65 = 2019).
        assert_eq!(outcome.prints.len(), 31);
    });
}

#[test]
fn the_nesting_budget_follows_the_host_limit_and_is_catchable() {
    on_documented_stack(|| {
        let limits = NyblLimits::standard().with_max_nesting(200);
        let outcome = both(
            &format!(
                "{}print(try_call(fn() {{ return d(1) }}).is_err())\nprint(\"after\")\n",
                deep_body_recursion(60).replace("d(1)\n", "")
            ),
            &[],
            &limits,
        );
        assert_eq!(outcome.error, None);
        // The root (6), the lambda (4), and two 65-deep frames fit in 200; the
        // third call does not. `try_call` catches the error and execution
        // continues.
        assert_eq!(outcome.prints, ["1", "2", "true", "after"]);

        let root_too_deep = both(
            "let x = [[[1]]]",
            &[],
            &NyblLimits::standard().with_max_nesting(3),
        );
        assert_eq!(error_message(&root_too_deep), NESTING);
    });
}

#[test]
fn a_module_load_is_one_frame_of_shared_call_depth() {
    on_documented_stack(|| {
        // `use` inside the recursion loads `m`, whose top level makes one call.
        // The load itself is a frame, so with 62 active calls the module's call
        // reaches depth 64 and succeeds; with 63 it would be the 65th frame.
        let source = |calls: u32| {
            format!(
                "fn d(k) {{\n    if k == 1 {{\n        use m\n        return value\n    }}\n    return d(k - 1)\n}}\nprint(d({calls}))\n"
            )
        };
        let modules = &[("m", "fn one() { return 1 }\nlet value = one()")];
        let fits = both(&source(62), modules, &NyblLimits::standard());
        assert_eq!(fits.error, None);
        assert_eq!(fits.prints, ["1"]);
        let too_deep = both(&source(63), modules, &NyblLimits::standard());
        assert_eq!(error_message(&too_deep), CALL_DEPTH);
    });
}

#[test]
fn a_seventy_module_import_chain_stops_at_the_call_depth_cap() {
    on_documented_stack(|| {
        let modules: Vec<(String, String)> = (0..70)
            .map(|index| {
                let source = if index < 69 {
                    format!("use m{}\nlet value = {index}", index + 1)
                } else {
                    format!("let value = {index}")
                };
                (format!("m{index}"), source)
            })
            .collect();
        let modules: Vec<(&str, &str)> = modules
            .iter()
            .map(|(name, source)| (name.as_str(), source.as_str()))
            .collect();
        let outcome = both("use m0\nprint(value)", &modules, &NyblLimits::standard());
        assert_eq!(error_message(&outcome), CALL_DEPTH);
    });
}

/// Smallest `max_steps` at which `source` runs to completion.
fn steps_needed(
    engine: fn(&str, &[(&str, &str)], &NyblLimits) -> Outcome,
    source: &str,
    modules: &[(&str, &str)],
) -> u64 {
    let completes = |max_steps| {
        let limits = NyblLimits {
            max_steps,
            ..NyblLimits::standard()
        };
        engine(source, modules, &limits).error.is_none()
    };
    let (mut low, mut high) = (0, 100_000);
    assert!(completes(high), "{source} needs more than {high} steps");
    while high - low > 1 {
        let middle = (low + high) / 2;
        if completes(middle) {
            high = middle;
        } else {
            low = middle;
        }
    }
    high
}

#[test]
fn a_module_top_level_shares_its_importers_step_budget() {
    on_documented_stack(|| {
        let modules = &[("m", "let x = 0\nrepeat 400 { x += 1 }")];
        let root_work = "let y = 0\nrepeat 400 { y += 1 }\n";
        for (engine, run) in [
            (
                "walker",
                walker as fn(&str, &[(&str, &str)], &NyblLimits) -> Outcome,
            ),
            ("vm", vm),
        ] {
            // Each half fits the budget on its own; together they must not.
            let budget = steps_needed(run, "use m", modules).max(steps_needed(run, root_work, &[]));
            let limits = NyblLimits {
                max_steps: budget,
                ..NyblLimits::standard()
            };
            let outcome = run(&format!("{root_work}use m"), modules, &limits);
            assert_eq!(
                outcome.error.as_ref().map(|error| error.0.as_str()),
                Some(STEPS),
                "{engine}: the module restarted the step budget"
            );
        }
    });
}

/// `fn f() { return [1, 1, ... 1] }` with 100 elements: `fn f ( ) {`,
/// `return [`, 100 integers, 99 commas, `] }` — 208 tokens. Loading it runs
/// only the declaration.
fn costly_module() -> String {
    format!("fn f() {{ return [{}] }}", vec!["1"; 100].join(", "))
}
const COSTLY_MODULE_TOKENS: u64 = 208;

fn with_steps(max_steps: u64) -> NyblLimits {
    NyblLimits {
        max_steps,
        ..NyblLimits::standard()
    }
}

#[test]
fn loading_a_module_costs_one_step_per_token_once() {
    on_documented_stack(|| {
        let module = costly_module();
        let modules = &[("m", module.as_str())];

        let below = both("use m", modules, &with_steps(COSTLY_MODULE_TOKENS - 1));
        assert_eq!(error_message(&below), STEPS);
        assert_eq!(
            below.error.as_ref().unwrap().1,
            Some(1),
            "charged at the `use`"
        );
        // The walker also ticks once for the `use` and once for the declaration.
        let above = both("use m", modules, &with_steps(COSTLY_MODULE_TOKENS + 2));
        assert_eq!(above.error, None);

        // A cached module is free to import again.
        let again = both(
            "use m as first\nuse m as second",
            modules,
            &with_steps(COSTLY_MODULE_TOKENS + 3),
        );
        assert_eq!(again.error, None);

        // The root program's own source is not charged.
        let root = format!("let x = [{}]", vec!["1"; 100].join(", "));
        assert_eq!(both(&root, &[], &with_steps(20)).error, None);
    });
}

#[test]
fn instances_charge_lazy_and_precompiled_module_loads_identically() {
    on_documented_stack(|| {
        let module = costly_module();
        let modules = [("m", module.as_str())];
        let source = "use m";
        for (max_steps, loads) in [
            (COSTLY_MODULE_TOKENS - 1, false),
            (COSTLY_MODULE_TOKENS + 2, true),
        ] {
            let limits = with_steps(max_steps);
            let walker = nybl::NyblInstance::load(source, &mut Host::new(&modules), &limits);
            let lazy = nybl_vm::NyblInstance::load(source, &mut Host::new(&modules), &limits);
            let program = nybl_vm::CompiledScript::compile_with_modules(source, |path| {
                modules
                    .iter()
                    .find(|(name, _)| *name == path)
                    .map(|(_, source)| Ok(source.to_string()))
            })
            .expect("compile module graph");
            let precompiled =
                nybl_vm::NyblInstance::from_compiled(&program, &mut Host::new(&[]), &limits);
            let messages = [
                walker.err().map(|error| error.message),
                lazy.err().map(|error| error.message),
                precompiled.err().map(|error| error.message),
            ];
            let expected = (!loads).then(|| STEPS.to_string());
            assert_eq!(
                messages,
                [expected.clone(), expected.clone(), expected],
                "max_steps {max_steps}"
            );
        }
    });
}

#[test]
fn instance_calls_restart_the_nesting_budget() {
    on_documented_stack(|| {
        let source = format!(
            "{}pub fn deep() {{ return d(1) }}\n",
            deep_body_recursion(60).replace("d(1)\n", "")
        );
        let limits = NyblLimits::standard();
        let mut walker = nybl::NyblInstance::load(&source, &mut Host::new(&[]), &limits).unwrap();
        let mut vm = nybl_vm::NyblInstance::load(&source, &mut Host::new(&[]), &limits).unwrap();
        // Every call gets the whole budget again: the entry (4) plus 31 frames
        // of 65 fit, so each failing call prints the same 31 depths.
        for _ in 0..3 {
            for (engine, error, prints) in [
                {
                    let mut host = Host::new(&[]);
                    let error = walker.call("deep", &[], &mut host).unwrap_err();
                    ("walker", error, host.prints)
                },
                {
                    let mut host = Host::new(&[]);
                    let error = vm.call("deep", &[], &mut host).unwrap_err();
                    ("vm", error, host.prints)
                },
            ] {
                assert_eq!(error.message, NESTING, "{engine}");
                assert_eq!(prints.len(), 31, "{engine}");
            }
        }
    });
}
