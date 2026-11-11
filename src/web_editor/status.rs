//! Small stdout statuses; arbitrary guest text never reaches diagnostics.

use std::io::{ErrorKind, Read};

use anyhow::{Result, bail, ensure};

const MAX_LINE_BYTES: usize = 256;
const MAX_EVENTS: usize = 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) enum Stage {
    #[default]
    SshSetup,
    Preparing,
    Installing,
    Starting,
    Launched,
    Ready,
}

impl Stage {
    pub(super) fn ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::SshSetup => "SSH connection and local port forwarding",
            Self::Preparing => "guest preparation",
            Self::Installing => "code-server installation",
            Self::Starting => "guest systemd launch",
            Self::Launched => "code-server startup",
            Self::Ready => "the running editor session",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Event {
    Stage(Stage),
    Downloaded(u64),
    Failed(&'static str),
}

fn parse(line: &[u8]) -> Result<Event> {
    let line =
        std::str::from_utf8(line).map_err(|_| anyhow::anyhow!("Invalid editor status line"))?;
    let stage = match line {
        "preparing" => Stage::Preparing,
        "installing" => Stage::Installing,
        "starting" => Stage::Starting,
        "launched" => Stage::Launched,
        "ready" => Stage::Ready,
        _ => {
            if let Some(bytes) = line.strip_prefix("downloaded ") {
                ensure!(
                    !bytes.is_empty() && bytes.bytes().all(|b| b.is_ascii_digit()),
                    "Invalid editor download progress"
                );
                let bytes = bytes
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid editor download progress"))?;
                return Ok(Event::Downloaded(bytes));
            }
            // These messages are host-authored. Never quote unknown guest text.
            let message = match line {
                "failed preparing unsupported_architecture" => {
                    "Guest architecture is unsupported by the browser editor"
                }
                "failed preparing project_missing" => {
                    "Guest project directory is missing; choose an existing --project directory in the VM"
                }
                "failed preparing setup_failed" => {
                    "Guest editor preparation failed; check guest storage and permissions"
                }
                "failed installing lock_timeout" => {
                    "Guest editor install lock timed out; another installation may still be running"
                }
                "failed installing setup_failed" => {
                    "Guest editor installation failed; check guest storage and permissions"
                }
                "failed download timeout" => {
                    "Guest code-server download timed out; check VM connectivity to GitHub and retry"
                }
                "failed download dns" => {
                    "Guest code-server download failed DNS resolution; check the VM's DNS configuration"
                }
                "failed download connection" => {
                    "Guest code-server download could not connect; check VM connectivity to GitHub"
                }
                "failed download http" => {
                    "GitHub rejected the guest code-server download; check release availability and VM network access"
                }
                "failed download tls" => {
                    "Guest code-server download failed TLS verification or negotiation; check guest certificates and proxy configuration"
                }
                "failed download transfer_failed" => {
                    "Guest code-server download failed; check VM connectivity and guest disk space"
                }
                "failed verification checksum_mismatch" => {
                    "Guest code-server archive failed SHA-256 verification; retry the download"
                }
                "failed extraction archive_invalid" => {
                    "Guest code-server archive could not be installed; check guest disk space and archive contents"
                }
                "failed systemd_launch launch_failed" => {
                    "Guest systemd could not launch code-server; check passwordless sudo, systemd and guest storage"
                }
                "failed code_server exited" => {
                    "Guest editor service exited; inspect ~/.local/share/coop-web-editor/editor.log in the VM"
                }
                _ => bail!("Invalid editor status line"),
            };
            return Ok(Event::Failed(message));
        }
    };
    Ok(Event::Stage(stage))
}

#[derive(Debug)]
pub(super) enum Poll {
    Pending,
    Closed,
    Event(Event),
}

pub(super) struct Reader<R> {
    output: R,
    buffer: [u8; MAX_LINE_BYTES],
    buffered: usize,
    events: usize,
}

impl<R: Read> Reader<R> {
    pub(super) fn new(output: R) -> Self {
        Self {
            output,
            buffer: [0; MAX_LINE_BYTES],
            buffered: 0,
            events: 0,
        }
    }

    /// The caller supplies a nonblocking pipe; partial lines survive a poll.
    pub(super) fn poll(&mut self) -> Result<Poll> {
        loop {
            if let Some(end) = self.buffer[..self.buffered]
                .iter()
                .position(|&b| b == b'\n')
            {
                self.events += 1;
                ensure!(self.events <= MAX_EVENTS, "Too many editor status lines");
                let event = parse(&self.buffer[..end])?;
                self.buffer.copy_within(end + 1..self.buffered, 0);
                self.buffered -= end + 1;
                return Ok(Poll::Event(event));
            }
            ensure!(
                self.buffered < MAX_LINE_BYTES,
                "Editor status line is too long"
            );
            match self.output.read(&mut self.buffer[self.buffered..]) {
                Ok(0) => {
                    ensure!(self.buffered == 0, "Truncated editor status line");
                    return Ok(Poll::Closed);
                }
                Ok(count) => self.buffered += count,
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
                {
                    return Ok(Poll::Pending);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use std::io::{Cursor, Error, ErrorKind, Read};

    use crate::web_editor::status::{Event, Poll, Reader, Stage, parse};

    #[test]
    fn stages_have_stable_labels_and_only_ready_is_ready() {
        for (stage, ready, label) in [
            (
                Stage::SshSetup,
                false,
                "SSH connection and local port forwarding",
            ),
            (Stage::Preparing, false, "guest preparation"),
            (Stage::Installing, false, "code-server installation"),
            (Stage::Starting, false, "guest systemd launch"),
            (Stage::Launched, false, "code-server startup"),
            (Stage::Ready, true, "the running editor session"),
        ] {
            assert_eq!(stage.ready(), ready, "stage {stage:?}");
            assert_eq!(stage.label(), label, "stage {stage:?}");
        }
    }

    #[test]
    fn every_guest_failure_token_maps_to_fixed_host_authored_text() {
        for (line, message) in [
            (
                "failed preparing unsupported_architecture",
                "Guest architecture is unsupported by the browser editor",
            ),
            (
                "failed preparing project_missing",
                "Guest project directory is missing; choose an existing --project directory in the VM",
            ),
            (
                "failed preparing setup_failed",
                "Guest editor preparation failed; check guest storage and permissions",
            ),
            (
                "failed installing lock_timeout",
                "Guest editor install lock timed out; another installation may still be running",
            ),
            (
                "failed installing setup_failed",
                "Guest editor installation failed; check guest storage and permissions",
            ),
            (
                "failed download timeout",
                "Guest code-server download timed out; check VM connectivity to GitHub and retry",
            ),
            (
                "failed download dns",
                "Guest code-server download failed DNS resolution; check the VM's DNS configuration",
            ),
            (
                "failed download connection",
                "Guest code-server download could not connect; check VM connectivity to GitHub",
            ),
            (
                "failed download http",
                "GitHub rejected the guest code-server download; check release availability and VM network access",
            ),
            (
                "failed download tls",
                "Guest code-server download failed TLS verification or negotiation; check guest certificates and proxy configuration",
            ),
            (
                "failed download transfer_failed",
                "Guest code-server download failed; check VM connectivity and guest disk space",
            ),
            (
                "failed verification checksum_mismatch",
                "Guest code-server archive failed SHA-256 verification; retry the download",
            ),
            (
                "failed extraction archive_invalid",
                "Guest code-server archive could not be installed; check guest disk space and archive contents",
            ),
            (
                "failed systemd_launch launch_failed",
                "Guest systemd could not launch code-server; check passwordless sudo, systemd and guest storage",
            ),
            (
                "failed code_server exited",
                "Guest editor service exited; inspect ~/.local/share/coop-web-editor/editor.log in the VM",
            ),
        ] {
            assert!(
                matches!(parse(line.as_bytes()), Ok(Event::Failed(actual)) if actual == message),
                "failure token {line:?} should map to fixed host-authored text"
            );
        }
    }

    #[test]
    fn reads_coalesced_statuses_and_failure_before_eof() {
        let mut reader = Reader::new(
            b"preparing\ninstalling\ndownloaded 1234\nfailed download timeout\n".as_slice(),
        );
        assert!(matches!(
            reader.poll().unwrap(),
            Poll::Event(Event::Stage(Stage::Preparing))
        ));
        assert!(matches!(
            reader.poll().unwrap(),
            Poll::Event(Event::Stage(Stage::Installing))
        ));
        assert!(matches!(
            reader.poll().unwrap(),
            Poll::Event(Event::Downloaded(1234))
        ));
        let message = match reader.poll().unwrap() {
            Poll::Event(Event::Failed(message)) => Some(message),
            _ => None,
        }
        .unwrap();
        assert!(message.contains("download timed out"));
        assert!(!message.contains("editor.log"));
        assert!(matches!(reader.poll().unwrap(), Poll::Closed));
    }

    struct Fragmented {
        bytes: Cursor<Vec<u8>>,
        block: bool,
    }

    impl Read for Fragmented {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            self.block = !self.block;
            if self.block {
                return Err(Error::from(ErrorKind::WouldBlock));
            }
            let count = output.len().min(3);
            self.bytes.read(&mut output[..count])
        }
    }

    struct PermissionDenied;

    impl Read for PermissionDenied {
        fn read(&mut self, _output: &mut [u8]) -> std::io::Result<usize> {
            Err(Error::from(ErrorKind::PermissionDenied))
        }
    }

    #[test]
    fn preserves_partial_lines_across_nonblocking_polls() {
        let mut reader = Reader::new(Fragmented {
            bytes: Cursor::new(b"starting\nlaunched\nready\n".to_vec()),
            block: false,
        });
        let mut stages = Vec::new();
        let mut pending = 0;
        for _ in 0..30 {
            match reader.poll().unwrap() {
                Poll::Pending => pending += 1,
                Poll::Event(event) => stages.push(event),
                Poll::Closed => break,
            }
        }
        assert!(pending > 0);
        assert_eq!(
            stages,
            [
                Event::Stage(Stage::Starting),
                Event::Stage(Stage::Launched),
                Event::Stage(Stage::Ready)
            ]
        );
    }

    #[test]
    fn preserves_hard_read_errors() {
        let error = Reader::new(PermissionDenied).poll().unwrap_err();
        assert_eq!(
            error.downcast_ref::<Error>().unwrap().kind(),
            ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn bounds_lines_and_event_count_and_never_echoes_unknown_guest_text() {
        for input in [
            b"failed download secret-\x1b[31m-in-stderr\n".to_vec(),
            b"downloaded 18446744073709551616\n".to_vec(),
            b"downloaded -1\n".to_vec(),
            b"downloaded \n".to_vec(),
            b"\xff\n".to_vec(),
        ] {
            let error = Reader::new(input.as_slice())
                .poll()
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret-"));
            assert!(!error.contains('\x1b'));
        }
        assert_eq!(
            Reader::new(vec![b'x'; 256].as_slice())
                .poll()
                .unwrap_err()
                .to_string(),
            "Editor status line is too long"
        );
        assert_eq!(
            Reader::new(b"ready".as_slice())
                .poll()
                .unwrap_err()
                .to_string(),
            "Truncated editor status line"
        );
        let input = b"preparing\n".repeat(1025);
        let mut reader = Reader::new(input.as_slice());
        for _ in 0..1024 {
            assert!(matches!(
                reader.poll().unwrap(),
                Poll::Event(Event::Stage(Stage::Preparing))
            ));
        }
        assert_eq!(
            reader.poll().unwrap_err().to_string(),
            "Too many editor status lines"
        );
    }
}
