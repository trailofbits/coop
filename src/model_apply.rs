//! Durable state for a live model configuration change.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::path::PathBuf;

use crate::config::Instance;

thread_local! {
    static ACTIVE: RefCell<Vec<(PathBuf, u64)>> = const { RefCell::new(Vec::new()) };
}

#[derive(Default, Serialize, Deserialize)]
struct ApplyState {
    generation: u64,
    pending: bool,
}

fn path(inst: &Instance) -> std::path::PathBuf {
    inst.dir.join("model-apply.json")
}

fn load(inst: &Instance) -> Result<ApplyState> {
    let path = path(inst);
    match crate::fs_util::read_optional_private(&path)? {
        Some(content) => serde_json::from_str(&content).context("Failed to parse model-apply.json"),
        None => Ok(ApplyState::default()),
    }
}

fn save(inst: &Instance, state: &ApplyState) -> Result<()> {
    let json = serde_json::to_string_pretty(state)?;
    crate::fs_util::atomic_write_with_mode(&path(inst), &json, 0o600)
}

/// Caller holds the instance operation lock and has confirmed a running VM.
pub(crate) fn begin(inst: &Instance) -> Result<u64> {
    let mut state = load(inst)?;
    state.generation = state
        .generation
        .checked_add(1)
        .context("Model apply generation overflow")?;
    state.pending = true;
    save(inst, &state)?;
    Ok(state.generation)
}

/// Caller holds the instance operation lock. A concurrent live apply must not
/// be able to publish success after a stop or disk replacement.
pub(crate) fn cancel(inst: &Instance) -> Result<()> {
    let mut state = load(inst)?;
    if !state.pending {
        return Ok(());
    }
    state.generation = state
        .generation
        .checked_add(1)
        .context("Model apply generation overflow")?;
    save(inst, &state)
}

pub(crate) fn is_pending(inst: &Instance) -> Result<bool> {
    Ok(load(inst)?.pending)
}

pub(crate) fn with_generation<T>(
    inst: &Instance,
    generation: u64,
    work: impl FnOnce() -> Result<T>,
) -> Result<T> {
    struct ActiveGuard;
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            ACTIVE.with(|active| {
                active.borrow_mut().pop();
            });
        }
    }
    let path = path(inst);
    ACTIVE.with(|active| active.borrow_mut().push((path.clone(), generation)));
    let _guard = ActiveGuard;
    work()
}

/// Called while holding the short proxy transition lock.
pub(crate) fn ensure_transition_current(inst: &Instance) -> Result<()> {
    let path = path(inst);
    let generation = ACTIVE.with(|active| {
        active
            .borrow()
            .iter()
            .rev()
            .find(|(active_path, _)| active_path == &path)
            .map(|(_, generation)| *generation)
    });
    if let Some(generation) = generation {
        ensure_generation(inst, generation)?;
    }
    Ok(())
}

pub(crate) fn ensure_ready(inst: &Instance) -> Result<()> {
    if is_pending(inst)? {
        bail!(
            "VM '{}' has a model change pending; restart it to apply the saved model configuration before launching an agent",
            inst.name
        );
    }
    Ok(())
}

pub(crate) fn ensure_generation(inst: &Instance, generation: u64) -> Result<()> {
    let state = load(inst)?;
    if state.generation != generation || !state.pending {
        bail!(
            "VM '{}' changed lifecycle state while applying its model configuration; retry after restart",
            inst.name
        );
    }
    Ok(())
}

/// Caller holds the instance operation lock and has reconfirmed liveness.
pub(crate) fn finish(inst: &Instance, generation: u64) -> Result<()> {
    ensure_generation(inst, generation)?;
    let mut state = load(inst)?;
    state.pending = false;
    save(inst, &state)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test fixture and assertions")]
mod tests {
    use super::*;
    use crate::config::{FirecrackerControlMode, ImageName, InstanceIndex, InstanceName};

    #[test]
    fn cancellation_prevents_late_apply_and_restart_can_recover() {
        let root = crate::test_support::private_tempdir().unwrap();
        let inst = Instance {
            name: InstanceName::new("model-pending").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: root.path().to_path_buf(),
            image: ImageName::new("default").unwrap(),
            firecracker_control: FirecrackerControlMode::Vsock,
        };
        let first = begin(&inst).unwrap();
        assert!(is_pending(&inst).unwrap());
        assert!(ensure_ready(&inst).is_err());
        with_generation(&inst, first, || ensure_transition_current(&inst)).unwrap();
        cancel(&inst).unwrap();
        assert!(finish(&inst, first).is_err());
        assert!(with_generation(&inst, first, || ensure_transition_current(&inst)).is_err());
        let retry = begin(&inst).unwrap();
        assert!(retry > first);
        finish(&inst, retry).unwrap();
        ensure_ready(&inst).unwrap();
        assert!(!is_pending(&inst).unwrap());
    }
}
