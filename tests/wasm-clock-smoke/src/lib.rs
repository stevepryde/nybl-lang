//! Runtime smoke module for `nybl-sys` on `wasm32-unknown-unknown`.
//!
//! CI invokes the exported function with wasmtime. Returning normally proves
//! the unsupported clock path produced the documented Nybl error without a
//! wasm trap; any unexpected success, error, or missing dispatch traps.

use nybl::NyblHost;
use nybl_sys::StandardHost;

#[unsafe(no_mangle)]
pub extern "C" fn nybl_sys_clock_smoke() {
    let mut host = StandardHost::new();
    for name in ["unix_time", "unix_time_ms"] {
        let Some(Err(error)) = host.call(name, &[], 1) else {
            panic!("{name} did not return the expected unsupported-clock error");
        };
        assert_eq!(
            error.message,
            "system clock is unavailable on wasm32-unknown-unknown; provide time through a custom NyblHost"
        );
    }
}
