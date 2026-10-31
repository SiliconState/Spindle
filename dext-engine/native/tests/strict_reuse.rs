//! Isolated runs of one place start from the workspace the last one left: a file is
//! written again only where its content or its stamp says it may differ, and nothing
//! the last run left (links, hard links, locked directories, changed modes, extra
//! files) reaches the next one.
#![cfg(all(target_os = "linux", feature = "strict"))]
use dext_native::strict::{Input, RunHost, RunOptions, SealedEntry, SealedOutput, StrictExecutor};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

fn executor() -> StrictExecutor {
    let path = Path::new("/usr/bin/bwrap");
    let approved = Sha256::digest(std::fs::read(path).unwrap()).into();
    StrictExecutor::open(path, approved).unwrap()
}

/// A store stand-in: input bytes by path, the place its runs come from, and the paths
/// each run loaded.
struct Place<'a> {
    files: &'a BTreeMap<String, Vec<u8>>,
    place: Option<String>,
    loaded: Vec<String>,
}
impl RunHost for Place<'_> {
    fn load(&mut self, path: &str, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        self.loaded.push(path.into());
        out.write_all(&self.files[path])
    }
    fn place(&self) -> Option<String> {
        self.place.clone()
    }
    fn content(&self, path: &str) -> Option<String> {
        let digest = Sha256::digest(self.files.get(path)?);
        Some(digest.iter().map(|byte| format!("{byte:02x}")).collect())
    }
}

/// Runs `script` over `files` (`executable` names the executable ones) for `place`;
/// returns the sealed output and the paths loaded.
fn run(
    executor: &StrictExecutor,
    place: &str,
    files: &BTreeMap<String, Vec<u8>>,
    executable: &[&str],
    script: &str,
) -> (SealedOutput, Vec<String>) {
    let tree: BTreeMap<String, Input> = files
        .iter()
        .map(|(path, bytes)| {
            let input = Input::File {
                size: bytes.len() as u64,
                executable: executable.contains(&path.as_str()),
            };
            (path.clone(), input)
        })
        .collect();
    let mut host = Place {
        files,
        place: Some(place.into()),
        loaded: vec![],
    };
    let output = executor
        .execute_tree(
            &tree,
            &["/bin/sh".into(), "-c".into(), script.into()],
            Duration::from_secs(10),
            &AtomicBool::new(false),
            RunOptions::default(),
            &[],
            false,
            &mut host,
        )
        .unwrap();
    let mut loaded = host.loaded;
    loaded.sort();
    (output, loaded)
}

fn files(entries: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
    entries
        .iter()
        .map(|(path, bytes)| ((*path).to_string(), bytes.to_vec()))
        .collect()
}

/// The places of one test, whose kept workspaces go when it ends (passed or not), so no
/// scratch outlives the test binary. Tests hold it one at a time: a process keeps at
/// most four places, and another test's could otherwise push this one's out.
struct Kept(
    Vec<String>,
    #[allow(dead_code)] std::sync::MutexGuard<'static, ()>,
);
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
impl Kept {
    fn new() -> Self {
        let turn = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        dext_native::strict::keep_workspaces(true);
        Self(vec![], turn)
    }
    fn place(&mut self, name: &str) -> String {
        let place = format!("{name}-{}", std::process::id());
        self.0.push(place.clone());
        place
    }
}
impl Drop for Kept {
    fn drop(&mut self) {
        for place in &self.0 {
            dext_native::strict::forget_workspace(place);
        }
    }
}

