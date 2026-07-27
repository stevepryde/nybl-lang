//! `nybl compile FILE` — AOT-transpile a script and build a
//! native binary.
//!
//! Pipeline:
//!
//! 1. Read source, set up a module resolver that layers
//!    filesystem-relative imports on top of `nybl-std`'s bundled
//!    stdlib (same surface `nybl run` sees via `nybl-sys`).
//! 2. `nybl_compile::transpile(source, opts)` → Rust source
//!    string.
//! 3. If `--emit-rs`: write that string to `-o OUT` (or
//!    `FILE.rs` beside the input) and stop.
//! 4. Otherwise: drop the Rust source into a scratch cargo
//!    project under the OS temp dir, declare `nybl-lang` /
//!    `nybl-sys` / `nybl-std` as deps at the current `nybl`
//!    version, run `cargo build --release`, and copy the
//!    produced binary to `-o OUT` (or the input file's stem;
//!    extensionless inputs use a `-bin` suffix).
//!
//! Errors:
//!
//! - Missing `cargo` on PATH → print a pointer to rustup + the
//!   `--emit-rs` escape hatch.
//! - Transpiler / cargo failures → surface stderr verbatim and
//!   return non-zero.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nybl_compile::{ModuleResolver, Options, transpile};

const SCRATCH_CREATE_ATTEMPTS: usize = 128;
static NEXT_SCRATCH_DIR: AtomicUsize = AtomicUsize::new(0);

pub fn compile_file(input: &str, output: Option<&str>, emit_rs: bool, keep: bool) -> ExitCode {
    let input_path = PathBuf::from(input);
    let source = match std::fs::read_to_string(&input_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading `{input}`: {e}");
            return ExitCode::from(1);
        }
    };
    let output_path = match output {
        Some(path) => PathBuf::from(path),
        None if emit_rs => default_rs_path(&input_path),
        None => default_binary_path(&input_path),
    };
    if let Err(message) = ensure_distinct_source_and_output(&input_path, &output_path) {
        eprintln!("{message}");
        return ExitCode::from(1);
    }

    // Build the resolver the transpiler feeds every `use`
    // through. Mirrors `nybl-sys::StdHost::resolve_module`: look
    // in `nybl-std` first for canonical stdlib names, then fall
    // back to a filesystem search rooted at the input's parent
    // directory so `use ./helpers` keeps working.
    let input_dir = crate::entry_path::module_root(&input_path);
    let resolver = make_resolver(input_dir);

    let opts = Options {
        emit_main: true,
        use_nybl_sys: true,
        sandbox: false,
        module_name: None,
        module_resolver: Some(resolver),
    };

    let rust_src = match transpile(&source, &opts) {
        Ok(s) => s,
        Err(e) => {
            eprint!("{}", e.render(&source));
            return ExitCode::from(1);
        }
    };

    if emit_rs {
        // Transpilation can take long enough for an existing output path to
        // be replaced, so repeat the source-preservation guard at write-out.
        if let Err(message) = ensure_distinct_source_and_output(&input_path, &output_path) {
            eprintln!("{message}");
            return ExitCode::from(1);
        }
        if let Err(e) = std::fs::write(&output_path, &rust_src) {
            eprintln!("error writing `{}`: {e}", output_path.display());
            return ExitCode::from(1);
        }
        eprintln!("wrote {}", output_path.display());
        return ExitCode::SUCCESS;
    }

    // Cargo config discovery is rooted at its working directory. Capture the
    // directory the user selected before creating scratch work so private
    // scratch storage cannot contribute configuration through its ancestors.
    let invocation_dir = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error reading the current directory: {error}");
            return ExitCode::from(1);
        }
    };

    // Cargo is the build driver — it's what ships with rustup,
    // and it handles the `nybl-lang` / `nybl-sys` deps that the
    // transpiled code needs. Check for its presence before
    // setting up scratch work so the failure mode is clean.
    if !cargo_available() {
        eprintln!(
            "error: couldn't find `cargo` on your PATH.\n\
             `nybl compile` needs a Rust toolchain to produce a native binary.\n\
             Install it from https://rustup.rs, or re-run with `--emit-rs`\n\
             to get the transpiled Rust source without building it."
        );
        return ExitCode::from(1);
    }

    let scratch = match build_native(&rust_src, &input_path, &output_path, &invocation_dir) {
        Ok(s) => s,
        Err(code) => return code,
    };

    if keep {
        eprintln!("scratch project kept at {}", scratch.display());
    } else {
        let _ = std::fs::remove_dir_all(&scratch);
    }

    eprintln!("built {}", output_path.display());
    ExitCode::SUCCESS
}

