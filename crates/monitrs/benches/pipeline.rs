//! Benchmarks for the hot paths §16.3 names.
//!
//! These live in the binary crate rather than in `monitrs-core` for a structural
//! reason: realistic input comes from
//! [`monitrs_collectors::FakeCollector`], and `monitrs-core` must not depend on
//! the collectors even as a dev-dependency — that would invert the dependency
//! direction §10.1 establishes. The binary is the one crate that legitimately
//! sees all three libraries.
//!
//! The reference workload follows §16.1: 8 logical CPUs, 200 processes, a
//! one-second interval, and five minutes of history. A 10,000-process variant
//! covers the high-load behaviour of §16.2.
//!
//! §16.3 is explicit that optimisation must not be guided by intuition. These
//! measure; they do not assert. Record the machine and the command alongside any
//! number taken from them.

// A benchmark is not production code: `expect` on a value the setup just built is
// the clearest way to state a precondition, and a panic here fails the benchmark
// run rather than corrupting a user's terminal. Same reasoning as the
// `cfg(test)`-scoped allowance on the library crates (§18.2).
#![allow(clippy::expect_used)]

use std::hint::black_box;
use std::time::{Duration, Instant, SystemTime};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use monitrs_collectors::fake::{Pattern, Scenario};
use monitrs_collectors::linux::diskstats::parse_diskstats;
use monitrs_collectors::linux::loadavg::parse_loadavg;
use monitrs_collectors::linux::meminfo::parse_meminfo;
use monitrs_collectors::linux::netdev::parse_net_dev;
use monitrs_collectors::linux::process::{parse_pid_io, parse_pid_stat, parse_pid_status};
use monitrs_collectors::linux::psi::parse_pressure;
use monitrs_collectors::linux::stat::parse_proc_stat;
use monitrs_collectors::{DueTiers, FakeCollector, SampleTick, SnapshotSource as _};
use monitrs_core::diagnostics::{PressureEngine, Thresholds};
use monitrs_core::history::{
    ContributorSet, HistoryConfig, HistoryLimits, HistoryMetric, HistoryRing, HistoryView,
    RecordOutcome,
};
use monitrs_core::model::ProcessIdentity;
use monitrs_core::process::{ProcessFilter, ProcessSort, ProcessSortKey, ProcessTree};
use monitrs_core::rates::{CounterTracker, CounterWidth, KeyedRateTrackers};
use monitrs_core::units::Percent;
use monitrs_core::units::{
    ByteUnits, format_age, format_byte_rate, format_bytes_compact, pad_left,
};
use monitrs_core::{MetricState, SystemSnapshot};
use monitrs_tui::app::{AppSettings, AppState, apply};
use monitrs_tui::event::{Event, KeyPress, TerminalEvent};
use monitrs_tui::glyphs::GlyphSet;
use monitrs_tui::theme::{ColorDepth, ThemeId};
use monitrs_tui::widgets::{Meter, Presentation, Sparkline};

/// The §16.1 reference process count.
const REFERENCE_PROCESSES: usize = 200;
/// The §16.2 high-load process count.
const HIGH_LOAD_PROCESSES: usize = 10_000;

/// Collects `count` real snapshots from a deterministic scenario.
fn snapshots(processes: usize, count: u64) -> Vec<SystemSnapshot> {
    let scenario = Scenario {
        cpu: Pattern::Sawtooth {
            low: 5.0,
            high: 95.0,
            period: 40,
        },
        ..Scenario::with_process_count(processes)
    };
    let mut collector = FakeCollector::new(scenario);
    let start = Instant::now();
    let mut tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
    let mut out = Vec::with_capacity(usize::try_from(count).unwrap_or(usize::MAX));
    for index in 0..count {
        if index > 0 {
            tick = tick.advance(
                start + Duration::from_secs(index),
                SystemTime::UNIX_EPOCH + Duration::from_secs(index),
                DueTiers::ALL,
            );
        }
        if let Ok(snapshot) = collector.sample(&tick) {
            out.push(snapshot);
        }
    }
    out
}

