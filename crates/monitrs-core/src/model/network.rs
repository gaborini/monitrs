//! Network interface metrics.
//!
//! §7.4 and §26: *network percentage is meaningless without known link
//! capacity*. [`NetworkSnapshot::utilization`] is therefore
//! [`MetricState::TemporarilyUnavailable`] with
//! [`UnavailableReason::LinkSpeedUnknown`] rather than a fabricated number
//! whenever the link speed is absent, which is the common case on Wi-Fi and in
//! virtual machines.
//!
//! Per-process network attribution is out of scope for v1 (§3.2, §7.4), so no
//! type here carries a process identity.

use std::net::IpAddr;

use crate::model::{MetricState, UnavailableReason};
use crate::units::{Percent, Rate};

/// Operational state of a link.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum LinkState {
    /// Carrier present and the interface is administratively up.
    Up,
    /// Administratively down or no carrier.
    Down,
    /// Waiting for an external event, e.g. an unassociated Wi-Fi interface.
    Dormant,
    /// The platform did not report an operational state.
    #[default]
    Unknown,
}

impl LinkState {
    /// A redundant non-color cue (§5.2).
    #[must_use]
    pub const fn symbol(self) -> char {
        match self {
            Self::Up => '+',
            Self::Down => '-',
            Self::Dormant => '.',
            Self::Unknown => '?',
        }
    }

    /// Lower-case label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Dormant => "dormant",
            Self::Unknown => "unknown",
        }
    }
}

/// What kind of interface this is.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum InterfaceKind {
    /// A hardware interface.
    Physical,
    /// The loopback interface.
    Loopback,
    /// A bridge, VLAN, bond, or container veth.
    Virtual,
    /// A VPN or other tunnel.
    Tunnel,
    /// Not classifiable from the interface name and flags alone.
    #[default]
    Unknown,
}

/// An address assigned to an interface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct InterfaceAddress {
    /// The address.
    pub ip: IpAddr,
    /// Prefix length, where reported.
    pub prefix_len: Option<u8>,
}

/// Error and drop counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct InterfaceErrors {
    /// Receive errors since boot.
    pub rx_errors: u64,
    /// Transmit errors since boot.
    pub tx_errors: u64,
    /// Receive drops since boot.
    pub rx_dropped: u64,
    /// Transmit drops since boot.
    pub tx_dropped: u64,
}

impl InterfaceErrors {
    /// Whether any counter is non-zero.
    #[must_use]
    pub const fn any(&self) -> bool {
        self.rx_errors > 0 || self.tx_errors > 0 || self.rx_dropped > 0 || self.tx_dropped > 0
    }
}

/// Cumulative byte and packet counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TrafficTotals {
    /// Bytes received.
    pub rx_bytes: u64,
    /// Bytes transmitted.
    pub tx_bytes: u64,
    /// Packets received.
    pub rx_packets: u64,
    /// Packets transmitted.
    pub tx_packets: u64,
}

/// State of one network interface.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct NetworkSnapshot {
    /// Interface name, e.g. `en0` or `eth0`.
    pub name: Box<str>,
    /// Classification.
    pub kind: InterfaceKind,
    /// Operational state.
    pub state: MetricState<LinkState>,
    /// Assigned addresses.
    pub addresses: Vec<InterfaceAddress>,
    /// Hardware address, where readable.
    pub mac: Option<Box<str>>,
    /// Receive throughput.
    pub rx: MetricState<Rate>,
    /// Transmit throughput.
    pub tx: MetricState<Rate>,
    /// Received packets per second.
    pub rx_packets: MetricState<Rate>,
    /// Transmitted packets per second.
    pub tx_packets: MetricState<Rate>,
    /// Error and drop counters.
    pub errors: MetricState<InterfaceErrors>,
    /// Negotiated link speed, where the platform reports it.
    pub link_speed_mbps: MetricState<u64>,
    /// Totals accumulated since monitrs launched.
    ///
    /// Distinct from `os_totals`: this one starts at zero and is always
    /// meaningful, whereas the OS counter may have wrapped or been reset (§7.4).
    pub since_launch: TrafficTotals,
    /// The OS's own counters, where exposed.
    pub os_totals: MetricState<TrafficTotals>,
}

