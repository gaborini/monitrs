//! The platform-neutral data model.
//!
//! Every type here is plain data. Nothing in this module knows how `/proc` is
//! parsed or how a process row is coloured — that is the dependency direction
//! §10.1 requires, and it is what makes the whole model testable with fixtures.

mod capability;
mod cpu;
mod health;
mod host;
mod identity;
mod measurement;
mod memory;
mod metric;
mod network;
mod pressure;
mod process;
mod sensors;
mod snapshot;
mod storage;

pub use capability::{CapabilitySnapshot, CapabilityState};
pub use cpu::{
    CoreClass, CpuBreakdown, CpuNormalization, CpuQuota, CpuSnapshot, CpuUsage, LoadSnapshot,
};
pub use health::{
    CollectorHealth, CollectorIssue, MAX_RETAINED_ISSUES, SelfOverhead, Tier, TierHealth,
};
pub use host::{
    ContainerIdentity, ContainerRuntime, EnvironmentKind, HostEnvironment, HostSnapshot,
};
pub use identity::{ProcessIdentity, UserIdentity};
pub use measurement::{Confidence, MeasuredValue, Measurement, Severity};
pub use memory::{MemoryDetail, MemorySemantics, MemorySnapshot, SwapSnapshot};
pub use metric::{MetricState, UnavailableReason};
pub use network::{
    InterfaceAddress, InterfaceErrors, InterfaceKind, LinkState, NetworkSnapshot, TrafficTotals,
};
pub use pressure::{
    PressureId, PressureSignal, PressureSnapshot, PressureState, PsiResource, PsiSnapshot,
};
pub use process::{
    AncestorEntry, OpenFileEntry, OpenFileKind, OpenFileList, ProcessDetail, ProcessDetailResult,
    ProcessIo, ProcessMemory, ProcessSnapshot, ProcessState,
};
pub use sensors::{
    BatteryCapacity, BatterySnapshot, ChargeState, SensorSnapshot, TemperatureReading,
};
pub use snapshot::SystemSnapshot;
pub use storage::{DiskSnapshot, DiskTotals, FilesystemKind, FilesystemSnapshot, InodeUsage};
