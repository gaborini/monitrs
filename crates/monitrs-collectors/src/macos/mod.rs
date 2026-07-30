//! macOS native enrichment on top of the `sysinfo` baseline (§9.3).
//!
//! # What this layer adds
//!
//! | Metric | Baseline | Here |
//! |---|---|---|
//! | Process identity | `(pid, start_seconds)`, and `(pid, 0)` for another user's process | `(pid, start_microseconds)` for every process |
//! | Processes the baseline cannot read | absent, or present with zeroed counters | present, with `PermissionDenied` counters |
//! | Memory | `total - available` from `sysinfo` | `host_statistics64`, with wired and compressed reported separately (§8.4) |
//! | Swap | capacity only | capacity plus swap-in and swap-out rates |
//! | CPU | aggregate and per-core percentages | the same, plus the user/system/nice/idle split |
//! | Threads, open files, niceness | unsupported | from `proc_pidinfo` |
//! | Open descriptors and socket counts | unsupported | from `PROC_PIDLISTFDS`, on demand (§8.6) |
//! | Filesystem inode counts | unsupported | from `getfsstat` |
//! | Link state and speed | unsupported | from `getifaddrs` |
//! | Battery | unsupported | from `IOPowerSources` |
//!
//! # What is deliberately absent
//!
//! * **Per-GPU metrics.** The only interfaces that expose them are `IOReport` and
//!   the private accelerator classes, which §9.3 forbids in the default build.
//!   They are therefore missing rather than approximated from something adjacent.
//! * **Device busy time.** §7.3 forbids deriving it from throughput, and there is
//!   no documented macOS API for the real thing.
//! * **Temperatures.** There is no documented thermal-sensor API on Apple Silicon.
//!   Whatever the baseline finds is passed through untouched; this layer adds none.
//! * **Battery cycle count and health.** Both live under undocumented
//!   `AppleSmartBattery` registry property names.
//! * **Per-process network throughput.** There is no documented per-process
//!   interface counter; `nettop` uses a private one. Absent rather than guessed.
//! * **A complete descriptor list for a process holding thousands.** The list is
//!   capped at [`monitrs_core::model::OpenFileList::MAX_LISTED`] because naming a
//!   descriptor costs a syscall, and the panel says how many it did not name rather
//!   than presenting a prefix as the whole table.
//!
//! # Constraints this module is built to satisfy
//!
//! * **No external commands, anywhere.** Not `ps`, `top`, `vm_stat`, `iostat`,
//!   `netstat`, `lsof`, or `system_profiler`; not in the sampling loop and not in a
//!   test. The `the_macos_module_never_spawns_an_external_command` test greps this
//!   directory for process-spawning constructs and fails if it finds one.
//! * **No private or undocumented APIs.** Every declaration in the `ffi` submodule
//!   comes from a public SDK header.
//! * **No Full Disk Access.** Nothing here opens a file at all: every read is a
//!   `sysctl`, a `libproc` call, a mach routine, or `getifaddrs`.
//! * **Both architectures.** The page size, the statistics-clock rate, and the mach
//!   timebase are all queried. Nothing about 4 KiB pages, 100 Hz, or a 1:1 timebase
//!   is assumed.
//! * **Every `unsafe` block names its invariant.** Unsafe is confined to `ffi` and
//!   to the wrapper functions in the sibling modules; the crate root denies
//!   `unsafe_op_in_unsafe_fn`, and clippy denies an unsafe block without a
//!   `SAFETY:` comment.
//!
//! # Off macOS
//!
//! Everything below is gated on `target_os = "macos"` and the `macos-native`
//! feature, so on Linux this module contains nothing but
//! [`NATIVE_ENRICHMENT_COMPILED`] and its documentation. The source-policy test is
//! deliberately *not* gated: it reads this directory as text, so it guards the
//! module from every platform's CI.

/// Whether the native macOS enrichment is compiled into this build.
///
/// False off macOS and false when the `macos-native` feature is disabled. Exposed
/// so a caller can report which collector it is about to construct without
/// duplicating the `cfg` predicate — and getting it subtly wrong.
pub const NATIVE_ENRICHMENT_COMPILED: bool =
    cfg!(all(target_os = "macos", feature = "macos-native"));

#[cfg(all(target_os = "macos", feature = "macos-native"))]
mod collector;
#[cfg(all(target_os = "macos", feature = "macos-native"))]
mod cpu;
#[cfg(all(target_os = "macos", feature = "macos-native"))]
mod ffi;
#[cfg(all(target_os = "macos", feature = "macos-native"))]
mod filesystem;
#[cfg(all(target_os = "macos", feature = "macos-native"))]
mod memory;
#[cfg(all(target_os = "macos", feature = "macos-native"))]
mod network;
#[cfg(all(target_os = "macos", feature = "macos-native"))]
mod power;
#[cfg(all(target_os = "macos", feature = "macos-native"))]
mod process;
#[cfg(all(target_os = "macos", feature = "macos-native"))]
mod signal;
#[cfg(all(target_os = "macos", feature = "macos-native"))]
mod sysctl;

