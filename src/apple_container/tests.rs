//! Backend tests against a scripted runtime and builder.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use super::cli::{Exec, Output, Request};
use super::*;
use crate::config::{ConfigPath, ImageName, InstanceIndex, InstanceName};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/coop-sandbox");
const FIXTURE_ID: &str = "coop-0a1b2c3d-00112233445566ff";
const FIXTURE_OWNER: &str = "0a1b2c3d00112233445566778899aabb";
const FIXTURE_ROOT: &str = "/Users/me/.coop-apple/backends/apple-container-v1/runtime";
const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINiqkOnkRV06x+SuorkF+O3KdBTVFznIV0+b58cidW1N root@guest\n";

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{FIXTURES}/{name}")).unwrap()
}

type Responder = Box<dyn Fn(&[String]) -> Output>;
type Calls = Rc<RefCell<Vec<Vec<String>>>>;

/// Records every argument vector and answers from a shared responder.
#[derive(Clone)]
struct FakeExec {
    calls: Calls,
    respond: Rc<Responder>,
}

impl Exec for FakeExec {
    fn run(&self, req: &Request) -> Result<Output> {
        self.calls.borrow_mut().push(req.args.clone());
        Ok((self.respond)(&req.args))
    }

    fn run_logged(&self, req: &Request, _log: &Path) -> Result<Output> {
        self.run(req)
    }

    fn run_streaming(
        &self,
        args: &[String],
        on_line: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<Output> {
        self.calls.borrow_mut().push(args.to_vec());
        let out = (self.respond)(args);
        for line in out.stdout.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            on_line(line)?;
        }
        Ok(out)
    }
}

fn ok(stdout: &str) -> Output {
    Output {
        code: Some(0),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

fn fail(stderr: &str) -> Output {
    Output {
        code: Some(1),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

/// The value after `name` in `args`, or "".
fn flag(args: &[String], name: &str) -> String {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].clone())
        .unwrap_or_default()
}

fn starts(args: &[String], prefix: &[&str]) -> bool {
    args.len() >= prefix.len() && args.iter().zip(prefix).all(|(a, p)| a == p)
}

/// The runtime's `version`, qualified or not.
fn version(args: &[String], qualified: bool) -> Option<Output> {
    starts(args, &["version"]).then(|| {
        let v = fixture("version.json");
        ok(&if qualified {
            v
        } else {
            v.replace("\"protocol\" : 2", "\"protocol\" : 99")
        })
    })
}

/// Runtime and builder backed by one responder (they are told apart by
/// their argument vectors).
fn backend(cfg: &CoopConfig, respond: Responder) -> (AppleContainerBackend, Calls) {
    let calls: Calls = Rc::new(RefCell::new(Vec::new()));
    let exec = FakeExec {
        calls: Rc::clone(&calls),
        respond: Rc::new(respond),
    };
    (
        AppleContainerBackend::with_exec(cfg, Box::new(exec.clone()), Box::new(exec)),
        calls,
    )
}

/// Short deadlines: nothing in these tests really boots.
fn test_cfg(dir: &Path) -> CoopConfig {
    let mut cfg = CoopConfig {
        data_dir: ConfigPath::new(dir),
        ..CoopConfig::default()
    };
    cfg.apple_container.boot_timeout_seconds = crate::config::TimeoutSecs::new(2).unwrap();
    cfg
}

fn test_inst(cfg: &CoopConfig) -> Instance {
    let dir = cfg.instances_dir().join("t");
    std::fs::create_dir_all(&dir).unwrap();
    Instance {
        name: InstanceName::new("t").unwrap(),
        index: InstanceIndex::new(0).unwrap(),
        dir,
        image: ImageName::new("default").unwrap(),
    }
}

fn sandbox_name(owner: &Owner) -> MachineName {
    MachineName::new(format!("coop-{}-00112233445566ff", owner.id.short())).unwrap()
}

fn write_sidecar(inst: &Instance, owner: &Owner) -> MachineSidecar {
    let sidecar = MachineSidecar {
        schema_version: state::SCHEMA_VERSION,
        backend: state::BACKEND_TAG.into(),
        owner_id: owner.id.clone(),
        machine_id: sandbox_name(owner),
        image_ref: "local/coop-exp:fx".into(),
        image_digest: "sha256:3c8ada4041838a362f1d3c0805e487beff8ef85383044efeb07844cdd7c1e0b3"
            .into(),
        image_manifest_id: "m".into(),
        guest_user: crate::guest::GuestUser::default(),
        requested_cpus: 2,
        requested_memory_bytes: 2048 * 1024 * 1024,
        host_key_fingerprint: "SHA256:x".into(),
        last_observed_owner_pid: None,
        last_observed_ip: None,
        reenroll_host_key: false,
        created_at: "now".into(),
        runtime_identity: "test".into(),
    };
    sidecar.save(inst).unwrap();
    sidecar
}

/// A real inspect record rewritten for this test's owner, sandbox, and
/// runtime root, in `status`.
fn inspect_json(cfg: &CoopConfig, owner: &Owner, status: &str) -> String {
    let base = if status == "running" {
        fixture("inspect-running.json")
    } else {
        fixture("inspect-stopped.json")
    };
    let root = canonical_path(&cfg.state_root().join(RUNTIME_DIR));
    base.replace(FIXTURE_ID, sandbox_name(owner).as_str())
        .replace(FIXTURE_OWNER, owner.id.as_str())
        .replace(FIXTURE_ROOT, &root.display().to_string())
        .replace(
            "\"status\" : \"stopped\"",
            &format!("\"status\" : \"{status}\""),
        )
}

fn with_generation(json: &str, generation: u64) -> String {
    json.replace(
        "\"diskGeneration\" : 0",
        &format!("\"diskGeneration\" : {generation}"),
    )
}

/// Commands that change runtime state. None may run before the gate passes.
fn is_mutating(args: &[String]) -> bool {
    [
        &["create"][..],
        &["start"],
        &["stop"],
        &["exec"],
        &["set"],
        &["grow"],
        &["commit"],
        &["restore"],
        &["delete"],
        &["init"],
        &["image", "import"],
        &["image", "delete"],
        &["image", "save"],
        &["maintenance", "install"],
        &["disk", "delete"],
        &["build"],
        &["system", "start"],
        &["system", "stop"],
    ]
    .iter()
    .any(|p| starts(args, p))
}

fn mutations(calls: &Calls) -> Vec<String> {
    calls
        .borrow()
        .iter()
        .filter(|c| is_mutating(c))
        .map(|c| c.first().cloned().unwrap_or_default())
        .collect()
}

const CREATE: JournalOp = JournalOp::Create {
    stage: CreateStage::Reserved,
};

fn kind(err: &anyhow::Error) -> &AppleError {
    err.downcast_ref::<AppleError>().unwrap()
}

#[test]
fn unqualified_runtime_refuses_create_before_any_side_effect() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    let (be, calls) = backend(
        &cfg,
        Box::new(|args| version(args, false).unwrap_or_else(|| ok("[]"))),
    );
    let err = be.create_and_start(&cfg, &inst, None, &[]).unwrap_err();
    assert!(
        matches!(kind(&err), AppleError::RuntimeUnqualified(_)),
        "{err:#}"
    );
    assert!(mutations(&calls).is_empty(), "{:?}", calls.borrow());
    assert!(!MachineSidecar::path(&inst).exists());
    assert!(Journal::try_load(&inst).unwrap().is_none());
}

#[test]
fn unqualified_runtime_refuses_ssh_target_and_start() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    let json = inspect_json(&cfg, &owner, "running");
    let (be, calls) = backend(
        &cfg,
        Box::new(move |args| version(args, false).unwrap_or_else(|| ok(&json))),
    );
    for err in [
        be.ssh_target(&cfg, &inst).unwrap_err(),
        be.start_existing(&cfg, &inst).unwrap_err(),
    ] {
        assert!(
            matches!(kind(&err), AppleError::RuntimeUnqualified(_)),
            "{err:#}"
        );
    }
    assert!(mutations(&calls).is_empty());
}

#[test]
fn recovery_hint_names_the_command_for_each_operation() {
    let name = InstanceName::new("vm1").unwrap();
    let resources = Resources {
        cpus: 1,
        memory_bytes: 1,
    };
    let set = JournalOp::SetResources {
        operation: op("coop-a"),
        prior: resources,
    };
    let restore = JournalOp::RestoreDisk {
        operation: op("coop-r"),
        prior_generation: 0,
    };
    assert!(set.recovery_hint(&name).contains("`coop start vm1`"));
    assert!(restore.recovery_hint(&name).contains("`coop start vm1`"));
    let create = JournalOp::Create {
        stage: CreateStage::Reserved,
    }
    .recovery_hint(&name);
    assert!(
        create.contains("`coop destroy vm1`") && create.contains("`coop up`"),
        "{create}"
    );
    let destroy = JournalOp::Destroy {
        stage: DestroyStage::Reserved,
    }
    .recovery_hint(&name);
    assert!(
        destroy.contains("`coop destroy vm1`") && !destroy.contains("coop up"),
        "{destroy}"
    );
}

/// Listings report `unknown` (an error here) instead of `stopped` when
/// the state cannot be read, is transitional, or an operation is unfinished.
/// A crashed sandbox is definitively not running.
#[test]
fn probe_running_distinguishes_unknown_from_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);

    for (status, expected) in [
        ("running", Some(true)),
        ("stopped", Some(false)),
        ("booting", None),
        // Not running, and `start` accepts it.
        ("crashed", Some(false)),
    ] {
        let json = inspect_json(&cfg, &owner, status);
        let (be, _) = backend(
            &cfg,
            Box::new(move |args| version(args, true).unwrap_or_else(|| ok(&json))),
        );
        let probe = be.probe_running(&inst);
        assert_eq!(probe.as_ref().ok().copied(), expected, "{status}");
        if let Err(e) = probe {
            assert!(matches!(kind(&e), AppleError::OperationUncertain(_)));
        }
    }

    // No record yet: nothing exists to be running.
    let bare = Instance {
        name: InstanceName::new("fresh").unwrap(),
        dir: cfg.instances_dir().join("fresh"),
        ..inst.clone()
    };
    std::fs::create_dir_all(&bare.dir).unwrap();
    let (be, _) = backend(
        &cfg,
        Box::new(|args| version(args, true).unwrap_or_else(|| fail("unexpected"))),
    );
    assert_eq!(be.probe_running(&bare).ok(), Some(false));

    let (be, _) = backend(
        &cfg,
        Box::new(|args| version(args, true).unwrap_or_else(|| fail("owner unreachable"))),
    );
    assert!(be.probe_running(&inst).is_err());

    Journal::begin(&inst, &owner, CREATE, sandbox_name(&owner)).unwrap();
    let json = inspect_json(&cfg, &owner, "stopped");
    let (be, _) = backend(
        &cfg,
        Box::new(move |args| version(args, true).unwrap_or_else(|| ok(&json))),
    );
    let err = be.probe_running(&inst).unwrap_err();
    assert!(matches!(kind(&err), AppleError::OperationUncertain(_)));
    assert!(format!("{err:#}").contains("unfinished create"), "{err:#}");
}

#[test]
fn liveness_probe_errors_are_not_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);

    let (be, _) = backend(
        &cfg,
        Box::new(|args| version(args, true).unwrap_or_else(|| fail("interrupted"))),
    );
    assert!(be.as_stopped(inst.clone()).is_err());
    assert!(be.as_running(&cfg, inst.clone()).is_err());
    assert!(!be.is_running(&inst));

    for (status, stopped_ok) in [("booting", false), ("crashed", false), ("stopped", true)] {
        let json = inspect_json(&cfg, &owner, status);
        let (be, _) = backend(
            &cfg,
            Box::new(move |args| version(args, true).unwrap_or_else(|| ok(&json))),
        );
        assert_eq!(be.as_stopped(inst.clone()).is_ok(), stopped_ok, "{status}");
    }
}

#[test]
fn destroy_refuses_unowned_sandbox() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    let mut sidecar = write_sidecar(&inst, &owner);
    sidecar.machine_id = MachineName::new("users-own-sandbox").unwrap();
    sidecar.save(&inst).unwrap();
    let (be, calls) = backend(
        &cfg,
        Box::new(|args| version(args, true).unwrap_or_else(|| ok("[]"))),
    );
    let err = be.destroy_instance(&cfg, &inst).unwrap_err();
    assert!(matches!(kind(&err), AppleError::IdentityConflict(_)));
    assert!(mutations(&calls).is_empty());
    assert!(inst.dir.exists(), "metadata must survive a refused destroy");
}

