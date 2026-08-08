//! Bounded native and sandbox AOT scaling probe.
//!
//! This is ignored because it builds six generated release-mode modules. The
//! structural snapshot suite runs by default and verifies one incremental
//! publication per declaration; this probe additionally catches unexpected
//! runtime growth in the generated maps.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

use nybl_compile::{Options, transpile};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn type_source(count: usize) -> String {
    (0..count)
        .map(|index| format!("struct Type{index} {{}}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn generated_driver() -> String {
    let mut output =
        String::from("#![allow(dead_code, unused_imports, unused_variables, clippy::all)]\n");
    for sandbox in [false, true] {
        for count in [1_000, 2_000, 4_000] {
            let mode = if sandbox { "sandbox" } else { "native" };
            output.push_str(
                &transpile(
                    &type_source(count),
                    &Options {
                        emit_main: false,
                        use_nybl_sys: false,
                        sandbox,
                        module_name: Some(format!("{mode}_{count}")),
                        module_resolver: None,
                        ..Options::default()
                    },
                )
                .expect("transpile type-heavy program"),
            );
        }
    }
    output.push_str(
        r#"
struct Host;
impl ::nybl::NyblHost for Host {
    fn call(
        &mut self,
        _name: &str,
        _args: &[::nybl::value::Value],
        _line: u32,
    ) -> Option<Result<::nybl::value::Value, ::nybl::error::NyblError>> {
        None
    }
}

fn main() {
    let limits = ::nybl::NyblLimits {
        max_steps: 100_000,
        max_memory: 64 * 1024 * 1024,
        ..NyblLimits::standard()
    };
"#,
    );
    for sandbox in [false, true] {
        for count in [1_000, 2_000, 4_000] {
            let mode = if sandbox { "sandbox" } else { "native" };
            let call = if sandbox {
                format!("{mode}_{count}::run(&mut host, &limits)")
            } else {
                format!("{mode}_{count}::run(&mut host)")
            };
            writeln!(
                output,
                r#"    let best = (0..5).map(|_| {{
        let mut host = Host;
        let started = ::std::time::Instant::now();
        {call}.expect("generated program should run");
        started.elapsed()
    }}).min().unwrap();
    println!("{mode} {count} {{}}", best.as_nanos());"#,
            )
            .unwrap();
        }
    }
    output.push_str("}\n");
    output
}

#[test]
#[ignore = "builds and runs six release-mode generated programs"]
fn native_and_sandbox_aot_type_publication_stays_below_quadratic_growth() {
    let root = workspace_root();
    let dir = root.join("target/nybl-type-publication-scaling");
    std::fs::create_dir_all(dir.join("src")).expect("create scaling scratch project");
    let manifest = format!(
        r#"[package]
name = "nybl-type-publication-scaling"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
nybl = {{ path = "{}", package = "nybl-lang" }}

[workspace]
"#,
        root.join("nybl").display()
    );
    std::fs::write(dir.join("Cargo.toml"), manifest).expect("write scaling manifest");
    std::fs::write(dir.join("src/main.rs"), generated_driver()).expect("write scaling driver");

    let build = Command::new("cargo")
        .args(["build", "--quiet", "--release"])
        .current_dir(&dir)
        .output()
        .expect("build scaling driver");
    assert!(
        build.status.success(),
        "scaling driver failed to build:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let run = Command::new(dir.join("target/release/nybl-type-publication-scaling"))
        .output()
        .expect("run scaling driver");
    assert!(run.status.success(), "scaling driver failed");
    let stdout = String::from_utf8(run.stdout).expect("utf8 timings");
    eprintln!("{stdout}");

    for mode in ["native", "sandbox"] {
        let samples = stdout
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                (fields.next()? == mode)
                    .then(|| fields.nth(1)?.parse::<u128>().ok())
                    .flatten()
            })
            .collect::<Vec<_>>();
        assert_eq!(samples.len(), 3, "missing {mode} samples: {stdout}");
        for pair in samples.windows(2) {
            assert!(
                pair[1] < pair[0].saturating_mul(7) / 2,
                "{mode} AOT publication grew too quickly: {samples:?}"
            );
        }
    }
}
