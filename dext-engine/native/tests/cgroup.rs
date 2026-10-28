#![cfg(all(feature = "native", target_os = "linux"))]
use dext_native::cgroup::{OwnedCgroup, TreeCleanup, delegated_root};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn root() -> Option<std::path::PathBuf> {
    let root = delegated_root();
    if root.is_none() {
        assert!(
            std::env::var_os("SPINDLE_REQUIRE_CGROUP").is_none(),
            "SPINDLE_REQUIRE_CGROUP set but no delegated cgroup"
        );
        eprintln!(
            "skipping: no delegated cgroup v2 (run under systemd-run --user --scope -p Delegate=yes with DEXT_CGROUP_DELEGATED=1)"
        );
    }
    root
}

fn name(tag: &str) -> String {
    format!("dext-test-{tag}-{}", std::process::id())
}

#[test]
fn detached_descendants_are_killed_and_kernel_proves_empty() {
    let Some(root) = root() else { return };
    let group = OwnedCgroup::create(&root, &name("tree")).unwrap();
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("setsid sh -c 'sleep 300' & (sleep 300 &) ; sleep 300 & echo ready; wait")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    unsafe { command.pre_exec(group.enter_hook()) };
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while group.process_count().unwrap_or(0) < 4 {
        assert!(Instant::now() < deadline, "descendants did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(group.populated(), Some(true));
    assert_eq!(
        group.terminate(Duration::from_secs(5)),
        TreeCleanup::Emptied
    );
    assert!(!child.wait().unwrap().success());
    assert_eq!(group.process_count(), Some(0));
    let path = group.path().to_path_buf();
    group.remove().unwrap();
    assert!(!path.exists());
}

#[test]
fn existing_cgroup_is_never_adopted_and_bad_names_refused() {
    let Some(root) = root() else { return };
    let first = OwnedCgroup::create(&root, &name("dup")).unwrap();
    assert!(OwnedCgroup::create(&root, &name("dup")).is_err());
    assert!(OwnedCgroup::create(&root, "../escape").is_err());
    assert!(OwnedCgroup::create(&root, "UPPER").is_err());
    first.remove().unwrap();
}

#[test]
fn undelegated_parent_cannot_produce_evidence() {
    assert!(
        OwnedCgroup::create(std::path::Path::new("/sys/fs/cgroup/init.scope"), "dext-x").is_err()
    );
    assert!(OwnedCgroup::create(std::path::Path::new("/nonexistent"), "dext-x").is_err());
}

#[test]
fn drop_kills_members_of_an_unremoved_group() {
    let Some(root) = root() else { return };
    let group = OwnedCgroup::create(&root, &name("drop")).unwrap();
    let path = group.path().to_path_buf();
    let mut command = Command::new("/bin/sleep");
    command.arg("300");
    unsafe { command.pre_exec(group.enter_hook()) };
    let mut child = command.spawn().unwrap();
    drop(group);
    assert!(!child.wait().unwrap().success());
    assert!(!path.exists());
}

/// With delegation every strict run has its own capped cgroup, and ending the run
/// kills descendants that left its process group (an unisolated run's setsid'ed job).
#[cfg(feature = "strict")]
#[test]
fn strict_runs_get_a_capped_cgroup_that_ends_detached_descendants() {
    use dext_native::strict::StrictExecutor;
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicBool;
    if root().is_none() {
        return;
    }
    let script = "group=/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup); cat $group/pids.max; cat $group/../memory.max; setsid sh -c 'echo $$ > pid; exec sleep 300' & sleep 0.2; echo started";
    let output = StrictExecutor::host()
        .execute(
            &BTreeMap::new(),
            &["/bin/sh".into(), "-c".into(), script.into()],
            Duration::from_secs(20),
            &AtomicBool::new(false),
        )
        .unwrap();
    let stdout = String::from_utf8_lossy(output.stdout()).to_string();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.first(),
        Some(&"1024"),
        "{stdout}{}",
        String::from_utf8_lossy(output.stderr())
    );
    assert_ne!(lines.get(1), Some(&"max"), "aggregate memory cap: {stdout}");
    assert_eq!(lines.last(), Some(&"started"));
    let pid: i32 = String::from_utf8_lossy(&output.files()["pid"].bytes)
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "detached descendant outlived the run"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