#[cfg(all(target_os = "macos", feature = "macos-native"))]
pub use collector::{MachineFacts, MacosCollector};
#[cfg(all(target_os = "macos", feature = "macos-native"))]
pub use network::InterfaceLink;
#[cfg(all(target_os = "macos", feature = "macos-native"))]
pub use process::{KernelProcess, Timebase, read_process_arguments};
#[cfg(all(target_os = "macos", feature = "macos-native"))]
pub use signal::{MacosSignal, SignalOutcome, identity_is_current, send_signal};

#[cfg(test)]
mod tests {
    /// The directory this module's sources live in.
    fn source_directory() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/macos")
    }

    /// Every `.rs` file in this module, as `(name, contents)`.
    fn sources() -> Vec<(String, String)> {
        let directory = source_directory();
        let entries = std::fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
        let mut files = Vec::new();
        for entry in entries {
            let path = entry.expect("a readable directory entry").path();
            if path.extension().is_some_and(|extension| extension == "rs") {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
                files.push((name, text));
            }
        }
        assert!(
            files.len() >= 9,
            "expected the whole module, found {} files",
            files.len()
        );
        files
    }

    /// Strips comment lines so prose about a forbidden construct does not read as
    /// a use of it.
    fn code_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
        text.lines().enumerate().filter(|(_, line)| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("//") && !trimmed.starts_with("*")
        })
    }

    #[test]
    fn the_macos_module_never_spawns_an_external_command() {
        // §9.3 forbids `ps`, `top`, `vm_stat`, `iostat`, `netstat`, and `lsof`
        // anywhere, not merely in the sampling loop. Rather than blocklist tool
        // names — which a wrapper script would evade — this looks for the
        // constructs that can start a process at all.
        //
        // The needles are assembled from fragments at runtime so that this test's
        // own source does not match them.
        let forbidden = [
            format!("Command{}new", "::"),
            format!("process{}Command", "::"),
            format!("posix_{}", "spawn"),
            format!("exec{}", "ve("),
            format!("exec{}", "vp("),
            format!("exec{}", "l("),
            format!("pope{}", "n("),
            format!("syst{}", "em("),
            format!("fo{}", "rk("),
        ];

        let mut offences = Vec::new();
        for (name, text) in sources() {
            for (number, line) in code_lines(&text) {
                for needle in &forbidden {
                    if line.contains(needle.as_str()) {
                        offences.push(format!("{name}:{}: {}", number + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            offences.is_empty(),
            "the macOS collector must not start a process:\n{}",
            offences.join("\n")
        );
    }

    #[test]
    fn the_macos_module_reads_no_files_and_so_needs_no_full_disk_access() {
        // §9.3: Full Disk Access must not be required. The strongest form of that
        // guarantee is that nothing here opens a path at all — every read is a
        // syscall against kernel state. The test module itself does read files, so
        // only the non-test half of each file is examined.
        let forbidden = [
            format!("File{}open", "::"),
            format!("fs{}read", "::"),
            format!("OpenOpt{}", "ions"),
            format!("read_to_str{}", "ing("),
        ];
        for (name, text) in sources() {
            let code = text
                .split_once("mod tests {")
                .map_or(text.as_str(), |(before, _)| before)
                .to_owned();
            for (number, line) in code_lines(&code) {
                for needle in &forbidden {
                    assert!(
                        !line.contains(needle.as_str()),
                        "{name}:{}: the collector must not open files: {}",
                        number + 1,
                        line.trim()
                    );
                }
            }
        }
    }

    /// How many lines above an unsafe block its `SAFETY:` note may sit.
    ///
    /// A multi-line justification plus the line the block opens on: wide enough to
    /// survive reformatting, narrow enough that an unrelated note cannot satisfy it.
    const SAFETY_WINDOW: usize = 5;

    #[test]
    fn every_unsafe_block_carries_a_safety_comment() {
        // Clippy already denies this crate-wide, but the rule matters enough (§15.3)
        // to be visible as a test: a reviewer reading this file learns the
        // convention without having to know the lint configuration. The needles are
        // assembled at runtime so this test's own source does not match them.
        let opener = format!("unsafe{}", " {");
        let implementation = format!("{}{}", "unsafe ", "impl");
        for (name, text) in sources() {
            let lines: Vec<&str> = text.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                if !trimmed.contains(opener.as_str())
                    && !trimmed.starts_with(implementation.as_str())
                {
                    continue;
                }
                let window = index.saturating_sub(SAFETY_WINDOW)..index;
                let documented = lines
                    .get(window)
                    .is_some_and(|above| above.iter().any(|line| line.contains("SAFETY")));
                assert!(
                    documented,
                    "{name}:{}: unsafe without a SAFETY comment: {}",
                    index + 1,
                    trimmed
                );
            }
        }
    }

    #[test]
    fn the_compiled_in_flag_matches_the_platform_this_test_runs_on() {
        let expected = cfg!(all(target_os = "macos", feature = "macos-native"));
        assert_eq!(super::NATIVE_ENRICHMENT_COMPILED, expected);
    }
}
