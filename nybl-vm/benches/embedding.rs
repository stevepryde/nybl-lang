//! Embedding hot-path benchmarks (issue #185).
//!
//! Measures the costs a host engine pays when it drives Nybl scripts per
//! entity per tick: `instance.call()` dispatch, `NyblHost::call` round-trips,
//! a representative game-tick workload, Rust <-> `Value` conversion, and
//! one-shot `load` cost. Every engine-sensitive benchmark runs on both the
//! tree-walker (`nybl::NyblInstance`) and the bytecode VM
//! (`nybl_vm::NyblInstance`).
//!
//! Run with `cargo bench -p nybl-vm --bench embedding`. CI runs
//! `cargo bench -p nybl-vm --bench embedding -- --test` as a smoke check.
//! See `nybl-vm/benches/README.md` for recorded numbers and analysis.

use std::collections::BTreeMap;
use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use nybl::{IntoValue, NyblError, NyblHost, NyblLimits, Value};

/// Entities simulated by the `game_tick` benchmark.
const ENTITY_COUNT: usize = 100;

/// Generous limits so benchmarks measure engine overhead, not budget
/// enforcement. Both engines reset the step budget on every `call`.
fn bench_limits() -> NyblLimits {
    NyblLimits {
        max_steps: 100_000_000,
        max_memory: 100 * 1024 * 1024,
    }
}

// ─── Scripts ────────────────────────────────────────────────────────────────

/// The per-entity dispatch floor: an entry point that does nothing.
const TRIVIAL_SRC: &str = "
pub fn tick(a, b) {
}
";

/// One host call per invocation. Subtracting the `call_trivial` floor gives
/// the `NyblHost::call` round-trip cost including name-string dispatch.
const HOST_CALL_SRC: &str = "
pub fn tick(a, b) {
    return field_get(a, b)
}
";

/// Representative per-entity behavior: a small state machine that reads 5
/// fields, branches, writes 2-3 fields, and queues one command — roughly ten
/// host-boundary crossings per tick. Field indices: 0=hp, 1=x, 2=y, 3=state,
/// 4=target_x.
const GAME_TICK_SRC: &str = "
pub fn tick(id) {
    let hp = field_get(id, 0)
    let x = field_get(id, 1)
    let y = field_get(id, 2)
    let state = field_get(id, 3)
    let target = field_get(id, 4)
    if state == 0 {
        // Wander toward the target, retreating when hurt.
        if x < target {
            field_set(id, 1, x + 1)
        } else {
            field_set(id, 4, target + 16)
        }
        if hp < 20 {
            field_set(id, 3, 1)
        } else {
            field_set(id, 2, y + 1)
        }
        queue_command(id, 1, x, y)
    } else {
        // Recover, then return to wandering.
        if hp < 90 {
            field_set(id, 0, hp + 5)
        } else {
            field_set(id, 3, 0)
        }
        field_set(id, 1, x - 1)
        queue_command(id, 2, x, y)
    }
    return hp
}
";

// ─── Host ───────────────────────────────────────────────────────────────────

/// Fields per entity: hp, x, y, state, target_x.
const FIELD_COUNT: usize = 5;

/// A game-engine-shaped host: entity fields in flat storage plus a command
/// queue, dispatched by matching on the function name string like real
/// embedders do. The hot arms are deliberately not first in the match.
struct GameHost {
    fields: Vec<[i64; FIELD_COUNT]>,
    commands: Vec<[i64; 4]>,
}

impl GameHost {
    fn new(entities: usize) -> Self {
        Self {
            fields: (0..entities)
                .map(|id| [100, (id as i64) % 13, 0, (id as i64) % 2, 24])
                .collect(),
            commands: Vec::with_capacity(entities),
        }
    }
}

fn arg_int(args: &[Value], index: usize, line: u32) -> Result<i64, NyblError> {
    match args.get(index) {
        Some(Value::Int(value)) => Ok(*value),
        _ => Err(NyblError::runtime("expected Int argument", line)),
    }
}

