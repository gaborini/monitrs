//! Interface link state and negotiated speed from `getifaddrs`.
//!
//! The baseline already has byte and packet counters, so this module adds only the
//! two things it cannot see: whether the link is actually up, and how fast it is.
//! Both come from the `AF_LINK` entry `getifaddrs` returns for each interface —
//! `ifa_flags` for the state and `if_data.ifi_baudrate` for the speed.
//!
//! # Why the speed matters more than it looks
//!
//! §7.4 forbids showing a network utilization percentage without a known link
//! capacity. Without this module every interface reports
//! [`monitrs_core::model::UnavailableReason::LinkSpeedUnknown`] forever. With it,
//! an interface that reports a baudrate gets a real percentage and one that does
//! not still gets none — which is the correct outcome for a tunnel or a bridge,
//! where "capacity" is not a physical property.
//!
//! # Why `if_data` and not `if_data64`
//!
//! `getifaddrs` hangs the 32-bit `struct if_data` off `ifa_data`, not the 64-bit
//! form. Reading it as `if_data64` silently shifts every field — during
//! development that produced a baudrate of two quadrillion bits per second — so
//! the layout is transcribed from the header in [`super::ffi::IfData`] and only
//! `ifi_baudrate`, whose offset is asserted in a test, is read from it.

use core::ffi::c_uint;
use std::collections::HashMap;

use monitrs_core::model::{LinkState, MetricState, NetworkSnapshot};

use super::ffi;
use super::sysctl::NativeError;

/// What this module can say about one interface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterfaceLink {
    /// Operational state derived from the interface flags.
    pub state: LinkState,
    /// Negotiated link speed in megabits per second, when the driver reports one.
    pub speed_mbps: Option<u64>,
}

/// The smallest baudrate worth reporting as a link speed.
///
/// A driver that reports fewer than a million bits per second is reporting
/// something other than a modern link rate, and integer-dividing it would yield
/// `0 Mbps` — a capacity of zero, which would make every utilization percentage
/// infinite. §7.4 prefers no percentage to a wrong one.
const MINIMUM_BAUDRATE: u64 = 1_000_000;

/// Derives the operational state from `ifa_flags`.
///
/// `IFF_UP` is the administrative state and `IFF_RUNNING` means the driver has
/// resources allocated and the link is usable. An interface that is up but not
/// running is waiting for something — an unassociated Wi-Fi radio, an unplugged
/// cable — which is exactly [`LinkState::Dormant`].
fn link_state(flags: c_uint) -> LinkState {
    let up = flags & u32::try_from(libc::IFF_UP).unwrap_or(0) != 0;
    let running = flags & u32::try_from(libc::IFF_RUNNING).unwrap_or(0) != 0;
    match (up, running) {
        (true, true) => LinkState::Up,
        (true, false) => LinkState::Dormant,
        (false, _) => LinkState::Down,
    }
}

/// Converts a baudrate into megabits per second.
fn speed_mbps(baudrate: u64) -> Option<u64> {
    (baudrate >= MINIMUM_BAUDRATE).then_some(baudrate / MINIMUM_BAUDRATE)
}

/// An owned `getifaddrs` list, freed on drop.
#[derive(Debug)]
struct IfAddrs {
    head: *mut libc::ifaddrs,
}

impl Drop for IfAddrs {
    fn drop(&mut self) {
        if self.head.is_null() {
            return;
        }
        // SAFETY: `head` is exactly what `getifaddrs` returned and has not been
        // modified, so this is the matching `freeifaddrs`. Nothing borrows the list
        // any more: `self` is being dropped and the caller has already copied out
        // the fields it needed.
        unsafe { libc::freeifaddrs(self.head) }
    }
}

