//! Firecracker host-initiated vsock connection and byte relay.
//!
//! Firecracker exposes a base Unix socket per VM. A host client selects the
//! guest `AF_VSOCK` port by sending one bounded `CONNECT <port>\n` request and
//! waiting for one bounded `OK <host-port>\n` response. After that handshake,
//! the socket is an ordinary byte stream.

use std::io::{self, Read as _, Write as _};
use std::num::NonZeroU32;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use socket2::{Domain, SockAddr, Socket, Type};

const MAX_RESPONSE_LEN: usize = 64;
const MAX_VALID_RESPONSE_LEN: usize = b"OK 4294967295\n".len();

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

struct NonblockingFd<'fd> {
    fd: BorrowedFd<'fd>,
    original_flags: Option<libc::c_int>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputState {
    Reading,
    Draining,
    ShutDown,
}

impl<'fd> NonblockingFd<'fd> {
    fn new(fd: BorrowedFd<'fd>, description: &str) -> Result<Self> {
        // SAFETY: fcntl only inspects flags for the live borrowed descriptor.
        let original_flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if original_flags == -1 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("Failed to inspect {description} flags"));
        }
        let changed = original_flags & libc::O_NONBLOCK == 0;
        if changed {
            // SAFETY: fcntl updates flags for the same live descriptor.
            if unsafe {
                libc::fcntl(
                    fd.as_raw_fd(),
                    libc::F_SETFL,
                    original_flags | libc::O_NONBLOCK,
                )
            } == -1
            {
                return Err(io::Error::last_os_error())
                    .with_context(|| format!("Failed to make {description} nonblocking"));
            }
        }
        Ok(Self {
            fd,
            original_flags: changed.then_some(original_flags),
        })
    }
}

impl Drop for NonblockingFd<'_> {
    fn drop(&mut self) {
        let Some(original_flags) = self.original_flags else {
            return;
        };
        // SAFETY: BorrowedFd keeps the descriptor live for the guard's lifetime.
        // Restoration is best-effort during unwinding.
        if unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_SETFL, original_flags) } == -1 {
            tracing::debug!(
                "Failed to restore ProxyCommand descriptor flags: {}",
                io::Error::last_os_error()
            );
        }
    }
}

fn read_fd(fd: BorrowedFd<'_>, buffer: &mut [u8]) -> io::Result<usize> {
    // SAFETY: BorrowedFd is live, and the slice is writable for its length.
    let read = unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
    if read == -1 {
        Err(io::Error::last_os_error())
    } else {
        usize::try_from(read).map_err(|_| io::Error::other("Invalid descriptor read count"))
    }
}

