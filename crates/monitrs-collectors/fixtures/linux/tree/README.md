# Sanitized `/proc` and `/sys` tree fixture

A coherent, hand-written snapshot of one Linux host, used by the tests in
`src/linux/read.rs` to exercise the *path-reading* layer on any platform — the
host running the tests does not need to be Linux and does not need `/proc`.

The host it describes is deliberately awkward, because the awkward cases are the
ones §9.2 and §17.2 require to be covered:

* four logical CPUs, `MemAvailable` present, PSI present but the CPU resource has
  no `full` line — which is what most kernels report;
* `sys/fs/cgroup` is cgroup v2 (`cgroup.controllers` exists) with a 2 GiB
  `memory.max` and a 1.5-CPU `cpu.max`, so the container limit is *lower* than
  the host total and both must stay observable (§9.2);
* `sys/class/dmi/id/sys_vendor` says `QEMU`, so the environment heuristic sees
  both container and virtual-machine evidence and has to choose;
* PID 1 is `systemd`, PID 2 is the `kthreadd` kernel thread with an empty
  `cmdline` and no `io` file, PID 4242 is a container process, and PID 9182 is
  named `((weird) name) with spaces`;
* `sys/class/power_supply` holds three entries and only one of them is the system
  battery: `BAT0` (`type` `Battery`, `scope` `System`, an energy-reporting ACPI
  pack), `AC` (`type` `Mains` — the charger), and `hid-e4-battery` (`type`
  `Battery` but `scope` `Device` — a bluetooth peripheral's own cell). `BAT0`
  exports no `charge_full` and no `time_to_empty_now`, which is what makes those
  metrics `n/a` rather than zero;
* PID 9999 deliberately does **not** exist, so a read of it exercises the
  vanished-process path.

Nothing here came from a real machine: every counter, UID, and path was written
by hand, so there is no host name, user name, or command-line argument to leak
(§15.2).