impl NyblHost for GameHost {
    fn call(&mut self, name: &str, args: &[Value], line: u32) -> Option<Result<Value, NyblError>> {
        match name {
            "spawn_entity" => {
                self.fields.push([100, 0, 0, 0, 24]);
                Some(Ok(Value::Int(self.fields.len() as i64 - 1)))
            }
            "entity_count" => Some(Ok(Value::Int(self.fields.len() as i64))),
            "field_get" => Some((|| {
                let id = arg_int(args, 0, line)? as usize;
                let field = arg_int(args, 1, line)? as usize;
                self.fields
                    .get(id)
                    .and_then(|fields| fields.get(field))
                    .map(|value| Value::Int(*value))
                    .ok_or_else(|| NyblError::runtime("field_get out of range", line))
            })()),
            "field_set" => Some((|| {
                let id = arg_int(args, 0, line)? as usize;
                let field = arg_int(args, 1, line)? as usize;
                let value = arg_int(args, 2, line)?;
                self.fields
                    .get_mut(id)
                    .and_then(|fields| fields.get_mut(field))
                    .map(|slot| {
                        *slot = value;
                        Value::None
                    })
                    .ok_or_else(|| NyblError::runtime("field_set out of range", line))
            })()),
            "queue_command" => Some((|| {
                let id = arg_int(args, 0, line)?;
                let kind = arg_int(args, 1, line)?;
                let x = arg_int(args, 2, line)?;
                let y = arg_int(args, 3, line)?;
                self.commands.push([id, kind, x, y]);
                Ok(Value::None)
            })()),
            _ => None,
        }
    }
}

// ─── Engine abstraction ─────────────────────────────────────────────────────

/// The two embeddable engines share an API shape but not a type; this trait
/// lets every benchmark body be written once.
trait Engine {
    const NAME: &'static str;
    type Instance;

    fn load(source: &str, host: &mut dyn NyblHost, limits: &NyblLimits) -> Self::Instance;
    fn call(
        instance: &mut Self::Instance,
        name: &str,
        args: &[Value],
        host: &mut dyn NyblHost,
    ) -> Result<Value, NyblError>;
}

struct Walker;

impl Engine for Walker {
    const NAME: &'static str = "walker";
    type Instance = nybl::NyblInstance;

    fn load(source: &str, host: &mut dyn NyblHost, limits: &NyblLimits) -> Self::Instance {
        nybl::NyblInstance::load(source, host, limits).expect("walker load failed")
    }

    fn call(
        instance: &mut Self::Instance,
        name: &str,
        args: &[Value],
        host: &mut dyn NyblHost,
    ) -> Result<Value, NyblError> {
        instance.call(name, args, host)
    }
}

struct Vm;

impl Engine for Vm {
    const NAME: &'static str = "vm";
    type Instance = nybl_vm::NyblInstance;

    fn load(source: &str, host: &mut dyn NyblHost, limits: &NyblLimits) -> Self::Instance {
        nybl_vm::NyblInstance::load(source, host, limits).expect("vm load failed")
    }

    fn call(
        instance: &mut Self::Instance,
        name: &str,
        args: &[Value],
        host: &mut dyn NyblHost,
    ) -> Result<Value, NyblError> {
        instance.call(name, args, host)
    }
}

/// Load a script and prove one call succeeds before benching, so a broken
/// script fails loudly instead of skewing warmup.
fn load_checked<E: Engine>(
    source: &str,
    host: &mut GameHost,
    args: &[Value],
) -> (E::Instance, NyblLimits) {
    let limits = bench_limits();
    let mut instance = E::load(source, host, &limits);
    E::call(&mut instance, "tick", args, host).expect("sanity call failed");
    (instance, limits)
}

// ─── Benchmarks ─────────────────────────────────────────────────────────────

fn bench_call_trivial<E: Engine>(c: &mut Criterion) {
    let mut host = GameHost::new(1);
    let args = [Value::Int(0), Value::Int(1)];
    let (mut instance, _limits) = load_checked::<E>(TRIVIAL_SRC, &mut host, &args);
    c.bench_function(&format!("call_trivial/{}", E::NAME), |b| {
        b.iter(|| {
            let result = E::call(&mut instance, "tick", black_box(&args), &mut host)
                .expect("call_trivial failed");
            black_box(result)
        })
    });
}

