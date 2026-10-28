#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(target_os = "linux")]
mod linux {
    use dext_core::model::{Encoding, Identity, ModelWait, PreparedRequest};
    use dext_core::recovery::Contract;
    use dext_transcript::checkpoint::{NativeResponse, TranscriptCheckpoint};
    use dext_transcript::turn::{Effect, TurnContinuation};
    use dext_transcript::{Block, Message, Usage};
    use sha2::{Digest, Sha256};
    use spindle_store::broker::{
        DispatchOutcome, DispatchRequest, ImageControl, ImagePhase, ImageSource,
    };
    use spindle_store::completion::Attempt;
    use spindle_store::transition::{Observation, Proposal};
    use spindle_store::turn::{Action, ENGINE, execution_id, new_intents, receipt_binding};
    use spindle_store::{Command, Error, Store};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command as Process, ExitStatus};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    const JPEG: &[u8] = include_bytes!("fixtures/dext-worker-2x1.jpg");
    const DEADLINE: Duration = Duration::from_secs(10);

    struct Realm(PathBuf);
    impl Realm {
        fn new(case: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("spindle-worker-{}-{case}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Realm {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Host {
        child: Child,
        worker: Option<(libc::pid_t, OwnedFd)>,
    }
    impl Host {
        fn start(root: &Path) -> Self {
            Self {
                child: Process::new(std::env::current_exe().unwrap())
                    .arg("--host")
                    .current_dir(root)
                    .stdin(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
                worker: None,
            }
        }
        fn ready(&mut self, root: &Path) {
            let deadline = Instant::now() + DEADLINE;
            let marker = root.join("worker-ready");
            while !marker.exists() {
                assert!(
                    self.child.try_wait().unwrap().is_none(),
                    "host exited before worker handshake"
                );
                assert!(Instant::now() < deadline, "worker handshake timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
            let text = std::fs::read_to_string(marker).unwrap();
            let mut ids = text.split_whitespace().map(|id| id.parse::<u32>().unwrap());
            let pid = ids.next().unwrap();
            assert_eq!(ids.next().unwrap(), self.child.id());
            assert!(ids.next().is_none());
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            assert!(
                fd >= 0,
                "fixture requires pidfd_open: {}",
                std::io::Error::last_os_error()
            );
            self.worker = Some((pid as libc::pid_t, unsafe {
                OwnedFd::from_raw_fd(fd as i32)
            }));
        }
        fn wait(&mut self) -> ExitStatus {
            let deadline = Instant::now() + DEADLINE;
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return status;
                }
                assert!(Instant::now() < deadline, "host cleanup timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        fn kill_worker(&self) {
            let (_, fd) = self.worker.as_ref().unwrap();
            assert_eq!(
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                },
                0
            );
        }
        fn worker_alive(&self) {
            let (_, fd) = self.worker.as_ref().unwrap();
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(
                unsafe { libc::poll(&mut poll, 1, 0) },
                0,
                "worker stopped before committed intent was acknowledged"
            );
        }
        fn worker_exited(&self) {
            let (_, fd) = self.worker.as_ref().unwrap();
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(
                unsafe { libc::poll(&mut poll, 1, DEADLINE.as_millis() as i32) },
                1,
                "worker stayed alive"
            );
            assert_ne!(poll.revents & libc::POLLIN, 0);
        }
        fn reaped_by_host(&self) {
            let (pid, _) = self.worker.as_ref().unwrap();
            assert_eq!(
                unsafe { libc::waitpid(*pid, std::ptr::null_mut(), libc::WNOHANG) },
                -1,
                "host left a child for the fixture subreaper"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
        fn reap_adopted_worker(&self) {
            let (pid, _) = self.worker.as_ref().unwrap();
            let deadline = Instant::now() + DEADLINE;
            loop {
                let mut status = 0;
                let result = unsafe { libc::waitpid(*pid, &mut status, libc::WNOHANG) };
                if result == *pid {
                    assert!(libc::WIFSIGNALED(status));
                    assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
                    break;
                }
                assert_eq!(result, 0, "worker was not adopted by fixture subreaper");
                assert!(Instant::now() < deadline, "orphan cleanup timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    impl Drop for Host {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some((pid, fd)) = &self.worker {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                    libc::waitpid(*pid, std::ptr::null_mut(), 0);
                }
            }
        }
    }

    fn worker() -> dext_native::image::PinnedWorker {
        let path = std::env::current_exe().unwrap();
        let digest = Sha256::digest(std::fs::read(&path).unwrap()).into();
        dext_native::image::PinnedWorker::open(&path, digest).unwrap()
    }
    fn implementation(worker: &dext_native::image::PinnedWorker) -> String {
        format!(
            "dext-read-image-v1-{}",
            worker
                .digest()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    }
    fn snapshot(store: &mut Store) -> TurnContinuation {
        TurnContinuation::restore(&store.snapshot().unwrap().continuation).unwrap()
    }
    fn prepare(store: &mut Store, worker: &dext_native::image::PinnedWorker) -> DispatchRequest {
        let initial = TurnContinuation::new(
            "operator".into(),
            execution_id("operator", "start").unwrap(),
            Contract::OpenAiResponses,
            Some(4),
            TranscriptCheckpoint {
                route_binding: "route-v1".into(),
                messages: vec![Message {
                    role: "user".into(),
                    content: vec![Block::Text {
                        text: "synthetic fixture".into(),
                    }],
                }],
            },
        )
        .unwrap();
        let request = PreparedRequest::new(
            Identity {
                effect: initial.next_model_id().unwrap(),
                provider: "fixture".into(),
                contract: "openai-responses".into(),
                endpoint: "https://example.invalid/responses".into(),
                model: "fixture".into(),
                credential_scope: "fixture".into(),
                configuration: "fixture".into(),
            },
            Encoding::Json,
            b"synthetic frozen request".to_vec(),
        )
        .unwrap();
        let action = Action::Start {
            initial: initial.checkpoint().unwrap(),
            request: ModelWait::new(request.clone()).checkpoint(),
        };
        let admission = store
            .admit(&Command {
                scope: "operator".into(),
                id: "start".into(),
                lineage: 0,
                kind: ENGINE.into(),
                payload: action.encode().unwrap(),
            })
            .unwrap();
        let waiting = initial.prepare(request, "route-v1").unwrap();
        let proposal = Proposal {
            id: "start-commit".into(),
            scope: "operator".into(),
            observation: Observation::Admission(admission.sequence),
            expected_revision: 0,
            owner_epoch: store.epoch(),
            engine: ENGINE.into(),
            continuation: waiting.checkpoint().unwrap(),
            workspace: None,
            effects: new_intents(None, &waiting).unwrap(),
            state: None,
        };
        store.commit_turn_action(&proposal).unwrap();
        let model = Attempt {
            id: "model-attempt".into(),
            effect: proposal.effects[0].id.clone(),
            owner_epoch: store.epoch(),
            implementation: "fixture".into(),
            grant_reference: "fixture".into(),
        };
        store.reserve_attempt(&model).unwrap();
        let Effect::Model {
            waiting: model_wait,
            ..
        } = waiting.pending_effects().unwrap().remove(0)
        else {
            panic!("model wait")
        };
        let response = NativeResponse {
            blocks: vec![Block::ToolUse {
                id: "image-call".into(),
                name: "read_image".into(),
                input: serde_json::json!({"path":"source.jpg"}),
            }],
            stop_reason: Some("stop".into()),
            usage: Usage::default(),
            unknown_events: 0,
            unfinished_tool_calls: 0,
        }
        .checkpoint()
        .unwrap();
        let recorded = model_wait
            .record(model_wait.request(), model_wait.attempt(), &response)
            .unwrap();
        let bytes = recorded.checkpoint();
        store.ingest_receipt(&model.id, &bytes).unwrap();
        let tools = waiting.apply_model(&recorded, "route-v1").unwrap();
        let proposal = Proposal {
            id: "tools-commit".into(),
            scope: "operator".into(),
            observation: Observation::Receipt(model.id.clone()),
            expected_revision: 1,
            owner_epoch: store.epoch(),
            engine: ENGINE.into(),
            continuation: tools.checkpoint().unwrap(),
            workspace: None,
            effects: new_intents(Some(&waiting), &tools).unwrap(),
            state: None,
        };
        let model_intent = store.intents(1).unwrap().remove(0);
        store
            .commit_turn_receipt(
                &proposal,
                &receipt_binding(&model, 1, &model_intent, &bytes),
            )
            .unwrap();
        let intent = proposal.effects[0].clone();
        DispatchRequest {
            scope: "operator".into(),
            owner_epoch: store.epoch(),
            expected_revision: 2,
            intent_revision: 2,
            attempt: Attempt {
                id: "image-attempt".into(),
                effect: intent.id.clone(),
                owner_epoch: store.epoch(),
                implementation: implementation(worker),
                grant_reference: "fresh-fixture-approval".into(),
            },
            intent,
        }
    }

    fn gated_worker() {
        // This delay exists only in the test executable, never the shipped worker.
        let mut signal = 0;
        assert_eq!(
            unsafe { libc::prctl(libc::PR_GET_PDEATHSIG, &mut signal) },
            0
        );
        assert_eq!(
            signal,
            libc::SIGKILL,
            "parent-death fence must survive exec"
        );
        let root = std::env::current_dir().unwrap();
        if root.join("fixture-clear-fence").exists() {
            assert_eq!(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, 0) }, 0);
        }
        let marker = format!("{} {}", std::process::id(), unsafe { libc::getppid() });
        let temporary = root.join("worker-ready.tmp");
        std::fs::write(&temporary, marker).unwrap();
        std::fs::rename(temporary, root.join("worker-ready")).unwrap();
        let deadline = Instant::now() + DEADLINE;
        while !root.join("worker-release").exists() {
            assert!(Instant::now() < deadline, "fixture worker gate timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::process::exit(dext_native::image::worker_main());
    }
    fn wait_marker(root: &Path, marker: &str) {
        let deadline = Instant::now() + DEADLINE;
        while !root.join(marker).exists() {
            assert!(
                Instant::now() < deadline,
                "fixture marker {marker} timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    struct Control {
        root: PathBuf,
        mode: String,
        acknowledged: bool,
    }
    impl ImageControl for Control {
        fn cancellation(&mut self, phase: ImagePhase) -> spindle_store::Result<Option<String>> {
            if phase == ImagePhase::Joined && self.mode == "cleanup-joined-kill" {
                std::fs::write(self.root.join("cleanup-joined"), []).unwrap();
                wait_marker(&self.root, "never-release");
            }
            if phase != ImagePhase::Running || !self.root.join("cancel").exists() {
                return Ok(None);
            }
            match self.mode.as_str() {
                "control-error" => return Err(Error::Invalid),
                "control-panic" => panic!("injected control panic"),
                "cancel-poll-kill" => {
                    std::fs::write(self.root.join("poll-ready"), []).unwrap();
                    wait_marker(&self.root, "never-release");
                }
                _ => {}
            }
            Ok(Some("durable-cancel".into()))
        }
        fn cleanup_committed(
            &mut self,
            cleanup: spindle_store::completion::ImageCleanup,
        ) -> spindle_store::Result<()> {
            use spindle_store::completion::ImageCleanup;
            let db = rusqlite::Connection::open_with_flags(
                self.root.join("realm/authority.sqlite"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let state: i64 = db.query_row(
                "SELECT state FROM image_executions WHERE attempt='image-attempt'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(
                state,
                match cleanup {
                    ImageCleanup::NotStarted => 1,
                    ImageCleanup::RootReaped => 2,
                    ImageCleanup::TreeEmptied => 3,
                    ImageCleanup::Unknown => panic!("unknown cleanup acknowledged"),
                }
            );
            if self.mode.starts_with("cleanup-") {
                std::fs::write(self.root.join("cleanup-committed"), []).unwrap();
                wait_marker(&self.root, "cleanup-release");
                match self.mode.as_str() {
                    "cleanup-ack-error" => return Err(Error::Invalid),
                    "cleanup-ack-panic" => panic!("injected cleanup acknowledgement panic"),
                    _ => {}
                }
            }
            Ok(())
        }

        fn committed(&mut self, command: &str, epoch: u64) -> spindle_store::Result<()> {
            let db = rusqlite::Connection::open_with_flags(
                self.root.join("realm/authority.sqlite"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let saved: (String, i64) = db.query_row(
                "SELECT command_id,epoch FROM cancellations WHERE scope='operator'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(saved, (command.into(), epoch as i64));
            self.acknowledged = true;
            std::fs::write(self.root.join("cancel-committed"), []).unwrap();
            wait_marker(&self.root, "cancel-release");
            match self.mode.as_str() {
                "cancel-ack-error" => Err(Error::Invalid),
                "cancel-ack-panic" => panic!("injected acknowledgement panic"),
                _ => Ok(()),
            }
        }
    }

    fn controlled_host(
        root: &Path,
        store: &mut Store,
        request: &DispatchRequest,
        worker: &dext_native::image::PinnedWorker,
    ) -> spindle_store::Result<DispatchOutcome> {
        let mode = std::fs::read_to_string(root.join("control-mode")).unwrap();
        let db = rusqlite::Connection::open(root.join("realm/authority.sqlite")).unwrap();
        let storage_fault = mode.starts_with("cancel-fail") || mode.starts_with("cleanup-fail");
        if storage_fault {
            let fault = if mode.ends_with("ignore") {
                "IGNORE"
            } else {
                "ABORT,'injected'"
            };
            if mode.starts_with("cleanup-") {
                db.execute_batch(&format!("CREATE TRIGGER fail_cleanup BEFORE UPDATE ON image_executions BEGIN SELECT RAISE({fault}); END;")).unwrap();
            } else {
                db.execute_batch(&format!("CREATE TRIGGER fail_cancel BEFORE INSERT ON cancellations BEGIN SELECT RAISE({fault}); END;")).unwrap();
            }
        }
        let mut control = Control {
            root: root.into(),
            mode: mode.clone(),
            acknowledged: false,
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store.dispatch_read_image_controlled(
                request,
                worker,
                |_| Ok(()),
                |_, _| {
                    Ok(ImageSource::new(
                        std::fs::File::open(root.join("source.jpg"))?,
                        Sha256::digest(JPEG).into(),
                    ))
                },
                &mut control,
            )
        }));
        if storage_fault {
            assert!(!control.acknowledged);
            assert!(matches!(store.snapshot(), Err(Error::Paused)));
            db.execute_batch(
                "DROP TRIGGER IF EXISTS fail_cancel; DROP TRIGGER IF EXISTS fail_cleanup;",
            )
            .unwrap();
        }
        if mode.ends_with("panic") {
            assert!(outcome.is_err());
            assert_eq!(control.acknowledged, mode == "cancel-ack-panic");
            return Err(Error::Invalid);
        }
        outcome.unwrap()
    }

    fn host() {
        let root = std::env::current_dir().unwrap();
        let mut store = Store::open(&root.join("realm")).unwrap();
        let worker = worker();
        let request = prepare(&mut store, &worker);
        std::fs::write(root.join("source.jpg"), JPEG).unwrap();
        let interrupt = AtomicBool::new(false);
        let finished = AtomicBool::new(false);
        let outcome = if root.join("control-mode").exists() {
            controlled_host(&root, &mut store, &request, &worker)
        } else {
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let deadline = Instant::now() + DEADLINE;
                    while !finished.load(Ordering::SeqCst) {
                        if root.join("interrupt").exists() {
                            interrupt.store(true, Ordering::SeqCst);
                            break;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "fixture interrupt observer timed out"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                });
                let outcome = store.dispatch_read_image(
                    &request,
                    &interrupt,
                    &worker,
                    |_| Ok(()),
                    |_, _| {
                        Ok(ImageSource::new(
                            std::fs::File::open(root.join("source.jpg"))?,
                            Sha256::digest(JPEG).into(),
                        ))
                    },
                );
                finished.store(true, Ordering::SeqCst);
                outcome
            })
        };
        let result = match outcome {
            Ok(DispatchOutcome::Recorded(_)) => "recorded",
            Err(Error::Cancelled) => "interrupted",
            Err(Error::Invalid) => "worker-failed",
            Err(Error::Sql(_) | Error::Incompatible) => "storage-failed",
            other => panic!("unexpected dispatch result: {other:?}"),
        };
        std::fs::write(root.join("host-result"), result).unwrap();
    }

    fn check_reserved(root: &Path) {
        let db = rusqlite::Connection::open_with_flags(
            root.join("realm/authority.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        for (table, count) in [
            ("attempts", 2),
            ("receipts", 1),
            ("objects", 0),
            ("staging_pins", 0),
            ("image_provenance", 0),
        ] {
            assert_eq!(
                db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                count
            );
        }
    }
    fn check_reopened(
        root: &Path,
        complete: bool,
        historical: bool,
        cancelled: bool,
        cleanup: Option<spindle_store::completion::ImageCleanup>,
    ) {
        let mut store = Store::open(&root.join("realm")).unwrap();
        assert_eq!(
            store.snapshot().unwrap().revision,
            2,
            "execution is not adoption"
        );
        assert_eq!(
            store.image_cleanup("operator", "image-attempt").unwrap(),
            cleanup
        );
        assert_eq!(
            store.image_cleanup("wrong-scope", "image-attempt").unwrap(),
            None
        );
        let baseline = snapshot(&mut store);
        let db = rusqlite::Connection::open(root.join("realm/authority.sqlite")).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM cancellations", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            i64::from(cancelled)
        );
        if cancelled {
            let first_epoch: i64 = db
                .query_row(
                    "SELECT epoch FROM cancellations WHERE scope='operator'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                store.cancel_scope("operator", "durable-cancel").unwrap(),
                first_epoch as u64
            );
            assert!(matches!(
                store.cancel_scope("operator", "other-cancel"),
                Err(Error::Conflict)
            ));
            assert!(matches!(
                store.admit(&Command {
                    scope: "operator".into(),
                    id: "after-cancel".into(),
                    lineage: 0,
                    kind: "fixture".into(),
                    payload: vec![]
                }),
                Err(Error::Cancelled)
            ));
        }
        let epoch: i64 = db
            .query_row(
                "SELECT owner_epoch FROM attempts WHERE id='image-attempt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let intent = store.intents(2).unwrap().remove(0);
        let worker = worker();
        let mut request = DispatchRequest {
            scope: "operator".into(),
            owner_epoch: store.epoch(),
            expected_revision: 2,
            intent_revision: 2,
            attempt: Attempt {
                id: "image-attempt".into(),
                effect: intent.id.clone(),
                owner_epoch: epoch as u64,
                implementation: implementation(&worker),
                grant_reference: "fresh-fixture-approval".into(),
            },
            intent,
        };
        let outcome = store
            .dispatch_read_image(
                &request,
                &AtomicBool::new(false),
                &worker,
                |_| Ok(()),
                |_, _| panic!("worker/source redispatched after recovery"),
            )
            .unwrap();
        if complete || historical {
            let DispatchOutcome::Reused(bytes) = outcome else {
                panic!("lost sealed image result")
            };
            if !complete {
                return;
            }
            let images = spindle_store::image::ImageReceipt::decode(&bytes).unwrap();
            let after =
                spindle_store::turn::image_receipt_transition(&baseline, &request.intent, &images)
                    .unwrap();
            let proposal = Proposal {
                id: "image-adopt".into(),
                scope: "operator".into(),
                observation: Observation::Receipt(request.attempt.id.clone()),
                expected_revision: 2,
                owner_epoch: store.epoch(),
                engine: ENGINE.into(),
                continuation: after.checkpoint().unwrap(),
                workspace: None,
                effects: vec![],
                state: None,
            };
            store
                .commit_turn_image_receipt(
                    &proposal,
                    &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
                    *worker.digest(),
                )
                .unwrap();
            assert_eq!(store.snapshot().unwrap().revision, 3);
        } else {
            assert!(matches!(outcome, DispatchOutcome::Reconcile));
            assert!(store.receipt(&request.attempt.id).unwrap().is_none());
            assert_eq!(
                snapshot(&mut store).checkpoint().unwrap(),
                baseline.checkpoint().unwrap()
            );
            check_reserved(root);
            request.attempt.id = "replacement".into();
            request.attempt.owner_epoch = store.epoch();
            assert!(matches!(
                store.dispatch_read_image(
                    &request,
                    &AtomicBool::new(false),
                    &worker,
                    |_| Ok(()),
                    |_, _| panic!("replacement worker ran")
                ),
                Err(Error::Conflict)
            ));
        }
    }
    pub fn main() {
        match std::env::args().nth(1).as_deref() {
            Some(dext_native::image::WORKER_ARG) => gated_worker(),
            Some("--host") => {
                host();
                return;
            }
            None | Some("--nocapture" | "--quiet") => {}
            Some("--list") => {
                println!("pinned_worker_lifecycle: test");
                return;
            }
            _ => panic!("unknown fixture role"),
        }
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        for case in [
            "complete",
            "interrupt",
            "worker-kill",
            "owner-kill",
            "owner-kill-unfenced",
            "controlled-complete",
            "durable-cancel",
            "cancel-poll-kill",
            "cancel-commit-kill",
            "cancel-fail-abort",
            "cancel-fail-ignore",
            "control-error",
            "control-panic",
            "cancel-ack-error",
            "cancel-ack-panic",
            "cleanup-joined-kill",
            "cleanup-commit-kill",
            "cleanup-fail-abort",
            "cleanup-fail-ignore",
            "cleanup-ack-error",
            "cleanup-ack-panic",
        ] {
            let realm = Realm::new(case);
            let controlled = !matches!(
                case,
                "complete" | "interrupt" | "worker-kill" | "owner-kill" | "owner-kill-unfenced"
            );
            if controlled {
                std::fs::write(realm.0.join("control-mode"), case).unwrap();
            }
            if case == "owner-kill-unfenced" {
                std::fs::write(realm.0.join("fixture-clear-fence"), []).unwrap();
            }
            let mut host = Host::start(&realm.0);
            host.ready(&realm.0);
            check_reserved(&realm.0);
            assert!(matches!(
                Store::open(&realm.0.join("realm")),
                Err(Error::Locked)
            ));
            match case {
                "complete" | "controlled-complete" => {
                    std::fs::write(realm.0.join("worker-release"), []).unwrap();
                }
                _ if case.starts_with("cleanup-") => {
                    std::fs::write(realm.0.join("worker-release"), []).unwrap();
                    if case == "cleanup-joined-kill" {
                        wait_marker(&realm.0, "cleanup-joined");
                        host.worker_exited();
                        host.child.kill().unwrap();
                    } else if !case.starts_with("cleanup-fail") {
                        wait_marker(&realm.0, "cleanup-committed");
                        host.worker_exited();
                        if case == "cleanup-commit-kill" {
                            host.child.kill().unwrap();
                        } else {
                            std::fs::write(realm.0.join("cleanup-release"), []).unwrap();
                        }
                    }
                }
                "interrupt" => {
                    std::fs::write(realm.0.join("interrupt"), []).unwrap();
                }
                "worker-kill" => host.kill_worker(),
                "owner-kill" | "owner-kill-unfenced" => {
                    host.child.kill().unwrap();
                }
                _ if controlled => {
                    std::fs::write(realm.0.join("cancel"), []).unwrap();
                    match case {
                        "durable-cancel" | "cancel-commit-kill" | "cancel-ack-error"
                        | "cancel-ack-panic" => {
                            wait_marker(&realm.0, "cancel-committed");
                            let db = rusqlite::Connection::open_with_flags(
                                realm.0.join("realm/authority.sqlite"),
                                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                            )
                            .unwrap();
                            assert_eq!(
                                db.query_row(
                                    "SELECT command_id FROM cancellations WHERE scope='operator'",
                                    [],
                                    |row| row.get::<_, String>(0)
                                )
                                .unwrap(),
                                "durable-cancel"
                            );
                            host.worker_alive();
                            if case == "cancel-commit-kill" {
                                host.child.kill().unwrap();
                            } else {
                                std::fs::write(realm.0.join("cancel-release"), []).unwrap();
                            }
                        }
                        "cancel-poll-kill" => {
                            wait_marker(&realm.0, "poll-ready");
                            host.worker_alive();
                            host.child.kill().unwrap();
                        }
                        _ => {}
                    }
                }
                _ => unreachable!(),
            }
            let status = host.wait();
            if case.starts_with("owner-kill")
                || matches!(
                    case,
                    "cancel-poll-kill"
                        | "cancel-commit-kill"
                        | "cleanup-joined-kill"
                        | "cleanup-commit-kill"
                )
            {
                assert_eq!(status.signal(), Some(libc::SIGKILL));
                if case == "owner-kill-unfenced" {
                    let (_, fd) = host.worker.as_ref().unwrap();
                    let mut poll = libc::pollfd {
                        fd: fd.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    assert_eq!(
                        unsafe { libc::poll(&mut poll, 1, 0) },
                        0,
                        "negative control worker unexpectedly exited"
                    );
                    host.kill_worker();
                }
                host.worker_exited();
                if case.starts_with("cleanup-") {
                    host.reaped_by_host();
                } else {
                    host.reap_adopted_worker();
                }
                assert!(!realm.0.join("host-result").exists());
            } else {
                assert!(status.success(), "fixture host failed: {status}");
                host.worker_exited();
                host.reaped_by_host();
                let expected = match case {
                    "complete" | "controlled-complete" => "recorded",
                    "interrupt" | "durable-cancel" => "interrupted",
                    "cancel-fail-abort"
                    | "cancel-fail-ignore"
                    | "cleanup-fail-abort"
                    | "cleanup-fail-ignore" => "storage-failed",
                    _ => "worker-failed",
                };
                assert_eq!(
                    std::fs::read_to_string(realm.0.join("host-result")).unwrap(),
                    expected
                );
            }
            check_reopened(
                &realm.0,
                matches!(case, "complete" | "controlled-complete"),
                case == "cleanup-ack-error",
                matches!(
                    case,
                    "durable-cancel"
                        | "cancel-commit-kill"
                        | "cancel-ack-error"
                        | "cancel-ack-panic"
                ),
                if !controlled {
                    None
                } else {
                    use spindle_store::completion::ImageCleanup;
                    Some(
                        if matches!(
                            case,
                            "controlled-complete"
                                | "durable-cancel"
                                | "control-error"
                                | "cancel-ack-error"
                                | "cleanup-commit-kill"
                                | "cleanup-ack-error"
                                | "cleanup-ack-panic"
                        ) {
                            if dext_native::cgroup::delegated_root().is_some() {
                                ImageCleanup::TreeEmptied
                            } else {
                                ImageCleanup::RootReaped
                            }
                        } else {
                            ImageCleanup::Unknown
                        },
                    )
                },
            );
            println!("pinned worker lifecycle {case}: passed");
        }
    }
}
