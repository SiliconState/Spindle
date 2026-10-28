#![cfg(feature = "strict")]
//! The unisolated host runner on every platform: a private copy sealed back, a
//! deadline, and no process outliving the run (process group on Unix, job object on
//! Windows).
use dext_native::strict::{FileValue, StrictExecutor};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Runs take a point-in-time count of the user's threads, so a test that holds many
/// threads must not overlap another test's run.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Windows PowerShell, by absolute path (runs take no PATH lookup for their program).
#[cfg(windows)]
fn powershell() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    format!(r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe")
}

fn shell(script: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![
            dext_native::strict::system_shell(),
            "/d".into(),
            "/c".into(),
            script.into(),
        ]
    } else {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }
}

#[test]
fn host_runner_seals_a_copy_enforces_deadlines_and_ends_the_tree() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let runner = StrictExecutor::host();
    assert!(!runner.isolated());
    assert!(runner.implementation().starts_with("dext-host-bash-v1-"));
    runner.probe().unwrap();
    let files = BTreeMap::from([(
        "in.txt".to_string(),
        FileValue {
            bytes: b"hello".to_vec(),
            executable: true,
        },
    )]);
    let marker = std::env::temp_dir().join(format!("host-runner-orphan-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let script = if cfg!(windows) {
        format!(
            "copy in.txt out.txt >nul && mkdir sub && echo x> sub\\deep.txt && start /b cmd /d /c \"ping -n 3 127.0.0.1 >nul & echo late> {}\" && echo done",
            marker.display()
        )
    } else {
        format!(
            "cp in.txt out.txt && mkdir sub && echo x > sub/deep.txt && {{ (sleep 1; touch {}) & }} ; echo done",
            marker.display()
        )
    };
    let output = runner
        .execute(
            &files,
            &shell(&script),
            Duration::from_secs(20),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(
        output.success(),
        "{}{}",
        String::from_utf8_lossy(output.stdout()),
        String::from_utf8_lossy(output.stderr())
    );
    assert!(String::from_utf8_lossy(output.stdout()).contains("done"));
    assert_eq!(output.files()["out.txt"].bytes, b"hello");
    assert!(output.files().contains_key("sub/deep.txt"));
    assert!(output.files()["in.txt"].executable);
    std::thread::sleep(Duration::from_secs(4));
    assert!(!marker.exists(), "a background process outlived the run");
    let slow = if cfg!(windows) {
        "ping -n 30 127.0.0.1 >nul"
    } else {
        "sleep 30"
    };
    let error = runner
        .execute(
            &BTreeMap::new(),
            &shell(slow),
            Duration::from_millis(500),
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(
        dext_native::strict::sweep_abandoned_candidates().unwrap(),
        0
    );
}

/// RLIMIT_NPROC counts every thread the user owns, so a fixed run cap would make
/// each fork fail for a user who already runs that many; the cap sits on top.
#[cfg(target_os = "linux")]
#[test]
fn host_run_can_fork_while_the_user_already_runs_many_threads() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let release = std::sync::Arc::new(std::sync::Barrier::new(701));
    let threads: Vec<_> = (0..700)
        .map(|_| {
            let release = release.clone();
            std::thread::spawn(move || {
                release.wait();
            })
        })
        .collect();
    let output = StrictExecutor::host()
        .execute(
            &BTreeMap::new(),
            &shell("(echo forked) | cat"),
            Duration::from_secs(20),
            &AtomicBool::new(false),
        )
        .unwrap();
    release.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    assert!(String::from_utf8_lossy(output.stdout()).contains("forked"));
}

/// A descendant that leaves the run's process group (setsid) and keeps stdout open
/// used to block the output readers, and with them the run, forever. Readers now stop
/// shortly after the run ends; with a delegated cgroup the descendant is killed too.
#[cfg(unix)]
#[test]
fn host_run_returns_while_a_detached_descendant_holds_its_output() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let started = std::time::Instant::now();
    let output = StrictExecutor::host()
        .execute(
            &BTreeMap::new(),
            &shell("setsid sh -c 'echo $$ > pid; exec sleep 30' & sleep 0.2; echo left"),
            Duration::from_secs(20),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "run waited for its descendant"
    );
    assert!(String::from_utf8_lossy(output.stdout()).contains("left"));
    if let Some(pid) = output.files().get("pid").and_then(|file| {
        String::from_utf8_lossy(&file.bytes)
            .trim()
            .parse::<i32>()
            .ok()
    }) {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

/// Brokered host runs get the same dedicated request/reply descriptors as sandboxed
/// ones, so printed output is never parsed as a request.
#[cfg(unix)]
#[test]
fn host_brokered_run_uses_dedicated_descriptors() {
    let Some(python) = dext_native::strict::host_program(dext_native::strict::Program::Python)
    else {
        eprintln!("skipped: no python3");
        return;
    };
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let script = "import os,json\nreply_fd,request_fd=(int(x) for x in os.environ['SPINDLE_RPC_FDS'].split(','))\nprint('\\x1e{\"not\":\"a request\"}',flush=True)\nos.write(request_fd,b'{\"n\":41}\\n')\nprint(json.loads(os.fdopen(reply_fd,'rb').readline())['n'])";
    let output = StrictExecutor::host()
        .execute_brokered(
            &BTreeMap::new(),
            &[python, "-c".into(), script.into()],
            Duration::from_secs(20),
            &AtomicBool::new(false),
            dext_native::strict::RunOptions::default(),
            &[],
            &mut |request, _, _| {
                let value: serde_json::Value = serde_json::from_slice(request).unwrap();
                format!("{{\"n\":{}}}", value["n"].as_u64().unwrap() + 1).into_bytes()
            },
        )
        .unwrap();
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    assert_eq!(
        output.stdout(),
        "\u{1e}{\"not\":\"a request\"}\n42\n".as_bytes()
    );
}

/// Host jobs: output by offset, exit codes, and stopping ends the process group (the
/// job object on Windows).
#[test]
fn host_jobs_stream_output_and_stop_their_group() {
    use dext_native::strict::{Input, JobState, RunHost, RunOptions};
    struct Empty;
    impl RunHost for Empty {
        fn load(&mut self, _: &str, _: &mut dyn std::io::Write) -> std::io::Result<()> {
            Ok(())
        }
    }
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let runner = StrictExecutor::host();
    let tree: BTreeMap<String, Input> = BTreeMap::new();
    let marker = std::env::temp_dir().join(format!("host-job-orphan-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let script = if cfg!(windows) {
        format!(
            "echo up&& start /b cmd /d /c \"ping -n 3 127.0.0.1 >nul & echo late> {}\" && ping -n 60 127.0.0.1 >nul",
            marker.display()
        )
    } else {
        format!(
            "echo up; (sleep 2; touch {}) & while true; do sleep 0.2; done",
            marker.display()
        )
    };
    let job = runner
        .start_job(
            &tree,
            &shell(&script),
            Duration::from_secs(60),
            RunOptions::default(),
            &[],
            &mut Empty,
        )
        .unwrap();
    let never = AtomicBool::new(false);
    let page = job.read(0, 64, Duration::from_secs(10), &never);
    assert_eq!(String::from_utf8_lossy(&page.bytes).trim_end(), "up");
    assert_eq!(page.state, JobState::Running);
    assert_eq!(job.stop(), JobState::Ended("stopped".into()));
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !marker.exists(),
        "a stopped job's group must not outlive it"
    );
}

/// A run that rewrites a file in place with the same size and then restores its
/// modification time (archive extraction, `cp -p`, `touch -r`) still changed it: the
/// seal sees the change time, which the run cannot restore.
#[test]
fn host_run_keeps_a_same_size_write_behind_a_restored_modification_time() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let files = BTreeMap::from([(
        "data.txt".to_string(),
        FileValue {
            bytes: b"aaaa".to_vec(),
            executable: false,
        },
    )]);
    #[cfg(windows)]
    let argv = vec![
        powershell(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        "$f = Join-Path (Get-Location) 'data.txt'; $t = (Get-Item $f).LastWriteTimeUtc; \
         [IO.File]::WriteAllText($f, 'bbbb'); (Get-Item $f).LastWriteTimeUtc = $t; \
         if ((Get-Item $f).LastWriteTimeUtc -ne $t) { exit 3 }"
            .into(),
    ];
    #[cfg(not(windows))]
    let argv =
        shell("cp -p data.txt keep && printf bbbb > data.txt && touch -r keep data.txt && rm keep");
    let output = StrictExecutor::host()
        .execute(
            &files,
            &argv,
            Duration::from_secs(60),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(
        output.success(),
        "{}{}",
        String::from_utf8_lossy(output.stdout()),
        String::from_utf8_lossy(output.stderr())
    );
    assert_eq!(output.files()["data.txt"].bytes, b"bbbb");
}

/// Windows job objects hold what rlimits and cgroups hold on Linux: a run cannot start
/// more processes than its cap, and an allocation its memory cap refused is reported.
#[cfg(windows)]
#[test]
fn windows_job_caps_bound_processes_and_report_memory_exhaustion() {
    use dext_native::process_tree::{ChildProcessTree, TreeCaps};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const SUSPENDED: u32 = 0x4;
    let run = |program: &str, args: &[&str], caps: TreeCaps| {
        let mut child = Command::new(program)
            .args(args)
            .creation_flags(SUSPENDED)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let tree = ChildProcessTree::for_std_capped(&child, caps).unwrap();
        let mut stdout = String::new();
        std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut stdout).unwrap();
        let status = child.wait().unwrap();
        (status, stdout, tree.memory_exhausted())
    };
    let shell = dext_native::strict::system_shell();
    let (status, stdout, _) = run(
        &shell,
        &["/d", "/c", "echo outer && cmd /d /c echo nested"],
        TreeCaps {
            processes: Some(1),
            ..TreeCaps::default()
        },
    );
    assert!(stdout.contains("outer"), "{stdout}");
    assert!(!stdout.contains("nested"), "{status:?} {stdout}");
    let (_, stdout, _) = run(
        &shell,
        &["/d", "/c", "cmd /d /c echo nested"],
        TreeCaps::default(),
    );
    assert!(stdout.contains("nested"), "{stdout}");
    let allocate = |megabytes: u32| {
        run(
            &powershell(),
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                // A failed statement must end the script, not print the next line.
                &format!(
                    "$ErrorActionPreference = 'Stop'; $a = New-Object byte[] ({megabytes}MB); 'allocated'"
                ),
            ],
            TreeCaps {
                memory: Some(512 << 20),
                ..TreeCaps::default()
            },
        )
    };
    let (status, stdout, exhausted) = allocate(1024);
    assert!(
        !status.success() && !stdout.contains("allocated"),
        "{stdout}"
    );
    assert!(exhausted, "the refused allocation was not reported");
    let (status, stdout, exhausted) = allocate(16);
    assert!(status.success() && stdout.contains("allocated"), "{stdout}");
    assert!(!exhausted);
}
