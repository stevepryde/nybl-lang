//! Argument parsing for the `nybl` CLI.
//!
//! Hand-rolled rather than via `clap` because the surface is
//! tiny (three subcommands, a handful of flags) and the `nybl`
//! binary is one of the things we publish — keeping the
//! dependency count down matters for install times and supply-
//! chain surface.

pub enum Command {
    /// `nybl` or `nybl repl` — launch the interactive REPL.
    Repl,
    /// `nybl run FILE [--novm]` — execute a script.
    Run {
        file: String,
        no_vm: bool,
    },
    /// `nybl compile FILE [-o OUT] [--emit-rs] [--keep]` —
    /// transpile to Rust and (by default) build a native binary.
    Compile {
        file: String,
        output: Option<String>,
        emit_rs: bool,
        keep: bool,
    },
    Help,
    Version,
}

pub fn parse(argv: &[String]) -> Result<Command, String> {
    // argv[0] is the binary name — skip it.
    let args: &[String] = if argv.is_empty() { &[] } else { &argv[1..] };

    match args.first().map(String::as_str) {
        None => Ok(Command::Repl),
        Some("repl") => {
            forbid_extras("repl", &args[1..])?;
            Ok(Command::Repl)
        }
        Some("run") => parse_run(&args[1..]),
        Some("compile") => parse_compile(&args[1..]),
        Some("--help") | Some("-h") | Some("help") => Ok(Command::Help),
        Some("--version") | Some("-V") => Ok(Command::Version),
        // Legacy convenience: `nybl FILE.nybl` still works as an
        // alias for `nybl run FILE.nybl`. Keeps scripts and
        // shebangs that predate the subcommand split from
        // breaking on upgrade.
        Some(first) if !first.starts_with('-') => Ok(Command::Run {
            file: first.to_string(),
            no_vm: false,
        }),
        Some(unknown) => Err(format!("unknown argument: {unknown}")),
    }
}

fn parse_run(rest: &[String]) -> Result<Command, String> {
    let mut no_vm = false;
    let mut file: Option<String> = None;
    for arg in rest {
        match arg.as_str() {
            "--novm" => no_vm = true,
            other if other.starts_with('-') => {
                return Err(format!("`run`: unknown flag `{other}`"));
            }
            other => {
                if let Some(previous) = file.as_ref() {
                    return Err(format!(
                        "`run`: only one script file accepted (got `{other}` after `{previous}`)"
                    ));
                }
                file = Some(other.to_string());
            }
        }
    }
    let file = file.ok_or_else(|| "`run`: missing script file".to_string())?;
    Ok(Command::Run { file, no_vm })
}

fn parse_compile(rest: &[String]) -> Result<Command, String> {
    let mut output: Option<String> = None;
    let mut emit_rs = false;
    let mut keep = false;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        match arg.as_str() {
            "--emit-rs" => emit_rs = true,
            "--keep" => keep = true,
            "-o" | "--output" => {
                i += 1;
                match rest.get(i) {
                    Some(v) => output = Some(v.clone()),
                    None => return Err(format!("`compile`: `{arg}` needs a path argument")),
                }
            }
            other if other.starts_with('-') => {
                return Err(format!("`compile`: unknown flag `{other}`"));
            }
            other => {
                if let Some(previous) = file.as_ref() {
                    return Err(format!(
                        "`compile`: only one script file accepted (got `{other}` after `{previous}`)"
                    ));
                }
                file = Some(other.to_string());
            }
        }
        i += 1;
    }
    let file = file.ok_or_else(|| "`compile`: missing script file".to_string())?;
    Ok(Command::Compile {
        file,
        output,
        emit_rs,
        keep,
    })
}

fn forbid_extras(name: &str, rest: &[String]) -> Result<(), String> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(format!("`{name}`: unexpected extra argument `{}`", rest[0]))
    }
}

pub fn print_usage() {
    eprintln!(
        "\
nybl — a small, dynamically-typed, embeddable language.

USAGE:
    nybl                         Open the REPL
    nybl run FILE [--novm]       Execute a .nybl script
                                --novm runs the walker instead of the VM
    nybl compile FILE [OPTIONS]  Transpile + build a native binary
    nybl repl                    Open the REPL (explicit)
    nybl --version               Print version
    nybl --help                  This message

COMPILE OPTIONS:
    -o, --output PATH   Output path (default: script name with no extension,
                        `-bin` for extensionless scripts, or .rs path when
                        --emit-rs is set)
    --emit-rs           Emit transpiled Rust source only; don't invoke cargo
    --keep              Keep the scratch cargo project after building
                        (useful for inspecting the generated code)

Examples:
    nybl                         # interactive REPL
    nybl hello.nybl               # quick run (alias for `nybl run hello.nybl`)
    nybl run hello.nybl           # same, explicit
    nybl run hello.nybl --novm    # use the walker
    nybl compile hello.nybl       # produces ./hello (native binary)
    nybl compile hello.nybl -o h  # produces ./h
    nybl compile --emit-rs hello.nybl -o hello.rs
"
    );
}
