use nybl::{NyblError, NyblHost, NyblLimits, Value};

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

const SOURCE: &str = r#"
let total = 0

pub fn add(amount) {
    total += amount
    return total
}
"#;

fn call_walker(host: &mut Host, limits: &NyblLimits) -> Result<(), NyblError> {
    let mut instance = nybl::NyblInstance::load(SOURCE, host, limits)?;
    assert_eq!(instance.entry_points()[0].name(), "add");
    assert_eq!(instance.entry_points()[0].arity(), 1);
    assert_eq!(
        instance
            .call("add", &[Value::Int(4)], host)?
            .to_rust::<i64>()
            .unwrap(),
        4,
    );
    assert_eq!(
        instance
            .call("add", &[Value::Int(5)], host)?
            .to_rust::<i64>()
            .unwrap(),
        9,
    );
    Ok(())
}

fn call_vm(host: &mut Host, limits: &NyblLimits) -> Result<(), NyblError> {
    let mut instance = nybl_vm::NyblInstance::load(SOURCE, host, limits)?;
    assert_eq!(
        instance
            .call("add", &[Value::Int(4)], host)?
            .to_rust::<i64>()
            .unwrap(),
        4,
    );
    assert_eq!(
        instance
            .call("add", &[Value::Int(5)], host)?
            .to_rust::<i64>()
            .unwrap(),
        9,
    );
    Ok(())
}

fn run_example() -> Result<(), NyblError> {
    let limits = NyblLimits::standard();
    let mut host = Host;
    call_walker(&mut host, &limits)?;
    call_vm(&mut host, &limits)
}

fn main() {
    run_example().expect("persistent-instance example failed");
}

#[cfg(test)]
mod tests {
    #[test]
    fn state_persists_on_both_engines() {
        super::run_example().unwrap();
    }
}