/// The machine name's owner prefix is necessary but not sufficient: a
/// record for another installation whose id shares the 8-character prefix
/// is refused on the full owner id.
#[test]
fn destroy_checks_the_full_owner_id() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    let mut sidecar = write_sidecar(&inst, &owner);
    let twin = format!("{}{}", owner.id.short(), "f".repeat(24));
    assert_ne!(twin, owner.id.as_str());
    sidecar.owner_id = state::OwnerId::try_from(twin).unwrap();
    assert!(sidecar.machine_id.belongs_to(&owner.id));
    sidecar.save(&inst).unwrap();
    let (be, calls) = backend(
        &cfg,
        Box::new(|args| version(args, true).unwrap_or_else(|| ok("[]"))),
    );
    let err = be.destroy_instance(&cfg, &inst).unwrap_err();
    assert!(
        matches!(kind(&err), AppleError::IdentityConflict(_)),
        "{err:#}"
    );
    assert!(mutations(&calls).is_empty());
    assert!(inst.dir.exists(), "metadata must survive a refused destroy");
}

#[test]
fn destroy_reconciles_interrupted_create() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    let mut journal = Journal::begin(&inst, &owner, CREATE, sandbox_name(&owner)).unwrap();
    journal
        .advance(
            &inst,
            JournalOp::Create {
                stage: CreateStage::CreatingMachine,
            },
        )
        .unwrap();
    // The create never landed; an unrelated sandbox exists.
    let (be, calls) = backend(
        &cfg,
        Box::new(|args| {
            version(args, true).unwrap_or_else(|| {
                if starts(args, &["list"]) {
                    return ok(r#"[{"id":"someone-else","status":"running","owner":"x"}]"#);
                }
                if starts(args, &["reconcile"]) {
                    return ok("{}");
                }
                fail("unexpected")
            })
        }),
    );
    be.destroy_instance(&cfg, &inst).unwrap();
    assert!(mutations(&calls).is_empty());
    // The runtime sweeps what the interrupted create left inside it.
    assert!(calls.borrow().iter().any(|c| starts(c, &["reconcile"])));
    assert!(!inst.dir.exists());
}

#[test]
fn destroy_stops_then_deletes_owned_sandbox() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    let name = sandbox_name(&owner);
    let state = Rc::new(RefCell::new((false, false))); // (stopped, deleted)
    let (s, n) = (Rc::clone(&state), name.clone());
    let (running, stopped) = (
        inspect_json(&cfg, &owner, "running"),
        inspect_json(&cfg, &owner, "stopped"),
    );
    let owner_id = owner.id.as_str().to_string();
    let (be, calls) = backend(
        &cfg,
        Box::new(move |args| {
            if let Some(o) = version(args, true) {
                return o;
            }
            if starts(args, &["list"]) {
                return ok(&if s.borrow().1 {
                    "[]".into()
                } else {
                    format!(r#"[{{"id":"{n}","status":"running","owner":"{owner_id}"}}]"#)
                });
            }
            if starts(args, &["inspect"]) {
                return ok(if s.borrow().0 { &stopped } else { &running });
            }
            if starts(args, &["stop"]) {
                s.borrow_mut().0 = true;
                return ok("");
            }
            if starts(args, &["delete"]) {
                assert!(
                    args.windows(2)
                        .any(|w| w[0] == "--owner" && w[1] == owner_id)
                );
                s.borrow_mut().1 = true;
                return ok("");
            }
            fail("unexpected")
        }),
    );
    be.destroy_instance(&cfg, &inst).unwrap();
    assert_eq!(mutations(&calls), ["stop", "delete"]);
    assert!(!inst.dir.exists());
}

#[test]
fn unconfirmed_stop_is_uncertain_not_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let json = inspect_json(&cfg, &owner, "running");
    let (be, _) = backend(
        &cfg,
        Box::new(move |args| version(args, true).unwrap_or_else(|| ok(&json))),
    );
    let err = be
        .runtime()
        .unwrap()
        .stop_and_confirm(&sandbox_name(&owner))
        .unwrap_err();
    assert!(matches!(kind(&err), AppleError::OperationUncertain(_)));
}

#[test]
fn create_args_carry_exact_values_and_no_host_surfaces() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let (be, _) = backend(
        &cfg,
        Box::new(|args| version(args, true).unwrap_or_else(|| ok(""))),
    );
    let rt = be.runtime().unwrap();
    let name = sandbox_name(&owner);
    let args = create_args(
        rt,
        &name,
        &Source::Image("local/coop-0a1b2c3d:00"),
        NonZeroU8::new(4).unwrap(),
        4096,
        GiB::new(32).unwrap(),
        &owner,
    );
    let joined = args.join(" ");
    assert!(joined.starts_with("create --root /"), "{joined}");
    for want in [
        name.as_str(),
        "--image local/coop-0a1b2c3d:00",
        "--cpus 4",
        "--memory-mib 4096",
        "--disk-gib 32",
        &format!("--owner {}", owner.id.as_str()),
    ] {
        assert!(joined.contains(want), "{want}: {joined}");
    }
    for never in ["mount", "volume", "publish", "socket", "ssh", "network"] {
        assert!(!joined.contains(never), "{never}: {joined}");
    }
    let disk = MachineName::generate(&owner.id).unwrap();
    let joined = create_args(
        rt,
        &name,
        &Source::Disk(&disk),
        NonZeroU8::new(1).unwrap(),
        512,
        GiB::new(8).unwrap(),
        &owner,
    )
    .join(" ");
    assert!(joined.contains(&format!("--from-disk {disk}")) && !joined.contains("--image"));
    assert_eq!(mib_to_bytes(4096), 4 * 1024 * 1024 * 1024);
}

#[test]
fn guest_commands_are_argv_not_shell_text() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let (be, calls) = backend(
        &cfg,
        Box::new(|args| version(args, true).unwrap_or_else(|| ok(""))),
    );
    let rt = be.runtime().unwrap();
    let words = [
        "/usr/bin/printf",
        "%s\\n",
        "a b",
        "it's",
        "$(id)",
        "; true",
        "",
    ];
    rt.guest(
        &sandbox_name(&owner),
        &words,
        Duration::from_secs(5),
        MAX_TEXT_OUTPUT,
    )
    .unwrap();
    let call = calls.borrow().last().cloned().unwrap();
    let dashdash = call.iter().position(|a| a == "--").unwrap();
    assert_eq!(&call[dashdash + 1..], words);
}

#[test]
fn capabilities_include_disk_operations() {
    let be = AppleContainerBackend::new();
    let caps = be.capabilities();
    for cap in [
        Capability::DiskResize,
        Capability::DiskSnapshots,
        Capability::MachineResources,
    ] {
        assert!(caps.has(cap), "{cap:?}");
    }
    assert!(!caps.has(Capability::LiveMounts));
    assert!(!be.mounts_are_live());
    assert_eq!(
        be.local_endpoint_route(&NetworkConfig::default()),
        LocalEndpointRoute::ReverseTunnel
    );
}

#[test]
fn resolve_binary_rejects_relative_missing_and_writable() {
    assert!(resolve_binary(Some(Path::new("coop-sandbox")), &[], Tool::Runtime).is_err());
    assert!(
        resolve_binary(
            Some(Path::new("/nonexistent/coop-sandbox")),
            &[],
            Tool::Runtime
        )
        .is_err()
    );
    let tmp = tempfile::tempdir().unwrap();
    let writable = tmp.path().join("coop-sandbox");
    std::fs::write(&writable, "#!/bin/sh\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o777)).unwrap();
    }
    let err = resolve_binary(Some(&writable), &[], Tool::Runtime).unwrap_err();
    assert!(format!("{err:#}").contains("writable"), "{err:#}");
    // A configured path is the only candidate: defaults are not a fallback.
    let err = resolve_binary(
        Some(Path::new("/nonexistent/x")),
        &[writable],
        Tool::Runtime,
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("/nonexistent/x"), "{err:#}");
}

/// Only a regular, executable, user- or root-owned file that no group or
/// other user can write is accepted.
#[test]
fn check_binary_accepts_only_private_executables() {
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("tool");
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    let set = |mode| std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(mode));

    set(0o755).unwrap();
    assert_eq!(check_binary(&bin).unwrap(), bin.canonicalize().unwrap());
    for (mode, why) in [
        (0o644, "not an executable"),
        (0o775, "writable"),
        (0o757, "writable"),
    ] {
        set(mode).unwrap();
        let err = check_binary(&bin).unwrap_err();
        assert!(format!("{err:#}").contains(why), "{mode:o}: {err:#}");
    }
    let dir = tmp.path().join("dir");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(format!("{:#}", check_binary(&dir).unwrap_err()).contains("not an executable"));

    // Root-owned system binaries are trusted.
    assert!(check_binary(Path::new("/bin/sh")).is_ok());
}

/// A private binary inside a directory someone else can write is refused,
/// unless the directory is sticky.
#[test]
fn check_binary_refuses_a_writable_ancestor() {
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = tempfile::tempdir().unwrap();
    let parent = tmp.path().join("shared");
    let dir = parent.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("tool");
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let set = |mode| std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(mode));

    set(0o777).unwrap();
    let err = check_binary(&bin).unwrap_err();
    assert!(format!("{err:#}").contains("world-writable"), "{err:#}");
    set(0o1777).unwrap();
    assert!(check_binary(&bin).is_ok());
    set(0o755).unwrap();
    assert!(check_binary(&bin).is_ok());
}

#[test]
fn untrusted_dir_rules() {
    let me = 501;
    let staff = 20;
    for (mode, owner, group, want) in [
        (0o755, 0, 0, None),
        (0o755, me, staff, None),
        (0o755, 502, staff, Some("is owned by another user")),
        (0o1777, 502, 0, Some("is owned by another user")),
        (0o777, 0, 0, Some("is world-writable")),
        (0o1777, 0, 0, None),
        (0o775, me, staff, Some("is group-writable")),
        (0o1775, me, staff, None),
        // Homebrew: user-owned, `admin`-group-writable.
        (0o775, me, 80, None),
        (0o775, 0, 0, None),
    ] {
        assert_eq!(
            untrusted_dir(mode, owner, group, me),
            want,
            "{mode:o} {owner}:{group}"
        );
    }
}

#[test]
fn missing_binary_names_the_tool_and_how_to_get_it() {
    for (tool, name, hint) in [
        (Tool::Runtime, "`coop-sandbox`", "build-coop-sandbox.sh"),
        (Tool::Builder, "`container`", "[apple_container] builder"),
    ] {
        let err = resolve_binary(None, &[], tool).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains(name) && text.contains(hint), "{text}");
    }
    assert_eq!(AppleContainerBackend::new().to_string(), "apple-container");
}

fn op(id: &str) -> OperationId {
    OperationId::try_from(id.to_string()).unwrap()
}

/// `json` with `record.lastOperation` set.
fn with_last_operation(json: &str, op: &OperationId) -> String {
    let mut v: serde_json::Value = serde_json::from_str(json).unwrap();
    v["record"]["lastOperation"] = op.as_str().into();
    v.to_string()
}

