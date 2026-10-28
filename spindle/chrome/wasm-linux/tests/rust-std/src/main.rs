//! As /init: says what each part of std did, one `check: NAME VALUE` line each, then
//! powers the machine off. As a child (`/init child ARG`): echoes ARG and exits 3.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};

unsafe extern "C" {
    fn reboot(cmd: i32) -> i32;
}
const RB_POWER_OFF: i32 = 0x4321_fedc_u32 as i32;

fn check(name: &str, value: impl std::fmt::Display) {
    println!("check: {name} {value}");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("child") {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input).unwrap();
        print!("child got {} and {}", args[2], input.trim());
        std::process::exit(3);
    }
    check("args", args.len());

    // Threads, a mutex and a condvar (futexes, with a timeout too).
    let pair = Arc::new((Mutex::new(0u32), Condvar::new()));
    let workers: Vec<_> = (0..4)
        .map(|n| {
            let pair = Arc::clone(&pair);
            std::thread::spawn(move || {
                let (lock, ready) = &*pair;
                *lock.lock().unwrap() += n;
                ready.notify_all();
                n * 10
            })
        })
        .collect();
    let sum: u32 = workers.into_iter().map(|w| w.join().unwrap()).sum();
    let (lock, ready) = &*pair;
    let (total, timeout) = ready
        .wait_timeout(lock.lock().unwrap(), Duration::from_millis(30))
        .unwrap();
    check("threads", format!("{sum} {}", *total));
    check("condvar-timeout", timeout.timed_out());
    drop(total);

    // Clocks and sleeping.
    let start = Instant::now();
    std::thread::sleep(Duration::from_millis(50));
    let slept = start.elapsed().as_millis();
    check("sleep", (50..2000).contains(&slept));
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    check("clock", now > 1_700_000_000);

    // Files and directories.
    std::fs::create_dir_all("/tmp/check/sub").unwrap();
    std::fs::write("/tmp/check/sub/a.txt", "alpha\n").unwrap();
    std::fs::rename("/tmp/check/sub/a.txt", "/tmp/check/b.txt").unwrap();
    let text = std::fs::read_to_string("/tmp/check/b.txt").unwrap();
    let size = std::fs::metadata("/tmp/check/b.txt").unwrap().len();
    let mut names: Vec<_> = std::fs::read_dir("/tmp/check")
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    check("fs", format!("{} {size} {}", text.trim(), names.join(",")));

    // A child process (posix_spawn), its stdin, stdout and exit status.
    let mut child = Command::new("/init")
        .args(["child", "an-arg"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"its input\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    check(
        "process",
        format!(
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            output.status.code().unwrap()
        ),
    );

    // The environment.
    unsafe { std::env::set_var("SPINDLE_CHECK", "set") };
    check("env", std::env::var("SPINDLE_CHECK").unwrap());

    // A Unix socket pair, and a channel between threads.
    let (mut left, mut right) = UnixStream::pair().unwrap();
    let echo = std::thread::spawn(move || {
        let mut buffer = [0u8; 5];
        right.read_exact(&mut buffer).unwrap();
        right.write_all(&buffer).unwrap();
    });
    left.write_all(b"hello").unwrap();
    let mut back = [0u8; 5];
    left.read_exact(&mut back).unwrap();
    echo.join().unwrap();
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || send.send(7).unwrap());
    check(
        "sockets",
        format!(
            "{} {}",
            String::from_utf8_lossy(&back),
            receive.recv().unwrap()
        ),
    );

    check("done", "");
    std::io::stdout().flush().unwrap();
    unsafe { reboot(RB_POWER_OFF) };
}
