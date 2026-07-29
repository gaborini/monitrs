//! `/proc/net/dev` and the `/sys/class/net` attributes that go with it.
//!
//! The counters here are what the baseline cannot give us: **drop** counters
//! (`sysinfo` exposes errors but not drops) and the raw per-interface totals. The
//! two `/sys` attributes matter for a different reason — §7.4 forbids rendering a
//! utilisation percentage without a known link speed, so
//! [`parse_link_speed_mbps`] returning `None` is what *suppresses* a percentage
//! rather than something the UI has to remember to check.

use monitrs_core::model::{InterfaceErrors, LinkState, TrafficTotals};

use crate::linux::parse::{
    ParseFailure, ParseResult, fields, lines, parse_i64, parse_u64, to_text, trim_ascii,
};

/// One interface's counters from `/proc/net/dev`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NetDevEntry {
    /// Interface name as the kernel spells it, e.g. `eth0` or `enp0s31f6`.
    pub name: Box<str>,
    /// Byte and packet totals.
    pub totals: TrafficTotals,
    /// Error and drop counters, including the drops the baseline cannot see.
    pub errors: InterfaceErrors,
    /// Receive FIFO overruns: the counter that rises when the NIC outran the
    /// kernel's ability to drain its ring.
    pub rx_fifo: u64,
    /// Transmit collisions.
    pub collisions: u64,
    /// Transmit carrier losses.
    pub tx_carrier: u64,
    /// Multicast packets received.
    pub multicast: u64,
}

/// Parses `/proc/net/dev`.
///
/// The file starts with two header lines and then one line per interface, in the
/// form `name: 16 counters`. The name may abut its colon (`docker0:`) or be padded
/// away from it (`  eth0: `), so the split is on the colon and not on whitespace —
/// a whitespace split silently merges the name and the first counter for any
/// interface whose name is long enough to reach the column. The kernel forbids a
/// colon inside a network device name, which is what makes the first colon the
/// right one.
///
/// A header-only file yields an empty list: an interface list can legitimately be
/// empty inside a network-namespaced container that has not been wired up yet, and
/// that is not a parse failure.
pub fn parse_net_dev(bytes: &[u8]) -> ParseResult<Vec<NetDevEntry>> {
    let mut interfaces = Vec::new();
    for line in lines(bytes) {
        // Header lines are the two that contain `|`; every counter line has a
        // colon and no pipe.
        if line.contains(&b'|') {
            continue;
        }
        if let Ok(entry) = parse_line(line) {
            interfaces.push(entry);
        }
    }
    Ok(interfaces)
}

/// Parses one interface line.
fn parse_line(line: &[u8]) -> ParseResult<NetDevEntry> {
    let colon = line
        .iter()
        .position(|byte| *byte == b':')
        .ok_or(ParseFailure::Malformed("net/dev.name"))?;
    let name = trim_ascii(line.get(..colon).unwrap_or_default());
    if name.is_empty() {
        return Err(ParseFailure::Malformed("net/dev.name"));
    }
    let tail = line.get(colon + 1..).unwrap_or_default();

    let mut counters: [u64; 16] = [0; 16];
    let mut count = 0usize;
    for field in fields(tail) {
        let value = parse_u64(field, "net/dev.counter")?;
        if let Some(slot) = counters.get_mut(count) {
            *slot = value;
        }
        count += 1;
    }
    // The kernel writes exactly sixteen counters. Accepting fewer would mean
    // reading a transmit counter out of a receive column.
    if count < 16 {
        return Err(ParseFailure::Truncated("net/dev.counters"));
    }
    let at = |index: usize| counters.get(index).copied().unwrap_or(0);

    Ok(NetDevEntry {
        name: to_text(name),
        totals: TrafficTotals {
            rx_bytes: at(0),
            rx_packets: at(1),
            tx_bytes: at(8),
            tx_packets: at(9),
        },
        errors: InterfaceErrors {
            rx_errors: at(2),
            rx_dropped: at(3),
            tx_errors: at(10),
            tx_dropped: at(11),
        },
        rx_fifo: at(4),
        multicast: at(7),
        collisions: at(13),
        tx_carrier: at(14),
    })
}

