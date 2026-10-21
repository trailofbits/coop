//! Firecracker host-initiated vsock connection and byte relay.
//!
//! Firecracker exposes a base Unix socket per VM. A host client selects the
//! guest `AF_VSOCK` port by sending one bounded `CONNECT <port>\n` request and
//! waiting for one bounded `OK <host-port>\n` response. After that handshake,
//! the socket is an ordinary byte stream.

use std::io::{self, Read as _, Write as _};
use std::num::NonZeroU32;
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use socket2::{Domain, SockAddr, Socket, Type};

const MAX_RESPONSE_LEN: usize = 64;

#[derive(Debug, thiserror::Error)]
#[error("Invalid Firecracker vsock handshake response")]
struct InvalidHandshake;

/// A missing or refusing socket can be transient while Firecracker starts;
/// permission, invalid path structure, and protocol failures cannot be fixed
/// by guest boot.
pub(crate) fn permanent_readiness_error(error: &anyhow::Error) -> bool {
    if error.is::<InvalidHandshake>() {
        return true;
    }
    error.chain().any(|cause| {
        cause.downcast_ref::<io::Error>().is_some_and(|error| {
            matches!(
                error.raw_os_error(),
                Some(libc::EACCES | libc::EPERM | libc::ENOTDIR | libc::ELOOP | libc::ENAMETOOLONG)
            )
        })
    })
}

struct NonblockingFd {
    fd: std::os::fd::RawFd,
    original_flags: libc::c_int,
}

impl NonblockingFd {
    fn new(fd: std::os::fd::RawFd, description: &str) -> Result<Self> {
        // SAFETY: fcntl only inspects flags for the live borrowed descriptor.
        let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if original_flags == -1 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("Failed to inspect {description} flags"));
        }
        if original_flags & libc::O_NONBLOCK == 0 {
            // SAFETY: fcntl updates flags for the same live descriptor.
            if unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK) } == -1 {
                return Err(io::Error::last_os_error())
                    .with_context(|| format!("Failed to make {description} nonblocking"));
            }
        }
        Ok(Self { fd, original_flags })
    }
}

impl Drop for NonblockingFd {
    fn drop(&mut self) {
        // SAFETY: the relay only borrows standard I/O, which remains live
        // until process exit. Restoration is best-effort during unwinding.
        if unsafe { libc::fcntl(self.fd, libc::F_SETFL, self.original_flags) } == -1 {
            tracing::debug!(
                "Failed to restore ProxyCommand descriptor flags: {}",
                io::Error::last_os_error()
            );
        }
    }
}

fn poll_entry(fd: std::os::fd::RawFd, events: libc::c_short, active: bool) -> libc::pollfd {
    libc::pollfd {
        fd: if active { fd } else { -1 },
        events,
        revents: 0,
    }
}

/// Connect to a Firecracker base vsock socket and complete its host-initiated
/// handshake. Read and write deadlines bound a VMM that accepts but never
/// completes the handshake; callers may keep the returned stream indefinitely.
pub(crate) fn connect(
    socket_path: &Path,
    guest_port: NonZeroU32,
    timeout: Duration,
) -> Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)
        .context("Failed to create Firecracker vsock client")?;
    socket
        .set_nonblocking(true)
        .context("Failed to bound Firecracker vsock connection")?;
    let address = SockAddr::unix(socket_path)
        .with_context(|| format!("Invalid Firecracker vsock path {}", socket_path.display()))?;
    match socket.connect(&address) {
        Ok(()) => {}
        Err(error) if matches!(error.raw_os_error(), Some(libc::EINPROGRESS | libc::EAGAIN)) => {
            let revents = wait_fd(socket.as_raw_fd(), libc::POLLOUT, deadline)?;
            if let Some(error) = socket
                .take_error()
                .context("Failed to inspect Firecracker vsock connection")?
            {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to connect to Firecracker vsock at {}",
                        socket_path.display()
                    )
                });
            }
            if revents & libc::POLLOUT == 0 {
                bail!(
                    "Firecracker closed vsock while connecting at {}",
                    socket_path.display()
                );
            }
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to connect to Firecracker vsock at {}",
                    socket_path.display()
                )
            });
        }
    }
    let socket: std::os::fd::OwnedFd = socket.into();
    let mut stream = UnixStream::from(socket);
    let request = format!("CONNECT {guest_port}\n");
    let mut request = request.as_bytes();
    while !request.is_empty() {
        wait_ready(&stream, libc::POLLOUT, deadline)?;
        match stream.write(request) {
            Ok(0) => bail!("Firecracker closed vsock during CONNECT handshake"),
            Ok(written) => request = &request[written..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(error).context("Failed to send Firecracker vsock CONNECT request");
            }
        }
    }

    let response = read_response(&mut stream, deadline)?;
    validate_response(&response).context(InvalidHandshake)?;

    stream
        .set_nonblocking(false)
        .context("Failed to finish Firecracker vsock handshake")?;
    Ok(stream)
}

