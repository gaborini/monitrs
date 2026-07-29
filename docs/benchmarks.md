# Benchmark results

§16.3 requires that optimisation is not guided by intuition, and that baseline
results are stored — or at minimum that the reference machine and command are
documented. This file is that record.

**These are measurements of specific functions, not of the running application.**
The end-to-end budgets in [`architecture.md`](architecture.md) — idle CPU,
resident memory, input latency, frame time — are still budgets. They require the
interactive runtime and a soak test, and this file will not pretend otherwise.

## Reference machine

| | |
|---|---|
| CPU | Apple M4 Pro, 12 logical / 12 physical cores |
| Memory | 48 GiB |
| OS | macOS 26.5.2 (arm64) |
| Toolchain | rustc 1.97.1 |
| Profile | `bench` (inherits `release`: `lto = "thin"`, `codegen-units = 1`, debug symbols kept) |

This is a fast desktop-class machine. The §16.1 reference workload is a more
modest 8-CPU laptop, so treat these as a lower bound on real-world cost.

## Command

```sh
cargo bench -p monitrs --bench pipeline
```

The run below used shortened sampling for turnaround:

```sh
cargo bench -p monitrs --bench pipeline -- \
  --warm-up-time 1 --measurement-time 3 --sample-size 20
```

Criterion writes full reports to `target/criterion/`.

## Results

Two workloads: **200** processes (the §16.1 reference) and **10,000** (the §16.2
high-load case). Times are the median of Criterion's confidence interval.

### Rate engine

| Benchmark | 200 | 10,000 |
|---|---:|---:|
| `single_counter_observe` | 5.6 ns | — |
| `single_counter_with_resets` | 4.9 ns | — |
| `keyed_observe_per_process` | 1.74 µs | 102 µs |

Counter reset handling is not a slow path — detecting a reset is marginally
*cheaper* than computing a rate, because it skips the division. There is no
performance argument for cutting the §8.2 correctness check.

### History ring

| Benchmark | 200 | 10,000 |
|---|---:|---:|
| `record_one_sample` | 14.0 µs | 972 µs |
| `reject_a_re_delivered_sample` | 3.0 ns | — |
| `seek_back_steps` (1 / 10 / 150 / 299) | 507 / 503 / 498 / 506 ps | — |
| `seek_to_offset_150s` | 6.7 ns | — |
| `comparisons_against_30s_ago` | 17.3 ns | — |

Two things worth stating plainly:

**Seeking is measurably constant time.** Stepping back 299 samples costs the same
as stepping back 1 — around 0.5 ns either way. §21's M4 acceptance criterion asks
for "constant or effectively constant time"; this is the evidence, not an
assertion.

**Recording is dominated by contributor extraction**, not by the ring. At 10,000
processes `record_one_sample` (972 µs) is almost exactly
`contributors/top_10_of/10000` (977 µs). The ring itself is free; selecting the
top ten of ten thousand is the cost. That is where to look if history recording
ever needs to be faster.

### Contributor extraction

| Benchmark | 200 | 10,000 |
|---|---:|---:|
| `top_10_of` | 13.8 µs | 977 µs |
| `top_10_with_trend` | 13.9 µs | — |

Deriving trends against the previous sample is free within noise (13.78 → 13.90
µs), so the §2.5 comparison values cost nothing worth optimising.

### Process list

| Benchmark | 200 | 10,000 |
|---|---:|---:|
| `sort_by_cpu` | 17.0 µs | 1.61 ms |
| `sort_by_name` | 31.7 µs | 755 µs |
| `filter_plain_text` | 1.50 µs | 59.9 µs |
| `filter_no_matches` | 2.18 µs | 109 µs |
| `build_tree` | 10.3 µs | 690 µs |

**Sorting by CPU is slower than sorting by name at 10,000 processes** (1.61 ms vs
755 µs), which inverts the 200-process result. This is not a defect: the synthetic
workload gives its processes only 17 distinct CPU values, so nearly every
comparison ties and falls through to the `(pid, start_key)` tie-breaker that §7.2
requires for stable selection. Names, by contrast, are distinct enough to sort in
one pass.

The finding is real and worth keeping in mind: on a machine where most processes
are idle at 0%, CPU sorting is tie-breaker-bound. It is also the argument against
"optimising" the tie-breaker away — it is what stops rows jumping every tick.

`filter_no_matches` costing more than `filter_plain_text` is expected: a match can
stop at the first field, a non-match must check all of them.

### Formatting

| Benchmark | 200 | 10,000 |
|---|---:|---:|
| `format_table_rows` | 73.5 µs | 3.71 ms |
| `format_bytes_at_unit_boundaries` (10 values) | 525 ns | — |

`format_table_rows` formats every column of every process, which is deliberately
pessimistic — a real frame formats only the visible rows, typically 20 to 50. At
that scale the cost is a few microseconds. The 10,000-row figure is the answer to
"what if we formatted everything", and the answer is: don't.

Unit-boundary formatting is ~52 ns per value, so the stable-width guarantee of
§5.4 is not paid for in performance.

### End to end

| Benchmark | 200 | 10,000 |
|---|---:|---:|
| `fake_collector_tick` | 19.6 µs | 1.26 ms |

This is the fake collector, so it measures the snapshot *construction* cost with
no OS reads. It is a floor for the sampling loop, not a prediction of it: the
live `sysinfo` collector's cost is dominated by reading `/proc` or calling
`sysctl`, which this cannot model.

## What is not measured yet

Named here so their absence is not mistaken for a passing grade:

* Linux `/proc` fixture parsing — §16.3 lists it; the parsers exist but the
  benchmark does not.
* Diagnostic-rule evaluation.
* ASCII and Unicode graph generation.
* The end-to-end §16.1 budgets: idle self CPU, resident memory, input-to-visible
  response, frame render time, sample collection p95 against the live collector.
* The 12-hour soak test for unbounded memory and file-descriptor growth.

## Reading these numbers

Sum the relevant rows for a per-tick estimate, then remember what is missing.
For the 200-process reference workload, one fast tick's *pure computation* is
roughly: rate updates (1.7 µs) + history record (14 µs) + sort (17 µs) + filter
(1.5 µs) ≈ 35 µs, plus formatting for the visible rows only. Against a 200 ms p95
collection budget, computation is not the constraint — the OS reads are. That is
the useful conclusion, and it is the reason no micro-optimisation has been applied
to any of the above.
