//! Compile-once / instantiate-many behavior of [`CompiledScript`].
//!
//! The artifact is `Send + Sync` (compile-time asserted in `nybl-vm`);
//! instances stay `!Send`, so the supported cross-thread pattern is
//! create-on-worker from a shared artifact. These tests pin down the
//! contract: no recompilation, shared chunk storage, byte-identical
//! determinism across threads, and per-instance deny-list enforcement.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use nybl::{NyblError, NyblHost, NyblLimits, Value};
use nybl_vm::{CompiledScript, NyblInstance};

struct Host;

impl NyblHost for Host {
    fn call(
        &mut self,
        _name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        None
    }
}

#[derive(Default)]
struct NoResolveHost {
    resolutions: usize,
}

impl NyblHost for NoResolveHost {
    fn call(
        &mut self,
        _name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        None
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        self.resolutions += 1;
        Some(Err(NyblError::runtime(
            format!("runtime host unexpectedly resolved `{name}`"),
            0,
        )))
    }
}

/// Host that records every `print` so a full call sequence yields a
/// byte-comparable transcript.
#[derive(Default)]
struct RecordingHost {
    prints: Vec<String>,
}

impl NyblHost for RecordingHost {
    fn call(
        &mut self,
        _name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        None
    }

    fn on_print(&mut self, message: &str) {
        self.prints.push(message.to_string());
    }
}

/// Exercises per-instance RNG state, dict formatting, string
/// interpolation, and accumulated globals — everything that must be
/// freshly (and identically) initialized per instance.
const DETERMINISM_SRC: &str = r#"
let log = []
pub fn tick(n) {
    let roll = rand(100)
    let entry = {"n": n, "roll": roll, "score": n * 2 + roll}
    log.push("tick {n} -> {entry}")
    print("printed", entry["score"])
    return "{n}:{roll}:{entry}"
}
pub fn transcript() { return log }
"#;

/// One instance's full observable output for a fixed call sequence.
fn run_sequence(instance: &mut NyblInstance, host: &mut RecordingHost) -> String {
    let mut transcript = String::new();
    for n in 0..6 {
        let value = instance.call("tick", &[Value::Int(n)], host).unwrap();
        transcript.push_str(&value.inspect());
        transcript.push('\n');
    }
    transcript.push_str(&instance.call("transcript", &[], host).unwrap().inspect());
    transcript.push('\n');
    for line in &host.prints {
        transcript.push_str(line);
        transcript.push('\n');
    }
    transcript
}

#[test]
fn four_threads_from_one_artifact_match_a_plain_load_byte_for_byte() {
    let program = CompiledScript::compile(DETERMINISM_SRC).unwrap();

    let workers: Vec<std::thread::JoinHandle<String>> = (0..4)
        .map(|_| {
            let program = program.clone();
            std::thread::spawn(move || {
                let mut host = RecordingHost::default();
                let mut instance =
                    NyblInstance::from_compiled(&program, &mut host, &NyblLimits::standard())
                        .unwrap();
                run_sequence(&mut instance, &mut host)
            })
        })
        .collect();

    let mut host = RecordingHost::default();
    let mut loaded =
        NyblInstance::load(DETERMINISM_SRC, &mut host, &NyblLimits::standard()).unwrap();
    let reference = run_sequence(&mut loaded, &mut host);
    assert!(reference.contains("tick 0"), "sequence actually ran");

    for worker in workers {
        let transcript = worker.join().unwrap();
        assert_eq!(transcript, reference);
    }
}

#[test]
fn instances_share_the_artifacts_chunk_storage() {
    let program = CompiledScript::compile(DETERMINISM_SRC).unwrap();
    let artifact_chunk = program.__root_chunk_ptr() as usize;

    // A clone of the artifact shares the same chunk allocation.
    assert_eq!(program.clone().__root_chunk_ptr() as usize, artifact_chunk);

    let mut host = Host;
    let instances: Vec<NyblInstance> = (0..3)
        .map(|_| NyblInstance::from_compiled(&program, &mut host, &NyblLimits::standard()).unwrap())
        .collect();
    for instance in &instances {
        assert_eq!(
            instance.__root_chunk_ptr() as usize,
            artifact_chunk,
            "instance must execute the artifact's chunk, not a deep clone"
        );
    }

    // Worker-created instances share it too.
    let worker_chunk = {
        let program = program.clone();
        std::thread::spawn(move || {
            let mut host = Host;
            let instance =
                NyblInstance::from_compiled(&program, &mut host, &NyblLimits::standard()).unwrap();
            instance.__root_chunk_ptr() as usize
        })
        .join()
        .unwrap()
    };
    assert_eq!(worker_chunk, artifact_chunk);
}

