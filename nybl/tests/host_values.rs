use std::cell::Cell;

use nybl::{FromValue, HostValue, IntoValue, NyblError, NyblHost, NyblInstance, NyblLimits, Value};

#[derive(Default)]
struct HandleHost {
    prints: Vec<String>,
    method_calls: usize,
}

impl NyblHost for HandleHost {
    fn call(&mut self, name: &str, args: &[Value], line: u32) -> Option<Result<Value, NyblError>> {
        match (name, args) {
            ("make_counter", []) => Some(Ok(Value::new_host("counter", Cell::new(0_i64)))),
            ("make_counter", _) => Some(Err(NyblError::runtime(
                "make_counter() expects no arguments",
                line,
            ))),
            _ => None,
        }
    }

    fn call_method(
        &mut self,
        receiver: &HostValue,
        method: &str,
        args: &[Value],
        line: u32,
    ) -> Option<Result<Value, NyblError>> {
        let counter = receiver.downcast_ref::<Cell<i64>>()?;
        self.method_calls += 1;
        Some(match (method, args) {
            ("get", []) => Ok(Value::Int(counter.get())),
            ("add", [Value::Int(amount)]) => {
                counter.set(counter.get() + amount);
                Ok(Value::Int(counter.get()))
            }
            // Deliberately collides with a one-argument built-in array method.
            // Host methods have their own dynamic arity.
            ("push", [Value::Int(a), Value::Int(b)]) => {
                counter.set(counter.get() + a + b);
                Ok(Value::Int(counter.get()))
            }
            ("get" | "add" | "push", _) => {
                Err(NyblError::runtime("invalid counter arguments", line))
            }
            _ => return None,
        })
    }

    fn on_print(&mut self, message: &str) {
        self.prints.push(message.to_owned());
    }
}

#[test]
fn walker_dispatches_host_methods_after_common_methods() {
    let mut host = HandleHost::default();
    nybl::run(
        r#"
let counter = make_counter()
print(counter.type())
print(counter.inspect())
print(counter.add(2))
print(counter.push(3, 4))
let failed = try_call(fn() {
  counter.add(5)
  panic("stop")
})
print(counter.get())
"#,
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();

    assert_eq!(host.prints, ["counter", "<host counter>", "2", "9", "14"]);
    assert_eq!(host.method_calls, 4);
}

#[test]
fn unknown_host_methods_use_the_normal_method_error() {
    let mut host = HandleHost::default();
    let error = nybl::run(
        "let counter = make_counter()\ncounter.missing()",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap_err();

    assert!(error.message.contains("counter"));
    assert!(error.message.contains("missing"));
}

#[test]
fn persistent_instances_retain_handles_but_not_hosts() {
    let source = r#"
let counter = make_counter()
pub fn add(amount) { return counter.add(amount) }
pub fn get() { return counter.get() }
"#;
    let mut loading_host = HandleHost::default();
    let mut instance =
        NyblInstance::load(source, &mut loading_host, &NyblLimits::standard()).unwrap();

    let mut calling_host = HandleHost::default();
    assert_eq!(
        instance
            .call("add", &[Value::Int(7)], &mut calling_host)
            .unwrap()
            .inspect(),
        "7"
    );
    assert_eq!(
        instance
            .call("get", &[], &mut calling_host)
            .unwrap()
            .inspect(),
        "7"
    );
    assert_eq!(calling_host.method_calls, 2);
}

#[test]
fn host_values_round_trip_through_conversion_traits_by_identity() {
    let handle = HostValue::new("token", String::from("secret"));
    let value = (&handle).into_value().unwrap();

    let borrowed: &HostValue = FromValue::from_value(&value).unwrap();
    let owned: HostValue = value.to_rust().unwrap();
    assert!(handle.ptr_eq(borrowed));
    assert!(handle.ptr_eq(&owned));
    assert_eq!(
        borrowed.downcast_ref::<String>().map(String::as_str),
        Some("secret")
    );
}
