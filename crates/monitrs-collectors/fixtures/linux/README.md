# Linux `/proc` and `/sys` fixtures

Sanitized inputs for the parsers in `src/linux/`. §17.2 requires the collectors to be
testable without a live filesystem, and these files are what make that true: every
parser takes `&[u8]`, so the whole Linux layer is compiled and tested on macOS and
Windows as well as on Linux.

## Layout

| Path | Purpose |
|---|---|
| `cases/<source>/<case>.txt` | One file per case, loaded with `include_bytes!` by the test module in the matching parser. A renamed or deleted fixture is a compile error rather than a test that quietly stops covering anything. |
| `cases/pid_cmdline/*.bin` | NUL-separated binaries, so the separator and the non-UTF-8 case are real rather than described. |
| `tree/{proc,sys}/…` | One coherent host, used by `src/linux/read.rs` to exercise the *path-reading* layer. See `tree/README.md`. |

## Nothing here came from a real machine

§15.2 forbids leaking host names, user names, and command-line arguments. Every
counter, UID, cgroup path, and container digest below was written by hand. The one
argument that looks like a secret — `postgres://user:hunter2@db/prod` in
`cases/pid_cmdline/with_secret_argument.bin` — is invented, and exists precisely so
the redaction §14.2 requires has something to redact.

## The adversarial cases, and why each exists

| Fixture | What it pins down |
|---|---|
| `pid_stat/parens_and_spaces_in_name.txt` | A process named `((weird) name) with spaces`. Splitting `/proc/<pid>/stat` on whitespace shifts *every* field after the name, including field 22 — the process identity. §9.2. |
| `pid_stat/reused_pid_same_second.txt` | The same PID with a start time in the same whole second but a different clock tick. The cross-platform baseline cannot tell these apart; the native start key can. §26. |
| `pid_stat/unterminated_name.txt`, `truncated.txt`, `empty.txt` | A `stat` with no closing parenthesis, one cut short, and one read from a process that had already gone. None may produce an identity. |
| `pid_stat/kernel_thread.txt` | `PF_KTHREAD` in the flags field, which is the only sound kernel-thread test. §7.2. |
| `pid_stat/zombie.txt` | A process a signal cannot affect. §15.1. |
| `pid_io/*` | The counters behind the disk columns, plus the empty and truncated reads. The `EACCES` case is not a fixture: it is constructed from the raw errno in `read.rs`, so the test is portable and cannot pass by accident when run as root. |
| `pressure/cpu_without_full.txt` | Most kernels omit the `full` line for CPU. It must read as unsupported, never as 0%. §9.2. |
| `pressure/io_idle.txt` | A genuinely idle resource, whose measured `0.00%` must stay distinguishable from the above. |
| `cgroup/memory.max_unlimited.txt`, `memory.max_v1_sentinel.txt` | The two spellings of "no limit": the literal `max` and cgroup v1's `9223372036854771712`. Neither may become a limit. §9.2. |
| `cgroup/cpu.max_malformed.txt` | One field where the kernel always writes two. |
| `diskstats/short_fields.txt` | The 4-field reduced form, which carries no field 10 — so no busy percentage may be derived. §7.3. |
| `diskstats/after_reset.txt`, `net_dev/after_reset.txt` | Counters that restarted. §8.2 forbids reporting the new value as one interval's traffic. |
| `diskstats/huge_counters.txt`, `net_dev/huge_counters.txt`, `proc_stat/huge_counters.txt` | Counters at or near `u64::MAX`, which must neither panic nor wrap. |
| `net_dev/header_only.txt` | A valid file listing no interfaces — a real state in a fresh network namespace, and not a parse failure. |
| `net_dev/truncated.txt` | Five counters where sixteen are expected. Accepting it would read a receive FIFO count as transmitted bytes. |
| `meminfo/no_memavailable.txt` | A kernel older than 3.14. §8.4 permits `total - available` only where a meaningful estimate exists, so the enrichment must decline rather than silently redefine "used". |
| `meminfo/malformed_units.txt` | `MemTotal` in `MB`. Assuming `kB` would understate memory a thousandfold. |
| `sys_class_net/speed_unknown_negative.txt` | The `-1` an unnegotiated link reports. §7.4 forbids a utilisation percentage without a known speed. |
| `dmi/sys_vendor_physical.txt` | A vendor string naming no hypervisor — which is *not* evidence of bare metal, and §7.5 has no such conclusion to draw. |
| `power_supply/type_mains.txt`, `scope_device.txt` | The two entries in `/sys/class/power_supply` that are **not** the system battery: the charger, and a bluetooth peripheral's own cell. Both are present on an ordinary laptop, and reporting either would put a mouse's charge on the Battery screen. |
| `power_supply/status_not_charging.txt` | `Not charging`, with the space the kernel writes. A pack held at 80% by a charge threshold is not `Full`, and calling it that would misreport it. |
| `power_supply/temp_314.txt` | Tenths of a degree, which is the unit the ABI specifies. The `-2731` "no sensor" sentinel is not a fixture: it is a literal in `power.rs`, because the point of the test is that a number reading as a plausible −273.1 °C is rejected. |
