//! Binary entrypoint. Command dispatch lives in the `coop` library crate so that
//! fuzz targets and integration tests can depend on it directly. This shim maps
//! typed guest-command SSH failures to process exit codes.

use std::process::ExitCode;

fn main() -> anyhow::Result<ExitCode> {
    match coop::run() {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(error) => match error.downcast::<coop::SshExit>() {
            Ok(status) => Ok(ExitCode::from(status.0)),
            Err(error) => Err(error),
        },
    }
}