/// §16.3: rate calculations.
fn bench_rates(c: &mut Criterion) {
    let mut group = c.benchmark_group("rates");

    group.bench_function("single_counter_observe", |b| {
        let mut tracker = CounterTracker::new(CounterWidth::Bits64);
        let start = Instant::now();
        let mut counter = 0u64;
        let mut step = 0u32;
        b.iter(|| {
            counter += 4096;
            step = step.wrapping_add(1);
            let at = start + Duration::from_millis(u64::from(step) * 1_000);
            black_box(tracker.rate(black_box(counter), at))
        });
    });

    // A counter that resets every other sample: the branch a naive
    // implementation would turn into a huge rate (§8.2).
    group.bench_function("single_counter_with_resets", |b| {
        let mut tracker = CounterTracker::new(CounterWidth::Bits64);
        let start = Instant::now();
        let mut step = 0u32;
        b.iter(|| {
            step = step.wrapping_add(1);
            let value = if step.is_multiple_of(2) {
                0
            } else {
                u64::from(step) * 4096
            };
            let at = start + Duration::from_millis(u64::from(step) * 1_000);
            black_box(tracker.rate(black_box(value), at))
        });
    });

    for &count in &[REFERENCE_PROCESSES, HIGH_LOAD_PROCESSES] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::new("keyed_observe_per_process", count),
            &count,
            |b, &count| {
                let mut trackers: KeyedRateTrackers<ProcessIdentity> =
                    KeyedRateTrackers::new(CounterWidth::Bits64);
                let identities: Vec<ProcessIdentity> = (0..count)
                    .map(|index| {
                        let pid = u32::try_from(index).unwrap_or(u32::MAX);
                        ProcessIdentity::new(pid, u64::from(pid) * 7)
                    })
                    .collect();
                let start = Instant::now();
                let mut step = 0u32;
                b.iter(|| {
                    step = step.wrapping_add(1);
                    let at = start + Duration::from_millis(u64::from(step) * 1_000);
                    for (index, identity) in identities.iter().enumerate() {
                        let value = (index as u64 + 1) * u64::from(step) * 1024;
                        black_box(trackers.observe(*identity, value, at));
                    }
                });
            },
        );
    }

    group.finish();
}

/// §16.3: history insertion and seeking.
fn bench_history(c: &mut Criterion) {
    let mut group = c.benchmark_group("history");

    // Each iteration must present a *newer* snapshot. `record` correctly rejects a
    // re-delivered one as `NotNewer` (§8.1, §16.2), so recording the same sample
    // repeatedly would measure the rejection branch — about 4ns — and make the
    // benchmark look a hundred times faster than the work it is supposed to time.
    // The assertion below exists so that mistake cannot come back unnoticed.
    let reference = snapshots(REFERENCE_PROCESSES, 2);
    let mut sample = reference.last().expect("two snapshots").clone();

    group.bench_function("record_one_sample_200_processes", |b| {
        let limits = HistoryLimits::resolve(HistoryConfig::default());
        let start = Instant::now();
        let mut ring = HistoryRing::new(limits, start);
        let mut step = 0u64;
        b.iter(|| {
            step += 1;
            sample.sequence = step;
            sample.captured_at = start + Duration::from_secs(step);
            let outcome = ring.record(black_box(&sample));
            assert!(
                matches!(outcome, RecordOutcome::Recorded { .. }),
                "the benchmark must exercise the record path, not the rejection path"
            );
            black_box(outcome)
        });
    });

    let high_load = snapshots(HIGH_LOAD_PROCESSES, 2);
    let mut big_sample = high_load.last().expect("two snapshots").clone();
    group.bench_function("record_one_sample_10000_processes", |b| {
        let limits = HistoryLimits::resolve(HistoryConfig::default());
        let start = Instant::now();
        let mut ring = HistoryRing::new(limits, start);
        let mut step = 0u64;
        b.iter(|| {
            step += 1;
            big_sample.sequence = step;
            big_sample.captured_at = start + Duration::from_secs(step);
            let outcome = ring.record(black_box(&big_sample));
            assert!(
                matches!(outcome, RecordOutcome::Recorded { .. }),
                "the benchmark must exercise the record path, not the rejection path"
            );
            black_box(outcome)
        });
    });

    // The rejection path itself, measured deliberately and named as such.
    group.bench_function("reject_a_re_delivered_sample", |b| {
        let limits = HistoryLimits::resolve(HistoryConfig::default());
        let start = Instant::now();
        let mut ring = HistoryRing::new(limits, start);
        let mut seed = reference.last().expect("two snapshots").clone();
        seed.sequence = 10;
        seed.captured_at = start + Duration::from_secs(10);
        ring.record(&seed);
        b.iter(|| {
            let outcome = ring.record(black_box(&seed));
            assert!(matches!(outcome, RecordOutcome::NotNewer));
            black_box(outcome)
        });
    });

    // A full ring, so eviction is on the measured path.
    let full_ring = {
        let limits = HistoryLimits::resolve(HistoryConfig::default());
        let mut ring = HistoryRing::new(limits, Instant::now());
        for snapshot in &snapshots(REFERENCE_PROCESSES, 320) {
            ring.record(snapshot);
        }
        ring
    };
    assert!(
        full_ring.len() >= 300,
        "the ring should be full: {}",
        full_ring.len()
    );

    // §21 M4 requires seeking to be constant or effectively constant time. If it
    // were linear in history length, the deep seek would cost far more than the
    // shallow one.
    for steps in [1usize, 10, 150, 299] {
        group.bench_with_input(
            BenchmarkId::new("seek_back_steps", steps),
            &steps,
            |b, &steps| {
                b.iter(|| {
                    let mut view = HistoryView::live();
                    black_box(view.step_back(&full_ring, black_box(steps)))
                });
            },
        );
    }

    group.bench_function("seek_to_offset_150s", |b| {
        b.iter(|| {
            let mut view = HistoryView::live();
            black_box(view.seek_to_offset(&full_ring, Duration::from_secs(150)))
        });
    });

    group.bench_function("comparisons_against_30s_ago", |b| {
        let mut view = HistoryView::live();
        view.step_back(&full_ring, 60);
        b.iter(|| black_box(view.comparisons(&full_ring, black_box(HistoryMetric::CpuBusy))));
    });

    group.finish();
}

