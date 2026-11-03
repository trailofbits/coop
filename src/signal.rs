use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static HANDLER_STATE: Mutex<HandlerState> = Mutex::new(HandlerState {
    users: 0,
    previous: None,
});

struct HandlerState {
    users: usize,
    previous: Option<(libc::sighandler_t, libc::sighandler_t)>,
}

extern "C" fn handle_signal(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
}

/// RAII guard that restores previous signal handlers on drop.
pub struct SignalGuard {
    _private: (),
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        let mut state = handler_state();
        state.users -= 1;
        if state.users == 0
            && let Some(previous) = state.previous.take()
        {
            restore_handlers(previous);
        }
    }
}

/// Install SIGINT and SIGTERM handlers that set a shutdown flag
/// instead of terminating the process immediately.
///
/// Returns a guard that restores the previous handlers after the last nested
/// or concurrent guard is dropped.
pub fn install_handlers() -> SignalGuard {
    let mut state = handler_state();
    if state.users == 0 {
        // SAFETY: `handle_signal` only performs an atomic store, which is
        // async-signal-safe. We read the previous handlers to restore later.
        state.previous = Some(unsafe {
            let prev_int = libc::signal(
                libc::SIGINT,
                handle_signal as *const () as libc::sighandler_t,
            );
            let prev_term = libc::signal(
                libc::SIGTERM,
                handle_signal as *const () as libc::sighandler_t,
            );
            (prev_int, prev_term)
        });
    }
    state.users += 1;
    SignalGuard { _private: () }
}

fn handler_state() -> MutexGuard<'static, HandlerState> {
    HANDLER_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn restore_handlers(prev: (libc::sighandler_t, libc::sighandler_t)) {
    // SAFETY: restoring handlers that were returned by a prior `signal()` call.
    unsafe {
        libc::signal(libc::SIGINT, prev.0);
        libc::signal(libc::SIGTERM, prev.1);
    }
}

/// Returns `true` if a SIGINT or SIGTERM has been received.
pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Relaxed)
}

/// Check for pending shutdown and return an error if one was requested.
pub fn check_shutdown() -> anyhow::Result<()> {
    if shutdown_requested() {
        anyhow::bail!("Interrupted by signal — cleaning up");
    }
    Ok(())
}
