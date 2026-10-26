//! On-demand browser editor orchestration. Reuses the managed SSH boundary,
//! private storage, backend probes and operation locks. OpenSSH forwards the
//! browser port directly; code-server handles authentication in the guest.

use std::ffi::OsStr;
use std::io::{BufRead as _, IsTerminal as _, Read as _, Write as _};
use std::net::TcpListener;
use std::num::NonZeroU16;
use std::os::fd::AsRawFd as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::backend::{SshTarget, VmBackend};
use crate::config::{CoopConfig, Instance, InstanceName};
use crate::fs_util::{PrivateDir, lock_sibling_bounded};
use crate::remote_command::RemoteCommand;

#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Control {
    Status,
    Stop,
}

#[derive(Serialize, Deserialize)]
struct Status {
    session: String,
    port: NonZeroU16,
    ready: bool,
}

#[derive(Serialize, Deserialize)]
struct Session {
    id: String,
    directory: PathBuf,
}

impl Session {
    fn parse(text: &str) -> Result<Self> {
        let session: Self =
            serde_json::from_str(text).context("Invalid editor session metadata")?;
        ensure!(
            session.id.len() == 32
                && session
                    .id
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
            "Invalid editor session ID: expected 32 lowercase hexadecimal characters"
        );
        Ok(session)
    }

    fn url(&self, port: NonZeroU16) -> String {
        format!("http://{}.localhost:{port}/?folder=/workspace", self.id)
    }
}

fn metadata_path(inst: &Instance) -> PathBuf {
    inst.dir.join("web-editor.json")
}
fn lock_path(inst: &Instance) -> PathBuf {
    inst.dir.join("web-editor-session")
}
fn password_path(inst: &Instance) -> PathBuf {
    inst.dir.join("web-editor-password")
}

/// Fresh OS randomness for editor passwords and session identities.
fn random_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).context("Cannot obtain host session randomness")?;
    Ok(hex::encode(bytes))
}