/// An interrupted resource change (forward or rollback) is settled from
/// the runtime's record, whichever side of the runtime update or the
/// sidecar write it stopped on, without another runtime update.
#[test]
fn interrupted_resource_change_is_reconciled_from_runtime() {
    let prior = Resources {
        cpus: 8,
        memory_bytes: 1 << 30,
    };
    let target = Resources {
        cpus: 2,
        memory_bytes: 2048 * 1024 * 1024,
    };
    // (journal op id, runtime's last operation, sidecar already written)
    for (journaled, committed, sidecar_written) in [
        ("coop-a", Some("coop-a"), false),
        ("coop-a", Some("coop-a"), true),
        ("coop-a", None, false),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let owner = Owner::load_or_init(&cfg).unwrap();
        let inst = test_inst(&cfg);
        let mut sidecar = write_sidecar(&inst, &owner);
        let recorded = if sidecar_written { target } else { prior };
        sidecar.requested_cpus = recorded.cpus;
        sidecar.requested_memory_bytes = recorded.memory_bytes;
        sidecar.save(&inst).unwrap();
        Journal::begin(
            &inst,
            &owner,
            JournalOp::SetResources {
                operation: op(journaled),
                prior,
            },
            sandbox_name(&owner),
        )
        .unwrap();
        // The fixture reports 2 vCPUs / 2 GiB.
        let stopped = inspect_json(&cfg, &owner, "stopped");
        let json = committed.map_or(stopped.clone(), |c| with_last_operation(&stopped, &op(c)));
        let (be, calls) = backend(
            &cfg,
            Box::new(move |args| version(args, true).unwrap_or_else(|| ok(&json))),
        );
        let rt = be.runtime().unwrap();
        AppleContainerBackend::recover_journal(rt, &cfg, &inst).unwrap();
        assert!(Journal::try_load(&inst).unwrap().is_none());
        assert_eq!(MachineSidecar::load(&inst).unwrap().resources(), target);
        // Recovery is idempotent and never updates the runtime.
        AppleContainerBackend::recover_journal(rt, &cfg, &inst).unwrap();
        assert_eq!(MachineSidecar::load(&inst).unwrap().resources(), target);
        assert!(mutations(&calls).is_empty(), "{journaled} {committed:?}");
    }

    // A sandbox that is not confirmed stopped is not reconciled.
    for status in ["running", "booting", "crashed"] {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let owner = Owner::load_or_init(&cfg).unwrap();
        let inst = test_inst(&cfg);
        write_sidecar(&inst, &owner);
        Journal::begin(
            &inst,
            &owner,
            JournalOp::SetResources {
                operation: op("coop-a"),
                prior,
            },
            sandbox_name(&owner),
        )
        .unwrap();
        let json = inspect_json(&cfg, &owner, status);
        let (be, calls) = backend(
            &cfg,
            Box::new(move |args| version(args, true).unwrap_or_else(|| ok(&json))),
        );
        let err =
            AppleContainerBackend::recover_journal(be.runtime().unwrap(), &cfg, &inst).unwrap_err();
        assert!(
            matches!(kind(&err), AppleError::OperationUncertain(_)),
            "{status}: {err:#}"
        );
        assert!(Journal::try_load(&inst).unwrap().is_some(), "{status}");
        assert!(mutations(&calls).is_empty(), "{status}");
    }
}

/// Only coop's own restore, correlated by its operation id, lets the
/// next start pin a new host key; a higher generation alone does not.
#[test]
fn interrupted_restore_reenrolls_only_for_coops_own_replacement() {
    for (journaled, generation, committed, reenroll) in [
        ("coop-r", 4, Some("coop-r"), true),
        ("coop-r", 4, Some("coop-other"), false),
        ("coop-r", 4, None, false),
        ("coop-r", 3, Some("coop-r"), false),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let owner = Owner::load_or_init(&cfg).unwrap();
        let inst = test_inst(&cfg);
        write_sidecar(&inst, &owner);
        Journal::begin(
            &inst,
            &owner,
            JournalOp::RestoreDisk {
                operation: op(journaled),
                prior_generation: 3,
            },
            sandbox_name(&owner),
        )
        .unwrap();
        let stopped = with_generation(&inspect_json(&cfg, &owner, "stopped"), generation)
            .replace("local/coop-exp:fx", "local/restored:1");
        let json = committed.map_or(stopped.clone(), |c| with_last_operation(&stopped, &op(c)));
        let (be, _) = backend(
            &cfg,
            Box::new(move |args| version(args, true).unwrap_or_else(|| ok(&json))),
        );
        AppleContainerBackend::recover_journal(be.runtime().unwrap(), &cfg, &inst).unwrap();
        assert!(Journal::try_load(&inst).unwrap().is_none());
        let after = MachineSidecar::load(&inst).unwrap();
        assert_eq!(
            after.reenroll_host_key, reenroll,
            "{journaled} {generation} {committed:?}"
        );
        // The restored image identity is taken only with the restore.
        let want = if reenroll {
            "local/restored:1"
        } else {
            "local/coop-exp:fx"
        };
        assert_eq!(after.image_ref, want);
    }
}

#[test]
fn restore_journals_the_generation_and_marks_reenrollment() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    let image = ImageName::new("default").unwrap();
    ImageManifest {
        schema_version: state::SCHEMA_VERSION,
        backend: state::BACKEND_TAG.into(),
        image_ref: "local/coop-exp:fx".into(),
        digest: "sha256:3c8ada4041838a362f1d3c0805e487beff8ef85383044efeb07844cdd7c1e0b3".into(),
        disk: None,
        manifest_id: "m2".into(),
        base_image: image::BASE_IMAGE.into(),
        platform: image::PLATFORM.into(),
        guest_user: crate::guest::GuestUser::default(),
        created: "now".into(),
    }
    .save(&cfg, &image)
    .unwrap();
    // The operation id of the restore, once the runtime has run it.
    let restored: Rc<RefCell<Option<OperationId>>> = Rc::new(RefCell::new(None));
    let r = Rc::clone(&restored);
    let stopped = inspect_json(&cfg, &owner, "stopped");
    let (be, calls) = backend(
        &cfg,
        Box::new(move |args| {
            if let Some(o) = version(args, true) {
                return o;
            }
            if starts(args, &["image", "list"]) {
                return ok(
                    r#"[{"reference":"local/coop-exp:fx","digest":"sha256:3c8ada4041838a362f1d3c0805e487beff8ef85383044efeb07844cdd7c1e0b3"}]"#,
                );
            }
            if starts(args, &["inspect"]) {
                return ok(&match &*r.borrow() {
                    Some(op) => with_last_operation(&with_generation(&stopped, 1), op),
                    None => stopped.clone(),
                });
            }
            if starts(args, &["restore"]) {
                assert!(
                    args.windows(2)
                        .any(|w| w[0] == "--image" && w[1] == "local/coop-exp:fx")
                );
                *r.borrow_mut() = Some(op(&flag(args, "--operation")));
                return ok("{}");
            }
            fail("unexpected")
        }),
    );
    // A disk built for another guest user is refused before any change.
    let other = ImageName::new("other").unwrap();
    let mut foreign = ImageManifest::load(&cfg, &image).unwrap();
    foreign.guest_user = crate::guest::GuestUser::new("dev").unwrap();
    foreign.save(&cfg, &other).unwrap();
    let err = be
        .restore_disk(&cfg, &StoppedInstance::new(inst.clone()), &other)
        .unwrap_err();
    assert!(format!("{err:#}").contains("guest user 'dev'"), "{err:#}");
    assert!(mutations(&calls).is_empty());
    assert!(Journal::try_load(&inst).unwrap().is_none());

    be.restore_disk(&cfg, &StoppedInstance::new(inst.clone()), &image)
        .unwrap();
    assert_eq!(mutations(&calls), ["restore"]);
    let after = MachineSidecar::load(&inst).unwrap();
    assert!(after.reenroll_host_key);
    assert_eq!(after.image_manifest_id, "m2");
    assert!(Journal::try_load(&inst).unwrap().is_none());
}

#[test]
fn resize_grows_but_never_shrinks() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    // The fixture disk is 8 GiB.
    let grown: Rc<RefCell<Option<OperationId>>> = Rc::new(RefCell::new(None));
    let g = Rc::clone(&grown);
    let stopped = inspect_json(&cfg, &owner, "stopped");
    let (be, calls) = backend(
        &cfg,
        Box::new(move |args| {
            if let Some(o) = version(args, true) {
                return o;
            }
            if starts(args, &["inspect"]) {
                let Some(op) = &*g.borrow() else {
                    return ok(&stopped);
                };
                let grown = stopped.replace(
                    "\"diskBytes\" : 8589934592",
                    &format!("\"diskBytes\" : {}", 32u64 << 30),
                );
                return ok(&with_last_operation(&grown, op));
            }
            if starts(args, &["grow"]) {
                assert_eq!(flag(args, "--disk-gib"), "32");
                *g.borrow_mut() = Some(op(&flag(args, "--operation")));
                return ok("{}");
            }
            fail("unexpected")
        }),
    );
    let stopped_inst = StoppedInstance::new(inst.clone());
    let err = be
        .resize_disk(&cfg, &stopped_inst, GiB::new(4).unwrap())
        .unwrap_err();
    assert!(format!("{err:#}").contains("shrinking"), "{err:#}");
    be.resize_disk(&cfg, &stopped_inst, GiB::new(8).unwrap())
        .unwrap();
    assert!(mutations(&calls).is_empty());
    be.resize_disk(&cfg, &stopped_inst, GiB::new(32).unwrap())
        .unwrap();
    assert_eq!(mutations(&calls), ["grow"]);
    assert!(be.disk_path(&inst).unwrap().ends_with(format!(
        "runtime/sandboxes/{}/rootfs.ext4",
        sandbox_name(&owner)
    )));
}

#[test]
fn failed_restart_boot_stops_the_sandbox_and_reports_the_console_log() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    let json = inspect_json(&cfg, &owner, "stopped");
    let (be, calls) = backend(
        &cfg,
        Box::new(move |args| {
            if let Some(o) = version(args, true) {
                return o;
            }
            if starts(args, &["inspect"]) {
                return ok(&json);
            }
            if starts(args, &["start"]) {
                return fail("owner failed: vmnet refused");
            }
            if starts(args, &["logs"]) {
                return ok("kernel panic \x1b[31m- not syncing\n");
            }
            if starts(args, &["stop"]) {
                return ok("");
            }
            fail("unexpected")
        }),
    );
    let err = be.start_existing(&cfg, &inst).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("APPLE_BOOT_TIMEOUT"), "{msg}");
    assert!(msg.contains("kernel panic ?[31m- not syncing"), "{msg}");
    assert_eq!(mutations(&calls), ["start", "stop"]);
}

/// A normal start must reject a changed host key; only a coop restore
/// lets the next start pin a new one.
#[test]
fn start_enforces_the_pin_unless_coop_replaced_the_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    let mut sidecar = write_sidecar(&inst, &owner);
    std::fs::write(state::known_hosts_path(&inst), "old-pin\n").unwrap();
    let respond = |cfg: &CoopConfig, owner: &Owner| -> Responder {
        let booted = Rc::new(RefCell::new(false));
        let (stopped, running) = (
            inspect_json(cfg, owner, "stopped"),
            inspect_json(cfg, owner, "running"),
        );
        Box::new(move |args| {
            if let Some(o) = version(args, true) {
                return o;
            }
            if starts(args, &["inspect"]) {
                return ok(if *booted.borrow() { &running } else { &stopped });
            }
            if starts(args, &["start"]) {
                *booted.borrow_mut() = true;
                return ok("{}");
            }
            if starts(args, &["exec"]) {
                return ok(KEY);
            }
            if starts(args, &["stop"]) {
                *booted.borrow_mut() = false;
                return ok("");
            }
            fail("unexpected")
        })
    };
    let (be, _) = backend(&cfg, respond(&cfg, &owner));
    let err = be.start_existing(&cfg, &inst).unwrap_err();
    assert!(
        matches!(kind(&err), AppleError::HostKeyChanged(_)),
        "{err:#}"
    );
    assert_eq!(
        std::fs::read_to_string(state::known_hosts_path(&inst)).unwrap(),
        "old-pin\n"
    );

    sidecar.reenroll_host_key = true;
    sidecar.save(&inst).unwrap();
    let (be, _) = backend(&cfg, respond(&cfg, &owner));
    // SSH never answers here, so this start fails after pinning; the
    // re-enroll window must close anyway.
    assert!(be.start_existing(&cfg, &inst).is_err());
    assert!(!MachineSidecar::load(&inst).unwrap().reenroll_host_key);
    let pinned = std::fs::read_to_string(state::known_hosts_path(&inst)).unwrap();
    assert!(
        pinned.contains("AAAAC3NzaC1lZDI1NTE5AAAAINiqkOnkRV06x"),
        "{pinned}"
    );
}

#[test]
#[expect(clippy::panic, reason = "test assertion")]
fn unqualified_running_sandbox_is_an_error_not_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    let json = inspect_json(&cfg, &owner, "running");
    let (be, _) = backend(
        &cfg,
        Box::new(move |args| version(args, false).unwrap_or_else(|| ok(&json))),
    );
    let Err(err) = be.as_running(&cfg, inst.clone()) else {
        panic!("a running sandbox on an unqualified runtime must not yield a target");
    };
    assert!(format!("{err:#}").contains("coop stop t"), "{err:#}");
}

