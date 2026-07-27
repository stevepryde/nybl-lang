use std::string::String;

use nybl::NyblError;

/// Thin wrapper around [`NyblError::runtime`] with the argument order used
/// throughout nybl-sys (line first, message second).
pub(crate) fn runtime(line: u32, message: impl Into<String>) -> NyblError {
    NyblError::runtime(message, line)
}

/// Error helper for I/O and resolver failures where no particular
/// source line applies (e.g. a module-resolution error carries no
/// Nybl call site — it originates in the host).
pub(crate) fn io_error(message: &str, line: Option<u32>) -> NyblError {
    NyblError {
        line,
        column: None,
        message: message.to_string(),
        friendly_hint: None,
        source_context: None,
        is_fatal: false,
        is_try_return: false,
    }
}
