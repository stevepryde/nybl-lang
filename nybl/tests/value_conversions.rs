use std::collections::BTreeMap;

use nybl::value::{BUILTIN_MODULE_PATH, EnumPayload, MAX_VALUE_DEPTH, VALUE_DEPTH_ERROR_MESSAGE};
use nybl::{
    FromValue, IntoValue, NyblError, NyblHost, NyblLimits, Value, ValuePathSegment, nybl_value,
};

#[test]
fn scalar_from_conversions_are_exact_and_lossless() {
    assert!(matches!(Value::from(()), Value::None));
    assert!(matches!(Value::from(i64::MIN), Value::Int(i64::MIN)));
    assert!(matches!(Value::from(i64::MAX), Value::Int(i64::MAX)));
    assert!(matches!(Value::from(u32::MAX), Value::Int(value) if value == i64::from(u32::MAX)));
    assert!(matches!(Value::from(1.25_f32), Value::Number(value) if value == 1.25));
    assert!(matches!(Value::from(true), Value::Bool(true)));
    assert!(matches!(Value::from("hello"), Value::Str(ref value) if value.as_str() == "hello"));
    assert!(matches!(Value::from(None::<i64>), Value::None));
    assert!(matches!(Value::from(Some(7_i32)), Value::Int(7)));
}

#[test]
fn wide_integers_are_checked_without_truncation() {
    assert!(matches!(
        (i64::MAX as u64).into_value().unwrap(),
        Value::Int(i64::MAX)
    ));

    let error = u64::MAX.into_value().unwrap_err();
    assert_eq!(error.expected(), "an integer in Nybl's i64 range");
    assert!(error.actual().contains(&u64::MAX.to_string()));
    assert!(error.path().is_empty());

    let error = Value::Int(-1).to_rust::<u64>().unwrap_err();
    assert_eq!(error.expected(), "int in Rust `u64` range");
    assert_eq!(error.actual(), "integer -1");
}

#[test]
fn reverse_numeric_conversion_preserves_int_number_distinction() {
    assert_eq!(Value::Int(42).to_rust::<i64>().unwrap(), 42);
    assert_eq!(Value::Number(42.5).to_rust::<f64>().unwrap(), 42.5);
    assert_eq!(Value::Number(42.5).to_rust::<f32>().unwrap(), 42.5_f32);
    assert_eq!(
        Value::Number(42.0).to_rust::<i64>().unwrap_err().actual(),
        "number"
    );
    assert_eq!(Value::Int(42).to_rust::<f64>().unwrap_err().actual(), "int");
    let error = Value::Int(42).to_rust::<f32>().unwrap_err();
    assert_eq!(error.expected(), "number");
    assert_eq!(error.actual(), "int");
}

#[test]
fn f32_values_round_trip_through_number_including_ieee_specials() {
    for input in [
        0.0_f32,
        -0.0,
        0.1,
        f32::MIN_POSITIVE,
        f32::from_bits(1),
        f32::MIN,
        f32::MAX,
        f32::NEG_INFINITY,
        f32::INFINITY,
    ] {
        let output = Value::from(input).to_rust::<f32>().unwrap();
        assert_eq!(output.to_bits(), input.to_bits());
    }

    assert!(Value::from(f32::NAN).to_rust::<f32>().unwrap().is_nan());
}

#[test]
fn f32_extraction_accepts_finite_bounds_and_rejects_overflow() {
    assert_eq!(
        Value::Number(f64::from(f32::MIN)).to_rust::<f32>().unwrap(),
        f32::MIN
    );
    assert_eq!(
        Value::Number(f64::from(f32::MAX)).to_rust::<f32>().unwrap(),
        f32::MAX
    );

    let above_max = f64::from(f32::MAX) * (1.0 + f64::EPSILON);
    let error = Value::Number(above_max).to_rust::<f32>().unwrap_err();
    assert_eq!(
        error.expected(),
        "number representable in Rust `f32` without overflow"
    );
    assert_eq!(error.actual(), format!("number {above_max}"));
    assert!(error.path().is_empty());
    assert_eq!(
        error.to_string(),
        format!(
            "value conversion failed at $: expected number representable in Rust `f32` without overflow, got number {above_max}"
        )
    );

    let below_min = f64::from(f32::MIN) * (1.0 + f64::EPSILON);
    assert!(Value::Number(below_min).to_rust::<f32>().is_err());
    assert!(Value::Number(f64::MAX).to_rust::<f32>().is_err());
}

