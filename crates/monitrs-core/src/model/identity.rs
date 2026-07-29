//! Stable process identity.
//!
//! §26: *PID alone is not a stable process identity.* A PID is reused by the
//! kernel, so pinning, selection, history attribution, and — most importantly —
//! signal delivery must all key on a value that changes when the process behind
//! a PID changes.

use core::fmt;

/// A process identity that survives PID reuse.
///
/// `start_key` is an opaque, platform-supplied value derived from the process
/// start time. It is deliberately *not* a `SystemTime`: on Linux it comes from
/// field 22 of `/proc/<pid>/stat` in clock ticks since boot, and on macOS from
/// the `kp_proc.p_starttime` timeval. Both are stable for the life of the
/// process and change on reuse, which is the only property this type needs.
///
/// Two identities compare equal only when both the PID and the start key match,
/// which is what makes [`crate::model::ProcessIdentity`] safe to attach to a
/// pending signal (§6.2, §15.1).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProcessIdentity {
    /// The OS process identifier.
    pub pid: u32,
    /// An opaque platform value that changes when this PID is reused.
    pub start_key: u64,
}

impl ProcessIdentity {
    /// Builds an identity from a PID and a platform start key.
    #[must_use]
    pub const fn new(pid: u32, start_key: u64) -> Self {
        Self { pid, start_key }
    }

    /// Whether `other` is the same PID but a *different* process.
    ///
    /// The signal path calls this after re-reading the live process table: a
    /// `true` result means the PID was reused and the pending action must abort
    /// rather than signal an unrelated process (§6.2).
    #[must_use]
    pub const fn is_reuse_of(&self, other: &Self) -> bool {
        self.pid == other.pid && self.start_key != other.start_key
    }
}

impl fmt::Display for ProcessIdentity {
    /// Renders just the PID: the start key is an internal correctness device and
    /// would be noise in the UI.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.pid)
    }
}

/// The owning user of a process.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct UserIdentity {
    /// Numeric user id.
    pub uid: u32,
    /// Resolved user name, when the OS lets us look it up.
    ///
    /// Name resolution can fail or be denied for another user's process, which
    /// is why the numeric id is always present and the name is not.
    pub name: Option<Box<str>>,
}

impl UserIdentity {
    /// The name if resolved, otherwise the numeric id rendered as text.
    #[must_use]
    pub fn display_name(&self) -> String {
        match &self.name {
            Some(name) => name.to_string(),
            None => self.uid.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_requires_both_pid_and_start_key_to_match() {
        let a = ProcessIdentity::new(31842, 900_100);
        let b = ProcessIdentity::new(31842, 900_100);
        let recycled = ProcessIdentity::new(31842, 977_400);
        let other = ProcessIdentity::new(1221, 900_100);

        assert_eq!(a, b);
        assert_ne!(a, recycled);
        assert_ne!(a, other);
    }

    #[test]
    fn pid_reuse_is_detected_and_a_different_pid_is_not_reuse() {
        let pinned = ProcessIdentity::new(31842, 900_100);
        let recycled = ProcessIdentity::new(31842, 977_400);
        let unrelated = ProcessIdentity::new(1221, 977_400);

        assert!(
            recycled.is_reuse_of(&pinned),
            "same PID, different start key"
        );
        assert!(!pinned.is_reuse_of(&pinned), "identical is not reuse");
        assert!(
            !unrelated.is_reuse_of(&pinned),
            "different PID is not reuse"
        );
    }

    #[test]
    fn display_shows_only_the_pid() {
        assert_eq!(ProcessIdentity::new(31842, 900_100).to_string(), "31842");
    }

    #[test]
    fn unresolvable_user_names_fall_back_to_the_numeric_id() {
        let named = UserIdentity {
            uid: 501,
            name: Some("gabor".into()),
        };
        let anonymous = UserIdentity {
            uid: 501,
            name: None,
        };
        assert_eq!(named.display_name(), "gabor");
        assert_eq!(anonymous.display_name(), "501");
    }
}
