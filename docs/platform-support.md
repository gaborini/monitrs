# Platform support

## Target matrix

| OS | Architecture | Tier | Expectation |
|---|---|---|---|
| Linux (glibc) | x86_64 | 1 | Full support, built and tested in CI |
| Linux (glibc) | aarch64 | 1 | Full support, built and tested in CI |
| macOS | arm64 | 1 | Full support, built and tested in CI |
| macOS | x86_64 | 1 | Full support, built in CI |
| Linux (musl) | x86_64 | 2 | Best-effort static binary, built in CI |
| Linux (musl) | aarch64 | 2 | Best-effort static binary, built in CI |

Windows is not supported. Neither are the BSDs, remote agents, or eBPF-based
collection. These are v1 scope decisions rather than oversights.

**Tier 2 means built, not exercised.** A musl static binary is produced and
smoke-tested, but glibc-specific metrics may behave differently and the musl
targets do not gate a release.

**macOS x86_64 is cross-compiled and has not run on a genuine Intel Mac.** GitHub is
retiring its Intel runners, so CI builds that target and cannot execute it. It has been
run by hand — under Rosetta on Apple Silicon, where it works: same process count, same
capabilities, one exception. It reports **temperatures as `unsupported`** where the arm64
build on the same machine reports them as available, because the sensor enumeration goes
through IOKit and a translated process does not see the same components. That is most
likely an artefact of the translation rather than of the target, and it is *reported
honestly either way* — `unsupported` rather than a fabricated zero (§4). Nobody has
checked it on real Intel hardware, and until somebody has, this paragraph is the whole of
what is known.

## Support is per metric, not per platform

A single "supported" flag per OS would be a lie in both directions: Linux exposes
PSI and per-process I/O that macOS does not, and macOS exposes memory compression
that Linux has no equivalent for. Every metric therefore carries its own
availability, and the Inspect screen (`6`) shows what **your** machine provides
right now — which is the only authoritative answer, since it also depends on your
kernel version, your privileges, and whether you are in a container.

The table below is what to expect. `-` means the platform has no such concept,
`root` means it exists but needs privileges you may not have.

| Metric | Linux | macOS | Notes |
|---|---|---|---|
| System CPU utilization | yes | yes | |
| Per-core CPU | yes | yes | |
| CPU time breakdown (user/system/nice/idle) | yes | yes | |
| iowait, irq, softirq, steal | yes | - | Linux-only fields of `/proc/stat` |
| Load average | yes | yes | |
| Memory total / available / used | yes | yes | Different definitions — see `metrics.md` |
| Page cache, buffers | yes | cache only | `buffers` is Linux-only |
| Wired, compressed pages | - | yes | No Linux equivalent |
| Swap capacity and usage | yes | yes | |
| Swap in/out rates | yes | partial | Depends on what the platform exposes |
| Process list, CPU, RSS, virtual | yes | yes | |
| Process thread count | yes | yes | |
| Process I/O bytes | yes | partial | Linux: `/proc/<pid>/io`, **root** for other users' processes |
| Process working directory | yes | own only | macOS restricts this for other users |
| Process open-file count | own only | own only | On-demand tier; needs root for others |
| Process open-file list, with paths | own only | own only | Capped at 256 descriptors; the panel says how many it left out |
| Process socket count | - | own only | macOS classifies the whole table in one call; `/proc/<pid>/fd` cannot |
| Kernel threads distinguishable | yes | - | So "hide kernel threads" is Linux-only |
| cgroup path and limits | yes (v2) | - | |
| Container detection | heuristic | heuristic | Labelled as a heuristic with its evidence |
| Filesystem capacity | yes | yes | |
| Filesystem inode counts | yes | yes | `statfs`/`getfsstat`; `n/a` where a filesystem has no fixed table |
| Device throughput | yes | yes | |
| Device busy percentage | yes | - | Only where semantically correct — see `metrics.md` |
| Device to mount-point mapping | yes | partial | On-demand tier; expensive |
| Interface counters | yes | yes | |
| Interface errors and drops | yes | partial | |
| Interface link speed | usually | often not | Without it, no utilization percentage is shown |
| Per-process network | no | no | Out of scope for v1 |
| Linux PSI | yes (4.20+) | - | The best memory and I/O pressure signal available |
| Temperatures | hwmon | partial | Absent on most servers and many VMs. Read every 30 s, 5 s while the Battery screen is open; carried over and aged between reads — see "What monitrs reads" below |
| Battery presence, charge, state | yes | yes | Absent on desktops, servers, VMs and containers. Same 30 s / 5 s cadence as temperatures |
| Battery time remaining | driver-dependent | yes | Only where the platform publishes an estimate; never derived |
| Battery cycle count | usually | - | macOS needs undocumented registry properties (§9.3) |
| Battery design vs full capacity | usually | - | Same; this is the figure that shows wear |
| Battery temperature | driver-dependent | - | Same |
| Battery power draw in watts | usually | - | Same |
| Process signals | yes | yes | Subject to permissions |
| Renice | yes | yes | Lowering niceness needs privileges |

