use nybl::{NyblError, NyblHost, NyblLimits, Value};

#[derive(Default)]
struct Host {
    output: Vec<String>,
}

impl NyblHost for Host {
    fn call(&mut self, name: &str, args: &[Value], line: u32) -> Option<Result<Value, NyblError>> {
        match (name, args) {
            ("double", [Value::Int(value)]) => Some(
                value
                    .checked_mul(2)
                    .map(Value::Int)
                    .ok_or_else(|| NyblError::runtime("double(value) overflowed", line)),
            ),
            ("double", _) => Some(Err(NyblError::runtime(
                "double(value) expects one Int",
                line,
            ))),
            _ => None,
        }
    }

    fn on_print(&mut self, message: &str) {
        self.output.push(message.to_owned());
    }

    fn function_hint(&self) -> &str {
        "Host functions: double(value)"
    }
}

fn run_example() -> Result<(), NyblError> {
    let source = "print(double(21))";
    let limits = NyblLimits::standard();
    let mut host = Host::default();

    nybl::run(source, &mut host, &limits)?;
    nybl_vm::run(source, &mut host, &limits)?;

    assert_eq!(host.output, ["42", "42"]);
    Ok(())
}

fn main() {
    run_example().expect("custom-host example failed");
}

#[cfg(test)]
mod tests {
    #[test]
    fn custom_host_runs_on_both_engines() {
        super::run_example().unwrap();
    }

    #[test]
    fn host_arithmetic_reports_overflow_without_panicking() {
        let mut host = super::Host::default();
        let error = nybl::run(
            "double(9223372036854775807)",
            &mut host,
            &nybl::NyblLimits::standard(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("overflowed"));
    }
}