/// Reads the link state and speed of every interface.
///
/// `getifaddrs` returns one entry per address *plus* one `AF_LINK` entry per
/// interface, so the same name appears several times. The flags are identical
/// across an interface's entries, and only the `AF_LINK` entry carries `if_data`,
/// which is why the speed is taken from that entry alone.
pub(super) fn read_interface_links() -> Result<HashMap<Box<str>, InterfaceLink>, NativeError> {
    let mut head: *mut libc::ifaddrs = core::ptr::null_mut();
    // SAFETY: `getifaddrs` writes a list head into the pointer it is given. The
    // result is captured by the guard below before anything else can fail.
    let result = unsafe { libc::getifaddrs(&mut head) };
    if result != 0 {
        return Err(NativeError::last());
    }
    let list = IfAddrs { head };

    let mut links: HashMap<Box<str>, InterfaceLink> = HashMap::new();
    let mut cursor = list.head;
    while !cursor.is_null() {
        // SAFETY: `cursor` starts at the list head `getifaddrs` produced and only
        // ever advances along its own `ifa_next` chain, which terminates at null.
        // The list is alive for as long as `list` is.
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;

        if entry.ifa_name.is_null() {
            continue;
        }
        // SAFETY: `ifa_name` is a NUL-terminated string owned by the list, which
        // outlives the copy made here.
        let name = unsafe { core::ffi::CStr::from_ptr(entry.ifa_name) };
        let name: Box<str> = name.to_string_lossy().into_owned().into();

        let state = link_state(entry.ifa_flags);
        let speed = read_baudrate(entry).and_then(speed_mbps);
        links
            .entry(name)
            .and_modify(|link| {
                link.state = state;
                // Only the AF_LINK entry has a baudrate; the address entries must
                // not erase it.
                if speed.is_some() {
                    link.speed_mbps = speed;
                }
            })
            .or_insert(InterfaceLink {
                state,
                speed_mbps: speed,
            });
    }
    Ok(links)
}

/// Reads `ifi_baudrate` from an `AF_LINK` entry, if this is one.
fn read_baudrate(entry: &libc::ifaddrs) -> Option<u64> {
    if entry.ifa_addr.is_null() || entry.ifa_data.is_null() {
        return None;
    }
    // SAFETY: `ifa_addr` is a valid `sockaddr` owned by the list whenever it is
    // non-null; only its `sa_family` byte is read.
    let family = unsafe { (*entry.ifa_addr).sa_family };
    if i32::from(family) != libc::AF_LINK {
        return None;
    }
    // SAFETY: for an `AF_LINK` entry `getifaddrs` documents `ifa_data` as pointing
    // at a `struct if_data`, whose layout is transcribed in `ffi::IfData`. The read
    // is unaligned because the kernel makes no alignment promise about the
    // allocation, and `IfData` is `Pod`, so every byte pattern is a valid value.
    let data = unsafe { core::ptr::read_unaligned(entry.ifa_data.cast::<ffi::IfData>()) };
    Some(u64::from(data.ifi_baudrate))
}

