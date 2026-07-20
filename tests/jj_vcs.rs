//! Integration test: coop's VCS-aware sync must not destroy Jujutsu (`jj`)
//! repositories, and its dirty checks must understand jj's working-copy model.
//!
//! This reproduces the no-VM experiment from the jj-support investigation. It
//! stands a plain local directory in for the guest and drives coop's *actual*
//! transfer filters ([`coop::vcs::rsync_vcs_filters`] /
//! [`coop::vcs::tar_vcs_excludes`]) and dirty detection
//! ([`coop::vcs::working_copy_dirty`]) against real jj repos — the same code
//! paths `coop push` / `coop pull` use, minus the SSH transport.
//!
//! The whole suite skips gracefully when `jj` is not installed, so it never
//! hard-fails a host or CI runner that lacks the binary. rsync-dependent cases
//! additionally skip when `rsync` is absent.
#![expect(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::io::Write as _;
use std::path::Path;
use std::process::Command;

use coop::vcs::{Vcs, rsync_vcs_filters, tar_vcs_excludes, working_copy_dirty};

/// Skip (return early with a printed note) if `bin` is not on PATH.
macro_rules! require_bin {
    ($bin:literal) => {
        if !have_bin($bin) {
            eprintln!("skipping: `{}` not installed", $bin);
            return;
        }
    };
}