fn bench_host_call_roundtrip<E: Engine>(c: &mut Criterion) {
    let mut host = GameHost::new(1);
    let args = [Value::Int(0), Value::Int(1)];
    let (mut instance, _limits) = load_checked::<E>(HOST_CALL_SRC, &mut host, &args);
    c.bench_function(&format!("host_call_roundtrip/{}", E::NAME), |b| {
        b.iter(|| {
            let result = E::call(&mut instance, "tick", black_box(&args), &mut host)
                .expect("host_call_roundtrip failed");
            black_box(result)
        })
    });
}

fn bench_game_tick<E: Engine>(c: &mut Criterion) {
    let mut host = GameHost::new(ENTITY_COUNT);
    let sanity_args = [Value::Int(0)];
    let (mut instance, _limits) = load_checked::<E>(GAME_TICK_SRC, &mut host, &sanity_args);
    let args: Vec<[Value; 1]> = (0..ENTITY_COUNT as i64)
        .map(|id| [Value::Int(id)])
        .collect();
    c.bench_function(&format!("game_tick_100_entities/{}", E::NAME), |b| {
        b.iter(|| {
            // Draining the command queue is part of a real engine tick.
            host.commands.clear();
            for entity_args in &args {
                let result = E::call(&mut instance, "tick", entity_args, &mut host)
                    .expect("game_tick failed");
                black_box(result);
            }
            black_box(host.commands.len())
        })
    });
}

fn bench_load<E: Engine>(c: &mut Criterion) {
    let limits = bench_limits();
    c.bench_function(&format!("load_game_tick/{}", E::NAME), |b| {
        b.iter_batched(
            || GameHost::new(ENTITY_COUNT),
            |mut host| black_box(E::load(black_box(GAME_TICK_SRC), &mut host, &limits)),
            BatchSize::SmallInput,
        )
    });
}

fn bench_value_conversion(c: &mut Criterion) {
    let mut group = c.benchmark_group("value_conversion");

    group.bench_function("i64_into_value", |b| {
        b.iter(|| black_box(123_i64).into_value().expect("i64 into_value"))
    });
    let int_value = Value::Int(123);
    group.bench_function("i64_from_value", |b| {
        b.iter(|| {
            black_box(&int_value)
                .to_rust::<i64>()
                .expect("i64 from_value")
        })
    });

    let dict: BTreeMap<String, i64> = [
        ("hp".to_string(), 100),
        ("x".to_string(), 7),
        ("y".to_string(), 3),
        ("state".to_string(), 1),
        ("target_x".to_string(), 24),
    ]
    .into_iter()
    .collect();
    group.bench_function("dict5_into_value", |b| {
        b.iter_batched(
            || dict.clone(),
            |dict| dict.into_value().expect("dict into_value"),
            BatchSize::SmallInput,
        )
    });
    let dict_value = dict.clone().into_value().expect("dict into_value");
    group.bench_function("dict5_from_value", |b| {
        b.iter(|| {
            black_box(&dict_value)
                .to_rust::<BTreeMap<String, i64>>()
                .expect("dict from_value")
        })
    });

    let array: Vec<i64> = (0..10).collect();
    group.bench_function("array10_into_value", |b| {
        b.iter_batched(
            || array.clone(),
            |array| array.into_value().expect("array into_value"),
            BatchSize::SmallInput,
        )
    });
    let array_value = array.clone().into_value().expect("array into_value");
    group.bench_function("array10_from_value", |b| {
        b.iter(|| {
            black_box(&array_value)
                .to_rust::<Vec<i64>>()
                .expect("array from_value")
        })
    });

    group.finish();
}

fn all_benches(c: &mut Criterion) {
    bench_call_trivial::<Walker>(c);
    bench_call_trivial::<Vm>(c);
    bench_host_call_roundtrip::<Walker>(c);
    bench_host_call_roundtrip::<Vm>(c);
    bench_game_tick::<Walker>(c);
    bench_game_tick::<Vm>(c);
    bench_value_conversion(c);
    bench_load::<Walker>(c);
    bench_load::<Vm>(c);
}

criterion_group!(benches, all_benches);
criterion_main!(benches);
