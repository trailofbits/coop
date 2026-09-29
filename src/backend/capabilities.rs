//! What a backend supports, and the typed error for a missing capability.

use anyhow::Result;

/// Something a backend may or may not support. Handlers check the one they
/// need with [`BackendCapabilities::require`] before any side effect (stopping
/// a VM, writing image metadata, looking up a disk path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Host directories are shared live (virtiofs) instead of copied.
    LiveMounts,
    /// Grow an instance disk (`coop resize --size`) or size it explicitly at
    /// creation (`coop up --disk`).
    DiskResize,
    /// Save or replace an instance filesystem (`coop commit` / `coop restore`).
    DiskSnapshots,
    /// Change memory and vCPUs of a stopped instance.
    MachineResources,
}

impl Capability {
    fn describe(self) -> &'static str {
        match self {
            Self::LiveMounts => "live host-directory mounts",
            Self::DiskResize => "explicit disk sizing and resizing",
            Self::DiskSnapshots => "filesystem commit/restore snapshots",
            Self::MachineResources => "changing memory or vCPUs",
        }
    }
}

/// What a backend supports, reported by [`crate::backend::VmBackend::capabilities`].
///
/// Capability reporting, not runtime backend selection: the backend is still
/// fixed at compile time by [`crate::backend::PlatformBackend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilities(&'static [Capability]);

impl BackendCapabilities {
    pub const fn new(supported: &'static [Capability]) -> Self {
        Self(supported)
    }

    pub fn has(self, cap: Capability) -> bool {
        self.0.contains(&cap)
    }

    /// Fail with a typed unsupported-capability error unless `cap` is present.
    pub fn require(
        self,
        backend: &(impl std::fmt::Display + ?Sized),
        cap: Capability,
    ) -> Result<()> {
        if self.has(cap) {
            return Ok(());
        }
        Err(UnsupportedCapability {
            backend: backend.to_string(),
            capability: cap,
        }
        .into())
    }
}

/// A requested operation the active backend does not implement. Raised before
/// any state-changing work.
#[derive(Debug, thiserror::Error)]
#[error(
    "CAPABILITY_UNSUPPORTED: the {backend} backend does not support {}; nothing was changed",
    capability.describe()
)]
pub struct UnsupportedCapability {
    pub backend: String,
    pub capability: Capability,
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use crate::backend::capabilities::{BackendCapabilities, Capability, UnsupportedCapability};

    #[test]
    fn capabilities_require_rejects_missing_capability() {
        let caps = BackendCapabilities::new(&[Capability::MachineResources]);
        let err = caps
            .require(&"apple-container", Capability::DiskResize)
            .unwrap_err();
        assert!(err.downcast_ref::<UnsupportedCapability>().is_some());
        assert!(err.to_string().contains("CAPABILITY_UNSUPPORTED"));
        assert!(
            err.to_string()
                .contains("explicit disk sizing and resizing")
        );
        assert!(err.to_string().contains("apple-container backend"));
        let full = BackendCapabilities::new(&[Capability::DiskSnapshots]);
        assert!(full.require(&"lima", Capability::DiskSnapshots).is_ok());
        assert!(!full.has(Capability::LiveMounts));
    }
}