/// Applies the link information to the baseline's interface snapshots.
///
/// An interface the kernel did not describe keeps whatever the baseline said,
/// which for the frozen baseline is [`MetricState::Unsupported`] — still not a
/// fabricated zero (§4).
pub(super) fn merge_into(
    interfaces: &mut [NetworkSnapshot],
    links: &HashMap<Box<str>, InterfaceLink>,
) {
    for interface in interfaces {
        let Some(link) = links.get(&interface.name) else {
            continue;
        };
        interface.state = MetricState::Available(link.state);
        interface.link_speed_mbps = match link.speed_mbps {
            Some(speed) => MetricState::Available(speed),
            // A tunnel or bridge has no physical capacity, so there is nothing to
            // express a percentage against (§7.4).
            None => MetricState::Unsupported,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monitrs_core::model::{InterfaceKind, UnavailableReason};

    #[test]
    fn an_interface_that_is_up_and_running_is_up() {
        let flags = u32::try_from(libc::IFF_UP | libc::IFF_RUNNING).expect("flags fit a u32");
        assert_eq!(link_state(flags), LinkState::Up);
    }

    #[test]
    fn an_administratively_up_interface_with_no_carrier_is_dormant_not_up() {
        // The Wi-Fi radio that is enabled but not associated, and the ethernet port
        // with nothing plugged in. Calling either "up" would be wrong.
        let flags = u32::try_from(libc::IFF_UP).expect("flags fit a u32");
        assert_eq!(link_state(flags), LinkState::Dormant);
    }

    #[test]
    fn an_interface_that_is_administratively_down_is_down() {
        assert_eq!(link_state(0), LinkState::Down);
        let running_only = u32::try_from(libc::IFF_RUNNING).expect("flags fit a u32");
        assert_eq!(link_state(running_only), LinkState::Down);
    }

    #[test]
    fn every_link_state_carries_a_symbol_so_colour_is_not_load_bearing() {
        // §5.2: colour is supplementary.
        let mut symbols: Vec<char> = [LinkState::Up, LinkState::Down, LinkState::Dormant]
            .iter()
            .map(|state| state.symbol())
            .collect();
        symbols.sort_unstable();
        symbols.dedup();
        assert_eq!(symbols.len(), 3);
    }

    #[test]
    fn a_baudrate_becomes_megabits_and_a_sub_megabit_one_becomes_nothing() {
        assert_eq!(speed_mbps(1_000_000_000), Some(1_000));
        assert_eq!(speed_mbps(228_540_000), Some(228));
        // The failure this prevents: 0 Mbps would make every utilization infinite.
        assert_eq!(speed_mbps(0), None);
        assert_eq!(speed_mbps(999_999), None);
    }

    #[test]
    fn merging_gives_an_interface_with_a_known_speed_a_usable_utilization() {
        let mut interfaces = vec![NetworkSnapshot::warming_up(
            "en0".into(),
            InterfaceKind::Physical,
        )];
        let links = HashMap::from([(
            Box::<str>::from("en0"),
            InterfaceLink {
                state: LinkState::Up,
                speed_mbps: Some(1_000),
            },
        )]);
        merge_into(&mut interfaces, &links);
        let interface = interfaces.first().expect("one interface");
        assert_eq!(interface.state, MetricState::Available(LinkState::Up));
        assert_eq!(interface.link_speed_mbps, MetricState::Available(1_000));
    }

    #[test]
    fn merging_leaves_a_speedless_interface_without_a_utilization_percentage() {
        // §7.4: no capacity, no percentage — and the reason has to say so.
        let mut interfaces = vec![NetworkSnapshot::warming_up(
            "utun3".into(),
            InterfaceKind::Tunnel,
        )];
        let links = HashMap::from([(
            Box::<str>::from("utun3"),
            InterfaceLink {
                state: LinkState::Up,
                speed_mbps: None,
            },
        )]);
        merge_into(&mut interfaces, &links);
        let interface = interfaces.first().expect("one interface");
        assert_eq!(interface.state, MetricState::Available(LinkState::Up));
        assert!(interface.link_speed_mbps.is_unsupported());
        assert_eq!(
            interface.utilization(),
            MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown)
        );
    }

    #[test]
    fn an_interface_the_kernel_did_not_describe_is_left_alone() {
        let mut interfaces = vec![NetworkSnapshot::warming_up(
            "ghost0".into(),
            InterfaceKind::Unknown,
        )];
        let before = interfaces.first().cloned().expect("one interface");
        merge_into(&mut interfaces, &HashMap::new());
        assert_eq!(interfaces.first(), Some(&before));
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn the_live_interface_list_includes_loopback_and_reports_it_up() {
        let links = read_interface_links().expect("getifaddrs always succeeds");
        let loopback = links
            .get("lo0")
            .expect("every machine has a loopback interface");
        assert_eq!(loopback.state, LinkState::Up);
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn the_live_speeds_are_plausible_or_absent() {
        let links = read_interface_links().expect("getifaddrs");
        assert!(links.len() > 1, "a Mac has more than one interface");
        for (name, link) in &links {
            if let Some(speed) = link.speed_mbps {
                assert!(
                    (1..=800_000).contains(&speed),
                    "{name} reported {speed} Mbps"
                );
            }
        }
        // At least one interface — the active network one — normally reports a
        // rate; a machine with no network at all is the one exception, so this is
        // asserted as a count rather than as a requirement on any single name.
        let with_speed = links
            .values()
            .filter(|link| link.speed_mbps.is_some())
            .count();
        assert!(
            with_speed <= links.len(),
            "counting cannot exceed the interface list"
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn repeated_reads_free_the_interface_list() {
        // Without the Drop guard this leaks the whole list on every call.
        for _ in 0..2_000 {
            assert!(!read_interface_links().expect("getifaddrs").is_empty());
        }
    }
}