fn make_resolver(root: PathBuf) -> ModuleResolver {
    use std::cell::RefCell;
    use std::rc::Rc;

    Rc::new(RefCell::new(move |name: &str| {
        // `nybl-std` canonical stdlib modules first.
        if let Some(src) = nybl::stdlib::resolve(name) {
            return Some(Ok(src.to_string()));
        }
        // Filesystem fallback shares runtime resolution's path,
        // validation, and NotFound-vs-I/O-error contract.
        nybl_sys::resolve_module_from_root(&root, name)
    }))
}

/// Create a scratch cargo project, drop the transpiled Rust in,
/// `cargo build --release`, and copy the binary out. Returns the
/// scratch dir so the caller can preserve it for `--keep`.
fn build_native(
    rust_src: &str,
    input_path: &Path,
    output_path: &Path,
    invocation_dir: &Path,
) -> Result<PathBuf, ExitCode> {
    let stem = input_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("script");
    // Cargo package/bin names are identifiers, not display
    // names. Keep them independent from the user's filename so
    // spaces, quotes, Unicode, and other path-safe characters
    // cannot produce an invalid or malformed manifest.
    let target_name = cargo_target_name(stem);
    let scratch = match create_scratch_dir(&target_name) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("error creating scratch dir: {e}");
            return Err(ExitCode::from(1));
        }
    };
    if let Err(e) = std::fs::create_dir(scratch.join("src")) {
        eprintln!(
            "error creating scratch source dir `{}`: {e}",
            scratch.display()
        );
        return Err(ExitCode::from(1));
    }

    let manifest = manifest_for_output(&target_name);
    if let Err(e) = std::fs::write(scratch.join("Cargo.toml"), manifest) {
        eprintln!("error writing scratch Cargo.toml: {e}");
        return Err(ExitCode::from(1));
    }
    if let Err(e) = std::fs::write(scratch.join("src/main.rs"), rust_src) {
        eprintln!("error writing scratch main.rs: {e}");
        return Err(ExitCode::from(1));
    }

    // Keep Cargo's artifact location independent of ambient
    // `CARGO_TARGET_DIR` and global Cargo configuration. The
    // explicit target dir makes the copy path below authoritative.
    //
    // Stdout goes through and stderr prints cargo's own diagnostics
    // if the build fails. We don't try to suppress or reformat them;
    // they're generally the right thing for a user to see.
    let status = cargo_build_command(&scratch, invocation_dir).status();
    let status = match status {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error invoking cargo: {e}");
            return Err(ExitCode::from(1));
        }
    };
    if !status.success() {
        eprintln!(
            "cargo build failed — generated Rust source is under {}",
            scratch.display()
        );
        return Err(ExitCode::from(1));
    }

    // Copy the produced binary to the user-facing output. Cargo uses
    // `target/release` by default and `target/<triple>/release` when a build
    // target is configured. The scratch root is private and fresh, so a
    // narrowly bounded scan can require exactly one regular-file candidate
    // without trusting an ambient target string as a path.
    let built = match find_built_binary(&scratch.join("target"), &target_name) {
        Ok(path) => path,
        Err(e) => {
            eprintln!(
                "error locating built binary `{target_name}` under `{}`: {e}",
                scratch.join("target").display()
            );
            return Err(ExitCode::from(1));
        }
    };
    // Re-check immediately before copy-out. The early preflight prevents
    // wasting a build on an invalid output path; this second guard also
    // protects the source if the output path was replaced while Cargo ran.
    if let Err(message) = ensure_distinct_source_and_output(input_path, output_path) {
        eprintln!("{message}");
        return Err(ExitCode::from(1));
    }
    if let Err(e) = std::fs::copy(&built, output_path) {
        eprintln!(
            "error copying built binary `{}` → `{}`: {e}",
            built.display(),
            output_path.display()
        );
        return Err(ExitCode::from(1));
    }
    // Make sure the output is executable on Unix. `copy`
    // preserves the source's permissions, which is usually fine
    // since cargo already set the executable bit — but be
    // defensive.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(output_path) {
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() | 0o111);
            let _ = std::fs::set_permissions(output_path, perms);
        }
    }

    Ok(scratch)
}

