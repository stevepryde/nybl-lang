use std::io::{self, Write};
use std::string::String;

use nybl::{NyblError, Value};

use crate::args::expect_string;
use crate::error::runtime;

pub(crate) fn readline(args: &[Value], line: u32) -> Result<Value, NyblError> {
    match args.len() {
        0 => {}
        1 => {
            print!("{}", expect_string("readline", &args[0], line)?);
            io::stdout()
                .flush()
                .map_err(|e| runtime(line, format!("readline failed to flush stdout: {e}")))?;
        }
        _ => {
            return Err(runtime(
                line,
                format!(
                    "`readline` expects 0 or 1 arguments, but got {}",
                    args.len()
                ),
            ));
        }
    }

    let mut input = String::new();
    let bytes = io::stdin()
        .read_line(&mut input)
        .map_err(|e| runtime(line, format!("readline failed: {e}")))?;

    if bytes == 0 {
        return Ok(Value::None);
    }

    trim_line_ending(&mut input);
    Ok(Value::new_str(input))
}

fn trim_line_ending(input: &mut String) {
    if input.ends_with('\n') {
        input.pop();
        if input.ends_with('\r') {
            input.pop();
        }
    }
}
