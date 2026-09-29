//! The decision behind [`crate::backend::VmBackend::stop_unproven`]: stop an
//! instance whose state came from the backend's control plane rather than a
//! [`crate::backend::RunningInstance`] proof.

use anyhow::Result;

/// Run `stop` when `probe` says the instance is running. A failed probe is
/// returned as the error, so it can never read as "already stopped".
pub(crate) fn stop_if_probed_running(
    probe: Result<bool>,
    stop: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if probe? { stop() } else { Ok(()) }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use std::cell::Cell;

    use crate::backend::unproven_stop::stop_if_probed_running;

    #[test]
    fn stops_only_a_running_instance() {
        let stopped = Cell::new(false);
        stop_if_probed_running(Ok(true), || {
            stopped.set(true);
            Ok(())
        })
        .unwrap();
        assert!(stopped.get());

        let stopped = Cell::new(false);
        stop_if_probed_running(Ok(false), || {
            stopped.set(true);
            Ok(())
        })
        .unwrap();
        assert!(!stopped.get(), "an already-stopped instance is left alone");
    }

    #[test]
    fn failed_probe_is_an_error_not_stopped() {
        let stopped = Cell::new(false);
        let err = stop_if_probed_running(Err(anyhow::anyhow!("limactl query failed")), || {
            stopped.set(true);
            Ok(())
        })
        .unwrap_err();
        assert!(err.to_string().contains("limactl query failed"));
        assert!(!stopped.get(), "an unknown state must not be stopped");
    }

    #[test]
    fn stop_failure_propagates() {
        let err =
            stop_if_probed_running(Ok(true), || Err(anyhow::anyhow!("stop failed"))).unwrap_err();
        assert!(err.to_string().contains("stop failed"));
    }
}