## What monitrs reads

§15.2 requires documenting exactly which local files and OS interfaces are read.
This is the complete list. Nothing else is touched, nothing is written, and no
network socket is opened.

### Everywhere

* The cross-platform `sysinfo` baseline, which on both platforms reads the same
  kernel interfaces listed below.
* Environment variables, read only to decide rendering: `TERM`, `COLORTERM`,
  `NO_COLOR`, `LANG`, `LC_ALL`, `LC_CTYPE`, and the platform config-directory
  variables (`XDG_CONFIG_HOME`, `HOME`).
* The configuration file, if one exists. Never created without `config init`.
* The debug log file, only when `--debug-log` is given.

**Not read:** any process's environment variables. `/proc/<pid>/environ` and its
macOS equivalent are never opened. §7.5 forbids displaying their values, and the
safest implementation of that rule is not to read them at all.

### Linux

Read-only, on the tier indicated:

| Path | Tier | For |
|---|---|---|
| `/proc/stat` | fast | CPU time counters |
| `/proc/meminfo` | fast | memory including `MemAvailable` |
| `/proc/loadavg` | fast | load averages |
| `/proc/uptime` | slow | uptime |
| `/proc/diskstats` | fast | device throughput, IOPS, busy time |
| `/proc/net/dev` | fast | interface counters |
| `/proc/<pid>/stat` | fast | process state, CPU time, start time |
| `/proc/<pid>/status` | fast | RSS, threads, uid |
| `/proc/<pid>/io` | fast | per-process read/write bytes |
| `/proc/<pid>/cmdline` | fast | command line |
| `/proc/<pid>/cgroup` | slow | cgroup path, container identity |
| `/proc/<pid>/cwd` | on demand | working directory (readlink) |
| `/proc/<pid>/fd/` | on demand | open-file count (one directory read) |
| `/proc/<pid>/fd/<n>` | on demand | what each descriptor points at (one `readlink` each, capped at 256) |
| `/proc/pressure/{cpu,memory,io}` | fast | PSI |
| `/sys/fs/cgroup/**` | slow | cgroup limits |
| `/sys/class/net/*` | slow | link state, speed, interface type |
| `/sys/class/hwmon/**` | sensors\* | temperatures |
| `/sys/class/power_supply/*/` — `type`, `scope`, `status`, `capacity`, `cycle_count`, `energy_full{,_design}`, `charge_full{,_design}`, `voltage_min_design`, `power_now`, `current_now`, `voltage_now`, `temp`, `time_to_{empty,full}_now` | sensors\* | battery |
| `/etc/passwd` (via libc) | slow | resolving uid to user name |

\* Temperatures and battery are not on any of the four tiers above; they are their
own **sensor group**, riding on the medium tier's interval (5 s by default) while
the Battery screen specifically is open — only that screen, not any screen that
happens to display a sensor reading; the header's own temperature is visible on
every screen without tightening the cadence — and on the slow tier's interval
(30 s) otherwise, rather than a fixed cadence of its own. Between reads, the last
value is carried forward and shown aged rather than dropped — the Battery
screen's own fields and the header's single temperature can be up to 30 seconds
old, and say so (`Stale { value, age }`, rendered as e.g. `temp 62.0C ~00:28`).
The cadence split exists because the read itself is not cheap: on macOS,
`Components::refresh` for temperatures costs about 85 ms regardless of how many
sensors exist, which on a plain 5-second schedule was enough to fail §16.1's idle
CPU p95 budget — and, measured after the split, still fails it, for a smaller and
not yet identified reason — see `benchmarks.md`.

Plus one syscall that is not a file read: `statfs(2)` on each mount point, on the
medium tier, for the inode counts. Nothing under `/proc` reports `f_files`, so
there is no file to read instead.

Notes on the awkward parts:

* `/proc/<pid>/stat` field 2 is the process name **in parentheses**, and process
  names may themselves contain spaces and parentheses. The parser finds the last
  `)` rather than splitting on whitespace. There are fixtures for exactly this.
* A process can disappear between any two reads. That is expected, is reported as
  `process exited` on the affected field, and is never logged as a warning — a
  busy machine would otherwise produce thousands of log lines a minute.
* `/proc` subtrees are never scanned recursively without a bound, and expensive
  per-process reads are capped so a 10,000-process machine cannot stall the fast
  tier.
* Permission denial is a metric state, never a fatal error.
* cgroup v2 is detected explicitly. Container limits are reported separately from
  host totals, and both are shown where observable.
* `/sys/class/power_supply` is one directory level, capped at 16 entries, and every
  attribute read is capped like any other `/sys` read. The directory is not a list of
  batteries — the charger, a UPS and a bluetooth mouse's own cell all appear in it —
  so the system battery is identified by `type` being `Battery` and `scope`, where
  present, being `System`. Both are documented in the kernel's
  `sysfs-class-power` ABI; a name whitelist would need extending for every new driver.
