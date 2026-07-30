//! Every foreign declaration the macOS collector makes, in one auditable place.
//!
//! # Why the declarations are collected here
//!
//! §9.3 requires FFI struct layouts and ownership to be explicit, and §15.3
//! requires unsafe code to stay tightly scoped. Both are easier to audit when
//! there is exactly one file to read: everything below is either a layout copied
//! from a public SDK header or a function signature copied from a public header,
//! and nothing below *calls* anything. The safe wrappers live in the sibling
//! modules.
//!
//! # What is deliberately absent
//!
//! No `IOReport`, no `AppleSMC`, no `IOAccelerator`, no `task_for_pid`. §9.3
//! forbids private and undocumented interfaces in the default build, which is why
//! per-GPU metrics and SMC temperatures are *absent* from this collector rather
//! than approximated from something adjacent.
//!
//! # Layout fidelity
//!
//! The `kinfo_proc` family is not in `libc` for Apple targets, so it is
//! transcribed here from `<sys/proc.h>` and `<sys/sysctl.h>`. Field names match
//! the headers verbatim so a reviewer can diff them line by line. Most fields are
//! never read — they exist because the kernel writes a whole `kinfo_proc` and the
//! offsets of the fields we *do* read depend on all the ones we do not.
//! [`tests::the_kinfo_proc_layout_matches_the_running_kernel`] asserts the total
//! size against the live kernel, which is the only check that can actually fail.

use core::ffi::{c_char, c_int, c_short, c_uchar, c_uint, c_void};

/// A type that can be materialised from arbitrary kernel-supplied bytes.
///
/// # Safety
///
/// Implementors must be `#[repr(C)]` (or a primitive), must contain no padding
/// that the kernel is not expected to write, and — crucially — **every** bit
/// pattern of their size must be a valid value. That rules out `bool`, `char`,
/// enums, and references, and admits integers, raw pointers, and `#[repr(C)]`
/// aggregates of those. It is what makes [`super::sysctl::scalar`] sound for an
/// arbitrary `T`.
pub(super) unsafe trait Pod: Copy {}

// SAFETY: every bit pattern of a fixed-width integer is a valid value of it.
unsafe impl Pod for u8 {}
// SAFETY: as above.
unsafe impl Pod for u32 {}
// SAFETY: as above.
unsafe impl Pod for u64 {}
// SAFETY: as above.
unsafe impl Pod for i32 {}
// SAFETY: as above.
unsafe impl Pod for i64 {}
// SAFETY: `timeval` is a `#[repr(C)]` pair of integers with no invalid values.
unsafe impl Pod for libc::timeval {}
// SAFETY: `xsw_usage` is a `#[repr(C)]` aggregate of integers; `xsu_encrypted` is
// a `boolean_t`, i.e. a plain `c_int`, so it too has no invalid bit pattern.
unsafe impl Pod for libc::xsw_usage {}
// SAFETY: `vm_statistics64` is a `#[repr(C, packed(8))]` aggregate of integers.
unsafe impl Pod for libc::vm_statistics64 {}
// SAFETY: `#[repr(C)]` aggregate of `c_int`s.
unsafe impl Pod for Clockinfo {}
// SAFETY: `#[repr(C)]` aggregates of integers and raw pointers only. Raw pointers
// admit every bit pattern, and none of them is ever dereferenced.
unsafe impl Pod for KinfoProc {}
// SAFETY: as above.
unsafe impl Pod for ExternProc {}
// SAFETY: `#[repr(C, packed(4))]` aggregate of integers.
unsafe impl Pod for IfData {}
// SAFETY: `#[repr(C)]` aggregates of integers and `c_char` arrays. `c_char` is
// `i8` on every Apple target, so every bit pattern is a valid value.
unsafe impl Pod for ProcFileinfo {}
// SAFETY: as above; `libc::vnode_info_path` is a `#[repr(C)]` aggregate of
// integers and a `c_char` array.
unsafe impl Pod for VnodeFdinfowithpath {}

