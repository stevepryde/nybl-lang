use std::io::{self, Write};
use std::path::{Path, PathBuf};

use nybl::{NyblError, NyblHost, Value};

#[derive(Debug, Clone)]
struct PrintFailure {
    kind: io::ErrorKind,
    message: String,
}

/// Standard host for running Nybl programs in a normal OS process.
#[derive(Debug, Clone, Default)]
pub struct StandardHost {
    /// Root directory used to resolve `use` paths. When `None`
    /// the current working directory at resolve time is used.
    module_root: Option<PathBuf>,
    print_failure: Option<PrintFailure>,
}

/// Short name for the standard host.
pub use StandardHost as StdHost;

/// Resolve one dot-separated module name beneath a filesystem root.
///
/// This is the filesystem-only half of [`NyblHost::resolve_module`],
/// exposed so compile-time resolvers can share the same validation, path
/// mapping, and I/O semantics as runtime resolution. Callers that bundle the
/// Nybl standard library should check [`nybl::stdlib::resolve`] first.
///
/// A missing module returns `None`. Every other read failure is returned as a
/// real [`NyblError`], so permission, directory, and device errors cannot be
/// mistaken for "module not found".
pub fn resolve_module_from_root(root: &Path, name: &str) -> Option<Result<String, NyblError>> {
    if let Err(error) = validate_module_name(name) {
        return Some(Err(error));
    }

    resolve_validated_module_from_root(root, name)
}

fn resolve_validated_module_from_root(
    root: &Path,
    name: &str,
) -> Option<Result<String, NyblError>> {
    let mut path = root.to_path_buf();
    for segment in name.split('.') {
        path.push(segment);
    }
    path.set_extension("nybl");
    match std::fs::read_to_string(&path) {
        Ok(source) => Some(Ok(source)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => Some(Err(crate::error::io_error(
            &format!("couldn't read module `{name}`: {err}"),
            None,
        ))),
    }
}

impl StandardHost {
    /// Create a host that resolves filesystem modules relative to the current
    /// working directory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure the root directory for module resolution. An
    /// `use foo.bar` then maps to `<root>/foo/bar.nybl`.
    pub fn with_module_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.module_root = Some(root.into());
        self
    }

    /// Whether stdout's reader closed while this host was printing.
    ///
    /// Command-line entry points use this to distinguish normal pipeline
    /// termination from other stdout failures after the runtime unwinds.
    pub fn is_broken_pipe(&self) -> bool {
        self.print_failure
            .as_ref()
            .is_some_and(|failure| failure.kind == io::ErrorKind::BrokenPipe)
    }

    fn write_stdout_line(message: &str) -> io::Result<()> {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        writeln!(output, "{message}")?;
        output.flush()
    }

    fn record_print_failure(&mut self, error: io::Error) {
        if self.print_failure.is_none() {
            self.print_failure = Some(PrintFailure {
                kind: error.kind(),
                message: error.to_string(),
            });
        }
    }
}

impl NyblHost for StandardHost {
    fn call(&mut self, name: &str, args: &[Value], line: u32) -> Option<Result<Value, NyblError>> {
        match name {
            "readline" => Some(crate::stdio::readline(args, line)),
            "read_file" => Some(crate::fs::read_file(args, line)),
            "write_file" => Some(crate::fs::write_file(args, line)),
            "append_file" => Some(crate::fs::append_file(args, line)),
            "file_exists" => Some(crate::fs::file_exists(args, line)),
            "env" => Some(crate::env::env(args, line)),
            "unix_time" => Some(crate::time::unix_time(args, line)),
            "unix_time_ms" => Some(crate::time::unix_time_ms(args, line)),
            _ => None,
        }
    }

    fn on_print(&mut self, message: &str) {
        if self.print_failure.is_some() {
            return;
        }
        if let Err(error) = Self::write_stdout_line(message) {
            self.record_print_failure(error);
        }
    }

    fn print_error(&self, line: u32) -> Option<NyblError> {
        self.print_failure.as_ref().map(|failure| {
            let message = if failure.kind == io::ErrorKind::BrokenPipe {
                "stdout pipe closed".to_string()
            } else {
                format!("failed to write to stdout: {}", failure.message)
            };
            NyblError::fatal(message, line)
        })
    }

    fn function_hint(&self) -> &str {
        enabled_function_hint()
    }

    /// Resolve `use foo.bar.baz`:
    ///
    /// 1. **Stdlib first** — when the `nybl-std` feature is on
    ///    (default), `std.*` names hit [`nybl::stdlib::resolve`],
    ///    which returns bundled Nybl source. Stdlib modules
    ///    never touch the filesystem, so they work in every
    ///    host (including ones that set a `module_root` pointed
    ///    at a directory with no stdlib). When the feature is
    ///    off the step is skipped, so `std.*` falls through to
    ///    the filesystem like any other name.
    /// 2. **Filesystem fallback** — the rest map to
    ///    `<root>/foo/bar/baz.nybl`. Missing files return
    ///    `None` so the runtime can raise a clean
    ///    *module not found* error; I/O errors (e.g.
    ///    permissions) are surfaced as `Some(Err(...))`.
    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        #[cfg(feature = "nybl-std")]
        {
            if let Some(src) = nybl::stdlib::resolve(name) {
                return Some(Ok(src.to_string()));
            }
        }
        if let Err(error) = validate_module_name(name) {
            return Some(Err(error));
        }
        let root = match self.module_root.clone() {
            Some(r) => r,
            None => match std::env::current_dir() {
                Ok(d) => d,
                Err(e) => {
                    return Some(Err(crate::error::io_error(
                        &format!("couldn't read current directory: {e}"),
                        None,
                    )));
                }
            },
        };
        resolve_validated_module_from_root(&root, name)
    }
}