#[test]
fn runs_of_one_place_write_only_the_files_that_may_differ() {
    let executor = executor();
    let mut kept = Kept::new();
    let place = kept.place("reuse");
    let first = files(&[
        ("a.txt", b"alpha"),
        ("dir/b.txt", b"beta"),
        ("c.txt", b"gamma"),
    ]);
    let (output, loaded) = run(
        &executor,
        &place,
        &first,
        &[],
        "printf changed > c.txt; mkdir -p junk/deep; printf x > junk/deep/j; printf y > extra",
    );
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    assert_eq!(loaded, ["a.txt", "c.txt", "dir/b.txt"]);
    drop(output);

    // The next run's input is what the store kept (c.txt changed); a.txt and
    // dir/b.txt are where the last run left them, untouched, so only c.txt is written.
    let second = files(&[
        ("a.txt", b"alpha"),
        ("dir/b.txt", b"beta"),
        ("c.txt", b"changed"),
    ]);
    let (output, loaded) = run(
        &executor,
        &place,
        &second,
        &[],
        "cat a.txt dir/b.txt c.txt; echo; ls -A; ls -A dir",
    );
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    assert_eq!(loaded, ["c.txt"]);
    assert_eq!(
        String::from_utf8_lossy(output.stdout()),
        "alphabetachanged\na.txt\nc.txt\ndir\nb.txt\n",
        "what the last run left beyond its input is gone"
    );
    assert!(matches!(
        output.entries()["a.txt"],
        SealedEntry::Unchanged { .. }
    ));
    drop(output);

    // Another input for the same paths (a rewind, another branch) is written again.
    let third = files(&[("a.txt", b"other"), ("dir/b.txt", b"beta")]);
    let (output, loaded) = run(&executor, &place, &third, &[], "cat a.txt; ls -A");
    assert_eq!(loaded, ["a.txt"]);
    assert_eq!(
        String::from_utf8_lossy(output.stdout()),
        "othera.txt\ndir\n"
    );
    drop(output);

    // Another place starts empty, and so does a place whose workspace was forgotten.
    let (_, loaded) = run(&executor, &kept.place("other"), &third, &[], "true");
    assert_eq!(loaded, ["a.txt", "dir/b.txt"]);
    dext_native::strict::forget_workspace(&place);
    let (_, loaded) = run(&executor, &place, &third, &[], "true");
    assert_eq!(loaded, ["a.txt", "dir/b.txt"]);
}