#[test]
fn stop_unproven_stops_without_qualification_or_ssh() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    let stopped = Rc::new(RefCell::new(false));
    let s = Rc::clone(&stopped);
    let (on, off) = (
        inspect_json(&cfg, &owner, "running"),
        inspect_json(&cfg, &owner, "stopped"),
    );
    let (be, calls) = backend(
        &cfg,
        Box::new(move |args| {
            if let Some(o) = version(args, false) {
                return o;
            }
            if starts(args, &["inspect"]) {
                return ok(if *s.borrow() { &off } else { &on });
            }
            if starts(args, &["stop"]) {
                *s.borrow_mut() = true;
                return ok("");
            }
            fail("unexpected")
        }),
    );
    be.stop_unproven(&cfg, &inst).unwrap();
    assert!(*stopped.borrow());
    assert_eq!(mutations(&calls), ["stop"]);
}

/// Stopping without proof still requires ownership of the record.
#[test]
fn stop_unproven_refuses_a_foreign_record() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    let mut sidecar = write_sidecar(&inst, &owner);
    sidecar.owner_id =
        state::OwnerId::try_from("ffffffff00112233445566778899aabb".to_string()).unwrap();
    sidecar.save(&inst).unwrap();
    let json = inspect_json(&cfg, &owner, "running");
    let (be, calls) = backend(
        &cfg,
        Box::new(move |args| version(args, false).unwrap_or_else(|| ok(&json))),
    );
    let err = be.stop_unproven(&cfg, &inst).unwrap_err();
    assert!(
        matches!(kind(&err), AppleError::IdentityConflict(_)),
        "{err:#}"
    );
    assert!(mutations(&calls).is_empty(), "{:?}", calls.borrow());
}

/// `start` boots a stopped or crashed sandbox; a running or booting one is
/// refused before any runtime change.
#[test]
fn start_gates_on_the_sandbox_status() {
    for (status, boots) in [
        ("stopped", true),
        ("crashed", true),
        ("running", false),
        ("booting", false),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let owner = Owner::load_or_init(&cfg).unwrap();
        let inst = test_inst(&cfg);
        write_sidecar(&inst, &owner);
        let json = inspect_json(&cfg, &owner, status);
        let (be, calls) = backend(
            &cfg,
            Box::new(move |args| {
                if let Some(o) = version(args, true) {
                    return o;
                }
                if starts(args, &["inspect"]) {
                    return ok(&json);
                }
                if starts(args, &["logs"]) || starts(args, &["stop"]) {
                    return ok("");
                }
                fail("boot refused by test")
            }),
        );
        let err = be.start_existing(&cfg, &inst).unwrap_err();
        let started = mutations(&calls).first().is_some_and(|m| m == "start");
        assert_eq!(started, boots, "{status}: {err:#}");
        match status {
            "running" => assert!(format!("{err:#}").contains("already running"), "{err:#}"),
            "booting" => assert!(
                matches!(kind(&err), AppleError::OperationUncertain(_)),
                "{err:#}"
            ),
            _ => assert!(matches!(kind(&err), AppleError::BootTimeout(_)), "{err:#}"),
        }
        if !boots {
            assert!(mutations(&calls).is_empty(), "{status}");
        }
    }
}

#[test]
fn cmd_stop_falls_back_to_stop_unproven_when_the_probe_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    let stopped = Rc::new(RefCell::new(false));
    let s = Rc::clone(&stopped);
    let (on, off) = (
        inspect_json(&cfg, &owner, "running"),
        inspect_json(&cfg, &owner, "stopped"),
    );
    let (be, calls) = backend(
        &cfg,
        Box::new(move |args| {
            if let Some(o) = version(args, false) {
                return o;
            }
            if starts(args, &["inspect"]) {
                return ok(if *s.borrow() { &off } else { &on });
            }
            if starts(args, &["stop"]) {
                *s.borrow_mut() = true;
                return ok("");
            }
            fail("unexpected")
        }),
    );
    assert!(
        be.as_running(&cfg, inst.clone()).is_err(),
        "precondition: the liveness probe fails on an unqualified runtime"
    );
    crate::commands::cmd_stop(&be, &cfg, &inst).unwrap();
    assert!(*stopped.borrow());
    assert_eq!(mutations(&calls), ["stop"]);
}

#[test]
fn follow_logs_replace_guest_control_bytes() {
    // `stream_logs` needs a RunningInstance, which cannot be minted without
    // SSH here, so this drives the same streaming call and sanitizer it uses.
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let (be, _) = backend(
        &cfg,
        Box::new(|args| {
            version(args, true).unwrap_or_else(|| {
                if starts(args, &["logs"]) {
                    return ok("ok\n\x1b]52;c;ZXZpbA==\x07pwned\n");
                }
                fail("unexpected")
            })
        }),
    );
    let rt = be.runtime().unwrap();
    let mut lines = Vec::new();
    rt.exec
        .run_streaming(&rt.args(&["logs"], &["m", "--follow"]), &mut |line| {
            lines.push(cli::sanitize_for_display(&String::from_utf8_lossy(line)));
            Ok(())
        })
        .unwrap();
    assert_eq!(lines, ["ok", "?]52;c;ZXZpbA==?pwned"]);
}

#[test]
fn host_key_read_retries_a_half_written_file_and_detects_restarts() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let sidecar = write_sidecar(&test_inst(&cfg), &owner);
    let running = inspect_json(&cfg, &owner, "running");
    for restarted in [false, true] {
        let reads = Rc::new(RefCell::new(0));
        let r = Rc::clone(&reads);
        let now = if restarted {
            let pid = serde_json::from_str::<serde_json::Value>(&running).unwrap()["live"]["pid"]
                .as_i64()
                .unwrap();
            running.replace(
                &format!("\"pid\" : {pid}"),
                &format!("\"pid\" : {}", pid + 1),
            )
        } else {
            running.clone()
        };
        let (be, _) = backend(
            &cfg,
            Box::new(move |args| {
                if let Some(o) = version(args, true) {
                    return o;
                }
                if starts(args, &["exec"]) {
                    *r.borrow_mut() += 1;
                    // First read catches the file mid-write.
                    return ok(if *r.borrow() == 1 {
                        "ssh-ed25519 AAAAC3Nza"
                    } else {
                        KEY
                    });
                }
                if starts(args, &["inspect"]) {
                    return ok(&now);
                }
                fail("unexpected")
            }),
        );
        let rt = be.runtime().unwrap();
        let inspect = protocol::parse_inspect(&running, &sidecar.machine_id).unwrap();
        let ready = security::verify_effective(&inspect, &rt.expected(&sidecar)).unwrap();
        let got = rt.read_host_key(&ready, Instant::now() + Duration::from_secs(5));
        assert_eq!(*reads.borrow(), 2);
        if restarted {
            assert!(matches!(
                kind(&got.unwrap_err()),
                AppleError::IdentityConflict(_)
            ));
        } else {
            assert!(got.unwrap().fingerprint().starts_with("SHA256:"));
        }
    }
}

fn manifest(image_ref: &str, disk: Option<CommittedDisk>) -> ImageManifest {
    ImageManifest {
        schema_version: state::SCHEMA_VERSION,
        backend: state::BACKEND_TAG.into(),
        image_ref: image_ref.into(),
        digest: format!("sha256:{}", "a".repeat(64)),
        disk,
        manifest_id: "m".into(),
        base_image: image::BASE_IMAGE.into(),
        platform: image::PLATFORM.into(),
        guest_user: crate::guest::GuestUser::default(),
        created: "now".into(),
    }
}

/// Only this installation's images and disks are deleted, and only once
/// no saved manifest still uses them; an unreadable manifest keeps all.
#[test]
fn release_deletes_only_owned_and_unreferenced_content() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let (be, calls) = backend(
        &cfg,
        Box::new(|args| version(args, true).unwrap_or_else(|| ok(""))),
    );
    let rt = be.runtime().unwrap();
    let owned_ref = format!("local/coop-{}:abc", owner.id.short());
    let owned_disk = MachineName::generate(&owner.id).unwrap();
    let foreign_disk = MachineName::new("coop-ffffffff-0011223344556677").unwrap();
    let committed = |name: &MachineName| {
        manifest(
            &owned_ref,
            Some(CommittedDisk {
                name: name.clone(),
                bytes: 1,
            }),
        )
    };
    let deleted = || -> Vec<Vec<String>> {
        calls
            .borrow()
            .iter()
            .filter(|c| is_mutating(c))
            .cloned()
            .collect()
    };
    let base = ImageName::new("default").unwrap();
    manifest(&owned_ref, None).save(&cfg, &base).unwrap();

    // Foreign content is never touched.
    release_manifest(
        rt,
        &cfg,
        &owner,
        &manifest("local/someone-else:1", None),
        None,
    );
    release_manifest(rt, &cfg, &owner, &committed(&foreign_disk), None);
    // The image tag is still used by `default`; the owned disk is not.
    release_manifest(rt, &cfg, &owner, &committed(&owned_disk), None);
    let got = deleted();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(starts(&got[0], &["disk", "delete"]) && got[0].last() == Some(&owned_disk.to_string()));

    // A disk another manifest uses is kept.
    calls.borrow_mut().clear();
    committed(&owned_disk)
        .save(&cfg, &ImageName::new("snap").unwrap())
        .unwrap();
    release_manifest(rt, &cfg, &owner, &committed(&owned_disk), Some(&base));
    let got = deleted();
    assert!(
        got.iter().all(|c| !starts(c, &["disk", "delete"])),
        "{got:?}"
    );

    // Once nothing else uses the tag, it goes with the last manifest.
    calls.borrow_mut().clear();
    std::fs::remove_dir_all(cfg.image_dir(&ImageName::new("snap").unwrap())).unwrap();
    release_manifest(rt, &cfg, &owner, &manifest(&owned_ref, None), Some(&base));
    let got = deleted();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(starts(&got[0], &["image", "delete"]) && got[0].last() == Some(&owned_ref));

    // An unreadable manifest might reference anything: keep everything.
    calls.borrow_mut().clear();
    let broken = ImageName::new("broken").unwrap();
    std::fs::create_dir_all(cfg.image_dir(&broken)).unwrap();
    std::fs::write(cfg.image_dir(&broken).join("apple-image.json"), "{").unwrap();
    release_manifest(rt, &cfg, &owner, &committed(&owned_disk), Some(&base));
    assert!(deleted().is_empty());
}

#[test]
fn verify_image_checks_presence_and_digest() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let digest = format!("sha256:{}", "a".repeat(64));
    let listed = format!(r#"[{{"reference":"local/x:1","digest":"{digest}"}}]"#);
    let (be, _) = backend(
        &cfg,
        Box::new(move |args| {
            version(args, true).unwrap_or_else(|| {
                if starts(args, &["image", "list"]) {
                    return ok(&listed);
                }
                if starts(args, &["disk", "list"]) {
                    return ok(
                        r#"[{"name":"coop-0a1b2c3d-1","logicalBytes":1,"allocatedBytes":1}]"#,
                    );
                }
                fail("unexpected")
            })
        }),
    );
    let rt = be.runtime().unwrap();
    rt.verify_image(&manifest("local/x:1", None)).unwrap();
    let mut stale = manifest("local/x:1", None);
    stale.digest = format!("sha256:{}", "b".repeat(64));
    for bad in [
        manifest("local/missing:1", None),
        stale,
        manifest(
            "local/x:1",
            Some(CommittedDisk {
                name: MachineName::generate(&owner.id).unwrap(),
                bytes: 1,
            }),
        ),
    ] {
        let err = rt.verify_image(&bad).unwrap_err();
        assert!(
            matches!(kind(&err), AppleError::IdentityConflict(_)),
            "{err:#}"
        );
    }
    let present = MachineName::new("coop-0a1b2c3d-1").unwrap();
    rt.verify_image(&manifest(
        "local/gone:1",
        Some(CommittedDisk {
            name: present,
            bytes: 1,
        }),
    ))
    .unwrap();
}