#[test]
fn deny_list_is_enforced_per_instantiation_from_an_unrestricted_artifact() {
    let mut host = Host;
    let denied = NyblLimits::standard().with_disabled_builtins(["rand"]);

    // The artifact compiles without restrictions...
    let program = CompiledScript::compile("pub fn roll() { return rand(6) }").unwrap();
    assert!(NyblInstance::from_compiled(&program, &mut host, &NyblLimits::standard()).is_ok());

    // ...and instantiation under a deny set fails exactly like load().
    let from_compiled = NyblInstance::from_compiled(&program, &mut host, &denied)
        .err()
        .expect("deny list must be enforced at instantiation");
    let from_load = NyblInstance::load("pub fn roll() { return rand(6) }", &mut host, &denied)
        .err()
        .expect("load must refuse a disabled-builtin reference");
    assert_eq!(from_compiled.message, from_load.message);
    assert_eq!(from_compiled.line, from_load.line);
    assert!(from_compiled.is_fatal);

    // A rand-free artifact instantiates fine under the same deny set.
    let clean = CompiledScript::compile("pub fn double(x) { return x * 2 }").unwrap();
    let mut instance = NyblInstance::from_compiled(&clean, &mut host, &denied).unwrap();
    assert_eq!(
        instance
            .call("double", &[Value::Int(21)], &mut host)
            .unwrap()
            .inspect(),
        "42"
    );
}

/// A program exercising functions, structs, enums, closures, and ref
/// parameters (mirroring the coverage style of `tests/instance.rs`).
const FEATURES_SRC: &str = r#"
struct Point { x, y }
enum Shape { Circle(r), Rect { w, h } }

let total = 0

fn area(shape) {
    return match shape {
        Shape::Circle(r) => 3 * r * r,
        Shape::Rect { w, h } => w * h,
    }
}

fn bump(ref target, amount) {
    target = target + amount
}

pub fn accumulate(amount) {
    bump(ref total, amount)
    return total
}

pub fn measure(w, h) {
    let p = Point { x: w, y: h }
    return area(Shape::Rect { w: p.x, h: p.y }) + area(Shape::Circle(1))
}

pub fn make_adder(n) {
    return fn(x) { return x + n }
}

pub fn read_total() { return total }
"#;

fn feature_transcript(instance: &mut NyblInstance, host: &mut Host) -> Vec<String> {
    let mut out = Vec::new();
    out.push(
        instance
            .call("measure", &[Value::Int(4), Value::Int(5)], host)
            .unwrap()
            .inspect(),
    );
    out.push(
        instance
            .call("accumulate", &[Value::Int(9)], host)
            .unwrap()
            .inspect(),
    );
    let adder = instance
        .call("make_adder", &[Value::Int(10)], host)
        .unwrap();
    out.push(
        instance
            .call_value(&adder, &[Value::Int(32)], host)
            .unwrap()
            .inspect(),
    );
    out.push(instance.call("read_total", &[], host).unwrap().inspect());
    out
}

#[test]
fn from_compiled_matches_load_across_language_features() {
    let mut host = Host;
    let program = CompiledScript::compile(FEATURES_SRC).unwrap();
    let mut via_artifact =
        NyblInstance::from_compiled(&program, &mut host, &NyblLimits::standard()).unwrap();
    let mut via_load =
        NyblInstance::load(FEATURES_SRC, &mut host, &NyblLimits::standard()).unwrap();

    let artifact_entries: Vec<(&str, usize)> = via_artifact
        .entry_points()
        .iter()
        .map(|entry| (entry.name(), entry.arity()))
        .collect();
    let load_entries: Vec<(&str, usize)> = via_load
        .entry_points()
        .iter()
        .map(|entry| (entry.name(), entry.arity()))
        .collect();
    assert_eq!(artifact_entries, load_entries);

    assert_eq!(
        feature_transcript(&mut via_artifact, &mut host),
        feature_transcript(&mut via_load, &mut host)
    );
}

