//! Process-level regressions for stdout readers that close early.

#![cfg(unix)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_SOURCE_ID: AtomicU64 = AtomicU64::new(0);

struct TempSource(PathBuf);

impl TempSource {
    fn new(source: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let id = TEMP_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nybl-broken-pipe-{}-{nonce}-{id}.nybl",
            std::process::id()
        ));
        std::fs::write(&path, source).expect("write temporary Nybl source");
        Self(path)
    }
}

impl Drop for TempSource {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn close_stdout_then_release_stdin(mut command: Command, stdin: &[u8]) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn nybl");

    // The child blocks on stdin before it prints. Close the only stdout reader
    // first, then release it, making the subsequent EPIPE deterministic.
    drop(child.stdout.take().expect("child stdout"));
    let mut child_stdin = child.stdin.take().expect("child stdin");
    child_stdin.write_all(stdin).expect("release child stdin");
    drop(child_stdin);

    child.wait_with_output().expect("wait for nybl")
}

fn assert_graceful_broken_pipe(output: Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "closed stdout should terminate successfully; stderr:\n{stderr}"
    );
    assert_ne!(output.status.code(), Some(101));
    assert!(
        !stderr.contains("panicked at") && !stderr.contains("stack backtrace"),
        "closed stdout leaked a Rust panic/backtrace:\n{stderr}"
    );
}

fn run_file_with_closed_stdout(no_vm: bool) -> Output {
    let source = TempSource::new("readline()\nprint(\"reader closed\")\n");
    let mut command = Command::new(env!("CARGO_BIN_EXE_nybl"));
    command.arg("run");
    if no_vm {
        command.arg("--novm");
    }
    command.arg(&source.0);
    close_stdout_then_release_stdin(command, b"\n")
}

#[test]
fn vm_run_handles_closed_stdout_without_panicking() {
    assert_graceful_broken_pipe(run_file_with_closed_stdout(false));
}

#[test]
fn walker_run_handles_closed_stdout_without_panicking() {
    assert_graceful_broken_pipe(run_file_with_closed_stdout(true));
}

#[test]
fn repl_print_handles_closed_stdout_without_panicking() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nybl"));
    command.arg("repl");
    assert_graceful_broken_pipe(close_stdout_then_release_stdin(
        command,
        b"print(\"reader closed\")\n",
    ));
}