#[test]
fn commit_saves_a_disk_manifest_from_the_runtime_record() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    let sidecar = write_sidecar(&inst, &owner);
    let stopped = inspect_json(&cfg, &owner, "stopped");
    let (be, calls) = backend(
        &cfg,
        Box::new(move |args| {
            if let Some(o) = version(args, true) {
                return o;
            }
            if starts(args, &["inspect"]) {
                return ok(&stopped);
            }
            if starts(args, &["commit"]) {
                let name = args.last().unwrap();
                return ok(&format!(
                    r#"{{"name":"{name}","logicalBytes":8589934592,"allocatedBytes":1}}"#
                ));
            }
            fail("unexpected")
        }),
    );
    let image = ImageName::new("snap").unwrap();
    be.commit_disk(&cfg, &StoppedInstance::new(inst.clone()), &image)
        .unwrap();
    let call = calls
        .borrow()
        .iter()
        .find(|c| starts(c, &["commit"]))
        .cloned()
        .unwrap();
    let disk = call.last().unwrap().clone();
    assert!(call.contains(&sidecar.machine_id.to_string()));
    assert!(
        MachineName::new(disk.clone())
            .unwrap()
            .belongs_to(&owner.id)
    );
    let saved = ImageManifest::load(&cfg, &image).unwrap();
    let committed = saved.disk.unwrap();
    assert_eq!(committed.name.as_str(), disk);
    assert_eq!(committed.bytes, 8 << 30);
    // Image identity comes from the runtime record, not the instance.
    assert_eq!(saved.image_ref, "local/coop-exp:fx");
    assert_eq!(covering_gib(committed.bytes).unwrap(), GiB::new(8).unwrap());
}

/// A commit the runtime reports as failed may still have published its
/// disk; nothing refers to it, so it is deleted and no manifest is written.
#[test]
fn failed_commit_deletes_its_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    let stopped = inspect_json(&cfg, &owner, "stopped");
    let (be, calls) = backend(
        &cfg,
        Box::new(move |args| {
            if let Some(o) = version(args, true) {
                return o;
            }
            if starts(args, &["inspect"]) {
                return ok(&stopped);
            }
            if starts(args, &["commit"]) {
                return fail("metadata write failed");
            }
            if starts(args, &["disk", "delete"]) {
                return ok("");
            }
            fail("unexpected")
        }),
    );
    let image = ImageName::new("snap").unwrap();
    let err = be
        .commit_disk(&cfg, &StoppedInstance::new(inst.clone()), &image)
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("metadata write failed"),
        "{err:#}"
    );
    let calls = calls.borrow();
    let commit = calls.iter().find(|c| starts(c, &["commit"])).unwrap();
    let deleted = calls
        .iter()
        .find(|c| starts(c, &["disk", "delete"]))
        .and_then(|c| c.last());
    assert_eq!(deleted, commit.last(), "the committed disk is deleted");
    assert!(ImageManifest::load(&cfg, &image).is_err());
}

#[test]
fn disk_bytes_round_up_to_whole_gib() {
    assert_eq!(covering_gib(1).unwrap(), GiB::new(1).unwrap());
    assert_eq!(covering_gib((8 << 30) + 1).unwrap(), GiB::new(9).unwrap());
    assert!(covering_gib(0).is_err());
}

/// A readback that does not match the request is uncertain, and the
/// journal stays for `start` to reconcile.
#[test]
fn resource_change_that_does_not_apply_is_uncertain() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    let stopped = inspect_json(&cfg, &owner, "stopped");
    let (be, calls) = backend(
        &cfg,
        Box::new(move |args| {
            if let Some(o) = version(args, true) {
                return o;
            }
            if starts(args, &["inspect"]) {
                return ok(&stopped);
            }
            if starts(args, &["set"]) {
                return ok("{}");
            }
            fail("unexpected")
        }),
    );
    let err = be
        .set_machine_resources(
            &cfg,
            &StoppedInstance::new(inst.clone()),
            None,
            NonZeroU8::new(6),
            false,
        )
        .unwrap_err();
    assert!(
        matches!(kind(&err), AppleError::OperationUncertain(_)),
        "{err:#}"
    );
    let set = calls
        .borrow()
        .iter()
        .find(|c| starts(c, &["set"]))
        .cloned()
        .unwrap();
    // The runtime is sent the whole target: the change plus what stays.
    assert_eq!(flag(&set, "--cpus"), "6");
    assert_eq!(flag(&set, "--memory-mib"), "2048");
    // The journal carries the operation, the resources the runtime had
    // before it, and its target.
    let prior = Resources {
        cpus: 2,
        memory_bytes: 2048 * 1024 * 1024,
    };
    assert_eq!(
        Journal::try_load(&inst).unwrap().unwrap().op,
        JournalOp::SetResources {
            operation: op(&flag(&set, "--operation")),
            prior,
        }
    );
}

// ── Simulated runtime ─────────────────────────────────────

struct SimBox {
    status: SandboxStatus,
    image: String,
    digest: String,
    cpus: u32,
    memory_bytes: u64,
    last_operation: Option<String>,
    /// Inspects left before a started sandbox reports its effective
    /// configuration.
    settling: u32,
}

/// A stateful stand-in for `coop-sandbox` and the stock builder, for
/// driving whole operations (setup, image verification) end to end.
struct Sim {
    root: String,
    owner: String,
    sandboxes: std::collections::BTreeMap<String, SimBox>,
    images: Vec<(String, String)>,
    saved: Option<String>,
    builder_images: Vec<String>,
    /// Status a started sandbox settles into (`running`, or `stopped`
    /// for a guest that powers off during boot).
    boots_to: SandboxStatus,
    settle_inspects: u32,
    host_key: Option<&'static str>,
    missing: Vec<String>,
    uid: &'static str,
    console: &'static str,
    faults: Vec<Fault>,
    /// Version of the installed maintenance artifact.
    maintenance: Option<String>,
}

/// Ways the simulated runtime or builder misbehaves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    BuilderDown,
    BuildFails,
    CreateFails,
    ServicesInactive,
    SetIgnoresMemory,
    /// A rollback `set` applies and commits, then reports failure (as
    /// when the caller is interrupted after the runtime changed).
    RollbackFailsAfterApplying,
    /// While a start fails, another client changes the resources.
    ChangedDuringStart,
    /// `stop` leaves the sandbox running.
    StopIgnored,
    MaintenanceInstallFails,
    /// `maintenance install` reports an image other than the one given.
    MaintenanceReportsOtherImage,
    LogsFail,
}

const SIM_DIGEST: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