#[test]
fn macro_builds_nested_values_and_is_hygienic() {
    mod shadowed_names {
        pub struct Value;
        pub trait IntoValue {}
    }

    let _ = core::mem::size_of::<shadowed_names::Value>();
    fn assert_shadow_trait<T: shadowed_names::IntoValue>() {}
    struct Shadow;
    impl shadowed_names::IntoValue for Shadow {}
    assert_shadow_trait::<Shadow>();

    assert!(matches!(nybl_value!([]).unwrap(), Value::Array(ref values) if values.is_empty()));
    assert!(matches!(nybl_value!({}).unwrap(), Value::Dict(ref entries) if entries.is_empty()));

    let value = nybl_value!({
        "name": "Ada",
        "stats": { "hp": 100, "mp": 40 },
        "tags": ["engineer", none, "mathematician"],
    })
    .unwrap();

    let fields: BTreeMap<String, Value> = value.to_rust().unwrap();
    assert_eq!(fields["name"].to_rust::<&str>().unwrap(), "Ada");
    let stats: BTreeMap<String, i64> = fields["stats"].to_rust().unwrap();
    assert_eq!(
        stats,
        BTreeMap::from([("hp".into(), 100), ("mp".into(), 40)])
    );
    let tags: Vec<Option<String>> = fields["tags"].to_rust().unwrap();
    assert_eq!(
        tags,
        vec![Some("engineer".into()), None, Some("mathematician".into())]
    );
}

#[test]
fn nested_failures_report_a_structured_root_to_leaf_path() {
    let value = nybl_value!([{ "stats": { "hp": "oops" } }]).unwrap();
    let error = value
        .to_rust::<Vec<BTreeMap<String, BTreeMap<String, i64>>>>()
        .unwrap_err();

    assert_eq!(error.expected(), "int");
    assert_eq!(error.actual(), "string");
    assert_eq!(
        error.path(),
        &[
            ValuePathSegment::Index(0),
            ValuePathSegment::Key("stats".into()),
            ValuePathSegment::Key("hp".into()),
        ]
    );
    assert_eq!(
        error.to_string(),
        "value conversion failed at $[0][\"stats\"][\"hp\"]: expected int, got string"
    );
}

#[test]
fn btree_maps_are_deterministic_and_duplicate_nybl_keys_are_rejected() {
    let map = BTreeMap::from([("z", 1_i64), ("a", 2_i64)]);
    let value = map.into_value().unwrap();
    let Value::Dict(entries) = &value else {
        panic!("expected dict");
    };
    assert_eq!(
        entries
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        ["a", "z"]
    );

    let duplicate = Value::try_new_dict(
        vec![
            ("same".into(), Value::Int(1)),
            ("same".into(), Value::Int(2)),
        ],
        0,
    )
    .unwrap();
    let error = duplicate.to_rust::<BTreeMap<String, i64>>().unwrap_err();
    assert_eq!(error.path(), &[ValuePathSegment::Key("same".into())]);
    assert!(error.actual().contains("duplicate key"));
}

#[test]
fn btree_map_forward_failures_add_each_key_to_the_path_once() {
    let map = BTreeMap::from([("hp", u64::MAX)]);
    let error = map.into_value().unwrap_err();

    assert_eq!(error.path(), &[ValuePathSegment::Key("hp".into())]);
    assert_eq!(
        error.to_string(),
        format!(
            "value conversion failed at $[\"hp\"]: expected an integer in Nybl's i64 range, got integer {}",
            u64::MAX
        )
    );
}