fn control(session: &Session, command: &Control) -> Result<Option<Status>> {
    let dir = PrivateDir::open_existing(&session.directory)?;
    let stat = dir.entry_stat(OsStr::new("control.sock"))?;
    ensure!(
        stat.st_mode & libc::S_IFMT == libc::S_IFSOCK,
        "Invalid editor control socket"
    );
    // The private parent excludes other users; the host owner is trusted.
    let mut stream = UnixStream::connect(session.directory.join("control.sock"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    serde_json::to_writer(&mut stream, command)?;
    stream.write_all(b"\n")?;
    if matches!(command, Control::Stop) {
        // Keep the peer alive until the owner accepts the request. On macOS,
        // setting socket timeouts after the peer closes can fail with EINVAL.
        let mut acknowledgement = [0];
        stream.read_exact(&mut acknowledgement)?;
        ensure!(
            acknowledgement == *b"\n",
            "Invalid editor stop acknowledgement"
        );
        return Ok(None);
    }
    let mut line = String::new();
    std::io::BufReader::new(stream.take(4096)).read_line(&mut line)?;
    let status: Status = serde_json::from_str(&line)?;
    ensure!(
        status.session == session.id,
        "Editor session identity mismatch"
    );
    Ok(Some(status))
}

fn load(inst: &Instance) -> Result<Option<Session>> {
    crate::fs_util::read_optional_private(&metadata_path(inst))?
        .map(|text| Session::parse(&text))
        .transpose()
}

/// Called while holding the instance operation lock, before VM/disk mutation.
pub(crate) fn stop(inst: &Instance) -> Result<()> {
    let session = load(inst)?;
    if let Some(session) = &session {
        // A failed connection may be stale state. Wait for any live owner;
        // never signal a PID read from disk. Abrupt-owner death is distinct
        // from successful cleanup: an orphan SSH may still be alive.
        let _ = control(session, &Control::Stop);
    }
    let _owner = lock_sibling_bounded(&lock_path(inst), Duration::from_secs(15))?;
    if let Some(session) = &session {
        clean_stale_directory(session)?;
    }
    crate::fs_util::remove_private_if_exists(&metadata_path(inst))?;
    crate::fs_util::remove_private_if_exists(&password_path(inst))?;
    Ok(())
}

/// Only remove our fixed socket entries, never recursively delete saved paths.
/// The caller holds the session lock, so no foreground owner still uses them.
fn clean_stale_directory(session: &Session) -> Result<()> {
    let path = &session.directory;
    ensure!(
        path.parent() == Some(std::fs::canonicalize("/tmp")?.as_path())
            && path
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|n| n.starts_with("coop-we-")),
        "Invalid editor session directory"
    );
    let Some(dir) = missing_is_none(PrivateDir::open_existing(path))? else {
        return Ok(());
    };
    let name = OsStr::new("control.sock");
    if let Some(stat) = missing_is_none(dir.entry_stat(name))? {
        ensure!(
            stat.st_mode & libc::S_IFMT == libc::S_IFSOCK,
            "Invalid stale editor socket"
        );
        dir.unlink_non_directory(name)?;
    }
    std::fs::remove_dir(path)?;
    Ok(())
}

fn missing_is_none<T>(result: Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn run(
    be: &impl VmBackend,
    cfg: &CoopConfig,
    name: Option<&InstanceName>,
    port: Option<NonZeroU16>,
) -> Result<()> {
    let inst = cfg.resolve_instance(name)?;
    let operation = crate::backend::lock_instance_operation(&inst)?;
    let previous = load(&inst)?;
    if let Some(session) = &previous
        && control(session, &Control::Status).is_ok()
    {
        drop(operation);
        return wait_attached(&inst, session);
    }
    let _owner = lock_sibling_bounded(&lock_path(&inst), Duration::from_secs(1))?;
    if let Some(session) = &previous {
        clean_stale_directory(session)?;
        crate::fs_util::remove_private_if_exists(&metadata_path(&inst))?;
        crate::fs_util::remove_private_if_exists(&password_path(&inst))?;
    }
    let running = be.as_running(cfg, inst.clone())?.with_context(|| {
        format!(
            "VM '{}' is stopped; run coop start {} first",
            inst.name, inst.name
        )
    })?;
    // OpenSSH owns the listener. A probe/bind collision is fatal through
    // ExitOnForwardFailure, before the remote command can report ready.
    let port = match port {
        Some(port) => port,
        None => NonZeroU16::new(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
            .context("Host assigned an invalid editor port")?,
    };
    let directory = tempfile::Builder::new()
        .prefix("coop-we-")
        .tempdir_in("/tmp")?;
    PrivateDir::open_existing(directory.path())?;
    let session = Session {
        id: random_token()?[..32].to_owned(),
        directory: directory.path().canonicalize()?,
    };
    let password = random_token()?;
    let listener = UnixListener::bind(session.directory.join("control.sock"))?;
    listener.set_nonblocking(true)?;
    let _signals = crate::signal::install_handlers();
    let mut command = ssh_launch(running.target(), port, &session.id)?;
    command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // OpenSSH closes inherited nonstandard descriptors. The session lock is
    // held by this owner only; each retry gets independent guest socket paths.
    let mut child = OwnedChild(command.spawn()?);
    let mut input = child.0.stdin.take().context("Missing SSH stdin")?;
    let output = child.0.stdout.take().context("Missing SSH stdout")?;
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut output = output;
        let mut marker = [0; 6];
        let ready = output.read_exact(&mut marker).is_ok() && marker == *b"ready\n";
        let _ = tx.send(ready);
    });
    let result = (|| {
        nonblocking_stdin(&input)?;
        writeln!(input, "{password}")?;
        crate::fs_util::atomic_write_with_mode(&password_path(&inst), &password, 0o600)?;
        crate::fs_util::atomic_write_json(
            &metadata_path(&inst),
            &serde_json::to_string(&session)?,
        )?;
        drop(operation);
        serve_session(
            &inst,
            &session,
            port,
            &password,
            &listener,
            &mut child.0,
            &mut input,
            &rx,
        )
    })();
    drop(input);
    child.finish();
    crate::fs_util::remove_private_if_exists(&metadata_path(&inst))?;
    crate::fs_util::remove_private_if_exists(&password_path(&inst))?;
    drop(directory);
    result
}

/// An attached terminal has the same foreground/stop contract as the owner.
fn wait_attached(inst: &Instance, session: &Session) -> Result<()> {
    let _signals = crate::signal::install_handlers();
    let mut shown = false;
    loop {
        if crate::signal::shutdown_requested() {
            // Do not stop a replacement session if this owner has already left.
            let _ = control(session, &Control::Stop);
            let _owner = lock_sibling_bounded(&lock_path(inst), Duration::from_secs(15))?;
            return Ok(());
        }
        let Ok(Some(status)) = control(session, &Control::Status) else {
            return Ok(());
        };
        if status.ready && !shown {
            let password = crate::fs_util::read_optional_private(&password_path(inst))?
                .context("Missing editor session password")?;
            show(inst, &session.url(status.port), &password);
            shown = true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "session owner keeps its resources scoped together"
)]
fn serve_session(
    inst: &Instance,
    session: &Session,
    port: NonZeroU16,
    password: &str,
    listener: &UnixListener,
    child: &mut Child,
    input: &mut ChildStdin,
    events: &mpsc::Receiver<bool>,
) -> Result<()> {
    let started = std::time::Instant::now();
    let mut heartbeat = started;
    let mut ready = false;
    tracing::info!(
        "Preparing browser editor in '{}' (first use installs code-server)",
        inst.name
    );
    loop {
        if crate::signal::shutdown_requested() {
            return Ok(());
        }
        if child.try_wait()?.is_some() {
            bail!(
                "Editor SSH session exited unexpectedly; check the requested port, VM and guest editor.log"
            );
        }
        if !ready {
            match events.try_recv() {
                Ok(true) => {
                    ready = true;
                    show(inst, &session.url(port), password);
                }
                Ok(false) | Err(mpsc::TryRecvError::Disconnected) => bail!(
                    "Editor startup failed; check the requested port, SSH connectivity, and guest editor.log"
                ),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            ensure!(
                started.elapsed() < Duration::from_secs(600),
                "Guest editor startup timed out"
            );
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let status = Status {
                    session: session.id.clone(),
                    port,
                    ready,
                };
                match control_request(stream, &status) {
                    Ok(true) => return Ok(()),
                    Ok(false) => {}
                    Err(error) => tracing::debug!(%error, "Editor control request failed"),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        if heartbeat.elapsed() >= Duration::from_secs(2) {
            input
                .write_all(b"alive\n")
                .context("Editor SSH lease failed")?;
            heartbeat = std::time::Instant::now();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn control_request(mut stream: UnixStream, status: &Status) -> Result<bool> {
    // macOS inherits the listener's nonblocking flag on accepted sockets.
    // The request may arrive after accept; use the bounded read timeout below.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    stream.set_write_timeout(Some(Duration::from_millis(200)))?;
    let mut line = String::new();
    std::io::BufReader::new((&stream).take(4096)).read_line(&mut line)?;
    match serde_json::from_str::<Control>(&line)? {
        Control::Stop => {
            // A disconnected requester does not undo an already parsed stop.
            let _ = stream.write_all(b"\n");
            Ok(true)
        }
        Control::Status => {
            serde_json::to_writer(&mut stream, status)?;
            stream.write_all(b"\n")?;
            Ok(false)
        }
    }
}

fn nonblocking_stdin(input: &ChildStdin) -> Result<()> {
    // SAFETY: input owns a live pipe descriptor. Only its status flags change.
    let flags = unsafe { libc::fcntl(input.as_raw_fd(), libc::F_GETFL) };
    ensure!(flags >= 0, "Cannot read SSH pipe flags");
    // SAFETY: same live descriptor, preserving existing flags.
    ensure!(
        unsafe { libc::fcntl(input.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
        "Cannot make SSH lease writes nonblocking"
    );
    Ok(())
}

fn ssh_launch(target: &SshTarget, port: NonZeroU16, session: &str) -> Result<Command> {
    let guest_dir = format!("/tmp/coop-we-{}", &session[..24]);
    let guest_socket = format!("{guest_dir}/editor.sock");
    let script = format!(
        "umask 077\nmkdir -- {guest_dir}\nsocket={guest_socket}\nsession_socket={guest_dir}/session.sock\n{}\n",
        include_str!("../scripts/guest/web-editor.sh"),
    );
    let remote = RemoteCommand::new()
        .literal("exec bash -c ")
        .arg(script)
        .into_string();
    let mut command = target.ssh_forward_command()?;
    command
        .args(["-T", "-o", "ExitOnForwardFailure=yes", "-L"])
        .arg(format!("127.0.0.1:{port}:{guest_socket}"))
        .arg(target.addr())
        .arg(remote);
    Ok(command)
}

#[expect(
    clippy::print_stderr,
    reason = "intentional interactive sign-in delivery"
)]
fn show(inst: &Instance, url: &str, password: &str) {
    if std::io::stderr().is_terminal() && std::io::stdin().is_terminal() {
        eprintln!(
            "Open: {url}\nPassword: {password}\nEnter this session password on code-server's login page. Do not share it.\nPress Ctrl+C to close the editor. The VM keeps running."
        );
    } else {
        tracing::info!("Open: {url}");
        tracing::info!("Password file: {}", password_path(inst).display());
    }
}

struct OwnedChild(Child);
impl OwnedChild {
    fn finish(&mut self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if self.0.try_wait().is_ok_and(|status| status.is_some()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use std::num::NonZeroU16;

    use crate::web_editor::{Session, clean_stale_directory};

    #[test]
    fn editor_tokens_are_64_hex_characters_and_fresh() {
        let first = super::random_token().unwrap();
        let second = super::random_token().unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn loaded_session_ids_are_32_lowercase_hex_characters() {
        use crate::config::{ImageName, Instance, InstanceIndex, InstanceName};

        let directory = tempfile::tempdir().unwrap();
        let inst = Instance {
            name: InstanceName::new("test").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: directory.path().into(),
            image: ImageName::new("default").unwrap(),
        };
        assert!(super::load(&inst).unwrap().is_none());
        for id in [
            "0".repeat(32),
            "f".repeat(32),
            "0123456789abcdef".repeat(2),
            super::random_token().unwrap()[..32].to_owned(),
        ] {
            let json = serde_json::json!({"id": id, "directory": "/unused"});
            crate::fs_util::atomic_write_json(&super::metadata_path(&inst), &json.to_string())
                .unwrap();
            let session = super::load(&inst).unwrap().unwrap();
            assert_eq!(session.id, id);
            assert_eq!(session.directory, std::path::Path::new("/unused"));
        }
        for id in [
            String::new(),
            "a".repeat(31),
            "a".repeat(33),
            "A".repeat(32),
            format!("{}g", "a".repeat(31)),
            format!("{}/", "a".repeat(31)),
            format!("{}.", "a".repeat(31)),
            format!("{}\n", "a".repeat(31)),
            "é".repeat(16),
        ] {
            let json = serde_json::json!({"id": id, "directory": "/unused"});
            crate::fs_util::atomic_write_json(&super::metadata_path(&inst), &json.to_string())
                .unwrap();
            let error = super::load(&inst).err().unwrap();
            assert!(error.to_string().starts_with("Invalid editor session ID:"));
        }
    }

    #[test]
    fn missing_entries_are_distinct_from_other_failures() {
        use std::io::{Error, ErrorKind};
        assert_eq!(super::missing_is_none(Ok(7)).unwrap(), Some(7));
        assert_eq!(
            super::missing_is_none::<()>(Err(Error::from(ErrorKind::NotFound).into())).unwrap(),
            None
        );
        assert!(
            super::missing_is_none::<()>(Err(Error::from(ErrorKind::PermissionDenied).into()))
                .is_err()
        );
        assert!(super::missing_is_none::<()>(Err(anyhow::anyhow!("invalid entry"))).is_err());
    }

    #[test]
    fn control_waits_for_a_request_on_an_accepted_nonblocking_socket() {
        use std::io::Write as _;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;
        let (mut client, server) = UnixStream::pair().unwrap();
        // Model macOS accept behavior on every platform.
        server.set_nonblocking(true).unwrap();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            client.write_all(b"{\"action\":\"stop\"}\n").unwrap();
        });
        let status = super::Status {
            session: "test".into(),
            port: NonZeroU16::new(1234).unwrap(),
            ready: true,
        };
        assert!(super::control_request(server, &status).unwrap());
        writer.join().unwrap();
    }

    #[test]
    fn stop_keeps_the_control_connection_until_acknowledged() {
        use std::os::unix::net::UnixListener;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::Duration;
        let directory = tempfile::tempdir().unwrap();
        let session = Session {
            id: "test".into(),
            directory: directory.path().into(),
        };
        let listener = UnixListener::bind(directory.path().join("control.sock")).unwrap();
        let accepted = Arc::new(AtomicBool::new(false));
        let witnessed = Arc::clone(&accepted);
        let owner = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(30));
            witnessed.store(true, Ordering::SeqCst);
            let status = super::Status {
                session: "test".into(),
                port: NonZeroU16::new(1234).unwrap(),
                ready: true,
            };
            assert!(super::control_request(stream, &status).unwrap());
        });
        assert!(
            super::control(&session, &super::Control::Stop)
                .unwrap()
                .is_none()
        );
        assert!(accepted.load(Ordering::SeqCst));
        owner.join().unwrap();
    }

    #[test]
    fn session_files_are_instance_local() {
        use crate::config::{ImageName, Instance, InstanceIndex, InstanceName};
        let inst = Instance {
            name: InstanceName::new("test").unwrap(),
            index: InstanceIndex::new(0).unwrap(),
            dir: "/private/instance".into(),
            image: ImageName::new("default").unwrap(),
        };
        assert_eq!(
            super::metadata_path(&inst),
            inst.dir.join("web-editor.json")
        );
        assert_eq!(
            super::password_path(&inst),
            inst.dir.join("web-editor-password")
        );
        assert_eq!(super::lock_path(&inst), inst.dir.join("web-editor-session"));
    }

    #[test]
    fn ssh_binds_loopback_directly_to_the_guest_socket() {
        use crate::backend::{Hostname, SshTarget, SshUser};
        let target = SshTarget {
            host: Hostname::new("127.0.0.1").unwrap(),
            port: std::num::NonZeroU16::new(2222).unwrap(),
            user: SshUser::new("ubuntu").unwrap(),
            key_path: "/private/vm_key".into(),
            host_tools: crate::config::SshHostToolsConfig::default(),
        };
        let command =
            super::ssh_launch(&target, NonZeroU16::new(45678).unwrap(), &"a".repeat(32)).unwrap();
        let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        assert!(args.windows(2).any(|a| a
            == [
                "-L",
                "127.0.0.1:45678:/tmp/coop-we-aaaaaaaaaaaaaaaaaaaaaaaa/editor.sock"
            ]));
        assert!(
            args.windows(2)
                .any(|a| a == ["-o", "ExitOnForwardFailure=yes"])
        );
        assert!(args.windows(2).any(|a| a == ["-F", "none"]));
        assert_eq!(command.get_current_dir(), Some(std::path::Path::new("/")));
    }

    #[test]
    fn stale_cleanup_only_removes_owned_socket_entries() {
        let temp = tempfile::Builder::new()
            .prefix("coop-we-")
            .tempdir_in("/tmp")
            .unwrap();
        let session = Session {
            id: "test".into(),
            directory: temp.path().canonicalize().unwrap(),
        };
        let socket =
            std::os::unix::net::UnixListener::bind(session.directory.join("control.sock")).unwrap();
        clean_stale_directory(&session).unwrap();
        assert!(!session.directory.exists());
        clean_stale_directory(&session).unwrap();
        drop(socket);

        let empty = tempfile::Builder::new()
            .prefix("coop-we-")
            .tempdir_in("/tmp")
            .unwrap();
        let empty_session = Session {
            id: "empty".into(),
            directory: empty.path().canonicalize().unwrap(),
        };
        clean_stale_directory(&empty_session).unwrap();
        assert!(!empty_session.directory.exists());

        let temp = tempfile::Builder::new()
            .prefix("coop-we-")
            .tempdir_in("/tmp")
            .unwrap();
        let session = Session {
            id: "test".into(),
            directory: temp.path().canonicalize().unwrap(),
        };
        std::fs::write(session.directory.join("control.sock"), "not a socket").unwrap();
        assert!(clean_stale_directory(&session).is_err());
        assert_eq!(
            std::fs::read_to_string(session.directory.join("control.sock")).unwrap(),
            "not a socket"
        );
        let unrelated = Session {
            id: "test".into(),
            directory: std::path::PathBuf::from("/tmp"),
        };
        assert!(clean_stale_directory(&unrelated).is_err());
        // An invalid existing parent is not the same as an absent session.
        let invalid = tempfile::Builder::new()
            .prefix("coop-we-")
            .tempfile_in("/tmp")
            .unwrap();
        let invalid_session = Session {
            id: "test".into(),
            directory: invalid.path().canonicalize().unwrap(),
        };
        assert!(clean_stale_directory(&invalid_session).is_err());
        assert!(invalid.path().is_file());
    }

    #[test]
    fn browser_url_is_host_defined_and_session_bound() {
        for id in ["a".repeat(32), "b".repeat(32)] {
            let session = Session {
                id: id.clone(),
                directory: "/unused".into(),
            };
            for port in [1, 80, 1234, u16::MAX] {
                assert_eq!(
                    session.url(NonZeroU16::new(port).unwrap()),
                    format!("http://{id}.localhost:{port}/?folder=/workspace")
                );
            }
        }
    }

    #[test]
    fn status_requires_a_nonzero_u16_port() {
        for port in [1, 80, 1234, u16::MAX] {
            let json = serde_json::json!({ "session": "abc", "port": port, "ready": true });
            let status: super::Status = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(status.port.get(), port);
            assert_eq!(serde_json::to_value(status).unwrap(), json);
        }
        for port in ["0", "-1", "65536", "1.5", "null", "\"80\""] {
            let json = format!(r#"{{"session":"abc","port":{port},"ready":true}}"#);
            assert!(
                serde_json::from_str::<super::Status>(&json).is_err(),
                "{json}"
            );
        }
        assert!(
            serde_json::from_str::<super::Status>(r#"{"session":"abc","ready":true}"#).is_err()
        );
    }

    #[test]
    fn control_status_is_session_bound() {
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let session = Session {
            id: "a".repeat(32),
            directory: directory.path().into(),
        };
        let listener = UnixListener::bind(directory.path().join("control.sock")).unwrap();
        for id in [session.id.clone(), "b".repeat(32)] {
            let expected_match = id == session.id;
            let result = std::thread::scope(|scope| {
                let owner = scope.spawn(|| {
                    let (stream, _) = listener.accept().unwrap();
                    let status = super::Status {
                        session: id,
                        port: NonZeroU16::new(80).unwrap(),
                        ready: true,
                    };
                    assert!(!super::control_request(stream, &status).unwrap());
                });
                let result = super::control(&session, &super::Control::Status);
                owner.join().unwrap();
                result
            });
            if expected_match {
                let status = result.unwrap().unwrap();
                assert_eq!(status.port.get(), 80);
                assert!(status.ready);
            } else {
                assert_eq!(
                    result.err().unwrap().to_string(),
                    "Editor session identity mismatch"
                );
            }
        }
    }
}
