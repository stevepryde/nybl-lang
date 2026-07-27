use std::time::Instant;

use nybl::parse;
use nybl_vm::compile;

fn alternating_locals_and_uses(binding_count: usize) -> String {
    let mut source = String::from("fn stress(parameter) {\n");
    for index in 0..binding_count {
        source.push_str(&format!("let local_{index} = {index}\nuse dep_{index}\n"));
    }
    source.push_str("}\n");
    source
}

fn compile_stress(binding_count: usize) -> (usize, usize, usize, std::time::Duration) {
    let source = alternating_locals_and_uses(binding_count);
    let ast = parse(&source).expect("stress source should parse");
    let started = Instant::now();
    let chunk = compile(&ast).expect("stress source should compile");
    let elapsed = started.elapsed();
    let body = &chunk.functions[0].chunk;
    let retained_names: usize = body
        .local_scopes
        .iter()
        .map(|scope| scope.entries.len())
        .sum();
    (
        body.use_specs.len(),
        body.local_scopes.len(),
        retained_names,
        elapsed,
    )
}

#[test]
fn local_use_metadata_storage_is_linear() {
    let binding_count = 2_048;
    let (use_count, scope_count, retained_names, _) = compile_stress(binding_count);

    assert_eq!(use_count, binding_count);
    assert_eq!(scope_count, 1);
    assert_eq!(retained_names, binding_count + 1);
}

#[test]
#[ignore = "benchmark-style stress coverage; run explicitly"]
fn compile_many_function_local_uses() {
    let binding_count = std::env::var("NYBL_USE_STRESS_BINDINGS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_000);
    let (use_count, scope_count, retained_names, elapsed) = compile_stress(binding_count);

    eprintln!(
        "bindings={binding_count} uses={} retained_names={retained_names} compile_ms={}",
        use_count,
        elapsed.as_millis()
    );
    assert_eq!(use_count, binding_count);
    assert_eq!(scope_count, 1);
    assert_eq!(retained_names, binding_count + 1);
}
