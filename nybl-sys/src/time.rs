use std::time::Duration;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::time::{SystemTime, UNIX_EPOCH};

use nybl::{NyblError, Value};

use crate::args::expect_args;
use crate::error::runtime;

pub(crate) fn unix_time(args: &[Value], line: u32) -> Result<Value, NyblError> {
    expect_args("unix_time", args, 0, line)?;

    Ok(Value::Number(unix_duration(line)?.as_secs_f64()))
}

pub(crate) fn unix_time_ms(args: &[Value], line: u32) -> Result<Value, NyblError> {
    expect_args("unix_time_ms", args, 0, line)?;

    Ok(Value::Number(unix_duration(line)?.as_millis() as f64))
}

fn unix_duration(line: u32) -> Result<Duration, NyblError> {
    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    {
        Err(runtime(
            line,
            "system clock is unavailable on wasm32-unknown-unknown; provide time through a custom NyblHost",
        ))
    }

    #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| runtime(line, format!("system clock is before Unix epoch: {e}")))
}
