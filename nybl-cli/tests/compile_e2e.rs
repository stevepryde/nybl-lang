//! Process-level regressions for `nybl compile`.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_PROJECT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_project() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let project_id = TEMP_PROJECT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "nybl-compile-e2e-{}-{nonce}-{project_id}",
        std::process::id(),
    ));
    std::fs::create_dir_all(&path).expect("create temporary project");
    path
}

fn path_with_fake_bin(fake_bin: &std::path::Path) -> std::ffi::OsString {
    std::env::join_paths(
        std::iter::once(fake_bin.to_path_buf()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )),
    )
    .unwrap()
}

#[test]
fn compile_finds_target_triple_artifact_with_external_target_dir() {
    let project = temp_project();
    let fake_bin = project.join("fake-bin");
    let external_target = project.join("ambient-target");
    let build_target = "test-target-triple";
    let cargo_log = project.join("cargo-target-dir.txt");
    let input = project.join("entry.nybl");
    let output_path = project.join("compiled-entry");
    std::fs::create_dir_all(&fake_bin).unwrap();
    std::fs::create_dir_all(&external_target).unwrap();
    std::fs::write(&input, "print(\"compiled\")").unwrap();

    // The fake keeps this regression quick while exercising the complete CLI
    // process boundary: cargo discovery, argument construction, deterministic
    // artifact lookup, copy-out, and scratch cleanup.
    let fake_cargo = fake_bin.join("cargo");
    std::fs::write(
        &fake_cargo,
        r#"#!/bin/sh
set -eu
if [ "${1-}" = "--version" ]; then
    echo "cargo 1.88.0"
    exit 0
fi

target_dir=""
while [ "$#" -gt 0 ]; do
    if [ "$1" = "--target-dir" ]; then
        shift
        target_dir="$1"
    fi
    shift
done

test -n "$target_dir"
artifact_dir="$target_dir/release"
if [ -n "${CARGO_BUILD_TARGET-}" ]; then
    artifact_dir="$target_dir/$CARGO_BUILD_TARGET/release"
fi
printf '%s\n%s' "$target_dir" "$artifact_dir" > "$FAKE_CARGO_LOG"
mkdir -p "$artifact_dir"
printf '#!/bin/sh\nexit 0\n' > "$artifact_dir/nybl_entry"
chmod +x "$artifact_dir/nybl_entry"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake_cargo).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&fake_cargo, permissions).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_nybl"))
        .arg("compile")
        .arg(&input)
        .arg("-o")
        .arg(&output_path)
        .env("PATH", path_with_fake_bin(&fake_bin))
        .env("CARGO_TARGET_DIR", &external_target)
        .env("CARGO_BUILD_TARGET", build_target)
        .env("FAKE_CARGO_LOG", &cargo_log)
        .output()
        .expect("run nybl compile");
    let stderr = String::from_utf8_lossy(&result.stderr);

    assert_eq!(result.status.code(), Some(0), "stderr:\n{stderr}");
    assert!(output_path.is_file(), "requested executable was not copied");

    let cargo_log = std::fs::read_to_string(&cargo_log).unwrap();
    let mut cargo_paths = cargo_log.lines().map(PathBuf::from);
    let explicit_target = cargo_paths
        .next()
        .expect("logged explicit target directory");
    let artifact_dir = cargo_paths.next().expect("logged Cargo artifact directory");
    assert!(
        cargo_paths.next().is_none(),
        "fake Cargo emitted an unexpected log shape"
    );
    assert_ne!(explicit_target, external_target);
    assert_eq!(
        explicit_target.file_name().and_then(|name| name.to_str()),
        Some("target")
    );
    assert_eq!(
        artifact_dir,
        explicit_target.join(build_target).join("release"),
        "process regression must exercise target/<triple>/release"
    );
    assert!(
        !explicit_target.exists(),
        "scratch directory should be cleaned after copy-out"
    );
    assert!(
        std::fs::read_dir(&external_target)
            .unwrap()
            .next()
            .is_none(),
        "ambient target directory should not receive build artifacts"
    );

    std::fs::remove_dir_all(project).expect("remove temporary project");
}

