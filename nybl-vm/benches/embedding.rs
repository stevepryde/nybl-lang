//! Embedding hot-path benchmarks (issue #185).
//!
//! Measures the costs a host engine pays when it drives Nybl scripts per
//! entity per tick: ordinary and prepared `instance.call()` dispatch,
//! 100 distinct cached entity-script instances driven by one worker host,
//! `NyblHost::call` round-trips, host-side and script-side batching of a
//! representative game-tick workload, Rust <-> `Value` conversion, and
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
        ..NyblLimits::standard()
    }
}

// ─── Scripts ────────────────────────────────────────────────────────────────

/// The per-entity dispatch floor: an entry point that does nothing.
const TRIVIAL_SRC: &str = "
pub fn tick(a, b) {
}
";

/// Empty callback with the same scalar argument shape as the cached entity
/// workload. Across 100 persistent instances this isolates public-entry and
/// executor-state handling from script execution and host calls.
const CACHED_TRIVIAL_SRC: &str = "
pub fn tick(dt) {
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

/// A persistent entity-script attachment model. The consumer binds the entity
/// in its worker-local host before each callback, so the script does not shuttle
/// an entity ID through every host call. `ticks` deliberately proves that each
/// cached instance owns mutable state across frames.
const CACHED_ENTITY_TICK_SRC: &str = "
let ticks = 0

pub fn tick(dt) {
    ticks += 1
    let hp = current_field_get(0)
    let x = current_field_get(1)
    let y = current_field_get(2)
    let state = current_field_get(3)
    let target = current_field_get(4)
    if state == 0 {
        if x < target {
            current_field_set(1, x + dt)
        } else {
            current_field_set(4, target + 16)
        }
        if hp < 20 {
            current_field_set(3, 1)
        } else {
            current_field_set(2, y + dt)
        }
        current_queue_command(1, x, y)
    } else {
        if hp < 90 {
            current_field_set(0, hp + 5)
        } else {
            current_field_set(3, 0)
        }
        current_field_set(1, x - dt)
        current_queue_command(2, x, y)
    }
    return ticks
}
";

/// Module-bearing equivalent used to measure K-instance startup. The root is
/// intentionally small while the helper carries enough real bytecode that
/// repeated module parsing and compilation is visible in the baseline.
const MODULE_BEARING_ROOT: &str = "
use bench.tick as behavior
pub fn tick(id) { return behavior.tick(id) }
";

const MODULE_TICK_SRC: &str = "
fn clamp(value, low, high) {
    if value < low { return low }
    if value > high { return high }
    return value
}

fn score(hp, x, y) {
    let position = x * 3 + y * 5
    return clamp(hp + position, 0, 10000)
}

fn tick(id) {
    let hp = field_get(id, 0)
    let x = field_get(id, 1)
    let y = field_get(id, 2)
    let next = score(hp, x, y)
    field_set(id, 4, next)
    return next
}
";

/// The same state machine and host crossings as `GAME_TICK_SRC`, expressed as
/// a script-level batch entry. This removes the per-entity public entry and
/// user-frame boundary without hiding host traffic.
const GAME_TICK_BATCH_SRC: &str = "
pub fn tick_batch(count) {
    for id in range(count) {
        let hp = field_get(id, 0)
        let x = field_get(id, 1)
        let y = field_get(id, 2)
        let state = field_get(id, 3)
        let target = field_get(id, 4)
        if state == 0 {
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
            if hp < 90 {
                field_set(id, 0, hp + 5)
            } else {
                field_set(id, 3, 0)
            }
            field_set(id, 1, x - 1)
            queue_command(id, 2, x, y)
        }
    }
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
    current_entity: Option<usize>,
}

impl GameHost {
    fn new(entities: usize) -> Self {
        Self {
            fields: (0..entities)
                .map(|id| [100, (id as i64) % 13, 0, (id as i64) % 2, 24])
                .collect(),
            commands: Vec::with_capacity(entities),
            current_entity: None,
        }
    }

    /// Start one engine-owned frame while retaining the ECS-like field cache.
    fn begin_frame(&mut self) {
        self.commands.clear();
        self.current_entity = None;
    }

    /// Bind an entity attachment before entering its persistent script.
    fn bind_entity(&mut self, entity: usize) {
        self.current_entity = Some(entity);
    }

    fn bound_entity(&self, line: u32) -> Result<usize, NyblError> {
        self.current_entity
            .ok_or_else(|| NyblError::runtime("no entity bound for script callback", line))
    }
}

fn arg_int(args: &[Value], index: usize, line: u32) -> Result<i64, NyblError> {
    match args.get(index) {
        Some(Value::Int(value)) => Ok(*value),
        _ => Err(NyblError::runtime("expected Int argument", line)),
    }
}

fn direct_host_int(host: &mut dyn NyblHost, name: &str, args: &[Value]) -> i64 {
    match host
        .call(black_box(name), black_box(args), 0)
        .expect("game host handles benchmark function")
    {
        Ok(Value::Int(value)) => value,
        Ok(other) => panic!("expected Int from game host, got {}", other.type_name()),
        Err(error) => panic!("game host call failed: {error}"),
    }
}

fn direct_host_none(host: &mut dyn NyblHost, name: &str, args: &[Value]) {
    match host
        .call(black_box(name), black_box(args), 0)
        .expect("game host handles benchmark function")
    {
        Ok(Value::None) => {}
        Ok(other) => panic!("expected None from game host, got {}", other.type_name()),
        Err(error) => panic!("game host call failed: {error}"),
    }
}

/// Execute the same field/branch/command work as `CACHED_ENTITY_TICK_SRC`
/// directly through the custom host's `Value` ABI. This is a lower bound for
/// consumer-owned host work: subtracting it from the script benchmark still
/// includes Nybl instruction execution and host-argument vector creation.
fn direct_custom_host_frame(host: &mut GameHost) {
    host.begin_frame();
    for entity in 0..ENTITY_COUNT {
        host.bind_entity(entity);
        let hp = direct_host_int(host, "current_field_get", &[Value::Int(0)]);
        let x = direct_host_int(host, "current_field_get", &[Value::Int(1)]);
        let y = direct_host_int(host, "current_field_get", &[Value::Int(2)]);
        let state = direct_host_int(host, "current_field_get", &[Value::Int(3)]);
        let target = direct_host_int(host, "current_field_get", &[Value::Int(4)]);
        if state == 0 {
            if x < target {
                direct_host_none(
                    host,
                    "current_field_set",
                    &[Value::Int(1), Value::Int(x + 1)],
                );
            } else {
                direct_host_none(
                    host,
                    "current_field_set",
                    &[Value::Int(4), Value::Int(target + 16)],
                );
            }
            if hp < 20 {
                direct_host_none(host, "current_field_set", &[Value::Int(3), Value::Int(1)]);
            } else {
                direct_host_none(
                    host,
                    "current_field_set",
                    &[Value::Int(2), Value::Int(y + 1)],
                );
            }
            direct_host_none(
                host,
                "current_queue_command",
                &[Value::Int(1), Value::Int(x), Value::Int(y)],
            );
        } else {
            if hp < 90 {
                direct_host_none(
                    host,
                    "current_field_set",
                    &[Value::Int(0), Value::Int(hp + 5)],
                );
            } else {
                direct_host_none(host, "current_field_set", &[Value::Int(3), Value::Int(0)]);
            }
            direct_host_none(
                host,
                "current_field_set",
                &[Value::Int(1), Value::Int(x - 1)],
            );
            direct_host_none(
                host,
                "current_queue_command",
                &[Value::Int(2), Value::Int(x), Value::Int(y)],
            );
        }
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
            "current_field_get" => Some((|| {
                let id = self.bound_entity(line)?;
                let field = arg_int(args, 0, line)? as usize;
                self.fields
                    .get(id)
                    .and_then(|fields| fields.get(field))
                    .map(|value| Value::Int(*value))
                    .ok_or_else(|| NyblError::runtime("current_field_get out of range", line))
            })()),
            "current_field_set" => Some((|| {
                let id = self.bound_entity(line)?;
                let field = arg_int(args, 0, line)? as usize;
                let value = arg_int(args, 1, line)?;
                self.fields
                    .get_mut(id)
                    .and_then(|fields| fields.get_mut(field))
                    .map(|slot| {
                        *slot = value;
                        Value::None
                    })
                    .ok_or_else(|| NyblError::runtime("current_field_set out of range", line))
            })()),
            "current_queue_command" => Some((|| {
                let id = self.bound_entity(line)? as i64;
                let kind = arg_int(args, 0, line)?;
                let x = arg_int(args, 1, line)?;
                let y = arg_int(args, 2, line)?;
                self.commands.push([id, kind, x, y]);
                Ok(Value::None)
            })()),
            _ => None,
        }
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, NyblError>> {
        (name == "bench.tick").then(|| Ok(MODULE_TICK_SRC.to_string()))
    }
}

// ─── Engine abstraction ─────────────────────────────────────────────────────

/// The two embeddable engines share an API shape but not a type; this trait
/// lets every benchmark body be written once.
trait Engine {
    const NAME: &'static str;
    type Instance;
    type Prepared;
    type CachedSource;

    fn load(source: &str, host: &mut dyn NyblHost, limits: &NyblLimits) -> Self::Instance;
    fn call(
        instance: &mut Self::Instance,
        name: &str,
        args: &[Value],
        host: &mut dyn NyblHost,
    ) -> Result<Value, NyblError>;
    fn prepare(instance: &Self::Instance, name: &str) -> Self::Prepared;
    fn call_prepared(
        instance: &mut Self::Instance,
        entry: &Self::Prepared,
        args: &[Value],
        host: &mut dyn NyblHost,
    ) -> Result<Value, NyblError>;
    fn call_batch(
        instance: &mut Self::Instance,
        entry: &Self::Prepared,
        calls: &[Vec<Value>],
        host: &mut dyn NyblHost,
    ) -> Result<Vec<Value>, NyblError>;
    fn cache_source(source: &str) -> Self::CachedSource;
    fn load_cached_instances(
        source: &Self::CachedSource,
        count: usize,
        host: &mut dyn NyblHost,
        limits: &NyblLimits,
    ) -> Vec<Self::Instance>;
}

struct Walker;

impl Engine for Walker {
    const NAME: &'static str = "walker";
    type Instance = nybl::NyblInstance;
    type Prepared = nybl::PreparedEntry;
    type CachedSource = String;

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

    fn prepare(instance: &Self::Instance, name: &str) -> Self::Prepared {
        instance.prepare_entry(name).expect("prepare failed")
    }

    fn call_prepared(
        instance: &mut Self::Instance,
        entry: &Self::Prepared,
        args: &[Value],
        host: &mut dyn NyblHost,
    ) -> Result<Value, NyblError> {
        instance.call_prepared(entry, args, host)
    }

    fn call_batch(
        instance: &mut Self::Instance,
        entry: &Self::Prepared,
        calls: &[Vec<Value>],
        host: &mut dyn NyblHost,
    ) -> Result<Vec<Value>, NyblError> {
        instance.call_batch(entry, calls, host)
    }

    fn cache_source(source: &str) -> Self::CachedSource {
        source.to_string()
    }

    fn load_cached_instances(
        source: &Self::CachedSource,
        count: usize,
        host: &mut dyn NyblHost,
        limits: &NyblLimits,
    ) -> Vec<Self::Instance> {
        // The walker has no `CompiledScript`: load every AST-backed instance
        // once when the entity attaches, outside the measured frame loop.
        (0..count)
            .map(|_| Self::load(source, host, limits))
            .collect()
    }
}

struct Vm;

impl Engine for Vm {
    const NAME: &'static str = "vm";
    type Instance = nybl_vm::NyblInstance;
    type Prepared = nybl_vm::PreparedEntry;
    type CachedSource = nybl_vm::CompiledScript;

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

    fn prepare(instance: &Self::Instance, name: &str) -> Self::Prepared {
        instance.prepare_entry(name).expect("prepare failed")
    }

    fn call_prepared(
        instance: &mut Self::Instance,
        entry: &Self::Prepared,
        args: &[Value],
        host: &mut dyn NyblHost,
    ) -> Result<Value, NyblError> {
        instance.call_prepared(entry, args, host)
    }

    fn call_batch(
        instance: &mut Self::Instance,
        entry: &Self::Prepared,
        calls: &[Vec<Value>],
        host: &mut dyn NyblHost,
    ) -> Result<Vec<Value>, NyblError> {
        instance.call_batch(entry, calls, host)
    }

    fn cache_source(source: &str) -> Self::CachedSource {
        nybl_vm::CompiledScript::compile(source).expect("cached script compiles")
    }

    fn load_cached_instances(
        program: &Self::CachedSource,
        count: usize,
        host: &mut dyn NyblHost,
        limits: &NyblLimits,
    ) -> Vec<Self::Instance> {
        // VM-only: every attachment borrows one Arc-backed compiled artifact,
        // while globals, RNG, imports, and runtime state remain per instance.
        (0..count)
            .map(|_| {
                nybl_vm::NyblInstance::from_compiled(program, host, limits)
                    .expect("cached VM instance initializes")
            })
            .collect()
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

fn bench_call_trivial_prepared<E: Engine>(c: &mut Criterion) {
    let mut host = GameHost::new(1);
    let args = [Value::Int(0), Value::Int(1)];
    let (mut instance, _limits) = load_checked::<E>(TRIVIAL_SRC, &mut host, &args);
    let entry = E::prepare(&instance, "tick");
    c.bench_function(&format!("call_trivial_prepared/{}", E::NAME), |b| {
        b.iter(|| {
            let result = E::call_prepared(&mut instance, &entry, black_box(&args), &mut host)
                .expect("call_trivial_prepared failed");
            black_box(result)
        })
    });
}

fn bench_call_trivial_batch<E: Engine>(c: &mut Criterion) {
    let mut host = GameHost::new(1);
    let args = vec![vec![Value::Int(0), Value::Int(1)]; ENTITY_COUNT];
    let (mut instance, _limits) = load_checked::<E>(TRIVIAL_SRC, &mut host, &args[0]);
    let entry = E::prepare(&instance, "tick");
    c.bench_function(&format!("call_trivial_batch_100/{}", E::NAME), |b| {
        b.iter(|| {
            let result = E::call_batch(&mut instance, &entry, black_box(&args), &mut host)
                .expect("call_trivial_batch failed");
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

fn bench_game_tick_prepared<E: Engine>(c: &mut Criterion) {
    let mut host = GameHost::new(ENTITY_COUNT);
    let sanity_args = [Value::Int(0)];
    let (mut instance, _limits) = load_checked::<E>(GAME_TICK_SRC, &mut host, &sanity_args);
    let entry = E::prepare(&instance, "tick");
    let args: Vec<Vec<Value>> = (0..ENTITY_COUNT as i64)
        .map(|id| vec![Value::Int(id)])
        .collect();
    c.bench_function(
        &format!("game_tick_100_entities_prepared/{}", E::NAME),
        |b| {
            b.iter(|| {
                host.commands.clear();
                for entity_args in &args {
                    let result =
                        E::call_prepared(&mut instance, &entry, black_box(entity_args), &mut host)
                            .expect("prepared game_tick failed");
                    black_box(result);
                }
                black_box(host.commands.len())
            })
        },
    );
}

fn bench_game_tick_batch<E: Engine>(c: &mut Criterion) {
    let mut host = GameHost::new(ENTITY_COUNT);
    let sanity_args = [Value::Int(0)];
    let (mut instance, _limits) = load_checked::<E>(GAME_TICK_SRC, &mut host, &sanity_args);
    let entry = E::prepare(&instance, "tick");
    let args: Vec<Vec<Value>> = (0..ENTITY_COUNT as i64)
        .map(|id| vec![Value::Int(id)])
        .collect();
    c.bench_function(&format!("game_tick_100_entities_batch/{}", E::NAME), |b| {
        b.iter(|| {
            host.commands.clear();
            let results = E::call_batch(&mut instance, &entry, black_box(&args), &mut host)
                .expect("batch game_tick failed");
            black_box(results);
            black_box(host.commands.len())
        })
    });
}

fn bench_game_tick_script_batch<E: Engine>(c: &mut Criterion) {
    let mut host = GameHost::new(ENTITY_COUNT);
    let args = [Value::Int(ENTITY_COUNT as i64)];
    let limits = bench_limits();
    let mut instance = E::load(GAME_TICK_BATCH_SRC, &mut host, &limits);
    E::call(&mut instance, "tick_batch", &args, &mut host).expect("sanity call failed");
    let entry = E::prepare(&instance, "tick_batch");
    c.bench_function(
        &format!("game_tick_100_entities_script_batch/{}", E::NAME),
        |b| {
            b.iter(|| {
                host.commands.clear();
                let results = E::call_prepared(&mut instance, &entry, black_box(&args), &mut host)
                    .expect("script batch game_tick failed");
                black_box(results);
                black_box(host.commands.len())
            })
        },
    );
}

/// Models the consumer-owned lifecycle from issue #199: one persistent script
/// instance per entity attachment and one custom worker host shared across all
/// callbacks. Setup, including VM compilation and all instance preparation,
/// occurs before `b.iter`; the measured body is exactly one 100-entity frame.
fn bench_cached_entity_instances<E: Engine>(c: &mut Criterion) {
    let limits = bench_limits();
    let source = E::cache_source(CACHED_ENTITY_TICK_SRC);
    // Keep independent but identically initialized consumer state so Criterion
    // can run each dispatch variant for a different iteration count without
    // advancing the other variant's ECS fields.
    let mut ordinary_host = GameHost::new(ENTITY_COUNT);
    let mut prepared_host = GameHost::new(ENTITY_COUNT);
    let mut ordinary = E::load_cached_instances(&source, ENTITY_COUNT, &mut ordinary_host, &limits);
    let mut prepared = E::load_cached_instances(&source, ENTITY_COUNT, &mut prepared_host, &limits);
    let entries: Vec<_> = prepared
        .iter()
        .map(|instance| E::prepare(instance, "tick"))
        .collect();
    let args = [Value::Int(1)];
    let mut group = c.benchmark_group("cached_entity_instances_100");

    group.bench_function(format!("{}/ordinary", E::NAME), |b| {
        b.iter(|| {
            ordinary_host.begin_frame();
            for (entity, instance) in ordinary.iter_mut().enumerate() {
                ordinary_host.bind_entity(entity);
                let result = E::call(instance, "tick", black_box(&args), &mut ordinary_host)
                    .expect("cached entity tick failed");
                black_box(result);
            }
            black_box(ordinary_host.commands.len())
        })
    });

    group.bench_function(format!("{}/prepared", E::NAME), |b| {
        b.iter(|| {
            prepared_host.begin_frame();
            for (entity, (instance, entry)) in prepared.iter_mut().zip(entries.iter()).enumerate() {
                prepared_host.bind_entity(entity);
                let result =
                    E::call_prepared(instance, entry, black_box(&args), &mut prepared_host)
                        .expect("prepared cached entity tick failed");
                black_box(result);
            }
            black_box(prepared_host.commands.len())
        })
    });

    group.finish();
}

fn bench_cached_trivial_instances<E: Engine>(c: &mut Criterion) {
    let limits = bench_limits();
    let source = E::cache_source(CACHED_TRIVIAL_SRC);
    let mut host = GameHost::new(ENTITY_COUNT);
    let mut ordinary = E::load_cached_instances(&source, ENTITY_COUNT, &mut host, &limits);
    let mut prepared = E::load_cached_instances(&source, ENTITY_COUNT, &mut host, &limits);
    let entries: Vec<_> = prepared
        .iter()
        .map(|instance| E::prepare(instance, "tick"))
        .collect();
    let args = [Value::Int(1)];
    let mut group = c.benchmark_group("cached_trivial_instances_100");

    group.bench_function(format!("{}/ordinary", E::NAME), |b| {
        b.iter(|| {
            for instance in &mut ordinary {
                black_box(
                    E::call(instance, "tick", black_box(&args), &mut host)
                        .expect("cached trivial call failed"),
                );
            }
        })
    });
    group.bench_function(format!("{}/prepared", E::NAME), |b| {
        b.iter(|| {
            for (instance, entry) in prepared.iter_mut().zip(entries.iter()) {
                black_box(
                    E::call_prepared(instance, entry, black_box(&args), &mut host)
                        .expect("prepared cached trivial call failed"),
                );
            }
        })
    });

    group.finish();
}

fn bench_custom_host_frame(c: &mut Criterion) {
    let mut host = GameHost::new(ENTITY_COUNT);
    c.bench_function("custom_host_frame_100/direct_value_abi", |b| {
        b.iter(|| {
            direct_custom_host_frame(&mut host);
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

/// VM only: instantiating from a pre-compiled shared artifact versus a full
/// `load` (parse + compile + validate + top-level execution). The difference
/// is the per-worker saving when N workers share one `CompiledScript`.
fn bench_instantiate_from_compiled(c: &mut Criterion) {
    let limits = bench_limits();
    let program =
        nybl_vm::CompiledScript::compile(GAME_TICK_SRC).expect("game tick script compiles");
    c.bench_function("instantiate_from_compiled/vm", |b| {
        b.iter_batched(
            || GameHost::new(ENTITY_COUNT),
            |mut host| {
                black_box(
                    nybl_vm::NyblInstance::from_compiled(black_box(&program), &mut host, &limits)
                        .expect("from_compiled failed"),
                )
            },
            BatchSize::SmallInput,
        )
    });
}

/// VM only: build 16 independent instances of a module-bearing program. The
/// legacy path resolves/parses/compiles the helper 16 times; the artifact path
/// shares the root and module chunks while still executing fresh top-level
/// state for every instance.
fn bench_module_bearing_instances(c: &mut Criterion) {
    const INSTANCE_COUNT: usize = 16;
    let limits = bench_limits();
    let program = nybl_vm::CompiledScript::compile_with_modules(MODULE_BEARING_ROOT, |path| {
        (path == "bench.tick").then(|| Ok(MODULE_TICK_SRC.to_string()))
    })
    .expect("module-bearing script compiles");
    let mut group = c.benchmark_group("instantiate_module_graph_16");

    group.bench_function("legacy_load", |b| {
        b.iter(|| {
            let instances: Vec<_> = (0..INSTANCE_COUNT)
                .map(|_| {
                    let mut host = GameHost::new(1);
                    nybl_vm::NyblInstance::load(MODULE_BEARING_ROOT, &mut host, &limits)
                        .expect("module-bearing load failed")
                })
                .collect();
            black_box(instances)
        })
    });
    group.bench_function("from_precompiled", |b| {
        b.iter(|| {
            let instances: Vec<_> = (0..INSTANCE_COUNT)
                .map(|_| {
                    let mut host = GameHost::new(1);
                    nybl_vm::NyblInstance::from_compiled(&program, &mut host, &limits)
                        .expect("module-bearing instantiation failed")
                })
                .collect();
            black_box(instances)
        })
    });
    group.finish();
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
    bench_call_trivial_prepared::<Walker>(c);
    bench_call_trivial_prepared::<Vm>(c);
    bench_call_trivial_batch::<Walker>(c);
    bench_call_trivial_batch::<Vm>(c);
    bench_host_call_roundtrip::<Walker>(c);
    bench_host_call_roundtrip::<Vm>(c);
    bench_game_tick::<Walker>(c);
    bench_game_tick::<Vm>(c);
    bench_game_tick_prepared::<Walker>(c);
    bench_game_tick_prepared::<Vm>(c);
    bench_game_tick_batch::<Walker>(c);
    bench_game_tick_batch::<Vm>(c);
    bench_game_tick_script_batch::<Walker>(c);
    bench_game_tick_script_batch::<Vm>(c);
    bench_cached_entity_instances::<Walker>(c);
    bench_cached_entity_instances::<Vm>(c);
    bench_cached_trivial_instances::<Walker>(c);
    bench_cached_trivial_instances::<Vm>(c);
    bench_custom_host_frame(c);
    bench_value_conversion(c);
    bench_load::<Walker>(c);
    bench_load::<Vm>(c);
    bench_instantiate_from_compiled(c);
    bench_module_bearing_instances(c);
}

criterion_group!(benches, all_benches);
criterion_main!(benches);