fn wait_ready(stream: &UnixStream, events: libc::c_short, deadline: Instant) -> Result<()> {
    let revents = wait_fd(stream.as_raw_fd(), events, deadline)?;
    if revents & (libc::POLLERR | libc::POLLNVAL) != 0
        || revents & libc::POLLHUP != 0 && revents & events == 0
    {
        let error = stream
            .take_error()
            .context("Failed to inspect Firecracker vsock handshake error")?
            .unwrap_or_else(|| io::Error::other("Firecracker closed vsock during handshake"));
        return Err(error.into());
    }
    Ok(())
}

fn wait_fd(
    fd: std::os::fd::RawFd,
    events: libc::c_short,
    deadline: Instant,
) -> Result<libc::c_short> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("Timed out during Firecracker vsock CONNECT handshake")?;
        let milliseconds = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
        let mut poll_fd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: poll_fd is live and initialized for this call.
        let ready = unsafe { libc::poll(&raw mut poll_fd, 1, milliseconds) };
        if ready == 0 {
            bail!("Timed out during Firecracker vsock CONNECT handshake");
        }
        if ready > 0 {
            return Ok(poll_fd.revents);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error).context("Failed to poll Firecracker vsock handshake");
        }
    }
}

fn read_response(stream: &mut UnixStream, deadline: Instant) -> Result<Vec<u8>> {
    let mut response = Vec::with_capacity(MAX_RESPONSE_LEN);
    loop {
        if response.len() == MAX_RESPONSE_LEN {
            return Err(anyhow::Error::new(InvalidHandshake).context(format!(
                "Firecracker vsock response exceeded {MAX_RESPONSE_LEN} bytes"
            )));
        }
        wait_ready(stream, libc::POLLIN, deadline)?;
        let mut byte = [0_u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => bail!("Firecracker closed vsock during CONNECT handshake"),
            Ok(_) => {
                response.push(byte[0]);
                if byte[0] == b'\n' {
                    return Ok(response);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("Failed to read Firecracker vsock response"),
        }
    }
}

fn validate_response(response: &[u8]) -> Result<()> {
    let Some(port) = response
        .strip_prefix(b"OK ")
        .and_then(|value| value.strip_suffix(b"\n"))
    else {
        bail!(
            "Invalid Firecracker vsock response: {}",
            String::from_utf8_lossy(response).escape_default()
        );
    };
    std::str::from_utf8(port)
        .context("Firecracker vsock response port is not UTF-8")?
        .parse::<NonZeroU32>()
        .context("Firecracker vsock response port is not a non-zero u32")?;
    Ok(())
}

/// Run the hidden OpenSSH `ProxyCommand` entrypoint.
pub(crate) fn run(socket_path: &Path, guest_port: NonZeroU32) -> Result<()> {
    let stream = connect(socket_path, guest_port, Duration::from_secs(10))?;
    relay(stream).context("Firecracker vsock relay failed")
}

/// Relay stdin/stdout without spawning a shell or a child process.
///
/// The socket is nonblocking and `poll(2)` drives both directions. This avoids
/// leaving a thread blocked on terminal input when the guest closes first and
/// lets the `ProxyCommand` process exit promptly for OpenSSH to reap.
#[expect(
    clippy::too_many_lines,
    reason = "single bidirectional poll state machine is clearer kept together"
)]
fn relay(mut stream: UnixStream) -> Result<()> {
    const BUFFER_LEN: usize = 64 * 1024;

    let _stdin_flags = NonblockingFd::new(libc::STDIN_FILENO, "ProxyCommand stdin")?;
    let _stdout_flags = NonblockingFd::new(libc::STDOUT_FILENO, "ProxyCommand stdout")?;
    stream
        .set_nonblocking(true)
        .context("Failed to make Firecracker vsock relay nonblocking")?;
    let mut to_guest = Vec::with_capacity(BUFFER_LEN);
    let mut to_host = Vec::with_capacity(BUFFER_LEN);
    let mut read_buffer = vec![0_u8; BUFFER_LEN];
    let mut stdin_eof = false;
    let mut write_shutdown = false;
    let mut guest_eof = false;

    while !guest_eof || !to_host.is_empty() {
        let poll_stdin = !stdin_eof && to_guest.len() < BUFFER_LEN;
        let poll_stream_read = !guest_eof && to_host.len() < BUFFER_LEN;
        let poll_stream_write = !to_guest.is_empty();
        let poll_stdout = !to_host.is_empty();
        let mut poll_fds = [
            poll_entry(libc::STDIN_FILENO, libc::POLLIN, poll_stdin),
            poll_entry(
                stream.as_raw_fd(),
                (if poll_stream_read { libc::POLLIN } else { 0 })
                    | (if poll_stream_write { libc::POLLOUT } else { 0 }),
                poll_stream_read || poll_stream_write,
            ),
            poll_entry(libc::STDOUT_FILENO, libc::POLLOUT, poll_stdout),
        ];
        // SAFETY: poll_fds is a live, initialized array for this call.
        let ready = unsafe {
            libc::poll(
                poll_fds.as_mut_ptr(),
                poll_fds
                    .len()
                    .try_into()
                    .context("Too many Firecracker vsock poll descriptors")?,
                -1,
            )
        };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("Failed to poll Firecracker vsock relay");
        }

        if poll_fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0
            && !stdin_eof
            && to_guest.len() < BUFFER_LEN
        {
            // SAFETY: read_buffer is writable for the supplied slice length;
            // STDIN_FILENO remains borrowed rather than owned by the relay.
            let read = unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    read_buffer.as_mut_ptr().cast(),
                    BUFFER_LEN - to_guest.len(),
                )
            };
            match read {
                0 => stdin_eof = true,
                1.. => {
                    let read = usize::try_from(read).context("Invalid ProxyCommand stdin read")?;
                    to_guest.extend_from_slice(&read_buffer[..read]);
                }
                _ => {
                    let error = io::Error::last_os_error();
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) {
                        return Err(error).context("Failed to read ProxyCommand stdin");
                    }
                }
            }
        }

        if stdin_eof && to_guest.is_empty() && !write_shutdown {
            stream
                .shutdown(std::net::Shutdown::Write)
                .context("Failed to close Firecracker vsock write side")?;
            write_shutdown = true;
        }

        if poll_fds[1].revents & (libc::POLLOUT | libc::POLLHUP) != 0 && !to_guest.is_empty() {
            match stream.write(&to_guest) {
                Ok(0) => bail!("Firecracker vsock closed while writing"),
                Ok(written) => {
                    to_guest.drain(..written);
                    if stdin_eof && to_guest.is_empty() {
                        stream
                            .shutdown(std::net::Shutdown::Write)
                            .context("Failed to close Firecracker vsock write side")?;
                        write_shutdown = true;
                    }
                }
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        && poll_fds[1].revents & libc::POLLHUP == 0 => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    return Err(error).context("Firecracker vsock hung up while writing");
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error).context("Failed to write Firecracker vsock"),
            }
        }

        if poll_fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0
            && !guest_eof
            && to_host.len() < BUFFER_LEN
        {
            match stream.read(&mut read_buffer[..BUFFER_LEN - to_host.len()]) {
                Ok(0) => guest_eof = true,
                Ok(read) => to_host.extend_from_slice(&read_buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error).context("Failed to read Firecracker vsock"),
            }
        }

        if poll_fds[1].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            let error = stream
                .take_error()
                .context("Failed to inspect Firecracker vsock relay error")?
                .unwrap_or_else(|| io::Error::other("Firecracker vsock poll error"));
            return Err(error.into());
        }

        if poll_fds[2].revents & libc::POLLOUT != 0 && !to_host.is_empty() {
            // SAFETY: to_host is readable for its length; STDOUT_FILENO
            // remains borrowed rather than owned by the relay.
            let written =
                unsafe { libc::write(libc::STDOUT_FILENO, to_host.as_ptr().cast(), to_host.len()) };
            match written {
                0 => bail!("ProxyCommand stdout closed while writing"),
                1.. => {
                    let written =
                        usize::try_from(written).context("Invalid ProxyCommand stdout write")?;
                    to_host.drain(..written);
                }
                _ => {
                    let error = io::Error::last_os_error();
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) {
                        return Err(error).context("Failed to write ProxyCommand stdout");
                    }
                }
            }
        }
        if poll_fds[0].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            bail!("ProxyCommand stdin poll error");
        }
        if poll_fds[2].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
            && !to_host.is_empty()
        {
            bail!("ProxyCommand stdout closed while writing");
        }
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test fixtures")]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::thread;

    use super::*;

    #[test]
    fn inactive_poll_entries_remove_the_descriptor() {
        let inactive = poll_entry(42, libc::POLLIN, false);
        assert_eq!(inactive.fd, -1);
        assert_eq!(inactive.events, libc::POLLIN);
        assert_eq!(inactive.revents, 0);

        let active = poll_entry(42, libc::POLLIN | libc::POLLOUT, true);
        assert_eq!(active.fd, 42);
        assert_eq!(active.events, libc::POLLIN | libc::POLLOUT);
        assert_eq!(active.revents, 0);
    }

    fn fixture(
        response: Vec<u8>,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        thread::JoinHandle<Vec<u8>>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vsock.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0_u8; 1];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            stream.write_all(&response).unwrap();
            request
        });
        (directory, path, handle)
    }

    #[test]
    fn connect_sends_fixed_request_and_accepts_bounded_ok() {
        let (_directory, path, server) = fixture(b"OK 1073741824\n".to_vec());
        let port = NonZeroU32::new(22).unwrap();
        let _stream = connect(&path, port, Duration::from_secs(1)).unwrap();
        assert_eq!(server.join().unwrap(), b"CONNECT 22\n");
    }

    #[test]
    fn connect_rejects_malformed_response() {
        for response in [b"NO 1\n".to_vec(), b"OK 0\n".to_vec(), b"OK one\n".to_vec()] {
            let (_directory, path, server) = fixture(response);
            let error =
                connect(&path, NonZeroU32::new(22).unwrap(), Duration::from_secs(1)).unwrap_err();
            assert!(permanent_readiness_error(&error), "{error:#}");
            server.join().unwrap();
        }
    }

    #[test]
    fn readiness_classifies_permission_failure_as_permanent() {
        let error = anyhow::Error::new(io::Error::from_raw_os_error(libc::EACCES))
            .context("Failed to connect to Firecracker vsock");
        assert!(permanent_readiness_error(&error));

        let error = anyhow::Error::new(io::Error::from_raw_os_error(libc::ENOENT))
            .context("Failed to connect to Firecracker vsock");
        assert!(!permanent_readiness_error(&error));
    }

    #[test]
    fn connect_rejects_response_without_bounded_newline() {
        let (_directory, path, server) = fixture(vec![b'1'; MAX_RESPONSE_LEN]);
        let error =
            connect(&path, NonZeroU32::new(22).unwrap(), Duration::from_secs(1)).unwrap_err();
        assert!(error.to_string().contains("exceeded"));
        assert!(permanent_readiness_error(&error));
        server.join().unwrap();
    }

    #[test]
    fn connect_times_out_when_vmm_never_answers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vsock.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(100));
        });
        let error = connect(
            &path,
            NonZeroU32::new(22).unwrap(),
            Duration::from_millis(20),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("Timed out"), "{error}");
        server.join().unwrap();
    }
}