/// §16.3: contributor top-K extraction.
fn bench_contributors(c: &mut Criterion) {
    let mut group = c.benchmark_group("contributors");

    for &count in &[REFERENCE_PROCESSES, HIGH_LOAD_PROCESSES] {
        let collected = snapshots(count, 2);
        let processes = &collected.last().expect("two snapshots").processes;
        group.throughput(Throughput::Elements(processes.len() as u64));
        group.bench_with_input(BenchmarkId::new("top_10_of", count), &count, |b, _| {
            b.iter(|| {
                black_box(ContributorSet::from_processes(
                    black_box(processes),
                    None,
                    10,
                ))
            });
        });
    }

    // With a previous set, so trend derivation is measured too.
    let collected = snapshots(REFERENCE_PROCESSES, 3);
    let processes = &collected.last().expect("snapshots").processes;
    let previous = ContributorSet::from_processes(processes, None, 10);
    group.bench_function("top_10_with_trend", |b| {
        b.iter(|| {
            black_box(ContributorSet::from_processes(
                black_box(processes),
                Some(&previous),
                10,
            ))
        });
    });

    group.finish();
}

/// §16.3: process stable sort, filter matching, and tree construction.
fn bench_process_list(c: &mut Criterion) {
    let mut group = c.benchmark_group("process_list");

    for &count in &[REFERENCE_PROCESSES, HIGH_LOAD_PROCESSES] {
        let collected = snapshots(count, 2);
        let snapshot = collected.last().expect("two snapshots");
        let processes = &snapshot.processes;
        group.throughput(Throughput::Elements(processes.len() as u64));

        // Sorting is measured on a fresh copy each iteration: sorting an
        // already-sorted slice is the best case and would flatter the result.
        group.bench_with_input(BenchmarkId::new("sort_by_cpu", count), &count, |b, _| {
            let order = ProcessSort::descending(ProcessSortKey::Cpu);
            b.iter_batched(
                || processes.clone(),
                |mut rows| {
                    order.sort(&mut rows);
                    black_box(rows)
                },
                criterion::BatchSize::LargeInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("sort_by_name", count), &count, |b, _| {
            let order = ProcessSort::ascending(ProcessSortKey::Name);
            b.iter_batched(
                || processes.clone(),
                |mut rows| {
                    order.sort(&mut rows);
                    black_box(rows)
                },
                criterion::BatchSize::LargeInput,
            );
        });

        group.bench_with_input(
            BenchmarkId::new("filter_plain_text", count),
            &count,
            |b, _| {
                let filter = ProcessFilter::parse("worker");
                b.iter(|| black_box(filter.match_indices(black_box(processes))));
            },
        );

        // A filter that matches nothing still has to look at every row.
        group.bench_with_input(
            BenchmarkId::new("filter_no_matches", count),
            &count,
            |b, _| {
                let filter = ProcessFilter::parse("zzz-no-such-process");
                b.iter(|| black_box(filter.match_indices(black_box(processes))));
            },
        );

        group.bench_with_input(BenchmarkId::new("build_tree", count), &count, |b, _| {
            b.iter(|| {
                black_box(ProcessTree::build(
                    black_box(processes),
                    ProcessSort::descending(ProcessSortKey::Cpu),
                ))
            });
        });
    }

    group.finish();
}

/// §16.3: large table formatting.
///
/// One row's worth of every formatter a process row needs, times the row count —
/// which is what actually runs per frame.
fn bench_formatting(c: &mut Criterion) {
    let mut group = c.benchmark_group("formatting");

    for &count in &[REFERENCE_PROCESSES, HIGH_LOAD_PROCESSES] {
        let collected = snapshots(count, 2);
        let processes = &collected.last().expect("two snapshots").processes;
        group.throughput(Throughput::Elements(processes.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("format_table_rows", count),
            &count,
            |b, _| {
                b.iter(|| {
                    let mut sink = 0usize;
                    for process in processes {
                        let pid = pad_left(
                            &process.identity.pid.to_string(),
                            6,
                            monitrs_core::units::Ellipsis::Ascii,
                        );
                        let cpu = match process.cpu {
                            MetricState::Available(percent) => percent.to_string(),
                            _ => "n/a".to_owned(),
                        };
                        let rss = match process.memory.rss_bytes {
                            MetricState::Available(bytes) => {
                                format_bytes_compact(bytes, ByteUnits::Iec)
                            }
                            _ => "n/a".to_owned(),
                        };
                        let read = match process.io.read {
                            MetricState::Available(rate) => format_byte_rate(rate, ByteUnits::Iec),
                            _ => "n/a".to_owned(),
                        };
                        let age = match process.age {
                            MetricState::Available(age) => format_age(age),
                            _ => "n/a".to_owned(),
                        };
                        let command = monitrs_core::units::truncate_middle(
                            process.command_or_name(),
                            40,
                            monitrs_core::units::Ellipsis::Ascii,
                        );
                        sink += pid.len()
                            + cpu.len()
                            + rss.len()
                            + read.len()
                            + age.len()
                            + command.len();
                    }
                    black_box(sink)
                });
            },
        );
    }

    // The unit-boundary values §5.4 says must not reflow, so the cost of the
    // stable-width path is visible rather than averaged away.
    group.bench_function("format_bytes_at_unit_boundaries", |b| {
        let values: Vec<u64> = vec![
            0,
            999,
            1_023,
            1_024,
            1_025,
            1_048_575,
            1_048_576,
            1_073_741_823,
            1_073_741_824,
            u64::MAX,
        ];
        b.iter(|| {
            let mut sink = 0usize;
            for &value in &values {
                sink += format_bytes_compact(black_box(value), ByteUnits::Iec).len();
            }
            black_box(sink)
        });
    });

    group.finish();
}

/// End-to-end: what one fast tick costs, which is the §16.1 budget that matters.
fn bench_sample(c: &mut Criterion) {
    let mut group = c.benchmark_group("sample");

    for &count in &[REFERENCE_PROCESSES, HIGH_LOAD_PROCESSES] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::new("fake_collector_tick", count),
            &count,
            |b, _| {
                let mut collector = FakeCollector::new(Scenario::with_process_count(count));
                let start = Instant::now();
                let mut tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
                let mut step = 0u64;
                b.iter(|| {
                    step += 1;
                    tick = tick.advance(
                        start + Duration::from_secs(step),
                        SystemTime::UNIX_EPOCH + Duration::from_secs(step),
                        DueTiers::ALL,
                    );
                    black_box(collector.sample(&tick).ok())
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------- §16.3
// The three items §16.3 names that had no benchmark: ASCII and Unicode graph
// generation, Linux fixture parsing, and diagnostic-rule evaluation.

/// A plottable series of `count` samples, shaped like a real one.
///
/// Every fourth sample is unavailable, because a sparkline's cost depends on how
/// often it has to draw the gap marker rather than a bar, and a series that is
/// entirely available would measure the cheap half only (§4).
fn plot_series(count: usize) -> Vec<MetricState<Percent>> {
    (0..count)
        .map(|index| {
            if index % 4 == 3 {
                MetricState::WarmingUp
            } else {
                let magnitude = (index % 100) as f32;
                MetricState::Available(Percent::new(magnitude).unwrap_or(Percent::ZERO))
            }
        })
        .collect()
}

/// §16.3: ASCII and Unicode graph generation.
///
/// Both glyph modes, because Unicode's eighth-block ramp has eight levels against
/// ASCII's five and the two take different paths through the ramp lookup — and §5.1
/// makes ASCII a first-class mode rather than a fallback, so its cost matters just as
/// much.
fn bench_graphs(c: &mut Criterion) {
    let mut group = c.benchmark_group("graphs");
    let theme = ThemeId::DefaultDark.theme();
    let series = plot_series(300);

    for (mode, glyphs) in [
        ("ascii", GlyphSet::ascii()),
        ("unicode", GlyphSet::unicode()),
    ] {
        let presentation = Presentation::new(glyphs, theme, ColorDepth::TrueColor);
        for width in [40u16, 96, 160] {
            group.throughput(Throughput::Elements(u64::from(width)));
            group.bench_with_input(
                BenchmarkId::new(format!("sparkline_{mode}"), width),
                &width,
                |b, &width| {
                    b.iter(|| {
                        black_box(
                            Sparkline::new(presentation, &series)
                                .with_label("CPU")
                                .with_label_width(6)
                                .line(width),
                        )
                    });
                },
            );
            group.bench_with_input(
                BenchmarkId::new(format!("meter_{mode}"), width),
                &width,
                |b, &width| {
                    let value = MetricState::Available(
                        Percent::new(63.5).expect("63.5 is a valid percentage"),
                    );
                    b.iter(|| {
                        black_box(
                            Meter::new(presentation, value)
                                .with_label("MEM")
                                .line(width),
                        )
                    });
                },
            );
        }
    }
    group.finish();
}

/// The sanitized Linux fixtures, embedded so a renamed one is a compile error.
///
/// The same files the parser tests read, reached by path rather than through the
/// library's own `cfg(test)` fixture module — a benchmark is not a test build, so it
/// cannot see that module. §17.2's property survives either way: delete a fixture and
/// this stops compiling.
mod fixtures {
    macro_rules! fixture {
        ($name:ident => $path:literal) => {
            pub(super) const $name: &[u8] = include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../monitrs-collectors/fixtures/linux/",
                $path
            ));
        };
    }

    fixture!(PROC_STAT => "cases/proc_stat/typical.txt");
    fixture!(MEMINFO => "cases/meminfo/typical.txt");
    fixture!(DISKSTATS => "cases/diskstats/typical.txt");
    fixture!(NET_DEV => "cases/net_dev/typical.txt");
    fixture!(LOADAVG => "cases/loadavg/typical.txt");
    fixture!(PRESSURE => "cases/pressure/cpu_with_full.txt");
    fixture!(PID_STAT => "cases/pid_stat/simple.txt");
    fixture!(PID_STAT_PARENS => "cases/pid_stat/parens_and_spaces_in_name.txt");
    fixture!(PID_STATUS => "cases/pid_status/typical.txt");
    fixture!(PID_IO => "cases/pid_io/typical.txt");
}

/// §16.3: Linux fixture parsing.
///
/// Runs on every platform, which is the point of every parser taking bytes rather
/// than a path: the cost of reading `/proc` is the kernel's, and the cost of parsing
/// what it returned is ours, and only the second one is a number we can act on.
///
/// `pid_stat` appears twice on purpose. The parenthesised-name case is the one that
/// cannot be split on whitespace — the process name may contain spaces and brackets —
/// so it takes the scan-from-the-right path, and a per-process parser is run once per
/// process per tick.
fn bench_linux_parsers(c: &mut Criterion) {
    let mut group = c.benchmark_group("linux_parsers");

    macro_rules! case {
        ($name:literal, $bytes:expr, $parse:expr) => {{
            let bytes: &[u8] = $bytes;
            group.throughput(Throughput::Bytes(bytes.len() as u64));
            group.bench_function($name, |b| b.iter(|| black_box($parse(black_box(bytes)))));
        }};
    }

    case!("proc_stat", fixtures::PROC_STAT, parse_proc_stat);
    case!("meminfo", fixtures::MEMINFO, parse_meminfo);
    case!("diskstats", fixtures::DISKSTATS, parse_diskstats);
    case!("net_dev", fixtures::NET_DEV, parse_net_dev);
    case!("loadavg", fixtures::LOADAVG, parse_loadavg);
    case!("pressure", fixtures::PRESSURE, parse_pressure);
    case!("pid_stat", fixtures::PID_STAT, parse_pid_stat);
    case!("pid_stat_parens", fixtures::PID_STAT_PARENS, parse_pid_stat);
    case!("pid_status", fixtures::PID_STATUS, parse_pid_status);
    case!("pid_io", fixtures::PID_IO, parse_pid_io);

    group.finish();
}

/// §16.3: diagnostic-rule evaluation.
///
/// Measured on a *warm* engine, because a cold one short-circuits: every tracker
/// returns warming up until its window fills, which skips the comparison work the
/// rules actually do (§11.3). The engine is therefore fed the full sustained window
/// before the timed loop starts.
///
/// Also measured at 10,000 processes, even though no rule iterates the process table —
/// so that the number can be *shown* not to scale with it, which is the interesting
/// property rather than an assumption.
fn bench_diagnostics(c: &mut Criterion) {
    let mut group = c.benchmark_group("diagnostics");

    for processes in [REFERENCE_PROCESSES, HIGH_LOAD_PROCESSES] {
        let series = snapshots(processes, 40);
        group.bench_with_input(
            BenchmarkId::new("observe_one_snapshot", processes),
            &processes,
            |b, _| {
                let mut engine = PressureEngine::new(Thresholds::default());
                // Warm past the sustained window so the timed observation does the
                // full comparison rather than returning warming up.
                for snapshot in series.iter().take(20) {
                    let _ = engine.observe(snapshot);
                }
                let mut next = 20usize;
                b.iter(|| {
                    let snapshot = &series[next % series.len()];
                    next = next.wrapping_add(1);
                    black_box(engine.observe(black_box(snapshot)))
                });
            },
        );
    }

    group.finish();
}

/// What the reducer costs to absorb one snapshot, which is the number that settles
/// "can a keypress queue behind a snapshot".
///
/// It had no benchmark, which is why a 90 ms worst-case input latency in a soak report
/// was explained for a while as a keypress waiting behind one of these. It is not: at
/// the reference workload an absorb is tens of microseconds, and even at ten thousand
/// processes it is under two milliseconds, so a keypress can be delayed by at most that.
/// Having the number here makes the claim checkable in one command instead of an
/// afternoon.
///
/// Each iteration feeds a *newer* snapshot, because the reducer correctly rejects one
/// that is not (`RecordOutcome::NotNewer`) and a benchmark that measured the rejection
/// would report a hot path as free.
fn bench_absorb(c: &mut Criterion) {
    let mut group = c.benchmark_group("absorb");

    for processes in [REFERENCE_PROCESSES, HIGH_LOAD_PROCESSES] {
        let series = snapshots(processes, 24);
        group.throughput(Throughput::Elements(processes as u64));
        group.bench_with_input(
            BenchmarkId::new("apply_snapshot", processes),
            &processes,
            |b, _| {
                let mut state = AppState::new(AppSettings {
                    size: (160, 48),
                    ..AppSettings::default()
                });
                let mut next = 0usize;
                let mut sequence = 0u64;
                b.iter(|| {
                    let mut snapshot = series[next % series.len()].clone();
                    next = next.wrapping_add(1);
                    sequence = sequence.wrapping_add(1);
                    snapshot.sequence = sequence;
                    black_box(apply::<()>(
                        &mut state,
                        Event::Snapshot(std::sync::Arc::new(snapshot)),
                    ))
                });
            },
        );
    }

    // A keypress and a tick, for scale: these are what a snapshot absorb would be
    // delaying, and they are three orders of magnitude smaller.
    let series = snapshots(REFERENCE_PROCESSES, 4);
    let mut state = AppState::new(AppSettings {
        size: (160, 48),
        ..AppSettings::default()
    });
    let _ = apply::<()>(
        &mut state,
        Event::Snapshot(std::sync::Arc::new(series[0].clone())),
    );
    group.bench_function("apply_keypress", |b| {
        b.iter(|| {
            black_box(apply::<()>(
                &mut state,
                Event::Terminal(TerminalEvent::Key(KeyPress::char('j'))),
            ))
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_rates,
    bench_history,
    bench_contributors,
    bench_process_list,
    bench_formatting,
    bench_sample,
    bench_graphs,
    bench_linux_parsers,
    bench_diagnostics,
    bench_absorb
);
criterion_main!(benches);
