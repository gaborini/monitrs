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
availability, and the Inspect screen (`5`) shows what **your** machine provides
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
| Process open files / sockets | yes | own only | |
| Kernel threads distinguishable | yes | - | So "hide kernel threads" is Linux-only |
| cgroup path and limits | yes (v2) | - | |
| Container detection | heuristic | heuristic | Labelled as a heuristic with its evidence |
| Filesystem capacity | yes | yes | |
| Device throughput | yes | yes | |
| Device busy percentage | yes | - | Only where semantically correct — see `metrics.md` |
| Device to mount-point mapping | yes | partial | On-demand tier; expensive |
| Interface counters | yes | yes | |
| Interface errors and drops | yes | partial | |
| Interface link speed | usually | often not | Without it, no utilization percentage is shown |
| Per-process network | no | no | Out of scope for v1 |
| Linux PSI | yes (4.20+) | - | The best memory and I/O pressure signal available |
| Temperatures | hwmon | partial | Absent on most servers and many VMs |
| Battery | yes | yes | Absent on desktops and servers |
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
| `/proc/<pid>/fd/` | on demand | open file and socket counts |
| `/proc/pressure/{cpu,memory,io}` | fast | PSI |
| `/sys/fs/cgroup/**` | slow | cgroup limits |
| `/sys/class/net/*` | slow | link state, speed, interface type |
| `/sys/class/hwmon/**` | medium | temperatures |
| `/sys/class/power_supply/**` | medium | battery |
| `/etc/passwd` (via libc) | slow | resolving uid to user name |

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
| `getfsstat` / `statfs` | mounted filesystems and capacity |
| `getifaddrs` | interfaces, addresses, and counters |
| IOKit (documented APIs only) | device throughput, battery |

Constraints, all of them deliberate:

* **No external commands.** `ps`, `top`, `vm_stat`, `iostat`, `netstat`, `lsof`,
  and `system_profiler` are never spawned. Not in the sampling loop, not anywhere.
* **No private or undocumented APIs** in the default build. That includes
  `IOReport` and the private GPU interfaces, which is why per-GPU metrics are
  absent rather than approximated.
* **Full Disk Access is not required.** monitrs works without it; some
  per-process details for other users' processes are unavailable, and say so.
* Another user's process may hide its command line and working directory. That is
  reported as `permission denied`, not as an empty string.
* Any FFI needed is confined to the macOS collector module, keeps
  CoreFoundation ownership explicit, and every `unsafe` block carries a `SAFETY:`
  comment naming the invariant that makes it sound. `monitrs-core` and
  `monitrs-tui` forbid `unsafe` outright.

## Privileges

monitrs runs unprivileged by design and **never escalates**. It does not invoke
`sudo`, does not re-exec itself, and does not run any external command.

Running as root additionally provides per-process I/O counters, working
directories, and open-file counts for processes you do not own. Nothing else
changes. Whether that is worth running a monitor as root is your decision; the
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