impl Sim {
    fn new(cfg: &CoopConfig, owner: &Owner) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            root: canonical_path(&cfg.state_root().join(RUNTIME_DIR))
                .display()
                .to_string(),
            owner: owner.id.as_str().to_string(),
            sandboxes: std::collections::BTreeMap::new(),
            images: Vec::new(),
            saved: None,
            builder_images: Vec::new(),
            boots_to: SandboxStatus::Running,
            settle_inspects: 0,
            host_key: Some(KEY),
            missing: Vec::new(),
            uid: "1000\n",
            console: "[    0.000000] Booting Linux\n",
            faults: Vec::new(),
            maintenance: None,
        }))
    }

    fn has(&self, fault: Fault) -> bool {
        self.faults.contains(&fault)
    }

    fn inspect(&mut self, id: &str) -> Output {
        let Some(b) = self.sandboxes.get_mut(id) else {
            return fail("no such sandbox");
        };
        let settling = b.status == SandboxStatus::Running && b.settling > 0;
        b.settling = b.settling.saturating_sub(1);
        let fixture_name = if b.status == SandboxStatus::Running {
            "inspect-running.json"
        } else {
            "inspect-stopped.json"
        };
        let mut v: serde_json::Value = serde_json::from_str(&fixture(fixture_name)).unwrap();
        v["status"] = b.status.label().into();
        for key in ["record", "effective"] {
            let Some(o) = v.get_mut(key).filter(|o| o.is_object()) else {
                continue;
            };
            o["id"] = id.into();
            o["imageReference"] = b.image.clone().into();
            o["imageDigest"] = b.digest.clone().into();
            o["cpus"] = b.cpus.into();
            o["memoryBytes"] = b.memory_bytes.into();
        }
        if let Some(op) = &b.last_operation {
            v["record"]["lastOperation"] = op.clone().into();
        }
        v["record"]["owner"] = self.owner.clone().into();
        if let Some(rootfs) = v.pointer_mut("/effective/rootfs/source") {
            *rootfs = format!("{}/sandboxes/{id}/rootfs.ext4", self.root).into();
        }
        if settling {
            v["effective"] = serde_json::Value::Null;
        }
        ok(&v.to_string())
    }

    fn guest(&self, argv: &[String]) -> Output {
        match argv.first().map(String::as_str) {
            Some("/bin/cat") => self.host_key.map_or_else(|| fail("No such file"), ok),
            Some("/bin/sh") => ok(&self
                .missing
                .iter()
                .filter(|m| argv.contains(m))
                .fold(String::new(), |out, m| out + m + "\n")),
            Some("/usr/bin/id") => ok(self.uid),
            Some("/usr/bin/systemctl") if argv.iter().any(|a| a == "--quiet") => {
                if self.has(Fault::ServicesInactive) {
                    fail("")
                } else {
                    ok("")
                }
            }
            Some("/usr/bin/systemctl") => ok("active\nactivating\n"),
            _ => fail("unexpected guest command"),
        }
    }

    fn respond(&mut self, args: &[String]) -> Output {
        let flag = |name: &str| {
            args.windows(2)
                .find(|w| w[0] == name)
                .map(|w| w[1].clone())
                .unwrap_or_default()
        };
        if !args.iter().any(|a| a == "--root") {
            // The stock builder.
            if starts(args, &["system", "status"]) {
                return if self.has(Fault::BuilderDown) {
                    fail("not running")
                } else {
                    ok("")
                };
            }
            if starts(args, &["build"]) {
                if self.has(Fault::BuildFails) {
                    return fail("build failed");
                }
                self.builder_images.push(flag("-t"));
                return ok("");
            }
            if starts(args, &["image", "save"]) {
                self.saved = args.last().cloned();
                return ok("");
            }
            if starts(args, &["image", "delete"]) {
                self.builder_images.retain(|i| Some(i) != args.last());
                return ok("");
            }
            return fail("unexpected builder command");
        }
        let id = args.get(3).cloned().unwrap_or_default();
        match (args[0].as_str(), args.get(1).map(String::as_str)) {
            ("init" | "reconcile", _) => ok("{}"),
            ("image", Some("import")) => {
                let reference = self.saved.clone().unwrap_or_default();
                self.images.push((reference.clone(), SIM_DIGEST.into()));
                ok(&format!(r#"[{{"reference":"{reference}","digest":"{SIM_DIGEST}"}}]"#))
            }
            ("image", Some("list")) => ok(&serde_json::to_string(
                &self
                    .images
                    .iter()
                    .map(|(r, d)| serde_json::json!({"reference": r, "digest": d}))
                    .collect::<Vec<_>>(),
            )
            .unwrap()),
            ("image", Some("delete")) => {
                self.images.retain(|(r, _)| Some(r) != args.last());
                ok("")
            }
            ("disk", Some("list")) => ok("[]"),
            ("maintenance", Some("inspect")) => match &self.maintenance {
                Some(version) => ok(&format!(
                    r#"{{"version":"{version}","reference":"r","digest":"{SIM_DIGEST}"}}"#
                )),
                None => ok("null"),
            },
            ("maintenance", Some("install")) => {
                let mut reference = flag("--image");
                if !self.images.iter().any(|(r, _)| *r == reference) {
                    return fail("no such image");
                }
                if self.has(Fault::MaintenanceInstallFails) {
                    return fail("install failed");
                }
                if self.has(Fault::MaintenanceReportsOtherImage) {
                    reference = "local/someone-else:1".into();
                }
                let version = flag("--version");
                self.maintenance = Some(version.clone());
                ok(&format!(
                    r#"{{"version":"{version}","reference":"{reference}","digest":"{SIM_DIGEST}"}}"#
                ))
            }
            ("list", _) => ok(&serde_json::to_string(
                &self
                    .sandboxes
                    .iter()
                    .map(|(id, b)| {
                        serde_json::json!({"id": id, "status": b.status.label(), "owner": self.owner})
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap()),
            _ => self.respond_sandbox(args, &id, &flag),
        }
    }

    fn respond_sandbox(
        &mut self,
        args: &[String],
        id: &str,
        flag: &dyn Fn(&str) -> String,
    ) -> Output {
        match args[0].as_str() {
            "create" => {
                if self.has(Fault::CreateFails) {
                    return fail("create failed");
                }
                let image = flag("--image");
                let Some((_, digest)) = self.images.iter().find(|(r, _)| *r == image) else {
                    return fail("no such image");
                };
                let memory_mib: u64 = flag("--memory-mib").parse().unwrap();
                self.sandboxes.insert(
                    id.to_string(),
                    SimBox {
                        status: SandboxStatus::Stopped,
                        image,
                        digest: digest.clone(),
                        cpus: flag("--cpus").parse().unwrap(),
                        memory_bytes: memory_mib * 1024 * 1024,
                        last_operation: None,
                        settling: 0,
                    },
                );
                ok("{}")
            }
            "inspect" => self.inspect(id),
            "start" => {
                let (to, settle) = (self.boots_to, self.settle_inspects);
                let interleaved = self.has(Fault::ChangedDuringStart);
                let Some(b) = self.sandboxes.get_mut(id) else {
                    return fail("no such sandbox");
                };
                b.status = to;
                b.settling = settle;
                if interleaved {
                    b.cpus = 7;
                    b.last_operation = Some("coop-elsewhere".into());
                }
                ok("{}")
            }
            "stop" => {
                let ignored = self.has(Fault::StopIgnored);
                if let Some(b) = self.sandboxes.get_mut(id)
                    && !ignored
                {
                    b.status = SandboxStatus::Stopped;
                }
                ok("")
            }
            "delete" => {
                self.sandboxes.remove(id);
                ok("")
            }
            "logs" if self.has(Fault::LogsFail) => fail("no console log"),
            "logs" => ok(self.console),
            "set" => {
                let ignores_memory = self.has(Fault::SetIgnoresMemory);
                let fails_after = self.has(Fault::RollbackFailsAfterApplying);
                let Some(b) = self.sandboxes.get_mut(id) else {
                    return fail("no such sandbox");
                };
                if b.status != SandboxStatus::Stopped {
                    return fail("not stopped");
                }
                let expect = flag("--expect-operation");
                if !expect.is_empty() && b.last_operation.as_deref() != Some(expect.as_str()) {
                    return fail("changed by another operation");
                }
                if let Ok(cpus) = flag("--cpus").parse() {
                    b.cpus = cpus;
                }
                if let Ok(mib) = flag("--memory-mib").parse::<u64>()
                    && !ignores_memory
                {
                    b.memory_bytes = mib * 1024 * 1024;
                }
                b.last_operation = Some(flag("--operation"));
                if fails_after && !expect.is_empty() {
                    return fail("interrupted");
                }
                ok("{}")
            }
            "exec" => {
                // exec --root R --timeout S ID -- ARGV
                let dashes = args.iter().position(|a| a == "--").unwrap();
                self.guest(&args[dashes + 1..])
            }
            _ => fail("unexpected runtime command"),
        }
    }
}

fn sim_backend(cfg: &CoopConfig, sim: &Rc<RefCell<Sim>>) -> (AppleContainerBackend, Calls) {
    let sim = Rc::clone(sim);
    backend(
        cfg,
        Box::new(move |args| version(args, true).unwrap_or_else(|| sim.borrow_mut().respond(args))),
    )
}

/// A test config whose kernel path exists, with a fresh owner.
fn setup_env(dir: &Path) -> (CoopConfig, Owner, SetupOptions) {
    let mut cfg = test_cfg(&dir.join("data"));
    let kernel = dir.join("vmlinux");
    std::fs::write(&kernel, "").unwrap();
    cfg.apple_container.kernel = Some(ConfigPath::new(&kernel));
    let owner = Owner::load_or_init(&cfg).unwrap();
    let opts = SetupOptions {
        skip_confirm: true,
        rebuild: false,
        profiles: Vec::new(),
        oci_features: Vec::new(),
        extra_packages: Vec::new(),
        post_install: None,
        image: ImageName::new("default").unwrap(),
        guest_user: crate::guest::GuestUser::default(),
        builder_timeout: None,
    };
    (cfg, owner, opts)
}

/// Setup builds with the stock builder, moves the image into the runtime,
/// proves it boots and meets the guest contract in a disposable sandbox,
/// then publishes the manifest. A second setup with the same inputs
/// reuses it; `--rebuild` replaces it and releases the old tag.
#[test]
fn setup_verifies_in_a_disposable_sandbox_then_publishes() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, owner, mut opts) = setup_env(tmp.path());
    let sim = Sim::new(&cfg, &owner);
    sim.borrow_mut().settle_inspects = 2;
    let (be, calls) = sim_backend(&cfg, &sim);
    assert!(!be.image_is_built(&cfg, &opts.image));

    be.setup(&cfg, &opts).unwrap();
    let manifest = ImageManifest::load(&cfg, &opts.image).unwrap();
    assert!(
        manifest
            .image_ref
            .starts_with(&format!("local/coop-{}:", owner.id.short()))
    );
    assert_eq!(manifest.digest, SIM_DIGEST);
    assert!(be.image_is_built(&cfg, &opts.image));
    assert!(crate::setup::TemplateConfig::load_for(&cfg, &opts.image).is_ok());
    {
        let s = sim.borrow();
        assert!(s.sandboxes.is_empty(), "verification sandbox left behind");
        assert!(s.builder_images.is_empty(), "builder copy left behind");
        assert_eq!(s.images.len(), 1);
    }
    assert_eq!(
        mutations(&calls),
        [
            // The maintenance image: built, imported, unpacked by the
            // runtime, then dropped from its store.
            "init",
            "build",
            "image",
            "image",
            "image",
            "maintenance",
            "image",
            // The application image, verified in a disposable sandbox.
            "build",
            "image",
            "image",
            "image",
            "create",
            "start",
            "exec",
            "exec",
            "exec",
            "exec",
            "stop",
            "delete"
        ]
    );
    assert_eq!(
        sim.borrow().maintenance.as_deref(),
        Some(image::MAINTENANCE_VERSION)
    );

    calls.borrow_mut().clear();
    be.setup(&cfg, &opts).unwrap();
    assert_eq!(
        mutations(&calls),
        ["init"],
        "an up-to-date image and maintenance image are reused"
    );

    opts.rebuild = true;
    be.setup(&cfg, &opts).unwrap();
    let rebuilt = ImageManifest::load(&cfg, &opts.image).unwrap();
    assert_ne!(rebuilt.image_ref, manifest.image_ref);
    let s = sim.borrow();
    assert_eq!(s.images.len(), 1, "superseded tag released");
    assert_eq!(s.images[0].0, rebuilt.image_ref);
}

/// Every failed check fails setup, removes the verification sandbox and
/// the candidate image, and publishes nothing.
#[test]
fn setup_rejects_an_image_that_fails_verification() {
    type Break = fn(&mut Sim);
    let cases: [(&str, Break); 7] = [
        ("service", |s| s.faults.push(Fault::BuilderDown)),
        ("Image build failed", |s| s.faults.push(Fault::BuildFails)),
        ("stopped during boot", |s| {
            s.boots_to = SandboxStatus::Stopped;
        }),
        ("SSH host key", |s| s.host_key = None),
        ("/usr/bin/docker", |s| {
            s.missing = vec!["/usr/bin/docker".into()];
        }),
        ("uid 1000", |s| s.uid = "1001\n"),
        ("services ssh, docker not active", |s| {
            s.faults.push(Fault::ServicesInactive);
        }),
    ];
    for (why, breaks) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, owner, opts) = setup_env(tmp.path());
        let sim = Sim::new(&cfg, &owner);
        breaks(&mut sim.borrow_mut());
        let (be, _) = sim_backend(&cfg, &sim);
        let err = be.setup(&cfg, &opts).unwrap_err();
        assert!(format!("{err:#}").contains(why), "{why}: {err:#}");
        assert!(
            ImageManifest::try_load(&cfg, &opts.image)
                .unwrap()
                .is_none(),
            "{why}"
        );
        assert!(!be.image_is_built(&cfg, &opts.image), "{why}");
        let s = sim.borrow();
        assert!(
            s.sandboxes.is_empty() && s.images.is_empty(),
            "{why}: left state behind"
        );
    }
}

/// A sandbox that never reports a running configuration fails at the
/// boot deadline, not before; the console log explains what it can.
#[test]
fn readiness_waits_for_the_deadline_then_reports_the_console() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, owner, opts) = setup_env(tmp.path());
    let sim = Sim::new(&cfg, &owner);
    sim.borrow_mut().settle_inspects = u32::MAX;
    let (be, _) = sim_backend(&cfg, &sim);
    let started = Instant::now();
    let err = be.setup(&cfg, &opts).unwrap_err();
    assert!(started.elapsed() >= Duration::from_secs(1), "gave up early");
    let text = format!("{err:#}");
    assert!(
        text.contains("did not report a running configuration"),
        "{text}"
    );
    assert!(text.contains("Booting Linux"), "{text}");
}

/// A sandbox matching `write_sidecar`, in `status`.
fn sim_sandbox(sim: &Rc<RefCell<Sim>>, owner: &Owner, status: SandboxStatus) {
    sim.borrow_mut().sandboxes.insert(
        sandbox_name(owner).to_string(),
        SimBox {
            status,
            image: "local/coop-exp:fx".into(),
            digest: SIM_DIGEST.into(),
            cpus: 2,
            memory_bytes: 2048 * 1024 * 1024,
            last_operation: None,
            settling: 0,
        },
    );
}

fn running(inst: &Instance) -> RunningInstance {
    let target = SshTarget {
        host: crate::backend::Hostname::from(std::net::Ipv4Addr::new(10, 231, 2, 2)),
        port: std::num::NonZeroU16::new(22).unwrap(),
        user: crate::backend::SshUser::new("coop").unwrap(),
        key_path: PathBuf::from("/nonexistent/key"),
        host_keys: crate::backend::HostKeyPolicy::Unverified,
    };
    RunningInstance::new(inst.clone(), target)
}

/// `up` refuses to overwrite existing state, and a failed boot stops the
/// sandbox it created and keeps the journal for `destroy`.
#[test]
fn create_refuses_existing_state_and_stops_a_failed_boot() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, owner, opts) = setup_env(tmp.path());
    let sim = Sim::new(&cfg, &owner);
    let (be, calls) = sim_backend(&cfg, &sim);
    be.setup(&cfg, &opts).unwrap();
    let inst = test_inst(&cfg);

    Journal::begin(&inst, &owner, CREATE, sandbox_name(&owner)).unwrap();
    calls.borrow_mut().clear();
    let err = be.create_and_start(&cfg, &inst, None, &[]).unwrap_err();
    assert!(
        matches!(kind(&err), AppleError::OperationUncertain(_)),
        "{err:#}"
    );
    assert!(mutations(&calls).is_empty());
    Journal::complete(&inst).unwrap();

    sim.borrow_mut().faults = vec![Fault::CreateFails];
    let err = be.create_and_start(&cfg, &inst, None, &[]).unwrap_err();
    assert!(format!("{err:#}").contains("create failed"), "{err:#}");
    Journal::complete(&inst).unwrap();

    {
        let mut s = sim.borrow_mut();
        s.faults.clear();
        s.settle_inspects = u32::MAX;
    }
    calls.borrow_mut().clear();
    let started = Instant::now();
    let err = be.create_and_start(&cfg, &inst, None, &[]).unwrap_err();
    assert!(started.elapsed() >= Duration::from_secs(1), "gave up early");
    assert!(matches!(kind(&err), AppleError::BootTimeout(_)), "{err:#}");
    assert_eq!(mutations(&calls), ["create", "start", "stop"]);
    assert!(
        sim.borrow()
            .sandboxes
            .values()
            .all(|b| b.status == SandboxStatus::Stopped)
    );
    assert!(MachineSidecar::try_load(&inst).unwrap().is_none());
    assert_eq!(
        Journal::try_load(&inst).unwrap().unwrap().op,
        JournalOp::Create {
            stage: CreateStage::MachineCreated
        }
    );
}

#[test]
fn stop_and_is_running_follow_the_runtime() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    let sim = Sim::new(&cfg, &owner);
    let (be, calls) = sim_backend(&cfg, &sim);
    assert!(!be.is_running(&inst), "no record");
    write_sidecar(&inst, &owner);
    sim_sandbox(&sim, &owner, SandboxStatus::Running);
    assert!(be.is_running(&inst));
    be.stop(&cfg, running(&inst)).unwrap();
    assert_eq!(mutations(&calls), ["stop"]);
    assert!(!be.is_running(&inst));
    assert!(!be.images_in_data_dir());
}

