#![cfg(all(target_os = "linux", feature = "strict"))]
use dext_native::strict::{
    FileValue, Input, RunHost, RunOptions, SealedEntry, StrictExecutor, TreeLimits, run_failure,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
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
fn namespace_candidate_is_private_and_output_is_sealed_after_exit() {
    let mut files = BTreeMap::new();
    files.insert(
        "input.txt".into(),
        FileValue {
            bytes: b"original".to_vec(),
            executable: false,
        },
    );
    let executor = executor();
    let output = executor.execute(&files, &["/bin/sh".into(), "-c".into(), "cat input.txt; printf changed > input.txt; mkdir src; printf code > src/main.rs; printf diagnostic >&2".into()], Duration::from_secs(5), &AtomicBool::new(false)).unwrap();
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    assert_eq!(output.stdout(), b"original");
    assert_eq!(output.stderr(), b"diagnostic");
    assert_eq!(output.files()["input.txt"].bytes, b"changed");
    assert_eq!(output.files()["src/main.rs"].bytes, b"code");
    assert_eq!(files["input.txt"].bytes, b"original");
    assert!(!format!("{output:?}").contains("diagnostic"));
}

#[test]
fn strict_child_cannot_read_host_files_write_runtime_or_reach_outside_its_loopback() {
    let host = std::env::temp_dir().join(format!("spindle-host-secret-{}", std::process::id()));
    std::fs::write(&host, b"fixture not a real secret").unwrap();
    let script = format!(
        "import os,socket,threading\nassert not os.path.exists({host:?})\nassert not os.path.exists('/home')\nassert not os.path.exists('/etc/passwd')\nassert not os.path.exists('/proc/1/root/home')\nassert set(os.environ)=={{'HOME','PATH','TMPDIR','PWD','LC_CTYPE'}}\nassert os.environ['PWD']=='/workspace' and os.environ['LC_CTYPE']=='C.UTF-8'\nfor domain in [socket.AF_INET,socket.AF_INET6,socket.AF_UNIX]:\n socket.socket(domain).close()\nsocket.socket(socket.AF_NETLINK, socket.SOCK_RAW, 0).close()\ntry: socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, 15); raise AssertionError('uevent netlink allowed')\nexcept PermissionError: pass\nserver=socket.socket(); server.bind(('127.0.0.1',0)); server.listen()\nthreading.Thread(target=lambda: server.accept()[0].sendall(b'loopback'),daemon=True).start()\nclient=socket.create_connection(server.getsockname()); assert client.recv(8)==b'loopback'\nunix=socket.socket(socket.AF_UNIX); unix.bind('/tmp/s'); unix.listen()\ntry: socket.create_connection(('1.1.1.1',53),timeout=2); raise AssertionError('outside reachable')\nexcept OSError: pass\ntry: open('/usr/spindle-escape','w'); raise AssertionError('runtime write allowed')\nexcept OSError: pass\nprint('isolated')"
    );
    let output = executor()
        .execute(
            &BTreeMap::new(),
            &["/usr/bin/python3".into(), "-c".into(), script],
            Duration::from_secs(10),
            &AtomicBool::new(false),
        )
        .unwrap();
    std::fs::remove_file(&host).unwrap();
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    assert_eq!(output.stdout(), b"isolated\n");
}

/// Kernel keyrings, perf events, userfaultfd and the new mount API are refused.
#[cfg(target_arch = "x86_64")]
#[test]
fn strict_seccomp_refuses_keyrings_perf_userfaultfd_and_mount_api() {
    let script = "import ctypes,os\nlibc=ctypes.CDLL(None,use_errno=True)\nfor nr in [250,248,249,298,323,430,428]:\n assert libc.syscall(nr,0,0,0,0,0)==-1 and ctypes.get_errno()==1, nr\nprint('refused')";
    let output = executor()
        .execute(
            &BTreeMap::new(),
            &["/usr/bin/python3".into(), "-c".into(), script.into()],
            Duration::from_secs(10),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    assert_eq!(output.stdout(), b"refused\n");
}

/// Runtimes that reserve large address ranges (WebAssembly guard regions, worker
/// isolates, npm) start under the run's limits.
#[test]
fn strict_runtimes_reserving_address_space_still_start() {
    if !Path::new("/usr/bin/node").exists() {
        eprintln!("skipped: no node");
        return;
    }
    let script = "new WebAssembly.Memory({initial:1}); const {Worker}=require('worker_threads'); const w=new Worker('1',{eval:true}); w.on('exit',()=>console.log('started'))";
    let output = executor()
        .execute(
            &BTreeMap::new(),
            &["/usr/bin/node".into(), "-e".into(), script.into()],
            Duration::from_secs(20),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    assert_eq!(output.stdout(), b"started\n");
}

#[test]
fn strict_exec_does_not_inherit_ambient_file_descriptors() {
    use std::os::fd::AsRawFd;
    let host = std::fs::File::open("/etc/passwd").unwrap();
    let leaked_fd = unsafe { libc::fcntl(host.as_raw_fd(), libc::F_DUPFD, 100) };
    assert!(leaked_fd >= 100);
    let output = run("for fd in /proc/self/fd/*; do readlink \"$fd\"; done").unwrap();
    unsafe {
        libc::close(leaked_fd);
    }
    assert!(!String::from_utf8_lossy(output.stdout()).contains("passwd"));
    assert!(!String::from_utf8_lossy(output.stdout()).contains("spindle-candidate"));
    assert!(!String::from_utf8_lossy(output.stdout()).contains("spindle-isolation"));
}

/// Links, hard links, Git metadata and Linux names seal; links are never followed.
#[test]
fn links_git_metadata_and_linux_names_seal_without_following_links() {
    let files = BTreeMap::from([(
        "kept.txt".to_string(),
        FileValue {
            bytes: b"kept".to_vec(),
            executable: false,
        },
    )]);
    let script = "git init -q repo && printf hi > repo/a && ln -s /etc/passwd escaped && ln -s kept.txt alias && printf data > a && ln a b && printf x > 'with space@1+2.txt' && printf a > A && mkdir -p empty/deeper && printf ok";
    let output = executor()
        .execute(
            &files,
            &["/bin/bash".into(), "-c".into(), script.into()],
            Duration::from_secs(10),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    let entries = output.entries();
    assert_eq!(entries["escaped"], SealedEntry::Link("/etc/passwd".into()));
    assert_eq!(entries["alias"], SealedEntry::Link("kept.txt".into()));
    assert_eq!(output.files()["a"].bytes, b"data");
    assert_eq!(output.files()["b"].bytes, b"data");
    assert_eq!(output.files()["with space@1+2.txt"].bytes, b"x");
    assert_eq!(output.files()["A"].bytes, b"a");
    assert!(output.files().contains_key("repo/.git/HEAD"));
    assert_eq!(entries["empty/deeper"], SealedEntry::Directory);
    assert!(
        entries.contains_key("repo/.git/refs/tags"),
        "empty Git directories survive"
    );
    assert_eq!(
        entries["kept.txt"],
        SealedEntry::Unchanged { executable: false }
    );
    assert!(!output.files().contains_key("escaped"));

    // The sealed tree comes back in: links are recreated (not followed) and Git works.
    let mut tree: BTreeMap<String, Input> = BTreeMap::new();
    for (path, entry) in entries {
        tree.insert(
            path.clone(),
            match entry {
                SealedEntry::Unchanged { executable } | SealedEntry::File { executable, .. } => {
                    Input::File {
                        size: output.files()[path].bytes.len() as u64,
                        executable: *executable,
                    }
                }
                SealedEntry::Link(target) => Input::Link(target.clone()),
                SealedEntry::Directory => Input::Directory,
            },
        );
    }
    struct Back<'a>(&'a BTreeMap<String, FileValue>);
    impl RunHost for Back<'_> {
        fn load(&mut self, path: &str, out: &mut dyn std::io::Write) -> std::io::Result<()> {
            out.write_all(&self.0[path].bytes)
        }
    }
    let again = executor()
        .execute_tree(
            &tree,
            &[
                "/bin/bash".into(),
                "-c".into(),
                "readlink escaped && cat alias && git -C repo status --short && printf more >> a"
                    .into(),
            ],
            Duration::from_secs(10),
            &AtomicBool::new(false),
            RunOptions::default(),
            &[],
            false,
            &mut Back(output.files()),
        )
        .unwrap();
    assert!(
        again.success(),
        "{}",
        String::from_utf8_lossy(again.stderr())
    );
    assert_eq!(again.stdout(), b"/etc/passwd\nkept?? a\n");
    assert_eq!(
        again.entries()["kept.txt"],
        SealedEntry::Unchanged { executable: false }
    );
    assert_eq!(
        again.entries()["a"],
        SealedEntry::File {
            size: 8,
            executable: false
        }
    );
    assert_eq!(again.read("a").unwrap(), b"datamore");
    assert!(
        again.read("kept.txt").is_err(),
        "unchanged files are not read"
    );
}

/// What cannot be sealed ends the run as a contained failure that says why and keeps
/// its output, so hosts record it instead of leaving the call unresolved.
#[test]
fn unsealable_output_fails_with_its_reason_and_output() {
    let small = executor().with_tree_limits(TreeLimits {
        entries: 20,
        file: 64 * 1024,
        tree: 100 * 1024,
    });
    // Sockets and pipes are left out with a note; the rest of the run is kept.
    let kept = small
        .execute(
            &BTreeMap::new(),
            &[
                "/bin/bash".into(),
                "-c".into(),
                "mkfifo fifo; echo kept > a.txt; chmod 000 a.txt; mkdir locked; echo in > locked/b.txt; chmod 000 locked; echo made".into(),
            ],
            Duration::from_secs(10),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(kept.stdout(), b"made\n");
    assert!(
        String::from_utf8_lossy(kept.stderr())
            .contains("not kept: special files (sockets, pipes, devices) `fifo`"),
        "{}",
        String::from_utf8_lossy(kept.stderr())
    );
    assert!(!kept.files().contains_key("fifo"));
    assert_eq!(
        kept.files()["a.txt"].bytes,
        b"kept\n",
        "an unreadable file is kept"
    );
    assert_eq!(
        kept.files()["locked/b.txt"].bytes,
        b"in\n",
        "an unreadable directory too"
    );
    for (script, reason) in [
        (
            "for i in $(seq 30); do : > f$i; done; echo made",
            "more than 20 workspace entries",
        ),
        (
            "head -c 70000 /dev/zero > big; echo made",
            "more than the 64.0 KiB file limit",
        ),
        (
            "for i in 1 2; do head -c 60000 /dev/zero > f$i; done; echo made",
            "more than 100.0 KiB",
        ),
        ("printf x > $'bad\\xff'; echo made", "not UTF-8"),
    ] {
        let error = small
            .execute(
                &BTreeMap::new(),
                &["/bin/bash".into(), "-c".into(), script.into()],
                Duration::from_secs(10),
                &AtomicBool::new(false),
            )
            .expect_err(script);
        let failure = run_failure(&error).expect("contained failure");
        assert!(
            failure.reason.contains(reason),
            "{script}: {}",
            failure.reason
        );
        assert_eq!(failure.stdout, b"made\n", "{script}");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}

/// bubblewrap's own status report, not stderr text, decides whether the sandbox
/// failed to start.
#[test]
fn sandbox_setup_failure_is_told_apart_from_a_failing_command() {
    let output = run("echo 'bwrap: not really' >&2; exit 3").unwrap();
    assert!(!output.success());
    assert_eq!(output.stderr(), b"bwrap: not really\n");
    let error = executor()
        .execute(
            &BTreeMap::new(),
            &["/nonexistent/program".into()],
            Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap_err();
    let failure = run_failure(&error).expect("contained failure");
    assert!(
        failure.reason.contains("setup refused"),
        "{}",
        failure.reason
    );
}

#[test]
fn killed_runs_keep_the_output_printed_before_the_kill() {
    let error = executor()
        .execute(
            &BTreeMap::new(),
            &[
                "/bin/sh".into(),
                "-c".into(),
                "echo started; sleep 20".into(),
            ],
            Duration::from_millis(500),
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(run_failure(&error).unwrap().stdout, b"started\n");
}

#[test]
fn process_namespace_stops_detached_descendant_before_seal() {
    let started = Instant::now();
    let output = run("setsid /bin/sh -c 'sleep 1; printf late > late.txt' >/dev/null 2>&1 & printf early > early.txt; exit 0").unwrap();
    assert!(output.success());
    assert_eq!(output.files()["early.txt"].bytes, b"early");
    assert!(!output.files().contains_key("late.txt"));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn strict_cancellation_timeout_and_output_flood_return_no_candidate() {
    let executor = executor();
    let interrupt = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            interrupt.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        assert!(
            executor
                .execute(
                    &BTreeMap::new(),
                    &[
                        "/bin/sh".into(),
                        "-c".into(),
                        "setsid /bin/sh -c 'sleep 20' & wait".into()
                    ],
                    Duration::from_secs(5),
                    &interrupt
                )
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    });
    assert!(
        executor
            .execute(
                &BTreeMap::new(),
                &["/bin/sleep".into(), "10".into()],
                Duration::from_millis(100),
                &AtomicBool::new(false)
            )
            .is_err()
    );
    assert!(run("yes output").is_err());
}

#[test]
fn helper_identity_and_invalid_inputs_refuse_before_execution() {
    assert!(StrictExecutor::open(Path::new("/usr/bin/bwrap"), [0; 32]).is_err());
    let mut files = BTreeMap::new();
    files.insert(
        "../escape".into(),
        FileValue {
            bytes: vec![],
            executable: false,
        },
    );
    assert!(
        executor()
            .execute(
                &files,
                &["/bin/true".into()],
                Duration::from_secs(5),
                &AtomicBool::new(false)
            )
            .is_err()
    );
    assert!(
        executor()
            .execute(
                &BTreeMap::new(),
                &[],
                Duration::from_secs(5),
                &AtomicBool::new(false)
            )
            .is_err()
    );
}

fn candidates() -> Vec<std::path::PathBuf> {
    std::fs::read_dir("/tmp")
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix("spindle-candidate-"))
                .is_some_and(|suffix| suffix.len() == 32)
        })
        .map(|entry| entry.path())
        .collect()
}

#[test]
fn strict_owner_child() {
    if std::env::var_os("SPINDLE_STRICT_OWNER").is_none() {
        return;
    }
    let _ = run("while :; do date +%s%N > tick; sleep 0.05; done");
}

/// Tests that sweep hold this: one test's sweep may reclaim another test's dead
/// candidate, which would break that test's own reclaim assertion.
static SWEEP: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn owner_death_stops_the_namespace_and_frees_the_candidate_for_sweep() {
    let _sweep = SWEEP.lock().unwrap_or_else(|error| error.into_inner());
    let mut owner = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "strict_owner_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("SPINDLE_STRICT_OWNER", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let candidate = loop {
        if let Some(path) = candidates()
            .into_iter()
            .find(|path| path.join("w/tick").exists())
        {
            break path;
        }
        assert!(Instant::now() < deadline, "owner never started the sandbox");
        std::thread::sleep(Duration::from_millis(20));
    };
    dext_native::strict::sweep_abandoned_candidates().unwrap();
    assert!(candidate.exists(), "live candidate must not be swept");
    unsafe { libc::kill(owner.id() as i32, libc::SIGKILL) };
    owner.wait().unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let first = std::fs::read(candidate.join("w/tick")).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let second = std::fs::read(candidate.join("w/tick")).unwrap();
    assert_eq!(first, second, "sandbox kept writing after owner death");
    assert!(dext_native::strict::sweep_abandoned_candidates().unwrap() >= 1);
    assert!(!candidate.exists(), "abandoned candidate not reclaimed");
}

#[test]
fn candidate_disk_flood_is_refused_before_seal() {
    let started = Instant::now();
    let error = executor()
        .with_tree_limits(TreeLimits {
            entries: 1000,
            file: 1 << 20,
            tree: 4 << 20,
        })
        .execute(
            &BTreeMap::new(),
            &[
                "/bin/sh".into(),
                "-c".into(),
                "i=0; while :; do head -c 1000000 /dev/zero > f$i; i=$((i+1)); done".into(),
            ],
            Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert!(error.to_string().contains("disk limit"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn permission_locked_candidates_are_still_removed() {
    use std::os::unix::fs::PermissionsExt;
    let _sweep = SWEEP.lock().unwrap_or_else(|error| error.into_inner());
    let marker = format!("permission-marker-{}", std::process::id());
    let script = format!(
        "printf x > {marker}; while [ ! -e go ]; do sleep 0.01; done; mkdir -p d/e; printf x > d/e/f; chmod 000 d/e d /workspace"
    );
    let (result, candidate) = std::thread::scope(|scope| {
        let handle = scope.spawn(|| run(&script));
        let deadline = Instant::now() + Duration::from_secs(5);
        let candidate = loop {
            if let Some(path) = candidates()
                .into_iter()
                .find(|path| path.join("w").join(&marker).exists())
            {
                break path;
            }
            assert!(Instant::now() < deadline, "sandbox never started");
            std::thread::sleep(Duration::from_millis(10));
        };
        std::fs::write(candidate.join("w/go"), b"").unwrap();
        (handle.join().unwrap(), candidate)
    });
    // The run is over, so its locked directories are opened up and sealed.
    let sealed = result.expect("a permission-locked tree seals");
    assert_eq!(sealed.files()["d/e/f"].bytes, b"x");
    drop(sealed);
    assert!(!candidate.exists(), "permission-locked candidate leaked");

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let abandoned = Path::new("/tmp").join(format!(
        "spindle-candidate-{:016x}{nanos:016x}",
        u64::from(std::process::id())
    ));
    std::fs::create_dir_all(abandoned.join("w/d/e")).unwrap();
    std::fs::write(abandoned.join("w/d/e/f"), b"x").unwrap();
    for locked in ["w/d/e", "w/d", "w"] {
        std::fs::set_permissions(
            abandoned.join(locked),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
    }
    dext_native::strict::sweep_abandoned_candidates().unwrap();
    assert!(
        !abandoned.exists(),
        "sweep left a dead owner's permission-locked candidate"
    );
}

/// Python side of the dedicated RPC descriptors named by SPINDLE_RPC_FDS.
const RPC: &str = "import os,json\nreply_fd,request_fd=(int(x) for x in os.environ['SPINDLE_RPC_FDS'].split(','))\nreplies=os.fdopen(reply_fd,'rb')\ndef call(value):\n os.write(request_fd,(json.dumps(value)+'\\n').encode())\n return json.loads(replies.readline())\n";

#[test]
fn brokered_script_uses_dedicated_descriptors_without_network_or_credentials() {
    let script = format!(
        "{RPC}import sys,socket\nprint('before \\x1e{{}} is plain output',flush=True)\nfor i in range(2):\n assert call({{'id':i}})['answer']==i+1\nassert 'OPENAI_API_KEY' not in os.environ\ntry: socket.create_connection(('1.1.1.1',443),timeout=2); raise AssertionError('network allowed')\nexcept OSError: pass\nprint('after',flush=True)"
    );
    let mut calls = 0;
    let output = executor()
        .execute_brokered(
            &BTreeMap::new(),
            &["/usr/bin/python3".into(), "-c".into(), script],
            Duration::from_secs(10),
            &AtomicBool::new(false),
            RunOptions::default(),
            &[],
            &mut |bytes, remaining, cancel| {
                assert!(!cancel.load(Ordering::SeqCst));
                assert!(!remaining.is_zero());
                let value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                assert_eq!(value["id"], calls);
                calls += 1;
                format!("{{\"answer\":{calls}}}").into_bytes()
            },
        )
        .unwrap();
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    assert_eq!(calls, 2);
    assert_eq!(output.stdout(), b"before \x1e{} is plain output\nafter\n");
}

/// A request cut off by the script's exit is dropped, not a failed run.
#[test]
fn brokered_partial_request_at_exit_is_ignored() {
    let script = format!("{RPC}os.write(request_fd,b'{{\"id\":0'); print('done')");
    let output = executor()
        .execute_brokered(
            &BTreeMap::new(),
            &["/usr/bin/python3".into(), "-c".into(), script],
            Duration::from_secs(10),
            &AtomicBool::new(false),
            RunOptions::default(),
            &[],
            &mut |_, _, _| panic!("no complete request was sent"),
        )
        .unwrap();
    assert!(output.success());
    assert_eq!(output.stdout(), b"done\n");
}

#[test]
fn brokered_reader_does_not_hang_after_rejected_reply_or_deadline() {
    use dext_native::strict::RunOptions;
    let script = format!("{RPC}call({{}})");
    let started = Instant::now();
    let error = executor()
        .execute_brokered(
            &BTreeMap::new(),
            &["/usr/bin/python3".into(), "-c".into(), script],
            Duration::from_secs(3),
            &AtomicBool::new(false),
            RunOptions::default(),
            &[],
            &mut |_, _, _| vec![b'x'; dext_native::strict::MAX_BROKER_REPLY + 1],
        )
        .expect_err("oversized response rejected");
    assert!(error.to_string().contains("reply too large"));
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[test]
fn brokered_deadline_signals_the_external_call_while_it_waits() {
    use dext_native::strict::RunOptions;
    let script = format!("{RPC}call({{}})");
    let start = Instant::now();
    let mut saw_cancel = false;
    let result = executor().execute_brokered(
        &BTreeMap::new(),
        &["/usr/bin/python3".into(), "-c".into(), script],
        // The invocation deadline includes the uid-wide slot queue and sandbox setup.
        // Keep room for parallel fixtures to reach the broker before testing its cancel.
        Duration::from_secs(3),
        &AtomicBool::new(false),
        RunOptions::default(),
        &[],
        &mut |_, _, cancel| {
            while !cancel.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            saw_cancel = true;
            b"{}".to_vec()
        },
    );
    assert!(result.is_err());
    assert!(
        saw_cancel,
        "broker was not reached before the invocation ended: {result:?}"
    );
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn strict_run_resolves_alternatives_and_streams_output_to_the_observer() {
    use dext_native::strict::RunOptions;
    use std::sync::Mutex;
    let seen = Mutex::new(Vec::new());
    let observe = |stream: u8, chunk: &[u8]| {
        seen.lock().unwrap().push((stream, chunk.to_vec()));
    };
    let script =
        "printf 'int main(){return 0;}' > a.c && cc a.c -o a && ./a && echo built; echo oops >&2";
    let output = executor()
        .execute_with(
            &BTreeMap::new(),
            &["/bin/bash".into(), "-c".into(), script.into()],
            Duration::from_secs(60),
            &AtomicBool::new(false),
            RunOptions {
                network: false,
                observe: Some(&observe),
                toolchain: None,
                local_ipc: false,
            },
        )
        .unwrap();
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    assert_eq!(output.stdout(), b"built\n");
    let seen = seen.into_inner().unwrap();
    let joined = |source: u8| -> Vec<u8> {
        seen.iter()
            .filter(|(stream, _)| *stream == source)
            .flat_map(|(_, chunk)| chunk.clone())
            .collect()
    };
    assert_eq!(joined(1), output.stdout());
    assert_eq!(joined(2), output.stderr());
}

#[test]
fn networked_run_allows_only_internet_sockets_and_socketpairs() {
    use dext_native::strict::RunOptions;
    let script = "import socket\nfor domain in [socket.AF_INET, socket.AF_INET6]:\n socket.socket(domain).close()\nsocket.socketpair()\nfor domain in [socket.AF_UNIX, socket.AF_NETLINK]:\n try: socket.socket(domain); raise AssertionError('socket allowed')\n except PermissionError: pass\nimport os\nassert os.path.exists('/etc/resolv.conf') or not os.path.exists('/etc')\nprint('ok')";
    let output = executor()
        .execute_with(
            &BTreeMap::new(),
            &["/usr/bin/python3".into(), "-c".into(), script.into()],
            Duration::from_secs(10),
            &AtomicBool::new(false),
            RunOptions {
                network: true,
                observe: None,
                toolchain: None,
                local_ipc: false,
            },
        )
        .unwrap();
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
}

/// Toolchain runs see host cargo/rustc read-only and build into the host cache,
/// never into the sealed workspace; the host registry stays untouched.
#[test]
fn toolchain_run_builds_into_the_cache_not_the_workspace() {
    use dext_native::strict::{RunOptions, Toolchain};
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let rustup = std::env::var_os("RUSTUP_HOME").map_or(home.join(".rustup"), Into::into);
    let cargo = std::env::var_os("CARGO_HOME").map_or(home.join(".cargo"), Into::into);
    if !rustup.is_dir() || !cargo.join("bin/cargo").exists() {
        eprintln!("skipped: no host rust toolchain");
        return;
    }
    let executor = executor();
    let cache = std::env::temp_dir().join(format!("spindle-toolchain-test-{}", std::process::id()));
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&cache)
            .unwrap();
    }
    let files = BTreeMap::from([
        (
            "Cargo.toml".to_string(),
            FileValue {
                bytes: b"[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
                    .to_vec(),
                executable: false,
            },
        ),
        (
            "src/main.rs".to_string(),
            FileValue {
                bytes: b"fn main() { println!(\"built in the sandbox\"); }\n".to_vec(),
                executable: false,
            },
        ),
    ]);
    let output = executor
        .execute_with(
            &files,
            &["/bin/bash".into(), "-c".into(), "cargo run -q 2>&1".into()],
            Duration::from_secs(300),
            &AtomicBool::new(false),
            RunOptions {
                toolchain: Some(Toolchain {
                    rustup_home: &rustup,
                    cargo_home: &cargo,
                    cache: &cache,
                    sources: &[],
                }),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stdout())
    );
    assert!(String::from_utf8_lossy(output.stdout()).contains("built in the sandbox"));
    assert!(
        output
            .files()
            .keys()
            .all(|path| !path.starts_with("target"))
    );
    assert!(output.files().contains_key("Cargo.lock"));
    assert!(cache.join("target/debug/hello").exists());
    // The kernel leaves the overlay work directory without permissions.
    let _ = std::process::Command::new("chmod")
        .arg("-R")
        .arg("u+rwx")
        .arg(&cache)
        .status();
    std::fs::remove_dir_all(&cache).unwrap();
}

/// The host runner has no isolation but keeps the same contract: a private copy
/// sealed back, output bounds, deadlines, and no process outliving the run.
#[test]
fn host_runner_seals_a_private_copy_and_ends_its_process_group() {
    let runner = StrictExecutor::host();
    assert!(!runner.isolated() && executor().isolated());
    assert!(runner.implementation().starts_with("dext-host-bash-v1-"));
    assert!(
        executor()
            .implementation()
            .starts_with("dext-strict-bash-v1-")
    );
    runner.probe().unwrap();
    let files = BTreeMap::from([(
        "in.txt".to_string(),
        FileValue {
            bytes: b"hello".to_vec(),
            executable: false,
        },
    )]);
    let marker = std::env::temp_dir().join(format!("host-runner-orphan-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let script = format!(
        "pwd; echo $HOME; cat in.txt > out.txt; (sleep 1; touch {}) & echo done",
        marker.display()
    );
    let output = runner
        .execute(
            &files,
            &["/bin/sh".into(), "-c".into(), script],
            Duration::from_secs(10),
            &AtomicBool::new(false),
        )
        .unwrap();
    let stdout = String::from_utf8_lossy(output.stdout()).to_string();
    assert!(output.success(), "{stdout}");
    assert!(stdout.contains("spindle-candidate-") && stdout.contains("done"));
    assert_eq!(output.files()["out.txt"].bytes, b"hello");
    assert_eq!(output.helper_digest(), runner.helper_digest());
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!marker.exists(), "a background job outlived the run");
    let error = runner
        .execute(
            &BTreeMap::new(),
            &["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            Duration::from_millis(300),
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}

struct Tree<'a>(&'a BTreeMap<String, Vec<u8>>);
impl RunHost for Tree<'_> {
    fn load(&mut self, path: &str, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        out.write_all(&self.0[path])
    }
}

/// A job starts over a private copy, keeps running after the thread that started it
/// has ended (the parent-death signal belongs to the job's own thread), streams its
/// output by offset, and ends with its whole tree when stopped. Nothing it writes
/// reaches the input.
#[test]
fn background_jobs_outlive_their_starter_stream_output_and_stop() {
    use dext_native::strict::{JobState, MAX_JOB_SECS};
    let executor = std::sync::Arc::new(executor());
    let bytes = BTreeMap::from([("in.txt".to_string(), b"seed".to_vec())]);
    let tree = BTreeMap::from([(
        "in.txt".to_string(),
        Input::File {
            size: 4,
            executable: false,
        },
    )]);
    let starter = {
        let executor = executor.clone();
        std::thread::spawn(move || {
            executor
                .start_job(
                    &tree,
                    &[
                        "/bin/sh".into(),
                        "-c".into(),
                        "cat in.txt; echo; echo changed > in.txt; (sleep 600 &); i=0; while true; do i=$((i+1)); echo tick $i; sleep 0.2; done".into(),
                    ],
                    Duration::from_secs(60),
                    RunOptions::default(),
                    &[],
                    &mut Tree(&bytes),
                )
                .unwrap()
        })
    };
    let job = starter.join().unwrap();
    let never = AtomicBool::new(false);
    let first = job.read(0, 4096, Duration::from_secs(10), &never);
    assert_eq!(first.state, JobState::Running);
    assert!(String::from_utf8_lossy(&first.bytes).starts_with("seed"));
    // Waiting for more output returns once there is some, from the given offset.
    let next = job.read(first.end, 4096, Duration::from_secs(10), &never);
    assert_eq!(next.start, first.end);
    assert!(next.end > next.start);
    let mut seen = String::from_utf8_lossy(&next.bytes).into_owned();
    let mut end = next.end;
    while !seen.contains("tick 2") {
        let page = job.read(end, 4096, Duration::from_secs(10), &never);
        assert_eq!(page.state, JobState::Running);
        seen.push_str(&String::from_utf8_lossy(&page.bytes));
        end = page.end;
    }
    // A wait is cut short by an interrupt.
    let interrupt = AtomicBool::new(true);
    let started = Instant::now();
    let _ = job.read(u64::MAX - 1, 16, Duration::from_secs(30), &interrupt);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(job.stop(), JobState::Ended("stopped".into()));
    let last = job.read(0, 1 << 20, Duration::ZERO, &never);
    assert_eq!(last.state, JobState::Ended("stopped".into()));
    assert!(last.total >= last.end);
    assert!(
        executor
            .start_job(
                &BTreeMap::new(),
                &["/bin/true".into()],
                Duration::from_secs(MAX_JOB_SECS + 1),
                RunOptions::default(),
                &[],
                &mut Tree(&BTreeMap::new()),
            )
            .is_err()
    );
}

#[test]
fn background_job_exit_codes_and_lifetimes_are_reported() {
    use dext_native::strict::JobState;
    let executor = executor();
    let empty = BTreeMap::new();
    let never = AtomicBool::new(false);
    let exited = executor
        .start_job(
            &BTreeMap::new(),
            &["/bin/sh".into(), "-c".into(), "echo bye; exit 3".into()],
            Duration::from_secs(30),
            RunOptions::default(),
            &[],
            &mut Tree(&empty),
        )
        .unwrap();
    let mut page = exited.read(0, 4096, Duration::from_secs(10), &never);
    while page.state == JobState::Running {
        page = exited.read(page.end, 4096, Duration::from_secs(10), &never);
    }
    assert_eq!(page.state, JobState::Exited(Some(3)));
    assert_eq!(exited.read(0, 4096, Duration::ZERO, &never).bytes, b"bye\n");
    let timed = executor
        .start_job(
            &BTreeMap::new(),
            &["/bin/sh".into(), "-c".into(), "sleep 600".into()],
            Duration::from_secs(1),
            RunOptions::default(),
            &[],
            &mut Tree(&empty),
        )
        .unwrap();
    let mut state = timed.state();
    let until = Instant::now() + Duration::from_secs(10);
    while state == JobState::Running && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
        state = timed.state();
    }
    assert!(matches!(state, JobState::Ended(reason) if reason.contains("lifetime")));
}