/// `mach_timebase_info_data_t` from `<mach/mach_time.h>`.
///
/// Declared here rather than taken from `libc`, whose Apple mach bindings are
/// deprecated in favour of a crate this workspace does not depend on (§13: no
/// dependency is added for something a public header already provides).
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub(super) struct MachTimebaseInfo {
    /// Numerator of the absolute-time-to-nanoseconds ratio.
    pub(super) numer: u32,
    /// Denominator of the same ratio.
    pub(super) denom: u32,
}

/// `struct clockinfo` from `<sys/time.h>`, the payload of `kern.clockrate`.
///
/// `hz` is the statistics-clock frequency the `processor_cpu_load_info` tick
/// counters are expressed in. It is queried rather than assumed because §9.3
/// requires supporting both Apple Silicon and Intel without hard-coded rates.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(super) struct Clockinfo {
    /// Clock frequency in hertz.
    pub(super) hz: c_int,
    /// Microseconds per hz tick.
    pub(super) tick: c_int,
    /// Clock skew rate, retained for layout only.
    pub(super) tickadj: c_int,
    /// Statistics clock frequency.
    pub(super) stathz: c_int,
    /// Profiling clock frequency.
    pub(super) profhz: c_int,
}

/// `struct timeval32` from `<sys/_types/_timeval32.h>`.
///
/// `if_data` embeds the 32-bit form on LP64 targets (`IF_DATA_TIMEVAL`), so this
/// is not interchangeable with [`libc::timeval`].
#[derive(Clone, Copy, Debug)]
#[repr(C, packed(4))]
pub(super) struct Timeval32 {
    /// Seconds.
    pub(super) tv_sec: i32,
    /// Microseconds.
    pub(super) tv_usec: i32,
}

/// `struct if_data` from `<net/if_var.h>`, inside `#pragma pack(4)`.
///
/// This is what `getifaddrs` hangs off an `AF_LINK` entry's `ifa_data`. It is
/// *not* `if_data64`: the counters here are 32 bits wide, which is why this
/// collector reads only `ifi_baudrate` from it and leaves byte counters to the
/// baseline (§9.1).
#[allow(
    dead_code,
    reason = "the unread fields fix the offset of ifi_baudrate; see the module docs"
)]
#[derive(Clone, Copy, Debug)]
#[repr(C, packed(4))]
pub(super) struct IfData {
    pub(super) ifi_type: c_uchar,
    pub(super) ifi_typelen: c_uchar,
    pub(super) ifi_physical: c_uchar,
    pub(super) ifi_addrlen: c_uchar,
    pub(super) ifi_hdrlen: c_uchar,
    pub(super) ifi_recvquota: c_uchar,
    pub(super) ifi_xmitquota: c_uchar,
    pub(super) ifi_unused1: c_uchar,
    pub(super) ifi_mtu: u32,
    pub(super) ifi_metric: u32,
    pub(super) ifi_baudrate: u32,
    pub(super) ifi_ipackets: u32,
    pub(super) ifi_ierrors: u32,
    pub(super) ifi_opackets: u32,
    pub(super) ifi_oerrors: u32,
    pub(super) ifi_collisions: u32,
    pub(super) ifi_ibytes: u32,
    pub(super) ifi_obytes: u32,
    pub(super) ifi_imcasts: u32,
    pub(super) ifi_omcasts: u32,
    pub(super) ifi_iqdrops: u32,
    pub(super) ifi_noproto: u32,
    pub(super) ifi_recvtiming: u32,
    pub(super) ifi_xmittiming: u32,
    pub(super) ifi_lastchange: Timeval32,
    pub(super) ifi_unused2: u32,
    pub(super) ifi_hwassist: u32,
    pub(super) ifi_reserved1: u32,
    pub(super) ifi_reserved2: u32,
}