/// Module-name validation: non-empty dot-separated segments of
/// `[A-Za-z0-9_]+`. Rejects leading/trailing/double dots and any
/// character that could escape the module root (`/`, `..`, NUL,
/// drive letters, …).
fn is_valid_module_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    name.split('.')
        .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

fn validate_module_name(name: &str) -> Result<(), NyblError> {
    if is_valid_module_name(name) {
        Ok(())
    } else {
        Err(crate::error::io_error(
            &format!("Invalid module name `{name}`"),
            None,
        ))
    }
}

fn enabled_function_hint() -> &'static str {
    "Available nybl-sys functions: readline(prompt?), read_file(path), write_file(path, contents), append_file(path, contents), file_exists(path), env(name), unix_time(), unix_time_ms()"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_host_does_not_handle_unknown_calls_by_default() {
        let mut host = StandardHost::new();

        assert!(host.call("unknown", &[], 1).is_none());
    }

    #[test]
    fn standard_host_exposes_function_hint() {
        let host = StandardHost::new();

        assert!(host.function_hint().contains("nybl-sys"));
    }

    #[test]
    fn standard_host_reads_and_writes_files() {
        let mut host = StandardHost::new();
        let path = temp_path("nybl_sys_file_test.txt");
        let path_value = Value::new_str(path.to_string_lossy().into_owned());

        host.call(
            "write_file",
            &[path_value.clone(), Value::new_str("hello".to_string())],
            1,
        )
        .expect("write_file should be handled")
        .expect("write_file should succeed");

        host.call(
            "append_file",
            &[path_value.clone(), Value::new_str(" world".to_string())],
            1,
        )
        .expect("append_file should be handled")
        .expect("append_file should succeed");

        let exists = host
            .call("file_exists", std::slice::from_ref(&path_value), 1)
            .expect("file_exists should be handled")
            .expect("file_exists should succeed");
        assert!(matches!(exists, Value::Bool(true)));

        let contents = host
            .call("read_file", std::slice::from_ref(&path_value), 1)
            .expect("read_file should be handled")
            .expect("read_file should succeed");
        assert_eq!(contents.to_string(), "hello world");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn standard_host_returns_none_for_missing_env_vars() {
        let mut host = StandardHost::new();
        let name = Value::new_str("NYBL_SYS_ENV_VAR_THAT_SHOULD_NOT_EXIST".to_string());

        let value = host
            .call("env", &[name], 1)
            .expect("env should be handled")
            .expect("env should succeed");

        assert!(matches!(value, Value::None));
    }

    #[test]
    fn standard_host_returns_unix_time() {
        let mut host = StandardHost::new();

        let value = host
            .call("unix_time_ms", &[], 1)
            .expect("unix_time_ms should be handled")
            .expect("unix_time_ms should succeed");

        match value {
            Value::Number(n) => assert!(n > 0.0),
            other => panic!("expected number, got {}", other.type_name()),
        }
    }

    #[test]
    fn standard_host_classifies_broken_pipe_as_a_fatal_print_error() {
        let mut host = StandardHost::new();
        host.record_print_failure(io::Error::new(io::ErrorKind::BrokenPipe, "reader closed"));

        assert!(host.is_broken_pipe());
        let error = host.print_error(7).expect("print failure");
        assert_eq!(error.message, "stdout pipe closed");
        assert_eq!(error.line, Some(7));
        assert!(error.is_fatal);
    }

    #[test]
    fn standard_host_surfaces_unrelated_stdout_failures() {
        let mut host = StandardHost::new();
        host.record_print_failure(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "write denied",
        ));

        assert!(!host.is_broken_pipe());
        let error = host.print_error(11).expect("print failure");
        assert_eq!(error.message, "failed to write to stdout: write denied",);
        assert_eq!(error.line, Some(11));
        assert!(error.is_fatal);
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("{}_{}", std::process::id(), name));
        path
    }

    #[test]
    fn resolve_module_maps_dotted_path_to_file() {
        let dir = std::env::temp_dir().join(format!("nybl_sys_resolve_{}", std::process::id()));
        let sub = dir.join("math");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("util.nybl");
        std::fs::write(&file, "let answer = 42").unwrap();

        let mut host = StandardHost::new().with_module_root(&dir);
        let source = host
            .resolve_module("math.util")
            .expect("module should resolve")
            .expect("should succeed");
        assert_eq!(source, "let answer = 42");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_module_missing_returns_none() {
        let dir =
            std::env::temp_dir().join(format!("nybl_sys_resolve_none_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut host = StandardHost::new().with_module_root(&dir);
        assert!(host.resolve_module("does_not_exist").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_module_surfaces_non_not_found_read_errors() {
        let dir =
            std::env::temp_dir().join(format!("nybl_sys_resolve_error_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("broken.nybl")).unwrap();

        let mut host = StandardHost::new().with_module_root(&dir);
        let error = host
            .resolve_module("broken")
            .expect("non-NotFound failures must be handled")
            .expect_err("reading a directory as module text must fail");
        assert!(
            error.message.contains("couldn't read module `broken`"),
            "unexpected error: {}",
            error.message
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_module_rejects_bad_names() {
        let mut host = StandardHost::new();
        // `..` is forbidden — prevents path traversal.
        let result = host.resolve_module("..").expect("should error");
        assert!(result.is_err());
        let result = host.resolve_module("foo/bar").expect("should error");
        assert!(result.is_err());
    }
}