fn have_bin(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `jj` in `dir` with a throwaway identity, asserting success.
fn jj(dir: &Path, args: &[&str]) {
    // No `--repository`: it points at an *existing* repo and so breaks
    // `git init`. Repo discovery from `current_dir` covers every call.
    let out = Command::new("jj")
        .arg("--config")
        .arg("user.name=coop-test")
        .arg("--config")
        .arg("user.email=coop-test@example.com")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run jj");
    assert!(
        out.status.success(),
        "jj {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `jj git init` (colocated by default since jj 0.30) with two commits and a
/// dirty working copy (an uncommitted `file2.txt` snapshot in `@`).
fn make_colocated(dir: &Path) {
    jj(dir, &["git", "init", "--colocate"]);
    std::fs::write(dir.join("file1.txt"), "hello").unwrap();
    jj(dir, &["describe", "-m", "first"]);
    jj(dir, &["new", "-m", "second"]);
    std::fs::write(dir.join("file2.txt"), "world").unwrap();
}

/// `jj git init --no-colocate`: only `.jj/`, git store hidden inside it, no
/// top-level `.git/`.
fn make_noncolocated(dir: &Path) {
    jj(dir, &["git", "init", "--no-colocate"]);
    std::fs::write(dir.join("file1.txt"), "hello").unwrap();
    jj(dir, &["describe", "-m", "first"]);
    jj(dir, &["new", "-m", "second"]);
    std::fs::write(dir.join("file2.txt"), "world").unwrap();
}

/// jj is healthy in `dir` iff `jj status` succeeds.
fn jj_healthy(dir: &Path) -> bool {
    Command::new("jj")
        .arg("--repository")
        .arg(dir)
        .arg("status")
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Simulate coop's rsync transfer between two local dirs with the exact
/// filters coop uses (see `workspace::rsync_base_args`).
fn rsync_transfer(src: &Path, dst: &Path, exclude_git: bool) {
    let mut args: Vec<String> = vec!["-a".into()];
    args.extend(rsync_vcs_filters(exclude_git));
    args.push("--filter=:- .gitignore".into());
    for exc in [
        "node_modules/",
        "target/",
        "__pycache__/",
        ".venv/",
        ".coop/",
    ] {
        args.push(format!("--exclude={exc}"));
    }
    args.push("--delete".into());
    args.push(format!("{}/", src.display()));
    args.push(format!("{}/", dst.display()));
    let status = Command::new("rsync")
        .args(&args)
        .status()
        .expect("run rsync");
    assert!(status.success(), "rsync failed with args {args:?}");
}

/// Simulate coop's tar-pipe transfer between two local dirs (see
/// `workspace::tar_pipe_transfer_to`), honoring the GNU-only
/// `--exclude-vcs-ignores`.
fn tar_transfer(src: &Path, dst: &Path, exclude_git: bool) {
    let mut excludes: Vec<String> = [
        "node_modules/",
        "target/",
        "__pycache__/",
        ".venv/",
        ".coop/",
    ]
    .iter()
    .map(|e| format!("--exclude={e}"))
    .collect();
    excludes.extend(tar_vcs_excludes(exclude_git));
    if cfg!(not(target_os = "macos")) {
        excludes.push("--exclude-vcs-ignores".into());
    }
    let mut create = Command::new("tar");
    create.args(["cf", "-"]);
    create.args(&excludes);
    create.arg("-C").arg(src).arg(".");
    let archive = create.output().expect("tar create");
    assert!(archive.status.success(), "tar create failed");

    let mut extract = Command::new("tar")
        .arg("xf")
        .arg("-")
        .arg("-C")
        .arg(dst)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("tar extract spawn");
    extract
        .stdin
        .take()
        .unwrap()
        .write_all(&archive.stdout)
        .unwrap();
    assert!(extract.wait().expect("tar extract").success());
}

#[test]
fn detects_colocated_and_noncolocated_jj() {
    require_bin!("jj");
    let tmp = tempfile::tempdir().unwrap();

    let colo = tmp.path().join("colo");
    std::fs::create_dir(&colo).unwrap();
    make_colocated(&colo);
    assert_eq!(Vcs::detect(&colo), Vcs::Jj, "colocated must detect as jj");
    assert!(colo.join(".git").exists(), "colocated has a top-level .git");

    let nono = tmp.path().join("nono");
    std::fs::create_dir(&nono).unwrap();
    make_noncolocated(&nono);
    assert_eq!(
        Vcs::detect(&nono),
        Vcs::Jj,
        "non-colocated must detect as jj"
    );
    assert!(
        !nono.join(".git").exists(),
        "non-colocated must NOT have a top-level .git — the case a \
         .git-only detector misses entirely"
    );
}

#[test]
fn dirty_check_sees_jj_working_copy_changes() {
    require_bin!("jj");
    let tmp = tempfile::tempdir().unwrap();

    for (name, mk) in [
        ("colo", make_colocated as fn(&Path)),
        ("nono", make_noncolocated as fn(&Path)),
    ] {
        let dir = tmp.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        mk(&dir);
        // file2.txt is an uncommitted working-copy change jj has snapshotted.
        let dirty = working_copy_dirty(&dir).expect("dirty check ok");
        assert!(
            dirty.as_deref().is_some_and(|s| s.contains("file2.txt")),
            "{name}: working_copy_dirty must report file2.txt, got {dirty:?}"
        );

        // A clean working copy (commit the change into a fresh @) reports clean.
        jj(&dir, &["describe", "-m", "capture file2"]);
        jj(&dir, &["new"]);
        let clean = working_copy_dirty(&dir).expect("dirty check ok");
        assert!(clean.is_none(), "{name}: expected clean, got {clean:?}");
    }
}

#[test]
fn rsync_roundtrip_preserves_jj() {
    require_bin!("jj");
    require_bin!("rsync");
    let tmp = tempfile::tempdir().unwrap();

    for (name, mk) in [
        ("colo", make_colocated as fn(&Path)),
        ("nono", make_noncolocated as fn(&Path)),
    ] {
        let src = tmp.path().join(format!("{name}_src"));
        let guest = tmp.path().join(format!("{name}_guest"));
        let back = tmp.path().join(format!("{name}_back"));
        for d in [&src, &guest, &back] {
            std::fs::create_dir(d).unwrap();
        }
        mk(&src);

        rsync_transfer(&src, &guest, false); // push
        rsync_transfer(&guest, &back, false); // pull

        assert!(
            back.join(".jj/repo/store/type").exists(),
            "{name}: rsync roundtrip dropped the jj store — the destructive \
             bug this fix prevents"
        );
        assert!(
            jj_healthy(&back),
            "{name}: jj is broken on the rsync-roundtripped copy"
        );
    }
}

#[test]
fn tar_roundtrip_preserves_jj() {
    require_bin!("jj");
    require_bin!("tar");
    let tmp = tempfile::tempdir().unwrap();

    for (name, mk) in [
        ("colo", make_colocated as fn(&Path)),
        ("nono", make_noncolocated as fn(&Path)),
    ] {
        let src = tmp.path().join(format!("{name}_src"));
        let guest = tmp.path().join(format!("{name}_guest"));
        let back = tmp.path().join(format!("{name}_back"));
        for d in [&src, &guest, &back] {
            std::fs::create_dir(d).unwrap();
        }
        mk(&src);

        tar_transfer(&src, &guest, false); // push
        tar_transfer(&guest, &back, false); // pull

        assert!(
            jj_healthy(&back),
            "{name}: jj is broken on the tar-roundtripped copy"
        );
    }
}

#[test]
fn exclude_git_drops_jj_metadata() {
    require_bin!("jj");
    require_bin!("rsync");
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let guest = tmp.path().join("guest");
    std::fs::create_dir(&src).unwrap();
    std::fs::create_dir(&guest).unwrap();
    make_noncolocated(&src);

    rsync_transfer(&src, &guest, true); // exclude_git=true

    assert!(
        !guest.join(".jj").exists(),
        "exclude_git must drop .jj/ just as it drops .git/"
    );
    assert!(
        guest.join("file1.txt").exists(),
        "exclude_git must still carry the tracked working-tree files"
    );
}
