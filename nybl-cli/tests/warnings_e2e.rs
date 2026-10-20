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
/// Returns `(stdout, stderr, exit code)`.
fn run_project(files: &[(&str, &str)], entry: &str, no_vm: bool) -> (String, String, i32) {
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
        String::from_utf8_lossy(&output.stdout).into_owned(),
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
fn walker_and_vm_report_flat_import_clashes_as_errors_without_warnings() {
    // A flat import over a slot-allocated local, a parameter, or a
    // module-top `fn` is an ordinary error in both engines (catchable in
    // a function body) and never a stderr warning.
    let files = &[
        (
            "main.nybl",
            r#"fn local_case() {
    let dup = "local"
    use shadow_exports
    return dup
}
fn param_case(dup) {
    use shadow_exports
    return dup
}
print(try_call(local_case).is_err())
print(try_call(fn() { return param_case("param") }).is_err())
fn shadowed() { return "root fn" }
use shadow_exports"#,
        ),
        (
            "shadow_exports.nybl",
            "fn dup() { return \"imported\" }\nlet shadowed = \"imported value\"",
        ),
    ];
    let (vm_stdout, vm_stderr, vm_code) = run_project(files, "main.nybl", false);
    let (walker_stdout, walker_stderr, walker_code) = run_project(files, "main.nybl", true);

    assert_eq!(vm_code, 1, "VM stderr: {vm_stderr}");
    assert_eq!(walker_code, 1, "walker stderr: {walker_stderr}");
    assert_eq!(vm_stdout, "true\ntrue\n");
    assert_eq!(vm_stdout, walker_stdout);
    assert_eq!(vm_stderr, walker_stderr);
    assert!(
        vm_stderr.starts_with(
            "error: `shadowed` is already defined — `use shadow_exports` would import a different `shadowed`"
        ),
        "VM stderr: {vm_stderr}"
    );
    assert!(!vm_stderr.contains("warning:"), "VM stderr: {vm_stderr}");
}
