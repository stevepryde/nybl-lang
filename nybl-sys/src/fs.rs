use std::io::Write;
use std::path::Path;

use nybl::{NyblError, Value};

use crate::args::{expect_args, expect_string};
use crate::error::runtime;

pub(crate) fn read_file(args: &[Value], line: u32) -> Result<Value, NyblError> {
    expect_args("read_file", args, 1, line)?;
    let path = expect_string("read_file", &args[0], line)?;

    std::fs::read_to_string(path)
        .map(Value::new_str)
        .map_err(|e| runtime(line, format!("read_file failed for `{path}`: {e}")))
}

pub(crate) fn write_file(args: &[Value], line: u32) -> Result<Value, NyblError> {
    expect_args("write_file", args, 2, line)?;
    let path = expect_string("write_file", &args[0], line)?;
    let contents = expect_string("write_file", &args[1], line)?;

    std::fs::write(path, contents)
        .map(|_| Value::None)
        .map_err(|e| runtime(line, format!("write_file failed for `{path}`: {e}")))
}

pub(crate) fn append_file(args: &[Value], line: u32) -> Result<Value, NyblError> {
    expect_args("append_file", args, 2, line)?;
    let path = expect_string("append_file", &args[0], line)?;
    let contents = expect_string("append_file", &args[1], line)?;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| runtime(line, format!("append_file failed for `{path}`: {e}")))?;

    file.write_all(contents.as_bytes())
        .map(|_| Value::None)
        .map_err(|e| runtime(line, format!("append_file failed for `{path}`: {e}")))
}

pub(crate) fn file_exists(args: &[Value], line: u32) -> Result<Value, NyblError> {
    expect_args("file_exists", args, 1, line)?;
    let path = expect_string("file_exists", &args[0], line)?;

    Ok(Value::Bool(Path::new(path).exists()))
}