#[test]
fn status_reports_the_runtime_record() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    let sidecar = write_sidecar(&inst, &owner);
    let rec = protocol::parse_inspect(&inspect_json(&cfg, &owner, "running"), &sidecar.machine_id)
        .unwrap();
    let text = describe_sandbox(&inst, &sidecar, &rec, "coop-sandbox 0.1.0");
    for want in [
        "Instance 't' (running)",
        "Runtime: coop-sandbox 0.1.0",
        "Network: vmnet-shared:10.231.2.0/24 (dedicated)",
        "vCPUs: 2",
        "Memory: 2048 MiB",
        // 8724152320 and 752058368 bytes.
        "Disk: 8.1 GiB (0.7 GiB allocated)",
        "Address: 10.231.2.2 (host key SHA256:x)",
    ] {
        assert!(text.contains(want), "{want:?} not in:\n{text}");
    }
    let stopped =
        protocol::parse_inspect(&inspect_json(&cfg, &owner, "stopped"), &sidecar.machine_id)
            .unwrap();
    let text = describe_sandbox(&inst, &sidecar, &stopped, "r");
    assert!(text.contains("Network: unavailable"), "{text}");
    assert!(text.contains("Address: unavailable"), "{text}");
}

#[test]
fn logs_stream_or_fail_with_the_runtime() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    let sim = Sim::new(&cfg, &owner);
    sim_sandbox(&sim, &owner, SandboxStatus::Running);
    let (be, calls) = sim_backend(&cfg, &sim);
    for mode in [LogMode::Snapshot, LogMode::Follow] {
        be.stream_logs(&cfg, &running(&inst), mode).unwrap();
    }
    assert_eq!(
        calls
            .borrow()
            .iter()
            .filter(|c| starts(c, &["logs"]))
            .count(),
        2
    );
    sim.borrow_mut().faults = vec![Fault::LogsFail];
    for mode in [LogMode::Snapshot, LogMode::Follow] {
        assert!(be.stream_logs(&cfg, &running(&inst), mode).is_err());
    }
}

/// Memory that does not change is caught as well as CPUs; a restart that
/// fails with new resources restores the previous ones.
#[test]
fn resource_changes_are_verified_and_rolled_back() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    let sim = Sim::new(&cfg, &owner);
    sim_sandbox(&sim, &owner, SandboxStatus::Stopped);
    let (be, calls) = sim_backend(&cfg, &sim);
    let stopped = StoppedInstance::new(inst.clone());
    let mem = Some(VmMemory::new(crate::config::MiB::new(4096).unwrap()).unwrap());

    sim.borrow_mut().faults = vec![Fault::SetIgnoresMemory];
    let err = be
        .set_machine_resources(&cfg, &stopped, mem, NonZeroU8::new(4), false)
        .unwrap_err();
    assert!(
        matches!(kind(&err), AppleError::OperationUncertain(_)),
        "{err:#}"
    );
    Journal::complete(&inst).unwrap();
    sim.borrow_mut().faults.clear();

    be.set_machine_resources(&cfg, &stopped, mem, NonZeroU8::new(4), false)
        .unwrap();
    let sidecar = MachineSidecar::load(&inst).unwrap();
    assert_eq!(
        (sidecar.requested_cpus, sidecar.requested_memory_bytes),
        (4, 4096 * 1024 * 1024)
    );

    sim.borrow_mut().boots_to = SandboxStatus::Stopped;
    calls.borrow_mut().clear();
    let mem = Some(VmMemory::new(crate::config::MiB::new(6144).unwrap()).unwrap());
    let err = be
        .set_machine_resources(&cfg, &stopped, mem, NonZeroU8::new(6), true)
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("previous 4 vCPUs / 4096 MiB restored"),
        "{err:#}"
    );
    // Forward and back go through the same update; the rollback is
    // conditional on the forward change still being the last one.
    let sets = set_calls(&calls);
    assert_eq!(sets.len(), 2);
    assert!(sets[0].1.is_empty());
    assert_eq!(sets[1].1, sets[0].0);
    assert!(Journal::try_load(&inst).unwrap().is_none());
    let sidecar = MachineSidecar::load(&inst).unwrap();
    let prior = (4, 4096 * 1024 * 1024);
    assert_eq!(
        (sidecar.requested_cpus, sidecar.requested_memory_bytes),
        prior
    );
    let s = sim.borrow();
    let b = s.sandboxes.values().next().unwrap();
    assert_eq!((b.cpus, b.memory_bytes), prior);
}

/// The `set` calls sent so far, as (operation, expected operation).
fn set_calls(calls: &Calls) -> Vec<(String, String)> {
    calls
        .borrow()
        .iter()
        .filter(|c| starts(c, &["set"]))
        .map(|c| (flag(c, "--operation"), flag(c, "--expect-operation")))
        .collect()
}

struct FailedRestart {
    err: anyhow::Error,
    sim: Rc<RefCell<Sim>>,
    calls: Calls,
    inst: Instance,
    cfg: CoopConfig,
    _tmp: tempfile::TempDir,
}

/// A stopped sandbox (2 vCPUs / 2 GiB) changed to 6 vCPUs / 6 GiB, then
/// restarted with `faults` and `prepare` in effect, which must fail.
fn failed_restart_after_change(faults: Vec<Fault>, prepare: fn(&mut Sim)) -> FailedRestart {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    let sim = Sim::new(&cfg, &owner);
    sim_sandbox(&sim, &owner, SandboxStatus::Stopped);
    {
        let mut s = sim.borrow_mut();
        s.faults = faults;
        prepare(&mut s);
    }
    let (be, calls) = sim_backend(&cfg, &sim);
    let mem = Some(VmMemory::new(crate::config::MiB::new(6144).unwrap()).unwrap());
    let err = be
        .set_machine_resources(
            &cfg,
            &StoppedInstance::new(inst.clone()),
            mem,
            NonZeroU8::new(6),
            true,
        )
        .unwrap_err();
    FailedRestart {
        err,
        sim,
        calls,
        inst,
        cfg,
        _tmp: tmp,
    }
}

/// Another change committed while the restart failed: the rollback
/// refuses rather than overwrite it, and says the outcome is uncertain.
#[test]
fn rollback_never_overwrites_a_newer_change() {
    let f = failed_restart_after_change(vec![Fault::ChangedDuringStart], |s| {
        s.boots_to = SandboxStatus::Stopped;
    });
    assert!(
        matches!(kind(&f.err), AppleError::OperationUncertain(_)),
        "{:#}",
        f.err
    );
    let text = format!("{:#}", f.err);
    assert!(text.contains("newer change"), "{text}");
    // The message names what superseded the change.
    assert!(text.contains("last operation coop-elsewhere"), "{text}");
    assert_eq!(set_calls(&f.calls).len(), 1, "no rollback was sent");
    assert_eq!(f.sim.borrow().sandboxes.values().next().unwrap().cpus, 7);
    assert!(Journal::try_load(&f.inst).unwrap().is_none());
}

/// A restart that fails and cannot be confirmed stopped gets no
/// rollback: nothing is changed under a VM that may be running.
#[test]
fn rollback_needs_a_confirmed_stop() {
    let f = failed_restart_after_change(vec![Fault::StopIgnored], |s| s.host_key = None);
    assert!(
        matches!(kind(&f.err), AppleError::OperationUncertain(_)),
        "{:#}",
        f.err
    );
    assert_eq!(set_calls(&f.calls).len(), 1, "no rollback was sent");
    let s = f.sim.borrow();
    let b = s.sandboxes.values().next().unwrap();
    assert_eq!((b.status, b.cpus), (SandboxStatus::Running, 6));
}

/// A rollback interrupted after the runtime applied it keeps its journal
/// and is reported uncertain; reconciling brings the sidecar to the
/// runtime's restored values without another update.
#[test]
fn interrupted_rollback_is_reconciled_from_its_journal() {
    let f = failed_restart_after_change(vec![Fault::RollbackFailsAfterApplying], |s| {
        s.boots_to = SandboxStatus::Stopped;
    });
    assert!(
        matches!(kind(&f.err), AppleError::OperationUncertain(_)),
        "{:#}",
        f.err
    );
    let sets = set_calls(&f.calls);
    assert_eq!(sets.len(), 2);
    assert_eq!(
        sets[1].1, sets[0].0,
        "the rollback names the change it undoes"
    );
    let prior = Resources {
        cpus: 2,
        memory_bytes: 2048 * 1024 * 1024,
    };
    // The rollback's own journal: its prior is the forward change.
    assert_eq!(
        Journal::try_load(&f.inst).unwrap().unwrap().op,
        JournalOp::SetResources {
            operation: op(&sets[1].0),
            prior: Resources {
                cpus: 6,
                memory_bytes: 6144 * 1024 * 1024
            },
        }
    );
    assert_eq!(MachineSidecar::load(&f.inst).unwrap().requested_cpus, 6);
    let (be, calls) = sim_backend(&f.cfg, &f.sim);
    AppleContainerBackend::recover_journal(be.runtime().unwrap(), &f.cfg, &f.inst).unwrap();
    assert_eq!(MachineSidecar::load(&f.inst).unwrap().resources(), prior);
    assert!(Journal::try_load(&f.inst).unwrap().is_none());
    assert!(mutations(&calls).is_empty());
}
/// The runtime refuses to inspect a sandbox whose staged disk update is
/// unreadable; destroying it still works, from the listing's status.
#[test]
fn destroy_does_not_need_inspect() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    let deleted = Rc::new(RefCell::new(false));
    let d = Rc::clone(&deleted);
    let (name, owner_id) = (sandbox_name(&owner), owner.id.as_str().to_string());
    let (be, calls) = backend(
        &cfg,
        Box::new(move |args| {
            if let Some(o) = version(args, true) {
                return o;
            }
            if starts(args, &["list"]) {
                return ok(&if *d.borrow() {
                    "[]".into()
                } else {
                    format!(r#"[{{"id":"{name}","status":"stopped","owner":"{owner_id}"}}]"#)
                });
            }
            if starts(args, &["inspect"]) {
                return fail("unreadable staged disk update");
            }
            if starts(args, &["delete"]) {
                *d.borrow_mut() = true;
                return ok("");
            }
            if starts(args, &["reconcile"]) {
                return ok("[]");
            }
            fail("unexpected")
        }),
    );
    be.destroy_instance(&cfg, &inst).unwrap();
    assert_eq!(mutations(&calls), ["delete"]);
    assert!(!inst.dir.exists());
}

/// A journal naming another sandbox than the instance record is an
/// identity conflict: nothing is reconciled, and the journal stays.
#[test]
fn recovery_refuses_a_journal_for_another_sandbox() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    let before = write_sidecar(&inst, &owner);
    Journal::begin(
        &inst,
        &owner,
        JournalOp::RestoreDisk {
            operation: op("coop-r"),
            prior_generation: 0,
        },
        MachineName::generate(&owner.id).unwrap(),
    )
    .unwrap();
    let json = with_last_operation(
        &with_generation(&inspect_json(&cfg, &owner, "stopped"), 1),
        &op("coop-r"),
    );
    let (be, calls) = backend(
        &cfg,
        Box::new(move |args| version(args, true).unwrap_or_else(|| ok(&json))),
    );
    let err =
        AppleContainerBackend::recover_journal(be.runtime().unwrap(), &cfg, &inst).unwrap_err();
    assert!(
        matches!(kind(&err), AppleError::IdentityConflict(_)),
        "{err:#}"
    );
    assert!(Journal::try_load(&inst).unwrap().is_some());
    assert_eq!(MachineSidecar::load(&inst).unwrap(), before);
    assert!(mutations(&calls).is_empty());
}