#[test]
fn callbacks_from_sibling_instances_of_one_artifact_stay_distinct() {
    // RUN-015..018: callable identity is per instance, even when two
    // instances share one compiled artifact.
    let mut host = Host;
    let program = CompiledScript::compile("pub fn make() { return fn(x) { return x } }").unwrap();
    let mut first =
        NyblInstance::from_compiled(&program, &mut host, &NyblLimits::standard()).unwrap();
    let mut second =
        NyblInstance::from_compiled(&program, &mut host, &NyblLimits::standard()).unwrap();

    let callback = first.call("make", &[], &mut host).unwrap();
    assert_eq!(
        first
            .call_value(&callback, &[Value::Int(7)], &mut host)
            .unwrap()
            .inspect(),
        "7"
    );
    let affinity = second
        .call_value(&callback, &[Value::Int(7)], &mut host)
        .unwrap_err();
    assert!(affinity.message.contains("different Nybl engine instance"));
}

const MODULE_GRAPH_ROOT: &str = r#"
use left as l
use right as r

pub fn tick() { return l.next_left() + r.next_right() }
pub fn make() { return l.make() }
"#;

fn module_sources() -> BTreeMap<String, String> {
    [
        (
            "left",
            "use shared as s\nfn next_left() { return s.next() }\nfn make() { return fn(x) { return x + s.current() } }",
        ),
        (
            "right",
            "use shared as s\nfn next_right() { return s.next() }",
        ),
        (
            "shared",
            "let count = 0\nfn next() { count += 1; return count }\nfn current() { return count }",
        ),
    ]
    .into_iter()
    .map(|(name, source)| (name.to_string(), source.to_string()))
    .collect()
}

#[test]
fn precompiled_transitive_diamond_is_resolved_once_and_shared_across_four_threads() {
    let sources = module_sources();
    let resolutions = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
    let resolution_counts = Arc::clone(&resolutions);
    let program = CompiledScript::compile_with_modules(MODULE_GRAPH_ROOT, move |path| {
        *resolution_counts
            .lock()
            .unwrap()
            .entry(path.to_string())
            .or_default() += 1;
        sources.get(path).cloned().map(Ok)
    })
    .unwrap();

    assert_eq!(
        *resolutions.lock().unwrap(),
        BTreeMap::from([
            ("left".to_string(), 1),
            ("right".to_string(), 1),
            ("shared".to_string(), 1),
        ]),
        "the diamond dependency must resolve only once at artifact build time"
    );

    let module_pointers: BTreeMap<String, usize> = ["left", "right", "shared"]
        .into_iter()
        .map(|path| {
            (
                path.to_string(),
                program.__module_chunk_ptr(path).unwrap() as usize,
            )
        })
        .collect();
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let program = program.clone();
            let module_pointers = module_pointers.clone();
            std::thread::spawn(move || {
                let mut host = NoResolveHost::default();
                let mut instance =
                    NyblInstance::from_compiled(&program, &mut host, &NyblLimits::standard())
                        .unwrap();
                for (path, pointer) in module_pointers {
                    assert_eq!(
                        instance.__module_chunk_ptr(&path).unwrap() as usize,
                        pointer,
                        "module chunks must be shared, not recompiled"
                    );
                }
                let first = instance.call("tick", &[], &mut host).unwrap().inspect();
                let second = instance.call("tick", &[], &mut host).unwrap().inspect();
                assert_eq!(
                    host.resolutions, 0,
                    "runtime source resolution is forbidden"
                );
                (first, second)
            })
        })
        .collect();

    for worker in workers {
        assert_eq!(worker.join().unwrap(), ("3".to_string(), "7".to_string()));
    }
}