/// `struct extern_proc` from `<sys/proc.h>`.
///
/// The leading `p_un` union holds either two list pointers or a `timeval`. Both
/// arms are 16 bytes with 8-byte alignment on every 64-bit Apple target, so
/// declaring the `timeval` arm directly reproduces the C layout exactly while
/// naming the field this collector actually reads (§9.3: process start time).
#[allow(
    dead_code,
    reason = "the unread fields fix the offsets of p_stat, p_pid, p_nice and p_comm"
)]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(super) struct ExternProc {
    /// `p_un.__p_starttime`: when the process started, with microsecond
    /// resolution. The basis of [`monitrs_core::model::ProcessIdentity`]'s start
    /// key on this platform.
    pub(super) p_starttime: libc::timeval,
    pub(super) p_vmspace: *mut c_void,
    pub(super) p_sigacts: *mut c_void,
    /// `P_*` flags from `<sys/proc.h>`; [`P_TRACED`] is read from here.
    pub(super) p_flag: c_int,
    /// `S*` status: one of [`SIDL`] through [`SZOMB`].
    pub(super) p_stat: c_char,
    pub(super) p_pid: i32,
    pub(super) p_oppid: i32,
    pub(super) p_dupfd: c_int,
    pub(super) user_stack: *mut c_char,
    pub(super) exit_thread: *mut c_void,
    pub(super) p_debugger: c_int,
    /// `boolean_t`, i.e. a `c_int`.
    pub(super) sigwait: c_int,
    pub(super) p_estcpu: c_uint,
    pub(super) p_cpticks: c_int,
    /// `fixpt_t`.
    pub(super) p_pctcpu: u32,
    pub(super) p_wchan: *mut c_void,
    pub(super) p_wmesg: *mut c_char,
    pub(super) p_swtime: c_uint,
    pub(super) p_slptime: c_uint,
    pub(super) p_realtimer: Itimerval,
    pub(super) p_rtime: libc::timeval,
    pub(super) p_uticks: u64,
    pub(super) p_sticks: u64,
    pub(super) p_iticks: u64,
    pub(super) p_traceflag: c_int,
    pub(super) p_tracep: *mut c_void,
    pub(super) p_siglist: c_int,
    pub(super) p_textvp: *mut c_void,
    pub(super) p_holdcnt: c_int,
    /// `sigset_t`, a `u32` on Apple targets.
    pub(super) p_sigmask: u32,
    pub(super) p_sigignore: u32,
    pub(super) p_sigcatch: u32,
    pub(super) p_priority: c_uchar,
    pub(super) p_usrpri: c_uchar,
    /// Scheduling niceness, `-20..=20`.
    pub(super) p_nice: c_char,
    /// `MAXCOMLEN + 1` bytes of NUL-padded short name.
    pub(super) p_comm: [c_char; MAXCOMLEN + 1],
    pub(super) p_pgrp: *mut c_void,
    pub(super) p_addr: *mut c_void,
    pub(super) p_xstat: u16,
    pub(super) p_acflag: u16,
    pub(super) p_ru: *mut c_void,
}

/// `struct itimerval` from `<sys/time.h>`.
#[allow(dead_code, reason = "layout only")]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(super) struct Itimerval {
    pub(super) it_interval: libc::timeval,
    pub(super) it_value: libc::timeval,
}

/// `struct _pcred` from `<sys/sysctl.h>`.
#[allow(dead_code, reason = "layout only")]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(super) struct Pcred {
    pub(super) pc_lock: [c_char; 72],
    pub(super) pc_ucred: *mut c_void,
    pub(super) p_ruid: libc::uid_t,
    pub(super) p_svuid: libc::uid_t,
    pub(super) p_rgid: libc::gid_t,
    pub(super) p_svgid: libc::gid_t,
    pub(super) p_refcnt: c_int,
}

