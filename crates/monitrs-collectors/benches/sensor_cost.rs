//! What a sensor read costs (§16.3, and the design document's A1).
//!
//! `docs/benchmarks.md` locates the idle self-CPU p95 failure in one call:
//! `Components::refresh(true)`, about 85 ms, on the 5-second medium tier. Before
//! rescheduling that read, this asks whether it can simply be made cheaper.
//!
//! Three variants, in increasing order of hope:
//!
//! 1. `refresh(true)` — what the collector does today, list rebuild included.
//! 2. `refresh(false)` — the same read without rebuilding the component list.
//! 3. one component refreshed alone — what the header's hottest reading needs.
//!
//! Run with `cargo bench -p monitrs-collectors --bench sensor_cost`. The numbers go
//! into `docs/benchmarks.md` by hand: a wall-clock assertion in CI would be a flake,
//! and §16.3 asks for measurements a human quoted, not a threshold a machine guessed.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use sysinfo::Components;

fn sensor_read(c: &mut Criterion) {
    let mut components = Components::new_with_refreshed_list();
    // Printed because the cost is per sensor: the same code is a different
    // measurement on a machine with four components and on one with forty.
    println!("components discovered: {}", components.len());

    let mut group = c.benchmark_group("sensor_read");
    // An 85 ms read does not need criterion's default hundred samples, and the
    // figure wanted here is the per-read cost rather than a tight interval.
    group.sample_size(20);

    group.bench_function("refresh_with_list", |b| {
        b.iter(|| black_box(&mut components).refresh(true));
    });
    group.bench_function("refresh_without_list", |b| {
        b.iter(|| black_box(&mut components).refresh(false));
    });
    // The header shows one reading: the hottest. If a single component can be
    // refreshed on its own, the header's cost is that of one sensor rather than of
    // every sensor the machine has.
    group.bench_function("one_component", |b| {
        b.iter(|| {
            if let Some(component) = black_box(&mut components).iter_mut().next() {
                component.refresh();
            }
        });
    });
    group.finish();
}

criterion_group!(benches, sensor_read);
criterion_main!(benches);
