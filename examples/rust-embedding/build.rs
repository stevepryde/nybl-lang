use std::{env, fs, path::PathBuf};

use nybl_compile::{Options, transpile};

fn main() {
    let source_path = "src/plugin.nybl";
    println!("cargo:rerun-if-changed={source_path}");

    let source = fs::read_to_string(source_path).expect("read Nybl plugin source");
    let generated = transpile(
        &source,
        &Options {
            emit_main: false,
            use_nybl_sys: false,
            sandbox: true,
            module_name: Some("plugin".to_owned()),
            module_resolver: None,
            ..Options::default()
        },
    )
    .expect("transpile Nybl plugin");
    let generated =
        format!("#[allow(clippy::all, clippy::pedantic, clippy::nursery)]\n{generated}",);

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("plugin.rs");
    fs::write(output, generated).expect("write generated Rust");
}