/// `struct _ucred` from `<sys/sysctl.h>`.
#[allow(
    dead_code,
    reason = "the unread fields fix the offset of the fields after e_ucred"
)]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(super) struct Ucred {
    pub(super) cr_ref: i32,
    /// Effective user id: the owning user of the process.
    pub(super) cr_uid: libc::uid_t,
    pub(super) cr_ngroups: c_short,
    pub(super) cr_groups: [libc::gid_t; NGROUPS],
}

/// `struct vmspace` from `<sys/vm.h>`, which exists only to keep `kinfo_proc`
/// the size the kernel expects.
#[allow(dead_code, reason = "layout only")]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(super) struct Vmspace {
    pub(super) dummy: i32,
    pub(super) dummy2: *mut c_char,
    pub(super) dummy3: [i32; 5],
    pub(super) dummy4: [*mut c_char; 3],
}

/// `struct eproc` from `<sys/sysctl.h>`.
#[allow(
    dead_code,
    reason = "the unread fields fix the offsets of e_ucred and e_ppid"
)]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(super) struct Eproc {
    pub(super) e_paddr: *mut c_void,
    pub(super) e_sess: *mut c_void,
    pub(super) e_pcred: Pcred,
    /// Current credentials; `cr_uid` is the process owner.
    pub(super) e_ucred: Ucred,
    pub(super) e_vm: Vmspace,
    /// Parent process id.
    pub(super) e_ppid: i32,
    pub(super) e_pgid: i32,
    pub(super) e_jobc: c_short,
    /// `dev_t`.
    pub(super) e_tdev: i32,
    pub(super) e_tpgid: i32,
    pub(super) e_tsess: *mut c_void,
    pub(super) e_wmesg: [c_char; WMESGLEN + 1],
    /// `segsz_t`.
    pub(super) e_xsize: i32,
    pub(super) e_xrssize: c_short,
    pub(super) e_xccount: c_short,
    pub(super) e_xswrss: c_short,
    pub(super) e_flag: i32,
    pub(super) e_login: [c_char; COMPAT_MAXLOGNAME],
    pub(super) e_spare: [i32; 4],
}

/// `struct proc_fileinfo` from `<sys/proc_info.h>`.
///
/// The first half of [`VnodeFdinfowithpath`]. None of it is read — the descriptor
/// walk wants the path that follows it — but the kernel writes the whole structure
/// and `pvip`'s offset depends on this one's size.
#[allow(dead_code, reason = "layout only; it fixes the offset of pvip")]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(super) struct ProcFileinfo {
    pub(super) fi_openflags: u32,
    pub(super) fi_status: u32,
    /// `off_t`.
    pub(super) fi_offset: i64,
    pub(super) fi_type: i32,
    pub(super) fi_guardflags: u32,
}

/// `struct vnode_fdinfowithpath` from `<sys/proc_info.h>`: what
/// [`PROC_PIDFDVNODEPATHINFO`] fills for one descriptor.
///
/// `libc` declares `vnode_info_path` and `proc_vnodepathinfo` but not this
/// combination, so it is transcribed here and the `vnode_info_path` half is
/// borrowed from `libc` rather than re-transcribed — which is also why
/// [`tests::the_transcribed_layouts_have_the_sizes_the_headers_imply`] asserts the
/// total: 1200 bytes, verified against the macOS 26 SDK headers.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(super) struct VnodeFdinfowithpath {
    /// The descriptor's own flags and offset.
    pub(super) pfi: ProcFileinfo,
    /// The vnode it refers to, including the tail end of its path.
    pub(super) pvip: libc::vnode_info_path,
}

/// `struct kinfo_proc` from `<sys/sysctl.h>`: the payload of every `kern.proc.*`
/// node.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(super) struct KinfoProc {
    /// The BSD process record.
    pub(super) kp_proc: ExternProc,
    /// The augmented record: credentials, parent, session.
    pub(super) kp_eproc: Eproc,
}

