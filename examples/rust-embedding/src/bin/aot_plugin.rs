use nybl::{NyblError, NyblHost, NyblLimits, Value};

include!(concat!(env!("OUT_DIR"), "/plugin.rs"));

struct Host;

impl NyblHost for Host {
    fn call(
        &mut self,
        _name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, NyblError>> {
        None
    }
}

fn run_example() -> Result<(), NyblError> {
    let mut host = Host;
    let mut instance = plugin::NyblInstance::load(&mut host, &NyblLimits::standard())?;

    assert_eq!(
        instance
            .call("next", &[], &mut host)?
            .to_rust::<i64>()
            .unwrap(),
        1,
    );
    assert_eq!(
        instance
            .call("next", &[], &mut host)?
            .to_rust::<i64>()
            .unwrap(),
        2,
    );
    Ok(())
}

fn main() {
    run_example().expect("AOT plugin example failed");
}

#[cfg(test)]
mod tests {
    #[test]
    fn generated_plugin_compiles_and_persists_state() {
        super::run_example().unwrap();
    }
}