#[test]
fn compile_roots_cargo_config_discovery_at_the_invocation_directory() {
    let root = temp_project();
    let invocation_dir = root.join("user-project");
    let scratch_parent = root.join("scratch-parent");
    let fake_bin = root.join("fake-bin");
    let cargo_log = root.join("cargo-invocation.txt");
    let input = invocation_dir.join("entry.nybl");
    let output = invocation_dir.join("compiled-entry");
    let scratch_config = scratch_parent.join(".cargo/config.toml");
    let user_config = invocation_dir.join(".cargo/config.toml");
    std::fs::create_dir_all(scratch_config.parent().unwrap()).unwrap();
    std::fs::create_dir_all(user_config.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&fake_bin).unwrap();
    std::fs::write(
        &scratch_config,
        "[build]\n# inert scratch ancestor marker\n",
    )
    .unwrap();
    std::fs::write(&user_config, "[term]\n# intentional user config marker\n").unwrap();
    std::fs::write(&input, "print(\"compiled\")").unwrap();

    // Record only harmless process metadata. The scratch ancestor config is
    // never interpreted or given executable settings: the working directory
    // and explicit paths are sufficient to prove Cargo's discovery boundary.
    let fake_cargo = fake_bin.join("cargo");
    std::fs::write(
        &fake_cargo,
        r#"#!/bin/sh
set -eu
if [ "${1-}" = "--version" ]; then
    echo "cargo 1.88.0"
    exit 0
fi

{
    pwd -P
    printf '%s\n' "$@"
} > "$FAKE_CARGO_LOG"

target_dir=""
while [ "$#" -gt 0 ]; do
    if [ "$1" = "--target-dir" ]; then
        shift
        target_dir="$1"
    fi
    shift
done

test -n "$target_dir"
mkdir -p "$target_dir/release"
printf '#!/bin/sh\nexit 0\n' > "$target_dir/release/nybl_entry"
chmod +x "$target_dir/release/nybl_entry"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake_cargo).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&fake_cargo, permissions).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_nybl"))
        .arg("compile")
        .arg("entry.nybl")
        .arg("-o")
        .arg("compiled-entry")
        .arg("--keep")
        .current_dir(&invocation_dir)
        .env("PATH", path_with_fake_bin(&fake_bin))
        .env("TMPDIR", &scratch_parent)
        .env("FAKE_CARGO_LOG", &cargo_log)
        .output()
        .expect("run nybl compile");
    let stderr = String::from_utf8_lossy(&result.stderr);

    assert_eq!(result.status.code(), Some(0), "stderr:\n{stderr}");
    assert!(output.is_file(), "relative output should use the user cwd");

    let recorded: Vec<_> = std::fs::read_to_string(&cargo_log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(recorded.len(), 7, "unexpected Cargo command: {recorded:?}");

    let canonical_invocation_dir = std::fs::canonicalize(&invocation_dir).unwrap();
    let canonical_scratch_parent = std::fs::canonicalize(&scratch_parent).unwrap();
    let manifest_path = PathBuf::from(&recorded[4]);
    let target_dir = PathBuf::from(&recorded[6]);
    assert_eq!(PathBuf::from(&recorded[0]), canonical_invocation_dir);
    assert_eq!(
        &recorded[1..],
        &[
            "build",
            "--release",
            "--manifest-path",
            recorded[4].as_str(),
            "--target-dir",
            recorded[6].as_str(),
        ]
    );
    assert!(manifest_path.is_absolute());
    assert!(target_dir.is_absolute());
    assert_eq!(manifest_path.file_name().unwrap(), "Cargo.toml");
    assert_eq!(target_dir, manifest_path.parent().unwrap().join("target"));
    assert!(
        manifest_path.starts_with(&canonical_scratch_parent),
        "generated manifest should remain under the selected temp hierarchy"
    );
    assert!(
        scratch_config.is_file(),
        "the inert config marker must remain above the scratch leaf"
    );
    assert!(
        user_config.is_file(),
        "Cargo should still discover intentional config from the user cwd"
    );
    assert!(
        !canonical_invocation_dir.starts_with(&canonical_scratch_parent),
        "scratch ancestry must not be part of Cargo config discovery"
    );

    std::fs::remove_dir_all(root).expect("remove temporary project");
}

