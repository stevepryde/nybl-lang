//! Warning parity through both real `nybl run` execution engines.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn run_source(source: &str, no_vm: bool) -> (String, i32) {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "nybl-warning-parity-{}-{unique}.nybl",
        std::process::id()
    ));
    std::fs::write(&path, source).expect("write temporary Nybl source");

    let mut command = Command::new(env!("CARGO_BIN_EXE_nybl"));
    command.arg("run").arg(&path);
    if no_vm {
        command.arg("--novm");
    }
    let output = command.output().expect("run nybl binary");
    std::fs::remove_file(&path).expect("remove temporary Nybl source");
    (
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

/// Like [`run_source`], but writes a whole project directory
/// (entry file plus importable modules) and runs the entry with the
/// project as the working directory so `use` resolves the modules.
fn run_project(files: &[(&str, &str)], entry: &str, no_vm: bool) -> (String, i32) {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nybl-warning-parity-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temporary Nybl project dir");
    for (name, source) in files {
        std::fs::write(dir.join(name), source).expect("write temporary Nybl module");
    }

    let mut command = Command::new(env!("CARGO_BIN_EXE_nybl"));
    command.arg("run").arg(entry).current_dir(&dir);
    if no_vm {
        command.arg("--novm");
    }
    let output = command.output().expect("run nybl binary");
    std::fs::remove_dir_all(&dir).expect("remove temporary Nybl project dir");
    (
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

#[test]
fn walker_and_vm_render_the_same_source_ordered_warning() {
    let source = r#"if true {
    enum Choice { Left, Missing }
    let _ = match Choice::Left { Choice::Left => 1 }
} else {
    enum Choice { Right }
}"#;
    let (vm_stderr, vm_code) = run_source(source, false);
    let (walker_stderr, walker_code) = run_source(source, true);

    assert_eq!(vm_code, 0, "VM stderr: {vm_stderr}");
    assert_eq!(walker_code, 0, "walker stderr: {walker_stderr}");
    assert_eq!(vm_stderr, walker_stderr);
    assert!(vm_stderr.contains("`Choice::Missing`"));
}

#[test]
fn walker_and_vm_warn_for_glob_shadows_of_slot_locals_and_params() {
    // Issue #117 gap 2: a fn-body glob import clashing with a
    // slot-allocated local or parameter must warn in the VM exactly
    // as it does in the walker — once per executed `use`, plus the
    // module-top warning for the fn declared before its import. This is also
    // the additive-feature regression gate: under `--all-features`, the
    // unified `std` + `no_std` dependency features must retain stderr delivery.
    let files = &[
        (
            "main.nybl",
            r#"fn shadowed() { return "root fn" }
use shadow_exports
fn local_case() {
    let dup = "local"
    use shadow_exports
    return dup
}
fn param_case(dup) {
    use shadow_exports
    return dup
}
print(shadowed())
print(local_case())
print(param_case("param"))
print(param_case("again"))"#,
        ),
        (
            "shadow_exports.nybl",
            "fn dup() { return \"imported\" }\nlet shadowed = \"imported value\"",
        ),
    ];
    let (vm_stderr, vm_code) = run_project(files, "main.nybl", false);
    let (walker_stderr, walker_code) = run_project(files, "main.nybl", true);

    assert_eq!(vm_code, 0, "VM stderr: {vm_stderr}");
    assert_eq!(walker_code, 0, "walker stderr: {walker_stderr}");
    assert_eq!(vm_stderr, walker_stderr);
    let expected = [
        "warning: `shadowed` from `shadow_exports` shadowed by an existing binding — the first definition wins",
        "warning: `dup` from `shadow_exports` shadowed by an existing binding — the first definition wins",
        "warning: `dup` from `shadow_exports` shadowed by an existing binding — the first definition wins",
        "warning: `dup` from `shadow_exports` shadowed by an existing binding — the first definition wins",
        "",
    ]
    .join("\n");
    assert_eq!(vm_stderr, expected);
}
