//! Process-level regressions for runtime failures that must terminate cleanly.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const VALUE_DEPTH_SOURCE: &str = "let a = [1]\nrepeat 128 { a = [a] }\n";

fn temp_script(name: &str, source: &str) -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("nybl-{name}-{}-{nonce}.nybl", std::process::id()));
    std::fs::write(&path, source).expect("write temporary Nybl script");
    path
}

#[test]
fn value_nesting_limit_exits_normally_in_both_runtime_engines() {
    let script = temp_script("value-depth", VALUE_DEPTH_SOURCE);
    let bin = env!("CARGO_BIN_EXE_nybl");

    for engine_args in [Vec::<&str>::new(), vec!["--novm"]] {
        let output = Command::new(bin)
            .arg("run")
            .args(engine_args)
            .arg(&script)
            .output()
            .expect("run nybl subprocess");
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(1),
            "runtime must return a normal failure exit, stderr:\n{stderr}"
        );
        assert!(
            stderr.contains("Value nesting limit exceeded"),
            "expected value-depth diagnostic, got:\n{stderr}"
        );
    }

    std::fs::remove_file(script).expect("remove temporary Nybl script");
}