#[test]
fn extensionless_source_uses_distinct_default_binary_name() {
    let project = temp_project();
    let fake_bin = project.join("fake-bin");
    let input = project.join("program");
    let output = project.join("program-bin");
    let original_source = "print(\"source remains intact\")";
    std::fs::create_dir_all(&fake_bin).unwrap();
    std::fs::write(&input, original_source).unwrap();

    let fake_cargo = fake_bin.join("cargo");
    std::fs::write(
        &fake_cargo,
        r#"#!/bin/sh
set -eu
if [ "${1-}" = "--version" ]; then
    echo "cargo 1.88.0"
    exit 0
fi

target_dir=""
while [ "$#" -gt 0 ]; do
    if [ "$1" = "--target-dir" ]; then
        shift
        target_dir="$1"
    fi
    shift
done

test -n "$target_dir"
mkdir -p "$target_dir/release"
printf '#!/bin/sh\nexit 0\n' > "$target_dir/release/nybl_program"
chmod +x "$target_dir/release/nybl_program"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake_cargo).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&fake_cargo, permissions).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_nybl"))
        .arg("compile")
        .arg("program")
        .current_dir(&project)
        .env("PATH", path_with_fake_bin(&fake_bin))
        .output()
        .expect("compile extensionless source");
    let stderr = String::from_utf8_lossy(&result.stderr);

    assert_eq!(result.status.code(), Some(0), "stderr:\n{stderr}");
    assert_eq!(std::fs::read_to_string(&input).unwrap(), original_source);
    assert!(output.is_file(), "default output should be `program-bin`");
    assert!(
        stderr.contains("built program-bin"),
        "unexpected stderr:\n{stderr}"
    );

    std::fs::remove_dir_all(project).expect("remove temporary project");
}

#[test]
fn explicit_output_resolving_to_input_is_rejected_before_cargo() {
    let project = temp_project();
    let fake_bin = project.join("fake-bin");
    let cargo_marker = project.join("cargo-was-called");
    let input = project.join("program.nybl");
    let original_source = "print(\"source remains intact\")";
    std::fs::create_dir_all(&fake_bin).unwrap();
    std::fs::write(&input, original_source).unwrap();

    let fake_cargo = fake_bin.join("cargo");
    std::fs::write(
        &fake_cargo,
        "#!/bin/sh\nprintf called > \"$FAKE_CARGO_MARKER\"\nexit 0\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake_cargo).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&fake_cargo, permissions).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_nybl"))
        .arg("compile")
        .arg("program.nybl")
        .arg("-o")
        .arg("./program.nybl")
        .current_dir(&project)
        .env("PATH", path_with_fake_bin(&fake_bin))
        .env("FAKE_CARGO_MARKER", &cargo_marker)
        .output()
        .expect("reject colliding output");
    let stderr = String::from_utf8_lossy(&result.stderr);

    assert_eq!(result.status.code(), Some(1), "stderr:\n{stderr}");
    assert_eq!(std::fs::read_to_string(&input).unwrap(), original_source);
    assert!(
        !cargo_marker.exists(),
        "output collision must be rejected before Cargo is invoked"
    );
    assert!(
        stderr.contains("refusing to overwrite the source"),
        "unexpected stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("choose a different output path with `-o <path>`"),
        "unexpected stderr:\n{stderr}"
    );

    std::fs::remove_dir_all(project).expect("remove temporary project");
}