/// `MAXCOMLEN` from `<sys/param.h>`.
pub(super) const MAXCOMLEN: usize = 16;
/// `WMESGLEN` from `<sys/sysctl.h>`.
pub(super) const WMESGLEN: usize = 7;
/// `COMAPT_MAXLOGNAME` from `<sys/sysctl.h>`, spelling and all.
pub(super) const COMPAT_MAXLOGNAME: usize = 12;
/// `NGROUPS` from `<sys/param.h>`, which is `NGROUPS_MAX`.
pub(super) const NGROUPS: usize = 16;

/// `KERN_CLOCKRATE` from `<sys/sysctl.h>`.
pub(super) const KERN_CLOCKRATE: c_int = 12;
/// `CTL_VM` from `<sys/sysctl.h>`.
pub(super) const CTL_VM: c_int = 2;
/// `VM_SWAPUSAGE` from `<sys/vm.h>`.
pub(super) const VM_SWAPUSAGE: c_int = 5;

/// `PROC_PIDFDVNODEPATHINFO` from `<sys/proc_info.h>`.
///
/// The one `proc_pidfdinfo` flavour this collector uses. `libc` exposes the
/// `PROC_PIDLISTFDS` and `PROX_FDTYPE_*` constants but not the per-descriptor
/// flavours, so this one is transcribed; its payload is [`VnodeFdinfowithpath`].
pub(super) const PROC_PIDFDVNODEPATHINFO: c_int = 2;

/// `P_TRACED` from `<sys/proc.h>`: the process is being debugged.
pub(super) const P_TRACED: c_int = 0x0000_0800;

/// `SIDL` from `<sys/proc.h>`: being created by `fork`.
pub(super) const SIDL: c_char = 1;
/// `SRUN`: runnable.
pub(super) const SRUN: c_char = 2;
/// `SSLEEP`: sleeping on an address.
pub(super) const SSLEEP: c_char = 3;
/// `SSTOP`: stopped by job control or a debugger.
pub(super) const SSTOP: c_char = 4;
/// `SZOMB`: exited, awaiting reap.
pub(super) const SZOMB: c_char = 5;

/// `kIOPSTimeRemainingUnknown` from `<IOKit/ps/IOPowerSources.h>`.
pub(super) const IOPS_TIME_REMAINING_UNKNOWN: f64 = -1.0;
/// `kIOPSTimeRemainingUnlimited` from `<IOKit/ps/IOPowerSources.h>`.
pub(super) const IOPS_TIME_REMAINING_UNLIMITED: f64 = -2.0;

/// `kCFStringEncodingUTF8` from `<CoreFoundation/CFString.h>`.
pub(super) const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// `kCFNumberIntType` from `<CoreFoundation/CFNumber.h>`.
pub(super) const CF_NUMBER_INT_TYPE: CFIndex = 9;

/// `CFTypeRef`. Opaque: never dereferenced, only handed back to CoreFoundation.
pub(super) type CFTypeRef = *const c_void;
/// `CFIndex`.
pub(super) type CFIndex = isize;
/// `CFTypeID`.
pub(super) type CFTypeID = usize;

// SAFETY (whole block): every signature below is transcribed from a public SDK
// header — `<mach/mach_host.h>`, `<mach/mach_init.h>`, `<mach/mach_time.h>` and
// `<mach/vm_map.h>`. These live in `libSystem`, which `std` already links, so no
// `link` attribute is needed. `libc` declares several of them too, but its Apple
// mach bindings are deprecated in favour of a crate this workspace deliberately
// does not depend on (§13).
unsafe extern "C" {
    /// `host_page_size` from `<mach/mach_host.h>`.
    ///
    /// Preferred over `sysconf(_SC_PAGESIZE)` because it comes from the same host
    /// port that produces `host_statistics64`, so the page counts and the page
    /// size cannot disagree — which they can for a translated process, whose
    /// user-space `vm_page_size` differs from the kernel's.
    pub(super) fn host_page_size(
        host: libc::host_t,
        out_page_size: *mut libc::vm_size_t,
    ) -> libc::kern_return_t;

    /// `mach_task_self` from `<mach/mach_init.h>`.
    pub(super) fn mach_task_self() -> libc::mach_port_t;

    /// `mach_host_self` from `<mach/mach_init.h>`.
    ///
    /// Returns a *name* for the host port, which needs no explicit release.
    pub(super) fn mach_host_self() -> libc::host_t;

    /// `mach_timebase_info` from `<mach/mach_time.h>`.
    pub(super) fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::kern_return_t;

    /// `vm_deallocate` from `<mach/vm_map.h>`.
    ///
    /// The counterpart of the allocation `host_processor_info` performs on our
    /// behalf: the array it returns is owned by the caller and leaks without this.
    pub(super) fn vm_deallocate(
        target_task: libc::mach_port_t,
        address: libc::vm_address_t,
        size: libc::vm_size_t,
    ) -> libc::kern_return_t;

}