fn cargo_build_command(scratch: &Path, invocation_dir: &Path) -> Command {
    debug_assert!(scratch.is_absolute());
    debug_assert!(invocation_dir.is_absolute());

    let mut command = Command::new("cargo");
    command
        .arg("build")
        .arg("--release")
        .arg("--manifest-path")
        .arg(scratch.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(scratch.join("target"))
        .current_dir(invocation_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command
}

fn find_built_binary(target_dir: &Path, target_name: &str) -> std::io::Result<PathBuf> {
    let mut candidates = Vec::new();
    collect_binary_candidates(&target_dir.join("release"), target_name, &mut candidates)?;

    // A configured Cargo target inserts exactly one directory between the
    // target root and profile. Do not recurse or construct a path from
    // `CARGO_BUILD_TARGET`: only direct, real directories inside the private
    // scratch target root are eligible.
    for entry in std::fs::read_dir(target_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        collect_binary_candidates(&entry.path().join("release"), target_name, &mut candidates)?;
    }

    candidates.sort();
    candidates.dedup();
    match candidates.as_slice() {
        [built] => Ok(built.clone()),
        [] => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no executable found in a release artifact directory",
        )),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "multiple executable candidates found: {}",
                candidates
                    .iter()
                    .map(|path| format!("`{}`", path.display()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

fn collect_binary_candidates(
    profile_dir: &Path,
    target_name: &str,
    candidates: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    // Cargo names executables for the build target, not the host running this
    // CLI. Consider both native desktop forms so Unix → Windows and Windows →
    // Unix cross-target configurations work without deriving behavior from an
    // ambient target string.
    for file_name in [target_name.to_string(), format!("{target_name}.exe")] {
        let candidate = profile_dir.join(file_name);
        if is_regular_file(&candidate)? {
            candidates.push(candidate);
        }
    }
    Ok(())
}

fn is_regular_file(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn cargo_available() -> bool {
    Command::new("cargo")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn default_rs_path(input: &Path) -> PathBuf {
    let mut p = input.to_path_buf();
    p.set_extension("rs");
    p
}

fn default_binary_path(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .unwrap_or_else(|| std::ffi::OsStr::new("script"));
    let mut file_name = stem.to_os_string();
    if input.extension().is_none() {
        file_name.push("-bin");
    }
    let mut p = PathBuf::from(file_name);
    if cfg!(windows) {
        p.set_extension("exe");
    }
    p
}

/// Reject output paths that identify the source itself.
///
/// Canonical paths catch relative aliases and symlinks. Existing-file
/// identities additionally catch hard links, whose canonical path text is
/// different even though copying over them would truncate the source inode.
fn ensure_distinct_source_and_output(input: &Path, output: &Path) -> Result<(), String> {
    match paths_resolve_to_same_file(input, output) {
        Ok(false) => Ok(()),
        Ok(true) => Err(format!(
            "error: output path `{}` resolves to the input file `{}`; refusing to overwrite the source\n\
             choose a different output path with `-o <path>`",
            output.display(),
            input.display()
        )),
        Err(error) => Err(format!(
            "error checking output path `{}` against input `{}`: {error}",
            output.display(),
            input.display()
        )),
    }
}

fn paths_resolve_to_same_file(input: &Path, output: &Path) -> std::io::Result<bool> {
    match same_file::is_same_file(input, output) {
        Ok(is_same) => Ok(is_same),
        // A missing output is the normal pre-build state and cannot yet
        // identify the existing source.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Atomically claim a fresh, private scratch project root.
///
/// The path must not be accepted when it already exists: Cargo automatically
/// discovers files such as `build.rs`, so reusing a directory an attacker can
/// populate would execute their code during `cargo build`. Candidate names are
/// deliberately hard to collide with, but the security boundary is the atomic
/// non-recursive `create`, not the secrecy of the name.
fn create_scratch_dir(stem: &str) -> std::io::Result<PathBuf> {
    let parent = std::env::temp_dir();
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_SCRATCH_DIR.fetch_add(1, Ordering::Relaxed) as u128;
    let pid = std::process::id();

    let candidates = (0..SCRATCH_CREATE_ATTEMPTS).map(|attempt| {
        let nonce = started_at
            .wrapping_add(sequence << 64)
            .wrapping_add(attempt as u128);
        parent.join(format!("nybl-compile-{stem}-{pid}-{nonce:032x}"))
    });
    // `std::env::temp_dir` is normally absolute, but canonicalizing the
    // directory we just created makes the paths passed to Cargo absolute even
    // when a platform or test-specific temp setting is relative.
    claim_first_available_scratch_dir(candidates).and_then(std::fs::canonicalize)
}

fn claim_first_available_scratch_dir(
    candidates: impl IntoIterator<Item = PathBuf>,
) -> std::io::Result<PathBuf> {
    for candidate in candidates {
        match create_private_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("`{}`: {error}", candidate.display()),
                ));
            }
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("couldn't claim a new scratch directory after {SCRATCH_CREATE_ATTEMPTS} attempts"),
    ))
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::DirBuilder::new().create(path)
    }
}

/// Derive a guaranteed-safe internal Cargo package and binary
/// name from the user-visible filename. The `nybl_` prefix keeps
/// leading digits and Rust keywords valid; limiting the body to
/// ASCII identifier characters also prevents TOML quoting and
/// platform-specific filename surprises.
fn cargo_target_name(stem: &str) -> String {
    let mut body = String::with_capacity(stem.len().min(48));
    let mut previous_was_separator = false;

    for ch in stem.chars() {
        if body.len() == 48 {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            body.push(ch.to_ascii_lowercase());
            previous_was_separator = false;
        } else if !body.is_empty() && !previous_was_separator {
            body.push('_');
            previous_was_separator = true;
        }
    }

    while body.ends_with('_') {
        body.pop();
    }
    if body.is_empty() {
        body.push_str("script");
    }

    format!("nybl_{body}")
}

/// Manifest for the scratch cargo crate we feed the transpiled
/// Rust into. Declares `nybl-lang` + `nybl-sys` at the current
/// `nybl` CLI's version — the CLI and libraries ship together,
/// so by construction the deps are always in lockstep. The
/// bundled stdlib comes for free via the `nybl-std` feature
/// (default on) on both deps — no separate crate line needed.
///
/// The generated crate carries `[workspace]` on its own so
/// cargo doesn't try to adopt it as a member of some surrounding
/// project the user happens to be sitting inside when they run
/// `nybl compile`.
///
/// # Local development
///
/// Setting `NYBL_DEV_WORKSPACE=/path/to/nybl-repo` points the
/// generated manifest at path-based deps pointing into that
/// workspace, so `nybl compile` works against the uncommitted
/// library code. Published builds leave the env var unset and
/// resolve against crates.io.
fn manifest_for_output(stem: &str) -> String {
    let dev_workspace = std::env::var("NYBL_DEV_WORKSPACE")
        .ok()
        .filter(|root| !root.is_empty());
    manifest_for_output_with_workspace(stem, dev_workspace.as_deref())
}

fn manifest_for_output_with_workspace(stem: &str, dev_workspace: Option<&str>) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let exact_version = format!("={version}");
    let deps = match dev_workspace {
        Some(root) => {
            let nybl_path = toml_basic_string_contents(&format!("{root}/nybl"));
            let nybl_sys_path = toml_basic_string_contents(&format!("{root}/nybl-sys"));
            format!(
                r#"nybl = {{ path = "{nybl_path}", version = "{exact_version}", package = "nybl-lang" }}
nybl-sys = {{ path = "{nybl_sys_path}", version = "{exact_version}" }}"#,
            )
        }
        _ => format!(
            r#"nybl = {{ version = "{exact_version}", package = "nybl-lang" }}
nybl-sys = "{exact_version}""#,
        ),
    };
    format!(
        r#"[package]
name = "{stem}"
version = "0.0.0"
edition = "2024"
publish = false

[[bin]]
name = "{stem}"
path = "src/main.rs"

[dependencies]
{deps}

[workspace]

[profile.release]
# Small + fast enough: matches what a hand-written Rust user
# would reach for when building a CLI. LTO trims the AOT-emitted
# dispatch noise without pushing build time into the stratosphere.
opt-level = 3
lto = "thin"
"#
    )
}

/// Encode string contents for a TOML basic string.
///
/// Generated development manifests place filesystem paths between `"` quotes,
/// so Windows separators, quotes, and TOML-forbidden control characters must
/// be escaped without changing ordinary Unicode path text.
fn toml_basic_string_contents(value: &str) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            '\u{0008}' => encoded.push_str("\\b"),
            '\t' => encoded.push_str("\\t"),
            '\n' => encoded.push_str("\\n"),
            '\u{000C}' => encoded.push_str("\\f"),
            '\r' => encoded.push_str("\\r"),
            '\u{0000}'..='\u{001F}' | '\u{007F}' => {
                write!(encoded, "\\u{:04X}", ch as u32).expect("writing into a String cannot fail");
            }
            _ => encoded.push(ch),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    fn resolver_test_root(label: &str) -> PathBuf {
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nybl_cli_resolver_{}_{}_{}",
            std::process::id(),
            label,
            sequence
        ));
        match std::fs::remove_dir_all(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("stale test resolver root should be removable: {error}"),
        }
        std::fs::create_dir_all(&root).expect("test resolver root should be created");
        root
    }

    fn resolve_once(
        resolver: &ModuleResolver,
        name: &str,
    ) -> Option<Result<String, nybl::NyblError>> {
        (resolver.borrow_mut())(name)
    }

    #[test]
    fn cargo_target_name_sanitizes_user_filename_stems() {
        assert_eq!(cargo_target_name("my prog"), "nybl_my_prog");
        assert_eq!(cargo_target_name("quoted\" name"), "nybl_quoted_name");
        assert_eq!(cargo_target_name("123 🚀"), "nybl_123");
        assert_eq!(cargo_target_name("\" 🚀 \""), "nybl_script");
        assert_eq!(cargo_target_name("type"), "nybl_type");
    }

    #[test]
    fn manifest_uses_only_the_safe_internal_target_name() {
        let unsafe_stem = "my \"program\"";
        let target_name = cargo_target_name(unsafe_stem);
        let manifest = manifest_for_output(&target_name);

        assert!(manifest.contains("name = \"nybl_my_program\""));
        assert!(!manifest.contains(unsafe_stem));
    }

    #[test]
    fn cargo_build_command_isolates_scratch_config_discovery() {
        let root = resolver_test_root("cargo_command");
        let scratch = root.join("scratch-project");
        let invocation_dir = root.join("user-project");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::create_dir_all(&invocation_dir).unwrap();
        let scratch = std::fs::canonicalize(scratch).unwrap();
        let invocation_dir = std::fs::canonicalize(invocation_dir).unwrap();
        let command = cargo_build_command(&scratch, &invocation_dir);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            args,
            vec![
                "build".to_string(),
                "--release".to_string(),
                "--manifest-path".to_string(),
                scratch.join("Cargo.toml").to_string_lossy().into_owned(),
                "--target-dir".to_string(),
                scratch.join("target").to_string_lossy().into_owned(),
            ]
        );
        assert_eq!(
            command.get_current_dir(),
            Some(invocation_dir.as_path()),
            "Cargo config discovery must start from the user's directory"
        );
        assert!(scratch.join("Cargo.toml").is_absolute());
        assert!(scratch.join("target").is_absolute());

        let _ = std::fs::remove_dir_all(root);
    }

    fn write_test_binary(profile_dir: &Path, file_name: &str) -> PathBuf {
        std::fs::create_dir_all(profile_dir).unwrap();
        let binary = profile_dir.join(file_name);
        std::fs::write(&binary, "test executable").unwrap();
        binary
    }

    #[test]
    fn built_binary_locator_accepts_default_release_layout() {
        let root = resolver_test_root("default_artifact");
        let target_dir = root.join("target");
        let expected = write_test_binary(&target_dir.join("release"), "nybl_script");

        assert_eq!(
            find_built_binary(&target_dir, "nybl_script").unwrap(),
            expected
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn built_binary_locator_accepts_target_triple_release_layout() {
        let root = resolver_test_root("triple_artifact");
        let target_dir = root.join("target");
        let expected = write_test_binary(
            &target_dir.join("test-target-triple").join("release"),
            "nybl_script",
        );

        assert_eq!(
            find_built_binary(&target_dir, "nybl_script").unwrap(),
            expected
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn built_binary_locator_accepts_cross_target_windows_executable() {
        let root = resolver_test_root("windows_cross_target_artifact");
        let target_dir = root.join("target");
        let expected = write_test_binary(
            &target_dir.join("x86_64-pc-windows-msvc").join("release"),
            "nybl_script.exe",
        );

        assert_eq!(
            find_built_binary(&target_dir, "nybl_script").unwrap(),
            expected
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn built_binary_locator_rejects_ambiguous_candidates() {
        let root = resolver_test_root("ambiguous_artifact");
        let target_dir = root.join("target");
        write_test_binary(&target_dir.join("release"), "nybl_script");
        write_test_binary(
            &target_dir.join("test-target-triple").join("release"),
            "nybl_script",
        );

        let error = find_built_binary(&target_dir, "nybl_script")
            .expect_err("multiple candidates must not be selected arbitrarily");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("multiple executable candidates"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn built_binary_locator_does_not_follow_symlink_candidates() {
        use std::os::unix::fs::symlink;

        let root = resolver_test_root("symlink_artifact");
        let target_dir = root.join("target");
        let outside = write_test_binary(&root.join("outside"), "outside");
        let candidate = target_dir.join("release").join("nybl_script");
        std::fs::create_dir_all(candidate.parent().unwrap()).unwrap();
        symlink(outside, candidate).unwrap();

        let error = find_built_binary(&target_dir, "nybl_script")
            .expect_err("symlinks must not qualify as Cargo-produced executables");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn registry_manifest_exactly_pins_lockstep_runtime_crates() {
        let exact_version = format!("={}", env!("CARGO_PKG_VERSION"));
        let manifest = manifest_for_output_with_workspace("nybl_script", None);

        assert!(manifest.contains(&format!(
            r#"nybl = {{ version = "{exact_version}", package = "nybl-lang" }}"#
        )));
        assert!(manifest.contains(&format!(r#"nybl-sys = "{exact_version}""#)));
    }

    #[test]
    fn development_manifest_keeps_paths_and_exact_version_guards() {
        let exact_version = format!("={}", env!("CARGO_PKG_VERSION"));
        let manifest =
            manifest_for_output_with_workspace("nybl_script", Some("/workspace/nybl-lang"));

        assert!(manifest.contains(&format!(
            r#"nybl = {{ path = "/workspace/nybl-lang/nybl", version = "{exact_version}", package = "nybl-lang" }}"#
        )));
        assert!(manifest.contains(&format!(
            r#"nybl-sys = {{ path = "/workspace/nybl-lang/nybl-sys", version = "{exact_version}" }}"#
        )));
    }

    #[test]
    fn toml_path_encoder_escapes_windows_separators_and_quotes() {
        assert_eq!(
            toml_basic_string_contents(r#"C:\Users\Steve "Nybl""#),
            "C:\\\\Users\\\\Steve \\\"Nybl\\\""
        );
        assert_eq!(
            toml_basic_string_contents("D:/Böp 🚀"),
            "D:/Böp 🚀",
            "ordinary Unicode path text should be preserved"
        );
    }

    #[test]
    fn toml_path_encoder_escapes_forbidden_control_characters() {
        let controls = "\0\u{0001}\u{0008}\t\n\u{000C}\r\u{001F}\u{007F}";
        assert_eq!(
            toml_basic_string_contents(controls),
            r"\u0000\u0001\b\t\n\f\r\u001F\u007F"
        );
    }

    #[test]
    fn development_manifest_serializes_windows_shaped_paths() {
        let manifest =
            manifest_for_output_with_workspace("nybl_script", Some(r#"C:\Users\Steve "Nybl""#));

        assert!(
            manifest.contains(r##"path = "C:\\Users\\Steve \"Nybl\"/nybl""##),
            "nybl path was not encoded as a TOML basic string:\n{manifest}"
        );
        assert!(
            manifest.contains(r##"path = "C:\\Users\\Steve \"Nybl\"/nybl-sys""##),
            "nybl-sys path was not encoded as a TOML basic string:\n{manifest}"
        );
    }

    #[test]
    fn default_output_path_preserves_the_user_visible_stem() {
        assert_eq!(
            default_binary_path(Path::new("my program.nybl")),
            PathBuf::from(if cfg!(windows) {
                "my program.exe"
            } else {
                "my program"
            })
        );
    }

    #[test]
    fn extensionless_source_gets_a_distinct_default_output_path() {
        assert_eq!(
            default_binary_path(Path::new("my program")),
            PathBuf::from(if cfg!(windows) {
                "my program-bin.exe"
            } else {
                "my program-bin"
            })
        );
    }

    #[test]
    fn output_collision_detects_path_aliases() {
        let root = resolver_test_root("output_alias");
        let input = root.join("program");
        std::fs::write(&input, "print(\"preserved\")").unwrap();
        let output_alias = root.join(".").join("program");

        assert!(paths_resolve_to_same_file(&input, &output_alias).unwrap());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_output_does_not_collide_with_the_source() {
        let root = resolver_test_root("missing_output");
        let input = root.join("program");
        std::fs::write(&input, "print(\"preserved\")").unwrap();

        assert!(!paths_resolve_to_same_file(&input, &root.join("program-bin")).unwrap());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn output_collision_detects_hard_links() {
        let root = resolver_test_root("output_hard_link");
        let input = root.join("program");
        let output = root.join("program-link");
        std::fs::write(&input, "print(\"preserved\")").unwrap();
        std::fs::hard_link(&input, &output).unwrap();

        assert!(paths_resolve_to_same_file(&input, &output).unwrap());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn scratch_creation_never_reuses_an_attacker_controlled_directory() {
        let root = resolver_test_root("scratch_collision");
        let injected = root.join("attacker-claimed");
        let fresh = root.join("nybl-claimed");
        std::fs::create_dir(&injected).unwrap();
        std::fs::write(
            injected.join("build.rs"),
            "compile_error!(\"attacker build script executed\");",
        )
        .unwrap();

        let scratch = claim_first_available_scratch_dir([injected.clone(), fresh.clone()]).unwrap();

        assert_eq!(scratch, fresh);
        assert!(!scratch.join("build.rs").exists());
        assert_eq!(
            std::fs::read_to_string(injected.join("build.rs")).unwrap(),
            "compile_error!(\"attacker build script executed\");"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn scratch_creation_uses_private_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = resolver_test_root("scratch_permissions");
        let scratch = claim_first_available_scratch_dir([root.join("nybl-claimed")]).unwrap();
        let mode = std::fs::metadata(&scratch).unwrap().permissions().mode();

        assert_eq!(mode & 0o077, 0, "scratch mode was {mode:o}");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compile_resolver_reads_filesystem_modules() {
        let root = resolver_test_root("success");
        let module_dir = root.join("math");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(module_dir.join("util.nybl"), "let answer = 42").unwrap();

        let resolver = make_resolver(root.clone());
        let source = resolve_once(&resolver, "math.util")
            .expect("module should be handled")
            .expect("module should be readable");
        assert_eq!(source, "let answer = 42");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compile_resolver_returns_none_only_for_not_found() {
        let root = resolver_test_root("missing");
        let resolver = make_resolver(root.clone());

        assert!(resolve_once(&resolver, "does_not_exist").is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compile_resolver_surfaces_non_not_found_read_errors() {
        let root = resolver_test_root("read_error");
        // The resolver expects a file here. A directory is deterministic and
        // unreadable as text without relying on permissions (which root can
        // bypass in CI containers).
        std::fs::create_dir(root.join("broken.nybl")).unwrap();
        let resolver = make_resolver(root.clone());

        let error = resolve_once(&resolver, "broken")
            .expect("non-NotFound failures must be handled")
            .expect_err("directory read must surface as an I/O error");
        assert!(
            error.message.contains("couldn't read module `broken`"),
            "unexpected error: {}",
            error.message
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compile_resolver_prefers_bundled_stdlib_over_filesystem() {
        let root = resolver_test_root("stdlib_precedence");
        let std_dir = root.join("std");
        std::fs::create_dir_all(&std_dir).unwrap();
        std::fs::write(std_dir.join("math.nybl"), "let shadow = true").unwrap();
        let resolver = make_resolver(root.clone());

        let source = resolve_once(&resolver, "std.math")
            .expect("stdlib module should be handled")
            .expect("stdlib module should resolve");
        assert_eq!(source, nybl::stdlib::resolve("std.math").unwrap());
        assert!(!source.contains("let shadow = true"));

        let _ = std::fs::remove_dir_all(root);
    }
}