/// Parses `/sys/class/net/<iface>/operstate`.
///
/// `unknown` is reported as [`LinkState::Unknown`] rather than as down: the
/// loopback interface and many virtual interfaces never report a carrier state at
/// all, and calling those "down" would put a red cue on a working interface (§5.2).
pub fn parse_operstate(bytes: &[u8]) -> ParseResult<LinkState> {
    let trimmed = trim_ascii(bytes);
    if trimmed.is_empty() {
        return Err(ParseFailure::Empty);
    }
    Ok(match trimmed {
        b"up" => LinkState::Up,
        // `lowerlayerdown` means a stacked interface whose parent is down, and
        // `notpresent` means the driver is loaded but the hardware is absent. Both
        // are operationally down.
        b"down" | b"lowerlayerdown" | b"notpresent" => LinkState::Down,
        b"dormant" => LinkState::Dormant,
        // `testing` and anything a future kernel adds: unknown is honest.
        _ => LinkState::Unknown,
    })
}

/// Parses `/sys/class/net/<iface>/speed` into megabits per second.
///
/// Returns `Ok(None)` for the `-1` the kernel writes when the link speed is not
/// negotiated or not applicable, and for a zero, which some drivers write instead.
/// §7.4: without a known speed there is no utilisation percentage, only throughput,
/// so `None` here is the mechanism that enforces the rule rather than a value the
/// caller has to interpret.
pub fn parse_link_speed_mbps(bytes: &[u8]) -> ParseResult<Option<u64>> {
    let trimmed = trim_ascii(bytes);
    if trimmed.is_empty() {
        // A wireless or virtual interface's `speed` file exists but reads empty on
        // some drivers.
        return Ok(None);
    }
    let speed = parse_i64(trimmed, "speed")?;
    Ok(if speed > 0 {
        u64::try_from(speed).ok()
    } else {
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fixtures;

    fn find<'a>(interfaces: &'a [NetDevEntry], name: &str) -> &'a NetDevEntry {
        interfaces
            .iter()
            .find(|entry| &*entry.name == name)
            .unwrap_or_else(|| panic!("{name} is missing"))
    }

    #[test]
    fn a_typical_file_yields_every_interface_with_its_drop_counters() {
        let interfaces = parse_net_dev(fixtures::NET_DEV_TYPICAL).expect("valid");
        assert_eq!(interfaces.len(), 4);
        let eth0 = find(&interfaces, "eth0");
        assert_eq!(eth0.totals.rx_bytes, 8_123_456_789);
        assert_eq!(eth0.totals.rx_packets, 9_123_456);
        assert_eq!(eth0.totals.tx_bytes, 1_234_567_890);
        assert_eq!(eth0.totals.tx_packets, 4_123_456);
        assert_eq!(eth0.errors.rx_errors, 12);
        assert_eq!(
            eth0.errors.rx_dropped, 7,
            "drop counters are the reason this file is read at all"
        );
        assert_eq!(eth0.errors.tx_errors, 3);
        assert_eq!(eth0.errors.tx_dropped, 2);
        assert_eq!(eth0.multicast, 40_213);
        assert!(eth0.errors.any());
    }

    #[test]
    fn a_name_abutting_its_colon_parses_as_well_as_a_padded_one() {
        let interfaces = parse_net_dev(fixtures::NET_DEV_TYPICAL).expect("valid");
        assert_eq!(&*find(&interfaces, "docker0").name, "docker0");
        assert_eq!(&*find(&interfaces, "lo").name, "lo");
    }

    #[test]
    fn an_idle_interface_reports_real_zeroes_rather_than_being_omitted() {
        let interfaces = parse_net_dev(fixtures::NET_DEV_TYPICAL).expect("valid");
        let wlan0 = find(&interfaces, "wlan0");
        assert_eq!(wlan0.totals.rx_bytes, 0);
        assert!(!wlan0.errors.any());
    }

    #[test]
    fn a_header_only_file_is_an_empty_list_not_a_failure() {
        assert!(
            parse_net_dev(fixtures::NET_DEV_HEADER_ONLY)
                .expect("valid")
                .is_empty()
        );
        assert!(
            parse_net_dev(fixtures::NET_DEV_EMPTY)
                .expect("valid")
                .is_empty()
        );
    }

    #[test]
    fn a_truncated_line_is_dropped_rather_than_read_out_of_the_wrong_column() {
        // Accepting five counters would report a receive FIFO count as tx_bytes.
        let interfaces = parse_net_dev(fixtures::NET_DEV_TRUNCATED).expect("valid");
        assert_eq!(interfaces.len(), 1);
        assert_eq!(&*interfaces[0].name, "lo");
    }

    #[test]
    fn a_near_u64_max_counter_parses_exactly() {
        let interfaces = parse_net_dev(fixtures::NET_DEV_HUGE).expect("valid");
        let eth0 = find(&interfaces, "eth0");
        assert_eq!(eth0.totals.rx_bytes, 18_446_744_073_709_551_610);
        assert_eq!(eth0.totals.rx_packets, u64::MAX);
    }

    #[test]
    fn operstate_maps_every_kernel_string_the_specification_cares_about() {
        assert_eq!(
            parse_operstate(fixtures::OPERSTATE_UP).expect("valid"),
            LinkState::Up
        );
        assert_eq!(
            parse_operstate(fixtures::OPERSTATE_DOWN).expect("valid"),
            LinkState::Down
        );
        assert_eq!(
            parse_operstate(fixtures::OPERSTATE_DORMANT).expect("valid"),
            LinkState::Dormant
        );
        assert_eq!(
            parse_operstate(fixtures::OPERSTATE_UNKNOWN).expect("valid"),
            LinkState::Unknown
        );
        assert_eq!(
            parse_operstate(b"lowerlayerdown").expect("valid"),
            LinkState::Down
        );
        assert_eq!(
            parse_operstate(b"testing").expect("valid"),
            LinkState::Unknown
        );
        assert_eq!(parse_operstate(b""), Err(ParseFailure::Empty));
    }

    #[test]
    fn an_unknown_operstate_is_not_reported_as_down() {
        // Loopback reads `unknown` on every Linux system; a red "down" cue on it
        // would be wrong (§5.2).
        assert_ne!(
            parse_operstate(fixtures::OPERSTATE_UNKNOWN).expect("valid"),
            LinkState::Down
        );
    }

    #[test]
    fn a_negative_or_empty_speed_means_unknown_so_no_utilization_is_rendered() {
        // §7.4: this `None` is what suppresses the percentage.
        assert_eq!(
            parse_link_speed_mbps(fixtures::SPEED_UNKNOWN_NEGATIVE).expect("valid"),
            None
        );
        assert_eq!(
            parse_link_speed_mbps(fixtures::SPEED_EMPTY).expect("valid"),
            None
        );
        assert_eq!(parse_link_speed_mbps(b"0\n").expect("valid"), None);
    }

    #[test]
    fn a_negotiated_speed_is_returned_in_megabits() {
        assert_eq!(
            parse_link_speed_mbps(fixtures::SPEED_1000).expect("valid"),
            Some(1_000)
        );
        assert_eq!(
            parse_link_speed_mbps(b"40000\n").expect("valid"),
            Some(40_000)
        );
    }

    #[test]
    fn a_malformed_speed_is_a_failure_rather_than_a_fabricated_capacity() {
        assert!(parse_link_speed_mbps(b"fast\n").is_err());
    }
}