#[test]
fn precompiled_module_state_and_callable_affinity_are_per_instance() {
    let sources = module_sources();
    let program = CompiledScript::compile_with_modules(MODULE_GRAPH_ROOT, |path| {
        sources.get(path).cloned().map(Ok)
    })
    .unwrap();
    let mut host = NoResolveHost::default();
    let mut first =
        NyblInstance::from_compiled(&program, &mut host, &NyblLimits::standard()).unwrap();
    let mut second =
        NyblInstance::from_compiled(&program, &mut host, &NyblLimits::standard()).unwrap();

    assert_eq!(first.call("tick", &[], &mut host).unwrap().inspect(), "3");
    assert_eq!(first.call("tick", &[], &mut host).unwrap().inspect(), "7");
    assert_eq!(
        second.call("tick", &[], &mut host).unwrap().inspect(),
        "3",
        "module globals must start fresh in a sibling instance"
    );

    let callback = first.call("make", &[], &mut host).unwrap();
    let affinity = second
        .call_value(&callback, &[Value::Int(10)], &mut host)
        .unwrap_err();
    assert!(affinity.message.contains("different Nybl engine instance"));
    assert_eq!(host.resolutions, 0);
}

#[test]
fn precompiled_cycles_keep_the_runtime_circular_import_diagnostic() {
    let sources = BTreeMap::from([
        ("a".to_string(), "use b\nlet a_value = 1".to_string()),
        ("b".to_string(), "use a\nlet b_value = 2".to_string()),
    ]);
    let program =
        CompiledScript::compile_with_modules("use a", |path| sources.get(path).cloned().map(Ok))
            .expect("a cyclic graph can be compiled without executing it");
    let mut host = NoResolveHost::default();
    let failure = NyblInstance::from_compiled(&program, &mut host, &NyblLimits::standard())
        .err()
        .expect("executing the cycle must still fail");
    assert!(failure.message.contains("Circular import"));
    assert_eq!(failure.source_context.unwrap().module_path, "b");
    assert_eq!(host.resolutions, 0);
}

#[test]
fn precompiled_module_builtin_usage_is_checked_with_each_instances_limits() {
    let source = "let value = rand(6)".to_string();
    let program = CompiledScript::compile_with_modules("use dice", |path| {
        (path == "dice").then(|| Ok(source.clone()))
    })
    .unwrap();
    let mut host = NoResolveHost::default();
    assert!(NyblInstance::from_compiled(&program, &mut host, &NyblLimits::standard()).is_ok());

    let denied = NyblLimits::standard().with_disabled_builtins(["rand"]);
    let failure = NyblInstance::from_compiled(&program, &mut host, &denied)
        .err()
        .expect("the module deny list must be enforced per instance");
    assert!(failure.is_fatal);
    assert!(
        failure.message.contains("builtin `rand` is disabled"),
        "{failure:?}"
    );
    assert_eq!(failure.source_context.unwrap().module_path, "dice");
    assert_eq!(host.resolutions, 0);
}

#[test]
fn compile_time_module_diagnostics_keep_the_deepest_source() {
    let sources = BTreeMap::from([
        ("outer".to_string(), "use broken".to_string()),
        ("broken".to_string(), "let nope = (".to_string()),
    ]);
    let failure = CompiledScript::compile_with_modules("use outer", |path| {
        sources.get(path).cloned().map(Ok)
    })
    .unwrap_err();
    let context = failure.source_context.expect("module context");
    assert_eq!(context.module_path, "broken");
    assert_eq!(context.source.as_deref(), Some("let nope = ("));
}

#[test]
fn precompiled_graph_discovers_lazy_function_uses_and_keeps_runtime_diagnostics() {
    let program = CompiledScript::compile_with_modules(
        "pub fn fail() { use lazy as m; return m.fail() }",
        |path| (path == "lazy").then(|| Ok("fn fail() { return missing_binding }".to_string())),
    )
    .unwrap();
    let mut host = NoResolveHost::default();
    let mut instance =
        NyblInstance::from_compiled(&program, &mut host, &NyblLimits::standard()).unwrap();
    assert_eq!(host.resolutions, 0, "the lazy module has not executed yet");

    let failure = instance.call("fail", &[], &mut host).unwrap_err();
    let context = failure.source_context.expect("module runtime context");
    assert_eq!(context.module_path, "lazy");
    assert_eq!(
        context.source.as_deref(),
        Some("fn fail() { return missing_binding }")
    );
    assert_eq!(host.resolutions, 0);
}