/// Each term of the rollback precondition refuses on its own: the runtime's
/// last operation, its resources, and the sidecar's resources must all still
/// be the forward change's. When all hold, the rollback is sent conditional
/// on that operation.
#[test]
fn rollback_precondition_terms_each_refuse() {
    let applied = Resources {
        cpus: 2,
        memory_bytes: 2048 * 1024 * 1024,
    };
    let prior = Resources { cpus: 4, ..applied };
    // (runtime's last operation, runtime cpus, sidecar cpus, refused)
    for (last, runtime_cpus, sidecar_cpus, refused) in [
        ("coop-other", 2, 2, true),
        ("coop-fwd", 5, 2, true),
        ("coop-fwd", 2, 5, true),
        ("coop-fwd", 2, 2, false),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let owner = Owner::load_or_init(&cfg).unwrap();
        let inst = test_inst(&cfg);
        let mut sidecar = write_sidecar(&inst, &owner);
        sidecar.requested_cpus = sidecar_cpus;
        sidecar.save(&inst).unwrap();
        let sim = Sim::new(&cfg, &owner);
        sim_sandbox(&sim, &owner, SandboxStatus::Stopped);
        {
            let mut s = sim.borrow_mut();
            let b = s.sandboxes.values_mut().next().unwrap();
            b.cpus = runtime_cpus;
            b.last_operation = Some(last.into());
        }
        let (be, calls) = sim_backend(&cfg, &sim);
        let undo = ResourceUpdate {
            operation: op("coop-fwd"),
            prior,
            applied,
        };
        let result = AppleContainerBackend::update_resources(
            be.runtime().unwrap(),
            &cfg,
            &inst,
            &owner,
            |_| prior,
            Some(&undo),
        );
        let case = format!("{last} {runtime_cpus} {sidecar_cpus}");
        assert!(Journal::try_load(&inst).unwrap().is_none(), "{case}");
        if refused {
            let err = result.err().unwrap();
            assert!(
                matches!(kind(&err), AppleError::OperationUncertain(_)),
                "{case}: {err:#}"
            );
            assert!(set_calls(&calls).is_empty(), "{case}");
        } else {
            result.unwrap();
            assert_eq!(set_calls(&calls).len(), 1, "{case}");
            assert_eq!(set_calls(&calls)[0].1, "coop-fwd");
            assert_eq!(MachineSidecar::load(&inst).unwrap().resources(), prior);
        }
    }
}

/// A grow the runtime reports as done is accepted only when its record
/// shows both this operation and the new size.
#[test]
fn grow_requires_its_own_committed_operation() {
    // (report this grow's operation, report the grown size)
    for (ours, grown) in [(false, true), (true, false)] {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let owner = Owner::load_or_init(&cfg).unwrap();
        let inst = test_inst(&cfg);
        write_sidecar(&inst, &owner);
        let sent: Rc<RefCell<Option<OperationId>>> = Rc::new(RefCell::new(None));
        let s = Rc::clone(&sent);
        let stopped = inspect_json(&cfg, &owner, "stopped");
        let (be, _) = backend(
            &cfg,
            Box::new(move |args| {
                if let Some(o) = version(args, true) {
                    return o;
                }
                if starts(args, &["inspect"]) {
                    let Some(sent) = &*s.borrow() else {
                        return ok(&stopped);
                    };
                    let bytes = if grown { 32u64 << 30 } else { 8u64 << 30 };
                    let json = stopped.replace(
                        "\"diskBytes\" : 8589934592",
                        &format!("\"diskBytes\" : {bytes}"),
                    );
                    let reported = if ours { sent.clone() } else { op("coop-other") };
                    return ok(&with_last_operation(&json, &reported));
                }
                if starts(args, &["grow"]) {
                    *s.borrow_mut() = Some(op(&flag(args, "--operation")));
                    return ok("{}");
                }
                fail("unexpected")
            }),
        );
        let err = be
            .resize_disk(
                &cfg,
                &StoppedInstance::new(inst.clone()),
                GiB::new(32).unwrap(),
            )
            .unwrap_err();
        assert!(
            matches!(kind(&err), AppleError::OperationUncertain(_)),
            "{ours} {grown}: {err:#}"
        );
    }
}

/// A grow that exits non-zero is uncertain when the runtime's record shows
/// it committed anyway or cannot be read, and a plain failure when the record
/// shows it did not commit.
#[test]
fn failed_grow_is_uncertain_only_when_it_committed() {
    // (runtime record after the grow: Some(committed) or unreadable)
    for after in [Some(true), Some(false), None] {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let owner = Owner::load_or_init(&cfg).unwrap();
        let inst = test_inst(&cfg);
        write_sidecar(&inst, &owner);
        let sent: Rc<RefCell<Option<OperationId>>> = Rc::new(RefCell::new(None));
        let s = Rc::clone(&sent);
        let stopped = inspect_json(&cfg, &owner, "stopped");
        let (be, calls) = backend(
            &cfg,
            Box::new(move |args| {
                if let Some(o) = version(args, true) {
                    return o;
                }
                if starts(args, &["inspect"]) {
                    return match (&*s.borrow(), after) {
                        (Some(_), None) => fail("inspect timed out"),
                        (Some(sent), Some(true)) => ok(&with_last_operation(&stopped, sent)),
                        _ => ok(&stopped),
                    };
                }
                if starts(args, &["grow"]) {
                    *s.borrow_mut() = Some(op(&flag(args, "--operation")));
                    return fail("resize2fs exited 1");
                }
                fail("unexpected")
            }),
        );
        let err = be
            .resize_disk(
                &cfg,
                &StoppedInstance::new(inst.clone()),
                GiB::new(32).unwrap(),
            )
            .unwrap_err();
        let uncertain = matches!(
            err.downcast_ref::<AppleError>(),
            Some(AppleError::OperationUncertain(_))
        );
        assert_eq!(uncertain, after != Some(false), "{after:?}: {err:#}");
        assert!(format!("{err:#}").contains("resize2fs exited 1"), "{err:#}");
        assert_eq!(mutations(&calls), ["grow"]);
    }
}

/// A restore is taken as done only when the runtime shows both a higher
/// generation and this restore's operation; otherwise the journal stays and
/// no new host key is authorized.
#[test]
fn restore_requires_its_own_committed_operation() {
    // (report this restore's operation, raise the generation)
    for (ours, raised) in [(false, true), (true, false)] {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let owner = Owner::load_or_init(&cfg).unwrap();
        let inst = test_inst(&cfg);
        write_sidecar(&inst, &owner);
        let image = ImageName::new("default").unwrap();
        manifest("local/coop-exp:fx", None)
            .save(&cfg, &image)
            .unwrap();
        let sent: Rc<RefCell<Option<OperationId>>> = Rc::new(RefCell::new(None));
        let s = Rc::clone(&sent);
        let stopped = inspect_json(&cfg, &owner, "stopped");
        let listed = format!(
            r#"[{{"reference":"local/coop-exp:fx","digest":"sha256:{}"}}]"#,
            "a".repeat(64)
        );
        let (be, _) = backend(
            &cfg,
            Box::new(move |args| {
                if let Some(o) = version(args, true) {
                    return o;
                }
                if starts(args, &["image", "list"]) {
                    return ok(&listed);
                }
                if starts(args, &["inspect"]) {
                    let Some(sent) = &*s.borrow() else {
                        return ok(&stopped);
                    };
                    let json = with_generation(&stopped, u64::from(raised));
                    let reported = if ours { sent.clone() } else { op("coop-other") };
                    return ok(&with_last_operation(&json, &reported));
                }
                if starts(args, &["restore"]) {
                    *s.borrow_mut() = Some(op(&flag(args, "--operation")));
                    return ok("{}");
                }
                fail("unexpected")
            }),
        );
        let err = be
            .restore_disk(&cfg, &StoppedInstance::new(inst.clone()), &image)
            .unwrap_err();
        let case = format!("{ours} {raised}");
        assert!(
            matches!(kind(&err), AppleError::OperationUncertain(_)),
            "{case}: {err:#}"
        );
        assert!(Journal::try_load(&inst).unwrap().is_some(), "{case}");
        assert!(
            !MachineSidecar::load(&inst).unwrap().reenroll_host_key,
            "{case}"
        );
    }
}

/// Setup reinstalls a maintenance image of another version, and removes
/// the store copy whether the install succeeds, fails, or reports
/// something other than what was installed.
#[test]
fn setup_installs_the_current_maintenance_image() {
    type Prepare = fn(&mut Sim);
    let cases: [(Prepare, Option<&str>); 3] = [
        (|s| s.maintenance = Some("0".into()), None),
        (
            |s| s.faults.push(Fault::MaintenanceInstallFails),
            Some("install failed"),
        ),
        (
            |s| s.faults.push(Fault::MaintenanceReportsOtherImage),
            Some("APPLE_RUNTIME_UNQUALIFIED"),
        ),
    ];
    for (prepare, error) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, owner, opts) = setup_env(tmp.path());
        let sim = Sim::new(&cfg, &owner);
        prepare(&mut sim.borrow_mut());
        let (be, calls) = sim_backend(&cfg, &sim);
        let result = be.setup(&cfg, &opts);
        let installs = calls
            .borrow()
            .iter()
            .filter(|c| starts(c, &["maintenance", "install"]))
            .count();
        assert_eq!(installs, 1, "{error:?}");
        let s = sim.borrow();
        assert!(
            !s.images.iter().any(|(r, _)| r.contains("-maintenance:")),
            "{error:?}: store copy left behind"
        );
        match error {
            None => {
                result.unwrap();
                assert_eq!(s.maintenance.as_deref(), Some(image::MAINTENANCE_VERSION));
            }
            Some(want) => {
                let err = result.unwrap_err();
                assert!(format!("{err:#}").contains(want), "{err:#}");
                assert!(
                    ImageManifest::try_load(&cfg, &opts.image)
                        .unwrap()
                        .is_none()
                );
            }
        }
    }
}
#[test]
fn destroying_images_releases_owned_content() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, owner, opts) = setup_env(tmp.path());
    let sim = Sim::new(&cfg, &owner);
    let (be, _) = sim_backend(&cfg, &sim);
    let missing = ImageName::new("missing").unwrap();
    assert!(be.destroy_image(&cfg, &missing).is_err());

    be.setup(&cfg, &opts).unwrap();
    be.destroy_image(&cfg, &opts.image).unwrap();
    assert!(!cfg.image_dir(&opts.image).exists());
    assert!(sim.borrow().images.is_empty());

    be.setup(&cfg, &opts).unwrap();
    be.destroy_shared(&cfg);
    assert!(!cfg.image_dir(&opts.image).exists());
    assert!(sim.borrow().images.is_empty());
}

/// Without console output (unreadable or blank), a boot failure says
/// where to look instead.
#[test]
fn boot_failure_without_a_console_log_says_so() {
    type Blank = fn(&mut Sim);
    let cases: [Blank; 2] = [|s| s.faults.push(Fault::LogsFail), |s| s.console = "  \n"];
    for blank in cases {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, owner, opts) = setup_env(tmp.path());
        let sim = Sim::new(&cfg, &owner);
        sim.borrow_mut().boots_to = SandboxStatus::Stopped;
        blank(&mut sim.borrow_mut());
        let (be, _) = sim_backend(&cfg, &sim);
        let err = be.setup(&cfg, &opts).unwrap_err();
        assert!(
            format!("{err:#}").contains("Console log unavailable"),
            "{err:#}"
        );
    }
}

/// A restart that never reports a running configuration fails at the
/// boot deadline, not before, and leaves the sandbox stopped.
#[test]
fn start_waits_for_the_boot_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_cfg(tmp.path());
    let owner = Owner::load_or_init(&cfg).unwrap();
    let inst = test_inst(&cfg);
    write_sidecar(&inst, &owner);
    let sim = Sim::new(&cfg, &owner);
    sim_sandbox(&sim, &owner, SandboxStatus::Stopped);
    sim.borrow_mut().settle_inspects = u32::MAX;
    let (be, calls) = sim_backend(&cfg, &sim);
    let started = Instant::now();
    let err = be.start_existing(&cfg, &inst).unwrap_err();
    assert!(started.elapsed() >= Duration::from_secs(1), "gave up early");
    assert!(matches!(kind(&err), AppleError::BootTimeout(_)), "{err:#}");
    assert_eq!(mutations(&calls), ["start", "stop"]);
}

/// Services get their own window after boot; setup waits it out.
#[test]
fn inactive_services_fail_only_after_their_window() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, owner, opts) = setup_env(tmp.path());
    let sim = Sim::new(&cfg, &owner);
    sim.borrow_mut().faults = vec![Fault::ServicesInactive];
    let (be, _) = sim_backend(&cfg, &sim);
    let started = Instant::now();
    let err = be.setup(&cfg, &opts).unwrap_err();
    assert!(started.elapsed() >= Duration::from_secs(1), "gave up early");
    let text = format!("{err:#}");
    assert!(text.contains("(active, activating)"), "{text}");
}

#[test]
fn canonical_path_resolves_through_the_existing_ancestor() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().canonicalize().unwrap();
    assert_eq!(canonical_path(&tmp.path().join("a/b")), real.join("a/b"));
}