* Two `/sys` attributes report zero for "I do not know", and both are treated as
  unknown rather than as measurements: `cycle_count` (also `4294967295`, the ACPI
  `_BIX` sentinel) and `time_to_empty_now`. `power_now` reporting zero *is* taken as a
  measurement, because a full pack on mains really draws nothing. See `metrics.md`.

### macOS

Through documented libc, `sysctl`, and Mach interfaces:

| Interface | For |
|---|---|
| `sysctl` `hw.*` | CPU counts, memory size, model |
| `sysctl` `kern.boottime` | boot time and uptime |
| `sysctl` `kern.proc.*` | process list, start time, uid, state |
| `host_statistics64` (Mach) | memory including wired and compressed pages |
| `host_processor_info` (Mach) | per-CPU time counters |
| `getloadavg` | load averages |
| `proc_pidinfo` | per-process CPU, memory, and I/O counters |
| `proc_pidinfo` `PROC_PIDLISTFDS` | open descriptors and their kinds, on demand |
| `proc_pidfdinfo` `PROC_PIDFDVNODEPATHINFO` | the path one descriptor points at, on demand |
| `getfsstat` / `statfs` | mounted filesystems, capacity, and inode counts |
| `getifaddrs` | interfaces, addresses, and counters |
| IOKit `IOPowerSources` (documented) | battery presence, charge, state, time remaining |
| IOKit (documented APIs only) | device throughput |

Constraints, all of them deliberate:

* **No external commands.** `ps`, `top`, `vm_stat`, `iostat`, `netstat`, `lsof`,
  and `system_profiler` are never spawned. Not in the sampling loop, not anywhere.
* **No private or undocumented APIs** in the default build. That includes
  `IOReport` and the private GPU interfaces, which is why per-GPU metrics are
  absent rather than approximated.
* The same rule costs four battery fields. `IOPowerSources` publishes the charge,
  the state, and the time-remaining estimate; cycle count, design capacity, pack
  temperature and instantaneous amperage live in the `AppleSmartBattery` I/O Registry
  node under property names Apple has never documented. All four read `n/a` on macOS,
  and the Battery screen says so rather than showing a nicer screen built on
  undocumented keys.
* **Full Disk Access is not required.** monitrs works without it; some
  per-process details for other users' processes are unavailable, and say so.
* Another user's process may hide its command line, working directory, and whole
  descriptor table. That is reported as `permission denied`, not as an empty string
  and not as zero open files.
* An open-file **path** is user data. It is shown on screen, never written to a log,
  and never serialized: a descriptor's path is replaced by its availability state in
  `ProcessDetail`'s own `Serialize` implementation, so no export can carry one
  (§15.2, §19).
* Any FFI needed is confined to the macOS collector module, keeps
  CoreFoundation ownership explicit, and every `unsafe` block carries a `SAFETY:`
  comment naming the invariant that makes it sound. `monitrs-core` and
  `monitrs-tui` forbid `unsafe` outright.

## Privileges

monitrs runs unprivileged by design and **never escalates**. It does not invoke
`sudo`, does not re-exec itself, and does not run any external command.

Running as root additionally provides per-process I/O counters, working
directories, and open descriptors — count, socket count, and paths — for processes
you do not own. Nothing else changes. Whether that is worth running a monitor as root is your decision; the
Inspect screen tells you exactly what you are missing so you can make it.

Elevated privileges cannot conjure a metric the kernel does not provide, which is
why `permission denied` and `n/a` are distinct states.

## Containers and virtual machines

Inside a container:

* Process visibility is whatever the PID namespace allows. A container showing
  three processes is correct, not broken.
* The applicable memory ceiling is the cgroup limit, not the host total. Both are
  shown and labelled where observable.
* CPU counts may be the host's while the cgroup quota is much smaller. Read the
  Inspect screen rather than the CPU count.
* Container detection is a **heuristic** and is labelled as one, with the
  evidence that produced it and a confidence level. There is deliberately no
  "bare metal" classification: absence of container evidence is not proof of its
  absence.

Inside a VM, `steal` time on Linux is the most useful signal that the hypervisor
is oversubscribed. It is unavailable on macOS.

## Terminals

monitrs is developed against a 256-colour or true-colour terminal but is designed
for the worst case, and the worst case is tested:

* Strict 7-bit ASCII mode renders on any terminal in any locale.
* `--color off` and the `NO_COLOR` convention leave every state legible, because
  each one carries a redundant symbol.
* Layouts are defined from 60×16 upward. Below the recommended size it degrades
  deliberately rather than wrapping; below 60×16 it prints what it needs and how
  to quit.
* Works over SSH and inside `tmux`. `TERM` and `COLORTERM` are the only signals
  used for capability detection.
* Zero-area render regions are handled everywhere, so an aggressive resize cannot
  panic.