// SAFETY (whole block): transcribed from `<IOKit/ps/IOPowerSources.h>`, whose
// symbols live in the public IOKit framework. Each entry records whether it
// follows CoreFoundation's Create/Copy rule (the caller owns the result) or its
// Get rule (the result is borrowed and must not be released); `power::Owned`
// discharges the former.
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    /// `IOPSCopyPowerSourcesInfo`: **Copy** rule, the result is owned.
    pub(super) fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;

    /// `IOPSCopyPowerSourcesList`: **Copy** rule, the result is owned.
    pub(super) fn IOPSCopyPowerSourcesList(blob: CFTypeRef) -> CFTypeRef;

    /// `IOPSGetPowerSourceDescription`: **Get** rule, the result is borrowed from
    /// `blob` and must *not* be released.
    pub(super) fn IOPSGetPowerSourceDescription(blob: CFTypeRef, ps: CFTypeRef) -> CFTypeRef;

    /// `IOPSGetTimeRemainingEstimate`: seconds, or one of the two documented
    /// sentinels [`IOPS_TIME_REMAINING_UNKNOWN`] and
    /// [`IOPS_TIME_REMAINING_UNLIMITED`].
    pub(super) fn IOPSGetTimeRemainingEstimate() -> f64;

}

// SAFETY (whole block): transcribed from `<CoreFoundation/CFBase.h>`,
// `CFArray.h`, `CFDictionary.h`, `CFString.h`, and `CFNumber.h`. Every parameter
// typed `CFTypeRef` is an opaque pointer that is only ever handed back to
// CoreFoundation, never dereferenced here.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    /// `CFRelease`. Balances every **Copy**- or **Create**-rule result.
    pub(super) fn CFRelease(cf: CFTypeRef);

    /// `CFArrayGetCount`.
    pub(super) fn CFArrayGetCount(array: CFTypeRef) -> CFIndex;

    /// `CFArrayGetValueAtIndex`: **Get** rule, borrowed.
    pub(super) fn CFArrayGetValueAtIndex(array: CFTypeRef, index: CFIndex) -> CFTypeRef;

    /// `CFDictionaryGetValue`: **Get** rule, borrowed.
    pub(super) fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;

    /// `CFStringCreateWithCString`: **Create** rule, the result is owned.
    pub(super) fn CFStringCreateWithCString(
        alloc: CFTypeRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFTypeRef;

    /// `CFStringGetCString`. Writes at most `buffer_size` bytes including the NUL.
    pub(super) fn CFStringGetCString(
        the_string: CFTypeRef,
        buffer: *mut c_char,
        buffer_size: CFIndex,
        encoding: u32,
    ) -> bool;

    /// `CFNumberGetValue`.
    pub(super) fn CFNumberGetValue(
        number: CFTypeRef,
        the_type: CFIndex,
        value_ptr: *mut c_void,
    ) -> bool;

    /// `CFBooleanGetValue`.
    pub(super) fn CFBooleanGetValue(boolean: CFTypeRef) -> bool;

    /// `CFGetTypeID`.
    pub(super) fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;

    /// `CFNumberGetTypeID`.
    pub(super) fn CFNumberGetTypeID() -> CFTypeID;

    /// `CFStringGetTypeID`.
    pub(super) fn CFStringGetTypeID() -> CFTypeID;

    /// `CFBooleanGetTypeID`.
    pub(super) fn CFBooleanGetTypeID() -> CFTypeID;
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn the_transcribed_layouts_have_the_sizes_the_headers_imply() {
        // These are the numbers the C compiler produces for the same
        // declarations on every 64-bit Apple target. A change here means a
        // transcription error, and every offset below depends on it.
        assert_eq!(size_of::<ExternProc>(), 296);
        assert_eq!(size_of::<Eproc>(), 352);
        assert_eq!(size_of::<KinfoProc>(), 648);
        assert_eq!(align_of::<KinfoProc>(), 8);
        assert_eq!(size_of::<IfData>(), 96);
        assert_eq!(size_of::<Clockinfo>(), 20);
        assert_eq!(size_of::<ProcFileinfo>(), 24);
        assert_eq!(size_of::<VnodeFdinfowithpath>(), 1200);
        // The kernel reports success by returning exactly this many bytes, so a
        // transcription error here would look like every descriptor being refused.
        assert_eq!(offset_of!(VnodeFdinfowithpath, pvip), 24);
    }

    #[test]
    fn the_fields_the_collector_reads_sit_where_the_headers_put_them() {
        // The start time is the head of the `p_un` union, which is what lets us
        // declare the `timeval` arm rather than the pointer pair.
        assert_eq!(offset_of!(ExternProc, p_starttime), 0);
        assert_eq!(offset_of!(ExternProc, p_flag), 32);
        assert_eq!(offset_of!(ExternProc, p_stat), 36);
        assert_eq!(offset_of!(ExternProc, p_pid), 40);
        assert_eq!(offset_of!(ExternProc, p_nice), 242);
        assert_eq!(offset_of!(ExternProc, p_comm), 243);
        assert_eq!(offset_of!(KinfoProc, kp_eproc), 296);
        assert_eq!(offset_of!(IfData, ifi_baudrate), 16);
    }

    #[test]
    fn the_status_codes_are_distinct_so_a_state_mapping_cannot_collide() {
        let codes = [SIDL, SRUN, SSLEEP, SSTOP, SZOMB];
        let mut sorted: Vec<c_char> = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len(), "two statuses share a value");
    }

    #[test]
    fn the_time_remaining_sentinels_are_negative_and_distinguishable() {
        // A caller that forgets to check them would otherwise turn "unknown" into a
        // negative duration, which is precisely the fabrication §4 forbids. The
        // values are compared through locals so this reads as a property of the
        // constants rather than as arithmetic the compiler can fold away.
        let unknown = IOPS_TIME_REMAINING_UNKNOWN;
        let unlimited = IOPS_TIME_REMAINING_UNLIMITED;
        assert!(unknown < 0.0);
        assert!(unlimited < 0.0);
        assert!(unknown > unlimited);
    }

    #[test]
    #[ignore = "platform smoke test: asks the live kernel for a kinfo_proc"]
    fn the_kinfo_proc_layout_matches_the_running_kernel() {
        // The one assertion that can catch a transcription error the compiler
        // cannot: the kernel reports how many bytes it wrote, and for a
        // `kern.proc.pid` query on a live process that is exactly one
        // `kinfo_proc`.
        let pid = i32::try_from(std::process::id()).expect("our own pid fits an i32");
        let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
        let mut buffer = [0u8; size_of::<KinfoProc>()];
        let written = super::super::sysctl::into_buffer(&mut mib, &mut buffer)
            .expect("our own process must be readable");
        assert_eq!(
            written,
            size_of::<KinfoProc>(),
            "the kernel's kinfo_proc is {written} bytes, ours is {}",
            size_of::<KinfoProc>()
        );
    }
}