fn write_fd(fd: BorrowedFd<'_>, buffer: &[u8]) -> io::Result<usize> {
    // SAFETY: BorrowedFd is live, and the slice is readable for its length.
    let written = unsafe { libc::write(fd.as_raw_fd(), buffer.as_ptr().cast(), buffer.len()) };
    if written == -1 {
        Err(io::Error::last_os_error())
    } else {
        usize::try_from(written).map_err(|_| io::Error::other("Invalid descriptor write count"))
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
            let revents = wait_fd(socket.as_fd(), libc::POLLOUT, deadline)?;
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
    parse_response_port(&response).context(InvalidHandshake)?;

    stream
        .set_nonblocking(false)
        .context("Failed to finish Firecracker vsock handshake")?;
    Ok(stream)
}

fn wait_ready(stream: &UnixStream, events: libc::c_short, deadline: Instant) -> Result<()> {
    let revents = wait_fd(stream.as_fd(), events, deadline)?;
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

fn wait_fd(fd: BorrowedFd<'_>, events: libc::c_short, deadline: Instant) -> Result<libc::c_short> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("Timed out during Firecracker vsock CONNECT handshake")?;
        let milliseconds = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
        let mut poll_fd = libc::pollfd {
            fd: fd.as_raw_fd(),
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

/// Parse Firecracker's complete acknowledgement without constructing an error.
/// Keeping the grammar separate from I/O lets Kani check arbitrary response bytes.
fn parse_response_port(response: &[u8]) -> Option<NonZeroU32> {
    if response.len() > MAX_VALID_RESPONSE_LEN {
        return None;
    }
    let port = response
        .strip_prefix(b"OK ")
        .and_then(|value| value.strip_suffix(b"\n"))?;
    if port.is_empty() {
        return None;
    }
    let mut value = 0_u32;
    for &byte in port {
        let digit = byte.checked_sub(b'0')?;
        if digit > 9 {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(digit))?;
    }
    NonZeroU32::new(value)
}

#[cfg(kani)]
mod proofs {
    use super::{MAX_VALID_RESPONSE_LEN, parse_response_port};

    /// Every response up to the longest canonical `OK <u32>\n` has exactly the
    /// documented grammar, a nonzero port, and no arithmetic truncation.
    #[kani::proof]
    #[kani::unwind(16)]
    fn firecracker_acknowledgement_matches_decimal_grammar() {
        let bytes: [u8; MAX_VALID_RESPONSE_LEN] = kani::any();
        let length: usize = kani::any();
        kani::assume(length <= bytes.len());
        let response = &bytes[..length];

        let expected = response
            .strip_prefix(b"OK ")
            .and_then(|port| port.strip_suffix(b"\n"))
            .and_then(|port| {
                let mut value = 0_u64;
                if port.is_empty() {
                    return None;
                }
                for &byte in port {
                    if !byte.is_ascii_digit() {
                        return None;
                    }
                    value = value * 10 + u64::from(byte - b'0');
                }
                (value > 0 && value <= u64::from(u32::MAX)).then_some(value as u32)
            });
        assert_eq!(
            parse_response_port(response).map(|port| port.get()),
            expected
        );
    }
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
fn relay(stream: UnixStream) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let _stdin_flags = NonblockingFd::new(stdin.as_fd(), "ProxyCommand stdin")?;
    let _stdout_flags = NonblockingFd::new(stdout.as_fd(), "ProxyCommand stdout")?;
    stream
        .set_nonblocking(true)
        .context("Failed to make Firecracker vsock relay nonblocking")?;
    Relay::new(stream, stdin.as_fd(), stdout.as_fd()).run()
}

const BUFFER_LEN: usize = 65_536;

/// Bytes waiting for one destination. Only `append` can grow this buffer, and
/// callers read no more than `remaining` bytes into it.
struct PendingBytes {
    bytes: Vec<u8>,
}

impl PendingBytes {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(BUFFER_LEN),
        }
    }

    fn remaining(&self) -> usize {
        BUFFER_LEN - self.bytes.len()
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    fn append(&mut self, bytes: &[u8]) {
        debug_assert!(bytes.len() <= self.remaining());
        self.bytes.extend_from_slice(bytes);
    }

    fn consume(&mut self, written: usize) {
        debug_assert!(written <= self.bytes.len());
        self.bytes.drain(..written);
    }
}

struct Readiness {
    stdin: libc::c_short,
    guest: libc::c_short,
    stdout: libc::c_short,
}

struct Relay<'fd> {
    stream: UnixStream,
    stdin: BorrowedFd<'fd>,
    stdout: BorrowedFd<'fd>,
    to_guest: PendingBytes,
    to_host: PendingBytes,
    read_buffer: Vec<u8>,
    input_state: InputState,
    guest_eof: bool,
}

impl<'fd> Relay<'fd> {
    fn new(stream: UnixStream, stdin: BorrowedFd<'fd>, stdout: BorrowedFd<'fd>) -> Self {
        Self {
            stream,
            stdin,
            stdout,
            to_guest: PendingBytes::new(),
            to_host: PendingBytes::new(),
            read_buffer: vec![0_u8; BUFFER_LEN],
            input_state: InputState::Reading,
            guest_eof: false,
        }
    }

    fn run(mut self) -> Result<()> {
        // ProxyCommand treats guest read EOF as session end. Flush already-read
        // guest output, then exit even if host stdin stays open.
        while !self.guest_eof || !self.to_host.is_empty() {
            let ready = self.poll()?;
            self.read_stdin(ready.stdin)?;
            self.write_guest(ready.guest)?;
            self.read_guest(ready.guest)?;
            self.check_guest_error(ready.guest)?;
            self.write_stdout(ready.stdout)?;
            self.check_stdio_errors(ready.stdin, ready.stdout)?;
            self.finish_input()?;
        }
        Ok(())
    }

    fn poll(&self) -> Result<Readiness> {
        let poll_stdin = self.input_state == InputState::Reading && self.to_guest.remaining() > 0;
        let poll_guest_read = !self.guest_eof && self.to_host.remaining() > 0;
        let poll_guest_write = !self.to_guest.is_empty();
        let poll_stdout = !self.to_host.is_empty();
        let mut poll_fds = [
            poll_entry(self.stdin.as_raw_fd(), libc::POLLIN, poll_stdin),
            poll_entry(
                self.stream.as_raw_fd(),
                (if poll_guest_read { libc::POLLIN } else { 0 })
                    | (if poll_guest_write { libc::POLLOUT } else { 0 }),
                poll_guest_read || poll_guest_write,
            ),
            poll_entry(self.stdout.as_raw_fd(), libc::POLLOUT, poll_stdout),
        ];
        loop {
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
            if ready >= 0 {
                return Ok(Readiness {
                    stdin: poll_fds[0].revents,
                    guest: poll_fds[1].revents,
                    stdout: poll_fds[2].revents,
                });
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error).context("Failed to poll Firecracker vsock relay");
            }
            for entry in &mut poll_fds {
                entry.revents = 0;
            }
        }
    }

    fn read_stdin(&mut self, events: libc::c_short) -> Result<()> {
        if events & (libc::POLLIN | libc::POLLHUP) == 0
            || self.input_state != InputState::Reading
            || self.to_guest.remaining() == 0
        {
            return Ok(());
        }
        let room = self.to_guest.remaining();
        match read_fd(self.stdin, &mut self.read_buffer[..room]) {
            Ok(0) => self.input_state = InputState::Draining,
            Ok(read) => self.to_guest.append(&self.read_buffer[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error).context("Failed to read ProxyCommand stdin"),
        }
        Ok(())
    }

    fn write_guest(&mut self, events: libc::c_short) -> Result<()> {
        if events & (libc::POLLOUT | libc::POLLHUP) == 0 || self.to_guest.is_empty() {
            return Ok(());
        }
        match self.stream.write(self.to_guest.as_slice()) {
            Ok(0) => bail!("Firecracker vsock closed while writing"),
            Ok(written) => self.to_guest.consume(written),
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && events & libc::POLLHUP == 0 => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Err(error).context("Firecracker vsock hung up while writing");
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("Failed to write Firecracker vsock"),
        }
        Ok(())
    }

    fn read_guest(&mut self, events: libc::c_short) -> Result<()> {
        if events & (libc::POLLIN | libc::POLLHUP) == 0
            || self.guest_eof
            || self.to_host.remaining() == 0
        {
            return Ok(());
        }
        match self
            .stream
            .read(&mut self.read_buffer[..self.to_host.remaining()])
        {
            Ok(0) => self.guest_eof = true,
            Ok(read) => self.to_host.append(&self.read_buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("Failed to read Firecracker vsock"),
        }
        Ok(())
    }

    fn check_guest_error(&self, events: libc::c_short) -> Result<()> {
        if events & (libc::POLLERR | libc::POLLNVAL) != 0 {
            let error = self
                .stream
                .take_error()
                .context("Failed to inspect Firecracker vsock relay error")?
                .unwrap_or_else(|| io::Error::other("Firecracker vsock poll error"));
            return Err(error.into());
        }
        Ok(())
    }

    fn write_stdout(&mut self, events: libc::c_short) -> Result<()> {
        if events & libc::POLLOUT == 0 || self.to_host.is_empty() {
            return Ok(());
        }
        match write_fd(self.stdout, self.to_host.as_slice()) {
            Ok(0) => bail!("ProxyCommand stdout closed while writing"),
            Ok(written) => self.to_host.consume(written),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error).context("Failed to write ProxyCommand stdout"),
        }
        Ok(())
    }

    fn check_stdio_errors(
        &self,
        stdin_events: libc::c_short,
        stdout_events: libc::c_short,
    ) -> Result<()> {
        if stdin_events & (libc::POLLERR | libc::POLLNVAL) != 0 {
            bail!("ProxyCommand stdin poll error");
        }
        if stdout_events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
            && !self.to_host.is_empty()
        {
            bail!("ProxyCommand stdout closed while writing");
        }
        Ok(())
    }

    fn finish_input(&mut self) -> Result<()> {
        if self.input_state == InputState::Draining && self.to_guest.is_empty() {
            self.stream
                .shutdown(std::net::Shutdown::Write)
                .context("Failed to close Firecracker vsock write side")?;
            self.input_state = InputState::ShutDown;
        }
        Ok(())
    }
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

    #[test]
    fn nonblocking_guard_restores_only_the_transition_it_made() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        stream.set_nonblocking(false).unwrap();
        {
            let _guard = NonblockingFd::new(stream.as_fd(), "test stream").unwrap();
            assert_ne!(descriptor_flags(&stream) & libc::O_NONBLOCK, 0);
        }
        assert_eq!(descriptor_flags(&stream) & libc::O_NONBLOCK, 0);

        stream.set_nonblocking(true).unwrap();
        {
            let _guard = NonblockingFd::new(stream.as_fd(), "test stream").unwrap();
            stream.set_nonblocking(false).unwrap();
        }
        assert_eq!(descriptor_flags(&stream) & libc::O_NONBLOCK, 0);
    }

    fn descriptor_flags(stream: &UnixStream) -> libc::c_int {
        // SAFETY: the stream owns the descriptor throughout this call.
        unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) }
    }

    #[test]
    fn pending_bytes_preserves_order_and_reports_backpressure() {
        let mut pending = PendingBytes::new();
        assert!(pending.is_empty());
        assert_eq!(pending.remaining(), BUFFER_LEN);

        pending.append(&[1, 2, 3]);
        assert_eq!(pending.as_slice(), &[1, 2, 3]);
        assert_eq!(pending.remaining(), BUFFER_LEN - 3);
        pending.consume(2);
        assert_eq!(pending.as_slice(), &[3]);
        assert!(!pending.is_empty());
        pending.consume(1);
        assert!(pending.is_empty());
        assert_eq!(pending.remaining(), BUFFER_LEN);

        pending.append(&vec![7; BUFFER_LEN]);
        assert_eq!(pending.remaining(), 0);
        pending.consume(BUFFER_LEN);
        assert!(pending.is_empty());
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
    fn response_parser_accepts_maximum_u32_and_rejects_overflow() {
        assert_eq!(
            parse_response_port(b"OK 4294967295\n"),
            NonZeroU32::new(u32::MAX)
        );
        assert_eq!(parse_response_port(b"OK 4294967296\n"), None);
    }

    #[test]
    fn connect_preserves_guest_bytes_after_acknowledgement() {
        let (_directory, path, server) = fixture(b"OK 1073741824\nSSH-2.0-fixture\r\n".to_vec());
        let mut stream =
            connect(&path, NonZeroU32::new(22).unwrap(), Duration::from_secs(1)).unwrap();
        let mut banner = [0_u8; b"SSH-2.0-fixture\r\n".len()];
        stream.read_exact(&mut banner).unwrap();
        assert_eq!(&banner, b"SSH-2.0-fixture\r\n");
        assert_eq!(server.join().unwrap(), b"CONNECT 22\n");
    }

    #[test]
    fn connect_rejects_malformed_response() {
        for response in [
            b"NO 1\n".to_vec(),
            b"OK 0\n".to_vec(),
            b"OK one\n".to_vec(),
            b"OK +1\n".to_vec(),
            b"OK 00000000001\n".to_vec(),
        ] {
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