#[test]
fn nothing_a_run_leaves_behind_reaches_the_next_one() {
    let executor = executor();
    let mut kept = Kept::new();
    let place = kept.place("tamper");
    let outside =
        std::env::temp_dir().join(format!("spindle-reuse-outside-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir(&outside).unwrap();
    let input = files(&[
        ("a.txt", b"alpha"),
        ("dir/b.txt", b"beta"),
        ("locked/c.txt", b"gamma"),
        ("run.sh", b"echo ran"),
        ("touched.txt", b"same bytes"),
    ]);
    // Same bytes everywhere afterwards, but: `dir` became a link to a host directory,
    // a.txt gained a second link, `locked` was shut, run.sh lost its executable bit,
    // and touched.txt was rewritten with its own bytes and its old times restored.
    let script = format!(
        "rm -rf dir && ln -s {} dir; ln a.txt hard; chmod 000 locked; chmod -x run.sh; \
         cp touched.txt t && cat t > touched.txt && touch -r t touched.txt && rm t",
        outside.display()
    );
    let (output, _) = run(&executor, &place, &input, &["run.sh"], &script);
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    drop(output);

    let (output, loaded) = run(
        &executor,
        &place,
        &input,
        &["run.sh"],
        "cat a.txt dir/b.txt locked/c.txt touched.txt; echo; ./run.sh; ls -A; stat -c %h a.txt",
    );
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    assert_eq!(
        loaded,
        ["a.txt", "dir/b.txt", "run.sh", "touched.txt"],
        "every file the last run touched is written again; only locked/c.txt is reused"
    );
    assert_eq!(
        String::from_utf8_lossy(output.stdout()),
        "alphabetagammasame bytes\nran\na.txt\ndir\nlocked\nrun.sh\ntouched.txt\n1\n"
    );
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        0,
        "nothing was written through the link the last run left"
    );
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn a_run_that_fails_to_seal_and_a_host_run_leave_nothing_to_reuse() {
    let input = files(&[("a.txt", b"alpha")]);
    // A run cut off by its deadline keeps nothing.
    let executor = executor();
    let mut kept = Kept::new();
    let place = kept.place("timeout");
    let tree = BTreeMap::from([(
        "a.txt".to_string(),
        Input::File {
            size: 5,
            executable: false,
        },
    )]);
    let mut host = Place {
        files: &input,
        place: Some(place.clone()),
        loaded: vec![],
    };
    let error = executor
        .execute_tree(
            &tree,
            &["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            Duration::from_millis(500),
            &AtomicBool::new(false),
            RunOptions::default(),
            &[],
            false,
            &mut host,
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    let (_, loaded) = run(&executor, &place, &input, &[], "true");
    assert_eq!(loaded, ["a.txt"]);

    // A host run (not isolated) never starts from an earlier one.
    let host_runner = StrictExecutor::host();
    let place = kept.place("host");
    for _ in 0..2 {
        let (_, loaded) = run(&host_runner, &place, &input, &[], "true");
        assert_eq!(loaded, ["a.txt"]);
    }
}

#[test]
fn a_write_no_stamp_shows_and_what_a_run_set_on_folders_never_reach_the_next_run() {
    if !Path::new("/usr/bin/python3").exists() {
        return;
    }
    let executor = executor();
    let mut kept = Kept::new();
    let place = kept.place("hidden");
    let input = files(&[
        ("a.txt", b"alpha"),
        ("dir/b.txt", b"beta"),
        ("c.txt", b"gamma"),
    ]);
    // Through a shared mapping a.txt changes with no stamp field moving; `dir` gets an
    // attribute, and it and the workspace a default ACL that would leave new files
    // unwritable.
    let script = "python3 -I -c 'import mmap,os,struct\n\
fd=os.open(\"a.txt\",os.O_RDWR)\nm=mmap.mmap(fd,0)\nm[0]\nm[0:5]=b\"EVIL!\"\nm.close()\n\
os.setxattr(\"dir\",\"user.note\",b\"from the last run\")\n\
acl=struct.pack(\"<I\",2)+struct.pack(\"<HHI\",1,5,0xffffffff)+struct.pack(\"<HHI\",4,0,0xffffffff)+struct.pack(\"<HHI\",0x20,0,0xffffffff)\n\
os.setxattr(\".\",\"system.posix_acl_default\",acl)\nos.setxattr(\"dir\",\"system.posix_acl_default\",acl)'";
    let (output, _) = run(&executor, &place, &input, &[], script);
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    assert!(
        matches!(output.entries()["a.txt"], SealedEntry::File { .. }),
        "the write through the mapping is seen (the file was opened for writing)"
    );
    assert!(matches!(
        output.entries()["c.txt"],
        SealedEntry::Unchanged { .. }
    ));
    drop(output);

    let (output, loaded) = run(
        &executor,
        &place,
        &input,
        &[],
        "cat a.txt; echo; python3 -I -c 'import os\nprint(os.listxattr(\"dir\"), os.listxattr(\".\"))'; \
         printf more >> c.txt && printf new > made.txt && echo writable",
    );
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(output.stderr())
    );
    assert_eq!(
        loaded,
        ["a.txt"],
        "a.txt, written through the mapping, is written again; the untouched files move over"
    );
    assert_eq!(
        String::from_utf8_lossy(output.stdout()),
        "alpha\n[] []\nwritable\n"
    );
}

#[test]
fn a_reused_file_a_run_changes_is_sealed_changed_and_shapes_and_modes_follow_the_input() {
    let executor = executor();
    let mut kept = Kept::new();
    let place = kept.place("shapes");
    let first = files(&[
        ("a.txt", b"alpha"),
        ("d/y", b"why"),
        ("tool.sh", b"echo tool"),
        ("x", b"a file"),
    ]);
    let (output, _) = run(&executor, &place, &first, &[], "true");
    drop(output);

    // `x` becomes a directory and `d` a file; tool.sh keeps its bytes but becomes
    // executable. The run changes the reused a.txt and fails, which still leaves its
    // untouched files for the next run.
    let second = files(&[
        ("a.txt", b"alpha"),
        ("d", b"dee"),
        ("tool.sh", b"echo tool"),
        ("x/inner", b"now a folder"),
    ]);
    let (output, loaded) = run(
        &executor,
        &place,
        &second,
        &["tool.sh"],
        "printf changed >> a.txt; cat x/inner d; echo; ./tool.sh; exit 3",
    );
    assert!(!output.success());
    assert_eq!(loaded, ["d", "tool.sh", "x/inner"]);
    assert_eq!(
        String::from_utf8_lossy(output.stdout()),
        "now a folderdee\ntool\n"
    );
    assert!(
        matches!(output.entries()["a.txt"], SealedEntry::File { .. }),
        "a reused file the run wrote to is sealed as changed"
    );
    assert!(matches!(
        output.entries()["d"],
        SealedEntry::Unchanged { .. }
    ));
    drop(output);

    // The store kept nothing of that run: a.txt comes back as it was, the rest moves over.
    let (output, loaded) = run(&executor, &place, &second, &["tool.sh"], "cat a.txt");
    assert_eq!(loaded, ["a.txt"]);
    assert_eq!(String::from_utf8_lossy(output.stdout()), "alpha");
}