impl NetworkSnapshot {
    /// Link utilization, or an explicit unavailability.
    ///
    /// Not a stored field: deriving it on demand makes it impossible to persist
    /// a utilization that was computed without a known link speed (§7.4). The
    /// higher of the two directions is used, since a duplex link saturates in
    /// whichever direction fills first.
    #[must_use]
    pub fn utilization(&self) -> MetricState<Percent> {
        let Some(&speed_mbps) = self.link_speed_mbps.fresh() else {
            return MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown);
        };
        if speed_mbps == 0 {
            return MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown);
        }
        let (Some(rx), Some(tx)) = (self.rx.fresh(), self.tx.fresh()) else {
            return MetricState::WarmingUp;
        };
        // Link speeds are quoted in megabits; throughput is measured in bytes.
        let capacity_bytes_per_second = speed_mbps as f64 * 1_000_000.0 / 8.0;
        let busiest = rx.per_second().max(tx.per_second());
        // Narrowing a percentage to f32 is intentional; `Percent::new` rejects
        // any value the narrowing could not represent.
        #[allow(clippy::cast_possible_truncation)]
        let percent = ((busiest / capacity_bytes_per_second) * 100.0) as f32;
        Percent::new(percent).map_or(
            MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
            MetricState::Available,
        )
    }

    /// An interface whose counters exist but whose rates need a second sample.
    #[must_use]
    pub fn warming_up(name: Box<str>, kind: InterfaceKind) -> Self {
        Self {
            name,
            kind,
            state: MetricState::WarmingUp,
            addresses: Vec::new(),
            mac: None,
            rx: MetricState::WarmingUp,
            tx: MetricState::WarmingUp,
            rx_packets: MetricState::WarmingUp,
            tx_packets: MetricState::WarmingUp,
            errors: MetricState::WarmingUp,
            link_speed_mbps: MetricState::WarmingUp,
            since_launch: TrafficTotals::default(),
            os_totals: MetricState::WarmingUp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interface() -> NetworkSnapshot {
        NetworkSnapshot::warming_up("en0".into(), InterfaceKind::Physical)
    }

    #[test]
    fn utilization_is_unavailable_without_a_known_link_speed() {
        let mut nic = interface();
        nic.rx = MetricState::Available(Rate::new(18_200_000.0).expect("valid"));
        nic.tx = MetricState::Available(Rate::new(2_300_000.0).expect("valid"));
        nic.link_speed_mbps = MetricState::Unsupported;

        assert_eq!(
            nic.utilization(),
            MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
            "§7.4 forbids a utilization percentage without known capacity"
        );
    }

    #[test]
    fn a_zero_link_speed_is_treated_as_unknown_not_as_infinite_utilization() {
        let mut nic = interface();
        nic.rx = MetricState::Available(Rate::new(1_000.0).expect("valid"));
        nic.tx = MetricState::Available(Rate::ZERO);
        nic.link_speed_mbps = MetricState::Available(0);
        assert_eq!(
            nic.utilization(),
            MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown)
        );
    }

    #[test]
    fn utilization_uses_the_busier_direction_of_a_duplex_link() {
        let mut nic = interface();
        // 1 Gbit/s = 125 MB/s. 62.5 MB/s in one direction is 50%.
        nic.rx = MetricState::Available(Rate::new(62_500_000.0).expect("valid"));
        nic.tx = MetricState::Available(Rate::new(1_000.0).expect("valid"));
        nic.link_speed_mbps = MetricState::Available(1_000);

        let percent = *nic
            .utilization()
            .fresh()
            .expect("speed and rates are known");
        assert!((percent.value() - 50.0).abs() < 0.1, "got {percent}");
    }

    #[test]
    fn utilization_warms_up_while_rates_are_still_unknown() {
        let mut nic = interface();
        nic.link_speed_mbps = MetricState::Available(1_000);
        assert!(nic.utilization().is_warming_up());
    }

    #[test]
    fn utilization_can_exceed_one_hundred_percent_rather_than_being_clamped() {
        // Reported link speeds are frequently wrong (aggregated links, stale
        // Wi-Fi negotiation). Clamping would hide that; the value is honest.
        let mut nic = interface();
        nic.rx = MetricState::Available(Rate::new(250_000_000.0).expect("valid"));
        nic.tx = MetricState::Available(Rate::ZERO);
        nic.link_speed_mbps = MetricState::Available(1_000);
        let percent = *nic.utilization().fresh().expect("known");
        assert!(percent.value() > 100.0, "got {percent}");
    }

    #[test]
    fn link_state_symbols_are_distinguishable_without_color() {
        let mut symbols: Vec<char> = [
            LinkState::Up,
            LinkState::Down,
            LinkState::Dormant,
            LinkState::Unknown,
        ]
        .iter()
        .map(|s| s.symbol())
        .collect();
        symbols.sort_unstable();
        symbols.dedup();
        assert_eq!(symbols.len(), 4);
    }

    #[test]
    fn error_counters_report_whether_anything_is_wrong() {
        assert!(!InterfaceErrors::default().any());
        assert!(
            InterfaceErrors {
                rx_dropped: 1,
                ..InterfaceErrors::default()
            }
            .any()
        );
    }
}
