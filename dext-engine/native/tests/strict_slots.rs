#![cfg(all(target_os = "linux", feature = "strict"))]
//! Own test binary with a single test: the slot cap is per user across processes,
//! so sandboxes from parallel tests would make these timings meaningless.
use dext_native::strict::StrictExecutor;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

fn executor() -> StrictExecutor {
    let path = Path::new("/usr/bin/bwrap");
    let approved = Sha256::digest(std::fs::read(path).unwrap()).into();
    StrictExecutor::open(path, approved).unwrap()
}
fn run(script: &str) -> std::io::Result<dext_native::strict::SealedOutput> {
    executor().execute(
        &BTreeMap::new(),
        &["/bin/sh".into(), "-c".into(), script.into()],
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
}

#[test]
fn concurrent_sandboxes_are_capped_and_queue_wait_counts_against_the_deadline() {
    capped_and_queued();
    queue_wait_counts_against_deadline();
    slots_are_shared_across_processes_and_freed_by_holder_death();
}

/// Slots are flocked files, so another owner process holding all of them blocks
/// this one, and SIGKILL of that holder frees them without cleanup code.
fn slots_are_shared_across_processes_and_freed_by_holder_death() {
    use dext_native::strict::MAX_CONCURRENT;
    struct Holders(Vec<std::process::Child>);
    impl Drop for Holders {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    assert!(run("true").unwrap().success());
    let directory = format!("/tmp/spindle-strict-slots-{}", unsafe { libc::geteuid() });
    let mut holders = Holders(vec![]);
    for index in 0..MAX_CONCURRENT {
        let ready = format!("{directory}/held-{index}-{}", std::process::id());
        let _ = std::fs::remove_file(&ready);
        holders.0.push(
            // One process owns the locked description: `flock CMD` would fork and
            // leave the lock with an orphaned child after the parent is killed.
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "exec 9>>{directory}/slot-{index}; /usr/bin/flock -x 9; touch {ready}; exec sleep 30"
                ))
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !Path::new(&ready).exists() {
            assert!(Instant::now() < deadline, "holder {index} never locked");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::remove_file(&ready).unwrap();
    }
    let error = executor()
        .execute(
            &BTreeMap::new(),
            &["/bin/true".into()],
            Duration::from_millis(400),
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert!(error.to_string().contains("deadline"), "{error}");
    unsafe { libc::kill(holders.0[0].id() as i32, libc::SIGKILL) };
    holders.0[0].wait().unwrap();
    assert!(
        run("true").unwrap().success(),
        "killed holder kept its slot"
    );
}

fn capped_and_queued() {
    use dext_native::strict::MAX_CONCURRENT;
    let started = Instant::now();
    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..MAX_CONCURRENT * 2)
            .map(|_| scope.spawn(|| run("sleep 0.4")))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(
        results
            .iter()
            .all(|r| r.as_ref().is_ok_and(|o| o.success()))
    );
    assert!(
        started.elapsed() >= Duration::from_millis(800),
        "more than MAX_CONCURRENT sandboxes ran at once"
    );
    let interrupt = AtomicBool::new(true);
    assert!(
        executor()
            .execute(
                &BTreeMap::new(),
                &["/bin/true".into()],
                Duration::from_secs(1),
                &interrupt
            )
            .is_err()
    );
}

fn queue_wait_counts_against_deadline() {
    use dext_native::strict::MAX_CONCURRENT;
    std::thread::scope(|scope| {
        for _ in 0..MAX_CONCURRENT {
            let _ = scope.spawn(|| run("sleep 1.5"));
        }
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        let error = executor()
            .execute(
                &BTreeMap::new(),
                &["/bin/true".into()],
                Duration::from_millis(700),
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert!(error.to_string().contains("deadline"), "{error}");
        assert!(started.elapsed() < Duration::from_millis(1200));
    });
}