#[test]
fn rust_results_round_trip_only_through_the_canonical_builtin_shape() {
    let value = Ok::<Vec<i64>, &str>(vec![1, 2, 3]).into_value().unwrap();
    let Value::EnumVariant(variant) = &value else {
        panic!("expected enum variant");
    };
    assert_eq!(variant.module_path(), BUILTIN_MODULE_PATH);
    assert_eq!(variant.type_name(), "Result");
    assert_eq!(variant.variant(), "Ok");
    assert!(matches!(variant.payload(), EnumPayload::Tuple(values) if values.len() == 1));
    assert_eq!(
        value.to_rust::<Result<Vec<i64>, &str>>().unwrap(),
        Ok(vec![1, 2, 3])
    );

    let error_value = Err::<i64, _>("borrowed").into_value().unwrap();
    assert_eq!(
        error_value.to_rust::<Result<i64, &str>>().unwrap(),
        Err("borrowed")
    );

    let user_result = Value::new_enum_tuple(
        "user.module".into(),
        "Result".into(),
        "Ok".into(),
        vec![Value::Int(1)],
    );
    assert!(user_result.to_rust::<Result<i64, String>>().is_err());

    let malformed = Value::new_enum_tuple(
        BUILTIN_MODULE_PATH.into(),
        "Result".into(),
        "Err".into(),
        Vec::new(),
    );
    let error = malformed.to_rust::<Result<i64, String>>().unwrap_err();
    assert_eq!(
        error.path(),
        &[ValuePathSegment::ResultVariant("Err".into())]
    );
}

#[test]
fn borrowed_extraction_does_not_copy_string_storage() {
    let value = Value::from("zero-copy");
    let Value::Str(storage) = &value else {
        panic!("expected string");
    };
    let borrowed: &str = FromValue::from_value(&value).unwrap();
    assert_eq!(borrowed.as_ptr(), storage.as_str().as_ptr());

    let cloned = value.to_rust::<Value>().unwrap();
    assert_eq!(cloned.to_rust::<&str>().unwrap(), "zero-copy");
}

#[test]
fn recursive_conversions_enforce_value_depth_without_panicking() {
    let mut value = Value::None;
    for _ in 0..MAX_VALUE_DEPTH {
        value = vec![value].into_value().unwrap();
    }

    let error = vec![value].into_value().unwrap_err();
    assert_eq!(error.expected(), "a Nybl value within runtime limits");
    assert_eq!(error.actual(), VALUE_DEPTH_ERROR_MESSAGE);
}

#[test]
fn conversions_fit_naturally_at_the_nybl_host_boundary() {
    #[derive(Default)]
    struct Host {
        output: Vec<String>,
    }

    impl NyblHost for Host {
        fn call(
            &mut self,
            name: &str,
            args: &[Value],
            line: u32,
        ) -> Option<Result<Value, NyblError>> {
            if name != "sum_values" {
                return None;
            }
            Some((|| {
                let input = args
                    .first()
                    .ok_or_else(|| NyblError::runtime("sum_values expects an array", line))?;
                let values: Vec<i64> = input
                    .to_rust()
                    .map_err(|error| NyblError::runtime(error.to_string(), line))?;
                values
                    .into_iter()
                    .sum::<i64>()
                    .into_value()
                    .map_err(|error| NyblError::runtime(error.to_string(), line))
            })())
        }

        fn on_print(&mut self, message: &str) {
            self.output.push(message.into());
        }
    }

    let mut host = Host::default();
    nybl::run(
        "print(sum_values([10, 20, 12]))",
        &mut host,
        &NyblLimits::standard(),
    )
    .unwrap();
    assert_eq!(host.output, ["42"]);
}
