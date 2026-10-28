#![cfg(feature = "dext-turn")]
use dext_core::model::{Encoding, Identity, ModelWait, PreparedRequest};
use dext_core::recovery::Contract;
use dext_core::turn::{Phase, Stop};
use dext_transcript::checkpoint::{NativeResponse, TranscriptCheckpoint};
use dext_transcript::turn::{Effect, TurnContinuation};
use dext_transcript::{Block, Message, Usage};
#[cfg(feature = "dext-provider")]
use sha2::Digest;
use spindle_store::completion::Attempt;
use spindle_store::transition::{Observation, Proposal};
use spindle_store::turn::{Action, ENGINE, execution_id, new_intents, receipt_binding};
use spindle_store::{Command, Error, Store};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Realm(PathBuf);
impl Realm {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "spindle-turn-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        )))
    }
}
impl Drop for Realm {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn request(state: &TurnContinuation) -> PreparedRequest<Vec<u8>> {
    PreparedRequest::new(
        Identity {
            effect: state.next_model_id().unwrap(),
            provider: "fake".into(),
            contract: "openai-responses".into(),
            endpoint: "https://example.invalid/responses".into(),
            model: "fake".into(),
            credential_scope: "operator/credential-v1".into(),
            configuration: "policy-v1".into(),
        },
        Encoding::Json,
        b"synthetic frozen request".to_vec(),
    )
    .unwrap()
}
fn state(store: &mut Store) -> TurnContinuation {
    TurnContinuation::restore(&store.snapshot().unwrap().continuation).unwrap()
}
fn action_proposal(
    store: &mut Store,
    id: &str,
    action: &Action,
    before: &TurnContinuation,
    after: &TurnContinuation,
) -> Proposal {
    action_proposal_in(store, 0, id, action, before, after)
}
/// Admissions carry the realm lineage, which each finalized rewind advances.
fn action_proposal_in(
    store: &mut Store,
    lineage: u64,
    id: &str,
    action: &Action,
    before: &TurnContinuation,
    after: &TurnContinuation,
) -> Proposal {
    let admission = store
        .admit(&Command {
            scope: "operator".into(),
            id: id.into(),
            lineage,
            kind: ENGINE.into(),
            payload: action.encode().unwrap(),
        })
        .unwrap();
    Proposal {
        id: format!("commit-{id}"),
        scope: "operator".into(),
        observation: Observation::Admission(admission.sequence),
        expected_revision: store.snapshot().unwrap().revision,
        owner_epoch: store.epoch(),
        engine: ENGINE.into(),
        continuation: after.checkpoint().unwrap(),
        workspace: None,
        effects: new_intents(Some(before), after).unwrap(),
        state: None,
    }
}
fn start(store: &mut Store, id: &str, transcript: TranscriptCheckpoint) -> Proposal {
    start_in(store, 0, id, transcript)
}
fn start_in(
    store: &mut Store,
    lineage: u64,
    id: &str,
    transcript: TranscriptCheckpoint,
) -> Proposal {
    let initial = TurnContinuation::new(
        "operator".into(),
        execution_id("operator", id).unwrap(),
        Contract::OpenAiResponses,
        Some(4),
        transcript,
    )
    .unwrap();
    let request = request(&initial);
    let next = initial.prepare(request.clone(), "route-v1").unwrap();
    let action = Action::Start {
        initial: initial.checkpoint().unwrap(),
        request: ModelWait::new(request).checkpoint(),
    };
    action_proposal_in(store, lineage, id, &action, &initial, &next)
}
fn response(waiting: &ModelWait<Vec<u8>>, calls: usize) -> ModelWait<Vec<u8>> {
    let mut blocks = vec![Block::Text {
        text: "synthetic response".into(),
    }];
    for i in 0..calls {
        blocks.push(Block::ToolUse {
            id: format!("call-{i}"),
            name: "inert-tool".into(),
            input: serde_json::json!({"position":i}),
        });
    }
    response_blocks(waiting, blocks)
}
fn response_blocks(waiting: &ModelWait<Vec<u8>>, blocks: Vec<Block>) -> ModelWait<Vec<u8>> {
    let response = NativeResponse {
        blocks,
        stop_reason: Some("stop".into()),
        usage: Usage::default(),
        unknown_events: 0,
        unfinished_tool_calls: 0,
    };
    waiting
        .record(
            waiting.request(),
            waiting.attempt(),
            &response.checkpoint().unwrap(),
        )
        .unwrap()
}
fn attempt(store: &mut Store, effect: &str, id: &str) -> Attempt {
    let attempt = Attempt {
        id: id.into(),
        effect: effect.into(),
        owner_epoch: store.epoch(),
        implementation: "fixture-v1".into(),
        grant_reference: "test-only".into(),
    };
    store.reserve_attempt(&attempt).unwrap();
    attempt
}
fn receipt_proposal(
    store: &mut Store,
    id: &str,
    attempt: &Attempt,
    before: &TurnContinuation,
    after: &TurnContinuation,
) -> Proposal {
    Proposal {
        id: id.into(),
        scope: "operator".into(),
        observation: Observation::Receipt(attempt.id.clone()),
        expected_revision: store.snapshot().unwrap().revision,
        owner_epoch: store.epoch(),
        engine: ENGINE.into(),
        continuation: after.checkpoint().unwrap(),
        workspace: None,
        effects: new_intents(Some(before), after).unwrap(),
        state: None,
    }
}
fn empty_transcript() -> TranscriptCheckpoint {
    TranscriptCheckpoint {
        route_binding: "route-v1".into(),
        messages: vec![Message {
            role: "user".into(),
            content: vec![Block::Text {
                text: "prompt".into(),
            }],
        }],
    }
}

fn dispatch_request(store: &Store, prepared: &Proposal) -> spindle_store::broker::DispatchRequest {
    let intent = prepared.effects[0].clone();
    spindle_store::broker::DispatchRequest {
        scope: "operator".into(),
        owner_epoch: store.epoch(),
        expected_revision: prepared.expected_revision + 1,
        intent_revision: prepared.expected_revision + 1,
        attempt: Attempt {
            id: "broker-attempt".into(),
            effect: intent.id.clone(),
            owner_epoch: store.epoch(),
            implementation: "fixture-v1".into(),
            grant_reference: "fresh-host-grant".into(),
        },
        intent,
    }
}

#[cfg(feature = "dext-provider")]
mod provider_tests {
    use super::*;
    use dext_native::model_effect::{Deadlines, LiveRoute, ModelTransport};
    use spindle_store::ModelError;
    use spindle_store::broker::{CommittedEffect, DispatchOutcome, DispatchRequest};
    use std::cell::Cell;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::{Duration, Instant};

    struct Mock {
        endpoint: String,
        stop: Arc<AtomicBool>,
        sends: Arc<AtomicUsize>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Mock {
        fn new(status: u16, body: impl Into<String>, delay: Duration) -> Self {
            Self::serve(status, body, delay, Some("text/event-stream"))
        }
        fn serve(
            status: u16,
            body: impl Into<String>,
            delay: Duration,
            content_type: Option<&'static str>,
        ) -> Self {
            let body = body.into();
            let typed = content_type
                .map(|kind| format!("Content-Type: {kind}\r\n"))
                .unwrap_or_default();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            let sends = Arc::new(AtomicUsize::new(0));
            let (stopped, count) = (stop.clone(), sends.clone());
            let thread = std::thread::spawn(move || {
                while !stopped.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(value) => value,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    };
                    // Windows sockets inherit the listener's non-blocking mode.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut request = vec![];
                    let mut chunk = [0; 4096];
                    while let Ok(length) = stream.read(&mut chunk) {
                        if length == 0 {
                            break;
                        }
                        request.extend_from_slice(&chunk[..length]);
                        assert!(request.len() < 64 * 1024);
                        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                        {
                            let headers =
                                String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                            let length = headers
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length: "))
                                .unwrap()
                                .parse::<usize>()
                                .unwrap();
                            if request.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    count.fetch_add(1, Ordering::SeqCst);
                    let deadline = Instant::now() + delay;
                    while Instant::now() < deadline && !stopped.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    if stopped.load(Ordering::SeqCst) {
                        break;
                    }
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} fixture\r\n{typed}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            });
            Self {
                endpoint,
                stop,
                sends,
                thread: Some(thread),
            }
        }
        fn good() -> Self {
            Self::new(200, GOOD_STREAM, Duration::ZERO)
        }
    }
    impl Drop for Mock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap();
        }
    }
    const GOOD_STREAM: &str = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n";

    /// The ChatGPT Codex backend streams SSE with no Content-Type header at all.
    #[tokio::test]
    async fn untyped_sse_stream_is_parsed_but_a_declared_non_sse_type_is_refused() {
        for (content_type, accepted) in [(None, true), (Some("application/json"), false)] {
            let realm = Realm::new();
            let mut store = Store::open(&realm.0).unwrap();
            let mock = Mock::serve(200, GOOD_STREAM, Duration::ZERO, content_type);
            let (proposal, request) = prepare(&mut store, &mock.endpoint);
            store.commit_turn_action(&proposal).unwrap();
            let result = store
                .dispatch_turn_model(
                    &request,
                    &AtomicBool::new(false),
                    &transport(),
                    |_| Ok(()),
                    live,
                )
                .await;
            if accepted {
                assert!(matches!(result, Ok(DispatchOutcome::Recorded(_))));
            } else {
                assert!(matches!(result, Err(Error::Model(ModelError::Protocol))));
            }
            assert_eq!(mock.sends.load(Ordering::SeqCst), 1);
        }
    }

    fn transport() -> ModelTransport {
        ModelTransport::new(Deadlines {
            connect: Duration::from_secs(1),
            first_byte: Duration::from_secs(1),
            idle: Duration::from_secs(1),
            total: Duration::from_secs(2),
        })
        .unwrap()
    }
    fn prepare(store: &mut Store, endpoint: &str) -> (Proposal, DispatchRequest) {
        let initial = TurnContinuation::new(
            "operator".into(),
            execution_id("operator", "provider").unwrap(),
            Contract::OpenAiResponses,
            Some(4),
            empty_transcript(),
        )
        .unwrap();
        let mut identity = request(&initial).identity().clone();
        identity.endpoint = endpoint.into();
        let request =
            PreparedRequest::new(identity, Encoding::Json, b"{\"stream\":true}".to_vec()).unwrap();
        let after = initial.prepare(request.clone(), "route-v1").unwrap();
        let proposal = action_proposal(
            store,
            "provider",
            &Action::Start {
                initial: initial.checkpoint().unwrap(),
                request: ModelWait::new(request).checkpoint(),
            },
            &initial,
            &after,
        );
        let mut dispatch = dispatch_request(store, &proposal);
        dispatch.attempt.implementation = "dext-provider-v1".into();
        (proposal, dispatch)
    }
    fn live(committed: &CommittedEffect) -> spindle_store::Result<LiveRoute> {
        let Effect::Model { waiting, .. } = committed.effect() else {
            panic!("model")
        };
        Ok(LiveRoute::new(waiting.request().identity().clone(), Default::default()).unwrap())
    }

    /// A caller that sends the request itself (as a host loop over a driver will):
    /// reserve, send once, ingest. The reply is reused after, also once the store is
    /// reopened, and the split path records exactly what `dispatch_turn_model` does.
    #[tokio::test]
    async fn a_reserved_request_is_sent_once_by_its_caller_then_recorded_and_reused() {
        use spindle_store::broker::ModelReservation;
        let mock = Mock::new(200, GOOD_STREAM, Duration::ZERO);
        let interrupt = AtomicBool::new(false);
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let (proposal, mut request) = prepare(&mut store, &mock.endpoint);
        store.commit_turn_action(&proposal).unwrap();
        let ModelReservation::Reserved(committed) = store
            .reserve_turn_model(&request, &interrupt, |_| Ok(()))
            .unwrap()
        else {
            panic!("a fresh request is reserved, not settled")
        };
        let Effect::Model { waiting, .. } = committed.effect() else {
            panic!("model")
        };
        let recorded = transport()
            .send_once_observed(waiting, live(&committed).unwrap(), &interrupt, &|_| {})
            .await
            .unwrap();
        let DispatchOutcome::Recorded(bytes) = store
            .ingest_turn_model(&request, &committed, recorded.checkpoint())
            .unwrap()
        else {
            panic!("recorded")
        };
        assert!(matches!(
            store.reserve_turn_model(&request, &interrupt, |_| Ok(())).unwrap(),
            ModelReservation::Settled(DispatchOutcome::Reused(ref saved)) if *saved == bytes
        ));
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        request.owner_epoch = store.epoch();
        assert!(matches!(
            store.reserve_turn_model(&request, &interrupt, |_| Ok(())).unwrap(),
            ModelReservation::Settled(DispatchOutcome::Reused(ref saved)) if *saved == bytes
        ));
        assert_eq!(mock.sends.load(Ordering::SeqCst), 1);
        // The one-call path records the same bytes for the same request and reply.
        let other = Realm::new();
        let mut store = Store::open(&other.0).unwrap();
        let (proposal, request) = prepare(&mut store, &mock.endpoint);
        store.commit_turn_action(&proposal).unwrap();
        let wrapped = store
            .dispatch_turn_model(&request, &interrupt, &transport(), |_| Ok(()), live)
            .await
            .unwrap();
        assert!(matches!(wrapped, DispatchOutcome::Recorded(ref saved) if *saved == bytes));
    }

    /// A reservation dropped without a response (its caller stopped, or died) is never
    /// reserved again: it is one to reconcile, in this epoch and after a reopen.
    #[tokio::test]
    async fn a_reservation_never_ingested_is_reconciled_and_never_reserved_twice() {
        use spindle_store::broker::ModelReservation;
        let mock = Mock::good();
        let interrupt = AtomicBool::new(false);
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let (proposal, mut request) = prepare(&mut store, &mock.endpoint);
        store.commit_turn_action(&proposal).unwrap();
        assert!(matches!(
            store
                .reserve_turn_model(&request, &interrupt, |_| Ok(()))
                .unwrap(),
            ModelReservation::Reserved(_)
        ));
        assert!(matches!(
            store
                .reserve_turn_model(&request, &interrupt, |_| Ok(()))
                .unwrap(),
            ModelReservation::Settled(DispatchOutcome::Reconcile)
        ));
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        request.owner_epoch = store.epoch();
        assert!(matches!(
            store
                .reserve_turn_model(&request, &interrupt, |_| Ok(()))
                .unwrap(),
            ModelReservation::Settled(DispatchOutcome::Reconcile)
        ));
        assert_eq!(mock.sends.load(Ordering::SeqCst), 0, "nothing was sent");
    }

    /// A response is recorded only against the reservation it was sent for.
    #[tokio::test]
    async fn a_response_is_recorded_only_for_its_own_reservation() {
        use spindle_store::broker::ModelReservation;
        let mock = Mock::good();
        let interrupt = AtomicBool::new(false);
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let (proposal, request) = prepare(&mut store, &mock.endpoint);
        store.commit_turn_action(&proposal).unwrap();
        let ModelReservation::Reserved(committed) = store
            .reserve_turn_model(&request, &interrupt, |_| Ok(()))
            .unwrap()
        else {
            panic!("reserved")
        };
        let Effect::Model { waiting, .. } = committed.effect() else {
            panic!("model")
        };
        let recorded = transport()
            .send_once_observed(waiting, live(&committed).unwrap(), &interrupt, &|_| {})
            .await
            .unwrap();
        let mut other = dispatch_request(&store, &proposal);
        other.attempt.implementation = "dext-provider-v1".into();
        other.attempt.id = "another-attempt".into();
        assert!(matches!(
            store.ingest_turn_model(&other, &committed, recorded.checkpoint()),
            Err(Error::Conflict)
        ));
        // Still open: the reservation's own response is recorded.
        assert!(matches!(
            store
                .ingest_turn_model(&request, &committed, recorded.checkpoint())
                .unwrap(),
            DispatchOutcome::Recorded(_)
        ));
    }

    /// Opt-in real provider call. Credentials come only from the environment and are
    /// never printed; the test makes exactly one paid send, then proves reuse is free.
    #[tokio::test]
    #[ignore = "live provider; set SPINDLE_LIVE_KEY and run with --ignored"]
    async fn live_provider_dispatch_records_once_then_reuses_without_resend() {
        let Ok(key) = std::env::var("SPINDLE_LIVE_KEY") else {
            panic!("SPINDLE_LIVE_KEY unset")
        };
        let endpoint = std::env::var("SPINDLE_LIVE_ENDPOINT")
            .unwrap_or_else(|_| "https://api.deepseek.com/chat/completions".into());
        let model = std::env::var("SPINDLE_LIVE_MODEL").unwrap_or_else(|_| "deepseek-chat".into());
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let initial = TurnContinuation::new(
            "operator".into(),
            execution_id("operator", "live").unwrap(),
            Contract::OpenAiChatCompletions,
            Some(2),
            empty_transcript(),
        )
        .unwrap();
        let mut identity = request(&initial).identity().clone();
        identity.provider = "live".into();
        identity.contract = "openai-chat-completions".into();
        identity.endpoint = endpoint;
        identity.model = model.clone();
        let body = serde_json::to_vec(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Reply with exactly the word: ok"}],
            "stream": true,
            "max_tokens": 8,
        }))
        .unwrap();
        let prepared = PreparedRequest::new(identity, Encoding::Json, body).unwrap();
        let after = initial.prepare(prepared.clone(), "route-v1").unwrap();
        let proposal = action_proposal(
            &mut store,
            "live",
            &Action::Start {
                initial: initial.checkpoint().unwrap(),
                request: ModelWait::new(prepared).checkpoint(),
            },
            &initial,
            &after,
        );
        store.commit_turn_action(&proposal).unwrap();
        let mut dispatch = dispatch_request(&store, &proposal);
        dispatch.attempt.implementation = "dext-provider-v1".into();
        let transport = ModelTransport::new(Deadlines {
            connect: Duration::from_secs(10),
            first_byte: Duration::from_secs(60),
            idle: Duration::from_secs(30),
            total: Duration::from_secs(90),
        })
        .unwrap();
        let resolved = Cell::new(0);
        let outcome = store
            .dispatch_turn_model(
                &dispatch,
                &AtomicBool::new(false),
                &transport,
                |_| Ok(()),
                |committed| {
                    resolved.set(resolved.get() + 1);
                    let Effect::Model { waiting, .. } = committed.effect() else {
                        panic!("model")
                    };
                    let mut headers = reqwest::header::HeaderMap::new();
                    headers.insert(
                        "authorization",
                        format!("Bearer {key}")
                            .parse()
                            .map_err(|_| Error::Invalid)?,
                    );
                    LiveRoute::new(waiting.request().identity().clone(), headers)
                        .map_err(|_| Error::Invalid)
                },
            )
            .await;
        let DispatchOutcome::Recorded(bytes) =
            outcome.unwrap_or_else(|error| panic!("live dispatch: {error:?}"))
        else {
            panic!("expected fresh receipt")
        };
        assert_eq!(
            resolved.get(),
            1,
            "credentials resolved exactly once, after reservation"
        );
        let before = state(&mut store);
        let review = before
            .apply_model(&ModelWait::restore(&bytes).unwrap(), "route-v1")
            .unwrap();
        let text: String = review
            .transcript()
            .unwrap()
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(!text.trim().is_empty(), "live provider returned no text");
        let applied = receipt_proposal(
            &mut store,
            "live-apply",
            &dispatch.attempt,
            &before,
            &review,
        );
        store
            .commit_turn_receipt(
                &applied,
                &receipt_binding(
                    &dispatch.attempt,
                    dispatch.intent_revision,
                    &dispatch.intent,
                    &bytes,
                ),
            )
            .unwrap();
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        dispatch.owner_epoch = store.epoch();
        let reused = store
            .dispatch_turn_model(
                &dispatch,
                &AtomicBool::new(false),
                &transport,
                |_| Ok(()),
                |_| panic!("reused receipt resolved credentials or resent"),
            )
            .await
            .unwrap();
        assert!(matches!(reused, DispatchOutcome::Reused(saved) if saved == bytes));
        let digest: String = sha2::Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        println!(
            "live ok: receipt_bytes={} receipt_sha256={digest} reply_chars={} phase={:?}",
            bytes.len(),
            text.chars().count(),
            state(&mut store).control().phase()
        );
    }

    #[cfg(feature = "dext-strict-workspace")]
    fn scoped_state(store: &mut Store, scope: &str) -> TurnContinuation {
        TurnContinuation::restore(&store.snapshot_scope(scope).unwrap().continuation).unwrap()
    }

    #[cfg(feature = "dext-strict-workspace")]
    fn scoped_action(
        store: &mut Store,
        scope: &str,
        id: &str,
        action: Action,
        after: &TurnContinuation,
    ) -> Proposal {
        let before = scoped_state(store, scope);
        let admission = store
            .admit(&Command {
                scope: scope.into(),
                id: id.into(),
                lineage: 0,
                kind: ENGINE.into(),
                payload: action.encode().unwrap(),
            })
            .unwrap();
        Proposal {
            id: format!("{scope}-{id}"),
            scope: scope.into(),
            observation: Observation::Admission(admission.sequence),
            expected_revision: store.snapshot_scope(scope).unwrap().revision,
            owner_epoch: store.epoch(),
            engine: ENGINE.into(),
            continuation: after.checkpoint().unwrap(),
            workspace: None,
            effects: new_intents(Some(&before), after).unwrap(),
            state: None,
        }
    }

    #[cfg(feature = "dext-strict-workspace")]
    fn scoped_request(state: &TurnContinuation, endpoint: &str) -> PreparedRequest<Vec<u8>> {
        let mut identity = request(state).identity().clone();
        identity.endpoint = endpoint.into();
        PreparedRequest::new(identity, Encoding::Json, b"{\"stream\":true}".to_vec()).unwrap()
    }

    #[cfg(feature = "dext-strict-workspace")]
    fn start_owned(
        store: &mut Store,
        scope: &str,
        parent: Option<&str>,
        endpoint: &str,
    ) -> DispatchRequest {
        use spindle_store::artifact::Stage;
        use spindle_store::ownership::ScopeKind;
        let initial = TurnContinuation::new(
            scope.into(),
            execution_id(scope, "start").unwrap(),
            Contract::OpenAiResponses,
            Some(4),
            empty_transcript(),
        )
        .unwrap();
        let request = scoped_request(&initial, endpoint);
        let next = initial.prepare(request.clone(), "route-v1").unwrap();
        let action = Action::Start {
            initial: initial.checkpoint().unwrap(),
            request: ModelWait::new(request).checkpoint(),
        };
        let admission = store
            .admit_owned(
                parent,
                ScopeKind::Task,
                &Command {
                    scope: scope.into(),
                    id: "start".into(),
                    lineage: 0,
                    kind: ENGINE.into(),
                    payload: action.encode().unwrap(),
                },
            )
            .unwrap();
        let stage = Stage {
            id: format!("{scope}-base"),
            scope: scope.into(),
            owner_epoch: store.epoch(),
        };
        store.begin_stage(&stage).unwrap();
        let object = store.put_object(&stage, scope.as_bytes()).unwrap();
        let root = store
            .stage_file_map(
                &stage,
                &[spindle_store::workspace::Entry {
                    path: "input.txt".into(),
                    object,
                    executable: false,
                }],
            )
            .unwrap();
        let proposal = Proposal {
            id: format!("{scope}-start"),
            scope: scope.into(),
            observation: Observation::Admission(admission.sequence),
            expected_revision: 0,
            owner_epoch: store.epoch(),
            engine: ENGINE.into(),
            continuation: next.checkpoint().unwrap(),
            workspace: Some(root),
            effects: new_intents(Some(&initial), &next).unwrap(),
            state: None,
        };
        let revision = store.commit_turn_workspace_start(&proposal).unwrap();
        scoped_dispatch(
            store,
            scope,
            revision,
            proposal.effects[0].clone(),
            "initial",
        )
    }

    #[cfg(feature = "dext-strict-workspace")]
    fn scoped_dispatch(
        store: &Store,
        scope: &str,
        revision: u64,
        intent: spindle_store::transition::EffectIntent,
        name: &str,
    ) -> DispatchRequest {
        DispatchRequest {
            scope: scope.into(),
            owner_epoch: store.epoch(),
            expected_revision: revision,
            intent_revision: revision,
            attempt: Attempt {
                id: format!("{scope}-{name}"),
                effect: intent.id.clone(),
                owner_epoch: store.epoch(),
                implementation: "dext-provider-v1".into(),
                grant_reference: "synthetic-current-policy".into(),
            },
            intent,
        }
    }

    #[cfg(feature = "dext-strict-workspace")]
    async fn apply_scoped_model(
        store: &mut Store,
        request: &DispatchRequest,
        transport: &ModelTransport,
        id: &str,
    ) -> Proposal {
        let DispatchOutcome::Recorded(bytes) = store
            .dispatch_turn_model(
                request,
                &AtomicBool::new(false),
                transport,
                |_| Ok(()),
                live,
            )
            .await
            .unwrap()
        else {
            panic!("ingress")
        };
        let before = scoped_state(store, &request.scope);
        let after = before
            .apply_model(&ModelWait::restore(&bytes).unwrap(), "route-v1")
            .unwrap();
        let proposal = Proposal {
            id: id.into(),
            scope: request.scope.clone(),
            observation: Observation::Receipt(request.attempt.id.clone()),
            expected_revision: store.snapshot_scope(&request.scope).unwrap().revision,
            owner_epoch: store.epoch(),
            engine: ENGINE.into(),
            continuation: after.checkpoint().unwrap(),
            workspace: None,
            effects: new_intents(Some(&before), &after).unwrap(),
            state: None,
        };
        store
            .commit_turn_receipt(
                &proposal,
                &receipt_binding(
                    &request.attempt,
                    request.intent_revision,
                    &request.intent,
                    &bytes,
                ),
            )
            .unwrap();
        proposal
    }

    #[cfg(all(target_os = "linux", feature = "dext-strict-workspace"))]
    #[tokio::test]
    async fn owned_provider_and_strict_child_keep_parent_state_and_workspace_pinned_across_takeover()
     {
        let realm = Realm::new();
        let parent_provider = Mock::good();
        let final_provider = Mock::good();
        let function = serde_json::json!({"type":"function_call","id":"item","call_id":"native-call",
            "name":"bash","arguments": "{\"command\":\"cat input.txt; printf child-output > result.txt\",\"timeout\":5}"});
        let body = format!(
            "data: {}\n\ndata: {}\n\n",
            serde_json::json!({"type":"response.output_item.added", "item":function}),
            serde_json::json!({"type":"response.completed", "response":{
            "status":"completed", "output":[function]}})
        );
        let child_provider = Mock::new(200, body, Duration::ZERO);
        let transport = transport();
        let executor = strict_executor();
        let mut store = Store::open(&realm.0).unwrap();
        let mut parent_request = start_owned(&mut store, "owner", None, &parent_provider.endpoint);
        let parent = store.snapshot_scope("owner").unwrap();
        let child_request =
            start_owned(&mut store, "child", Some("owner"), &child_provider.endpoint);
        assert_eq!(store.snapshot_scope("owner").unwrap(), parent);
        assert!(matches!(store.settle_owned("owner"), Err(Error::Conflict)));
        let tools = apply_scoped_model(
            &mut store,
            &child_request,
            &transport,
            "child-model-applied",
        )
        .await;
        let revision = store.snapshot_scope("child").unwrap().revision;
        let mut bash = scoped_dispatch(&store, "child", revision, tools.effects[0].clone(), "bash");
        let helper: String = executor
            .helper_digest()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        bash.attempt.implementation = format!("dext-strict-bash-v1-{helper}");
        let DispatchOutcome::Recorded(bytes) = store
            .dispatch_strict_workspace(&bash, &AtomicBool::new(false), &executor, |_| Ok(()))
            .unwrap()
        else {
            panic!("strict ingress")
        };
        assert_eq!(store.snapshot_scope("owner").unwrap(), parent);
        assert_eq!(store.snapshot_scope("child").unwrap().revision, revision);
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        bash.owner_epoch = store.epoch();
        assert!(
            matches!(store.dispatch_strict_workspace(&bash, &AtomicBool::new(false), &executor,
            |_| Ok(())).unwrap(), DispatchOutcome::Reused(saved) if saved == bytes)
        );
        let before = scoped_state(&mut store, "child");
        let after = before
            .apply_tool(
                &bash.intent.id,
                vec![serde_json::from_slice(&bytes).unwrap()],
            )
            .unwrap();
        let root = store.workspace_receipt(&bash.attempt.id).unwrap().unwrap();
        let applied = Proposal {
            id: "child-bash-applied".into(),
            scope: "child".into(),
            observation: Observation::Receipt(bash.attempt.id.clone()),
            expected_revision: revision,
            owner_epoch: store.epoch(),
            engine: ENGINE.into(),
            continuation: after.checkpoint().unwrap(),
            workspace: Some(root.clone()),
            effects: new_intents(Some(&before), &after).unwrap(),
            state: None,
        };
        store
            .commit_turn_workspace_receipt(
                &applied,
                &receipt_binding(&bash.attempt, bash.intent_revision, &bash.intent, &bytes),
                *executor.helper_digest(),
            )
            .unwrap();
        assert_eq!(
            store
                .read_versioned_file("child", &root, "result.txt")
                .unwrap(),
            b"child-output"
        );
        assert!(
            store
                .read_versioned_file("owner", &root, "result.txt")
                .is_err()
        );
        assert_eq!(store.snapshot_scope("owner").unwrap(), parent);
        let before = scoped_state(&mut store, "child");
        let prepared = scoped_request(&before, &final_provider.endpoint);
        let after = before.prepare(prepared.clone(), "route-v1").unwrap();
        let proposal = scoped_action(
            &mut store,
            "child",
            "prepare-final",
            Action::Prepare {
                request: ModelWait::new(prepared).checkpoint(),
            },
            &after,
        );
        let revision = store.commit_turn_action(&proposal).unwrap();
        let final_request = scoped_dispatch(
            &store,
            "child",
            revision,
            proposal.effects[0].clone(),
            "final-model",
        );
        apply_scoped_model(
            &mut store,
            &final_request,
            &transport,
            "child-final-applied",
        )
        .await;
        let done = scoped_state(&mut store, "child").finish().unwrap();
        let proposal = scoped_action(&mut store, "child", "finish", Action::Finish, &done);
        store.commit_turn_action(&proposal).unwrap();
        store.settle_owned("child").unwrap();
        parent_request.owner_epoch = store.epoch();
        parent_request.attempt.owner_epoch = store.epoch();
        apply_scoped_model(
            &mut store,
            &parent_request,
            &transport,
            "owner-final-applied",
        )
        .await;
        let done = scoped_state(&mut store, "owner").finish().unwrap();
        let proposal = scoped_action(&mut store, "owner", "finish", Action::Finish, &done);
        let completed = store.commit_turn_action(&proposal).unwrap();
        let cut = store.capture_owned_cut("owner", completed).unwrap();
        assert_eq!(cut.members.len(), 2);
        assert_eq!(cut.members[0].scope, "owner");
        assert_eq!(cut.members[1].scope, "child");
        assert_eq!(cut.members[1].workspace, root);
        assert_eq!(store.capture_owned_cut("owner", completed).unwrap(), cut);
        store.settle_owned("owner").unwrap();
        assert_eq!(
            store.snapshot_scope("owner").unwrap().workspace,
            parent.workspace
        );
        let child_before = store.snapshot_scope("child").unwrap();
        store
            .begin_rewind("owner", "owned-rewind", completed, completed, 0)
            .unwrap();
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        assert_eq!(
            store.owned_execution_cut("owner", completed).unwrap(),
            Some(cut.clone())
        );
        let finalized = store.finish_rewind("owner", "owned-rewind").unwrap();
        assert_eq!(finalized.revision, Some(completed + 1));
        assert_eq!(store.snapshot_scope("child").unwrap(), child_before);
        assert_eq!(store.receipt(&bash.attempt.id).unwrap(), Some(bytes));
        store.resume_rewind("owner", "owned-rewind").unwrap();
        store.cancel_scope("owner", "later-cancel").unwrap();
        assert_eq!(
            store.owned_execution_cut("owner", completed).unwrap(),
            Some(cut)
        );
        assert_eq!(parent_provider.sends.load(Ordering::SeqCst), 1);
        assert_eq!(child_provider.sends.load(Ordering::SeqCst), 1);
        assert_eq!(final_provider.sends.load(Ordering::SeqCst), 1);
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        assert!(
            store
                .owned_scopes()
                .unwrap()
                .iter()
                .all(|scope| scope.state == spindle_store::ownership::ScopeState::Settled)
        );
        assert_eq!(
            store
                .read_versioned_file("child", &root, "result.txt")
                .unwrap(),
            b"child-output"
        );
    }

    #[cfg(feature = "dext-strict-workspace")]
    async fn completed_owned_fixture() -> (Realm, Store, u64) {
        completed_owned_fixture_at(Realm::new()).await
    }

    #[cfg(feature = "dext-strict-workspace")]
    async fn completed_owned_fixture_at(realm: Realm) -> (Realm, Store, u64) {
        let mock = Mock::good();
        let transport = transport();
        let mut store = Store::open(&realm.0).unwrap();
        let owner = start_owned(&mut store, "owner", None, &mock.endpoint);
        let child = start_owned(&mut store, "child", Some("owner"), &mock.endpoint);
        apply_scoped_model(&mut store, &child, &transport, "child-apply").await;
        let done = scoped_state(&mut store, "child").finish().unwrap();
        let finish = scoped_action(&mut store, "child", "finish", Action::Finish, &done);
        store.commit_turn_action(&finish).unwrap();
        store.settle_owned("child").unwrap();
        apply_scoped_model(&mut store, &owner, &transport, "owner-apply").await;
        let done = scoped_state(&mut store, "owner").finish().unwrap();
        let finish = scoped_action(&mut store, "owner", "finish", Action::Finish, &done);
        let revision = store.commit_turn_action(&finish).unwrap();
        (realm, store, revision)
    }

    #[cfg(feature = "dext-strict-workspace")]
    async fn rewound_task_fixture(
        endpoint: &str,
    ) -> (
        Realm,
        Store,
        spindle_store::rewind::RewoundTaskRequest,
        TurnContinuation,
    ) {
        rewound_task_fixture_at(Realm::new(), endpoint).await
    }

    #[cfg(feature = "dext-strict-workspace")]
    async fn rewound_task_fixture_at(
        realm: Realm,
        endpoint: &str,
    ) -> (
        Realm,
        Store,
        spindle_store::rewind::RewoundTaskRequest,
        TurnContinuation,
    ) {
        use spindle_store::rewind::RewoundTaskRequest;
        let (realm, mut store, completed) = completed_owned_fixture_at(realm).await;
        store.capture_owned_cut("owner", completed).unwrap();
        store.settle_owned("owner").unwrap();
        store
            .begin_rewind("owner", "restart", completed, completed, 0)
            .unwrap();
        let finalized = store.finish_rewind("owner", "restart").unwrap();
        let source_revision = finalized.revision.unwrap();
        let source = scoped_state(&mut store, "owner");
        let initial = source
            .restart_settled(
                "fresh-owner".into(),
                execution_id("fresh-owner", "fresh-start").unwrap(),
                "explicit fresh work".into(),
            )
            .unwrap();
        let prepared = scoped_request(&initial, endpoint);
        let next = initial.prepare(prepared.clone(), "route-v1").unwrap();
        let request = RewoundTaskRequest {
            source_scope: "owner".into(),
            rewind_command: "restart".into(),
            source_revision,
            command: Command {
                scope: "fresh-owner".into(),
                id: "fresh-start".into(),
                lineage: 1,
                kind: ENGINE.into(),
                payload: Action::Start {
                    initial: initial.checkpoint().unwrap(),
                    request: ModelWait::new(prepared).checkpoint(),
                }
                .encode()
                .unwrap(),
            },
        };
        (realm, store, request, next)
    }

    #[cfg(feature = "dext-strict-workspace")]
    #[tokio::test]
    #[ignore = "invoked by owned recovery process-kill test"]
    async fn owned_recovery_cut_child() {
        let realm = Realm(PathBuf::from(
            std::env::var_os("SPINDLE_OWNED_REALM").unwrap(),
        ));
        let mode = std::env::var("SPINDLE_OWNED_CUT").unwrap();
        let (realm, mut store, completed) = completed_owned_fixture_at(realm).await;
        std::fs::write(realm.0.join("completed-revision"), completed.to_string()).unwrap();
        if mode == "cut-before" {
            wait_for_kill(&realm.0);
        }
        store.capture_owned_cut("owner", completed).unwrap();
        if mode == "cut-after" {
            wait_for_kill(&realm.0);
        }
        store.settle_owned("owner").unwrap();
        if mode == "settled" {
            wait_for_kill(&realm.0);
        }
        store
            .begin_rewind("owner", "restart", completed, completed, 0)
            .unwrap();
        if mode == "pending" {
            wait_for_kill(&realm.0);
        }
        if mode.starts_with("cancelled") {
            store.cancel_rewind("owner", "restart").unwrap();
            if mode == "cancelled-paused" {
                wait_for_kill(&realm.0);
            }
            store.resume_rewind("owner", "restart").unwrap();
            assert_eq!(mode, "cancelled-resumed");
            wait_for_kill(&realm.0);
        }
        let source_revision = store
            .finish_rewind("owner", "restart")
            .unwrap()
            .revision
            .unwrap();
        if mode == "finalized" {
            wait_for_kill(&realm.0);
        }
        store.resume_rewind("owner", "restart").unwrap();
        let mock = Mock::good();
        // A parent-owned provider lets the parent observe the send and kill mid-flight.
        let endpoint =
            std::env::var("SPINDLE_OWNED_ENDPOINT").unwrap_or_else(|_| mock.endpoint.clone());
        let source = scoped_state(&mut store, "owner");
        let initial = source
            .restart_settled(
                "fresh-owner".into(),
                execution_id("fresh-owner", "fresh-start").unwrap(),
                "explicit fresh work".into(),
            )
            .unwrap();
        let prepared = scoped_request(&initial, &endpoint);
        let next = initial.prepare(prepared.clone(), "route-v1").unwrap();
        let payload = Action::Start {
            initial: initial.checkpoint().unwrap(),
            request: ModelWait::new(prepared).checkpoint(),
        }
        .encode()
        .unwrap();
        std::fs::write(realm.0.join("restart-input"), &payload).unwrap();
        let request = spindle_store::rewind::RewoundTaskRequest {
            source_scope: "owner".into(),
            rewind_command: "restart".into(),
            source_revision,
            command: Command {
                scope: "fresh-owner".into(),
                id: "fresh-start".into(),
                lineage: 1,
                kind: ENGINE.into(),
                payload,
            },
        };
        if mode == "restart-before" {
            wait_for_kill(&realm.0);
        }
        let admitted = store.admit_rewound_task(&request, |_| Ok(())).unwrap();
        if mode == "restart-after" {
            wait_for_kill(&realm.0);
        }
        let proposal = Proposal {
            id: "fresh-start-commit".into(),
            scope: "fresh-owner".into(),
            observation: Observation::Admission(admitted.admission.sequence),
            expected_revision: 0,
            owner_epoch: store.epoch(),
            engine: ENGINE.into(),
            continuation: next.checkpoint().unwrap(),
            workspace: Some(admitted.workspace),
            effects: new_intents(None, &next).unwrap(),
            state: None,
        };
        let revision = store.commit_turn_workspace_start(&proposal).unwrap();
        if mode == "intent" {
            wait_for_kill(&realm.0);
        }
        let dispatch = scoped_dispatch(
            &store,
            "fresh-owner",
            revision,
            proposal.effects[0].clone(),
            "new-model",
        );
        // The in-flight cut needs the request held open until the parent kills us.
        let transport = if mode == "in-flight" {
            ModelTransport::new(Deadlines {
                connect: Duration::from_secs(1),
                first_byte: Duration::from_secs(30),
                idle: Duration::from_secs(30),
                total: Duration::from_secs(60),
            })
            .unwrap()
        } else {
            transport()
        };
        let checks = Cell::new(0);
        let DispatchOutcome::Recorded(bytes) = store
            .dispatch_turn_model(
                &dispatch,
                &AtomicBool::new(false),
                &transport,
                |_| {
                    checks.set(checks.get() + 1);
                    if mode == "reserved" && checks.get() == 2 {
                        wait_for_kill(&realm.0);
                    }
                    Ok(())
                },
                live,
            )
            .await
            .unwrap()
        else {
            panic!("receipt")
        };
        assert_eq!(mode, "receipt");
        assert_eq!(mock.sends.load(Ordering::SeqCst), 1);
        assert!(!bytes.is_empty());
        wait_for_kill(&realm.0);
    }

    #[cfg(feature = "dext-strict-workspace")]
    #[tokio::test]
    async fn owned_cut_and_restart_process_kills_preserve_scope_chains_without_redispatch() {
        use spindle_store::rewind::RewindPhase;
        struct Guard(std::process::Child);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        for mode in [
            "cut-before",
            "cut-after",
            "settled",
            "pending",
            "cancelled-paused",
            "cancelled-resumed",
            "finalized",
            "restart-before",
            "restart-after",
            "intent",
            "reserved",
            "in-flight",
            "receipt",
        ] {
            let realm = Realm::new();
            // The in-flight cut kills the owner once this provider holds its request.
            let slow = (mode == "in-flight").then(|| Mock::new(200, "", Duration::from_secs(30)));
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "provider_tests::owned_recovery_cut_child",
                    "--ignored",
                ])
                .env("SPINDLE_OWNED_REALM", &realm.0)
                .env("SPINDLE_OWNED_CUT", mode)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit());
            if let Some(slow) = &slow {
                command.env("SPINDLE_OWNED_ENDPOINT", &slow.endpoint);
            }
            let mut child = Guard(command.spawn().unwrap());
            let deadline = Instant::now() + Duration::from_secs(15);
            while !slow.as_ref().map_or_else(
                || realm.0.join("cut-ready").is_file(),
                |slow| slow.sends.load(Ordering::SeqCst) > 0,
            ) {
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "child exited at {mode}"
                );
                assert!(Instant::now() < deadline, "child stalled at {mode}");
                std::thread::sleep(Duration::from_millis(5));
            }
            drop(child);
            let completed: u64 = std::fs::read_to_string(realm.0.join("completed-revision"))
                .unwrap()
                .parse()
                .unwrap();
            let mut store = Store::open(&realm.0).unwrap();
            let child_before = store.snapshot_scope("child").unwrap();
            assert_eq!(
                scoped_state(&mut store, "child").control().phase(),
                Phase::Stopped(Stop::Complete)
            );
            let cut = store.owned_execution_cut("owner", completed).unwrap();
            if mode == "cut-before" {
                assert!(cut.is_none());
                assert_eq!(
                    store
                        .capture_owned_cut("owner", completed)
                        .unwrap()
                        .members
                        .len(),
                    2
                );
            } else {
                assert_eq!(cut.unwrap().members.len(), 2);
            }
            if matches!(mode, "cut-before" | "cut-after" | "settled") {
                assert!(store.rewind_status("owner", "restart").unwrap().is_none());
                assert_eq!(store.snapshot().unwrap().revision, completed);
                let owner = store
                    .owned_scopes()
                    .unwrap()
                    .into_iter()
                    .find(|node| node.scope == "owner")
                    .unwrap();
                assert_eq!(
                    owner.state == spindle_store::ownership::ScopeState::Settled,
                    mode == "settled",
                    "{mode}"
                );
                if mode == "settled" {
                    store
                        .begin_rewind("owner", "restart", completed, completed, 0)
                        .unwrap();
                    assert_eq!(
                        store.finish_rewind("owner", "restart").unwrap().revision,
                        Some(completed + 1)
                    );
                }
                assert_eq!(store.snapshot_scope("child").unwrap(), child_before);
                continue;
            }
            let plan = store.rewind_status("owner", "restart").unwrap().unwrap();
            if mode.starts_with("cancelled") {
                let paused = mode == "cancelled-paused";
                assert_eq!(
                    plan.phase,
                    if paused {
                        RewindPhase::CancelledPaused
                    } else {
                        RewindPhase::CancelledResumed
                    }
                );
                assert_eq!(plan.revision, None);
                assert_eq!(store.snapshot().unwrap().revision, completed);
                assert!(store.finish_rewind("owner", "restart").is_err());
                let initial = scoped_state(&mut store, "owner")
                    .restart_settled(
                        "fresh-owner".into(),
                        execution_id("fresh-owner", "fresh-start").unwrap(),
                        "explicit fresh work".into(),
                    )
                    .unwrap();
                let prepared = scoped_request(&initial, "http://127.0.0.1:9/responses");
                let restart = spindle_store::rewind::RewoundTaskRequest {
                    source_scope: "owner".into(),
                    rewind_command: "restart".into(),
                    source_revision: completed,
                    command: Command {
                        scope: "fresh-owner".into(),
                        id: "fresh-start".into(),
                        lineage: 1,
                        kind: ENGINE.into(),
                        payload: Action::Start {
                            initial: initial.checkpoint().unwrap(),
                            request: ModelWait::new(prepared).checkpoint(),
                        }
                        .encode()
                        .unwrap(),
                    },
                };
                if paused {
                    assert!(matches!(
                        store.admit_rewound_task(&restart, |_| Ok(())),
                        Err(Error::Paused)
                    ));
                    store.resume_rewind("owner", "restart").unwrap();
                }
                assert!(matches!(
                    store.admit_rewound_task(&restart, |_| Ok(())),
                    Err(Error::Conflict)
                ));
                let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
                let restarted: i64 = db
                    .query_row("SELECT count(*) FROM rewound_tasks", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(restarted, 0, "{mode}");
                assert_eq!(store.snapshot_scope("child").unwrap(), child_before);
                assert_eq!(store.snapshot_scope("owner").unwrap().revision, completed);
                continue;
            }
            if mode == "pending" {
                assert_eq!(plan.phase, RewindPhase::Pending);
                assert_eq!(store.snapshot().unwrap().revision, completed);
                store.finish_rewind("owner", "restart").unwrap();
                store.resume_rewind("owner", "restart").unwrap();
                assert_eq!(store.snapshot_scope("child").unwrap(), child_before);
                continue;
            }
            if mode == "finalized" {
                assert_eq!(plan.phase, RewindPhase::FinishedPaused);
                assert_eq!(store.snapshot().unwrap().revision, completed + 1);
                store.resume_rewind("owner", "restart").unwrap();
                assert_eq!(store.snapshot_scope("child").unwrap(), child_before);
                continue;
            }
            assert_eq!(plan.phase, RewindPhase::Resumed);
            let payload = std::fs::read(realm.0.join("restart-input")).unwrap();
            let Action::Start {
                initial,
                request: prepared,
            } = Action::decode(&payload).unwrap()
            else {
                panic!("start")
            };
            let initial = TurnContinuation::restore(&initial).unwrap();
            let next = initial
                .prepare(
                    ModelWait::restore(&prepared).unwrap().request().clone(),
                    "route-v1",
                )
                .unwrap();
            let request = spindle_store::rewind::RewoundTaskRequest {
                source_scope: "owner".into(),
                rewind_command: "restart".into(),
                source_revision: completed + 1,
                command: Command {
                    scope: "fresh-owner".into(),
                    id: "fresh-start".into(),
                    lineage: 1,
                    kind: ENGINE.into(),
                    payload,
                },
            };
            let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
            let count: i64 = db
                .query_row("SELECT count(*) FROM rewound_tasks", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, i64::from(mode != "restart-before"), "{mode}");
            let admitted = store.admit_rewound_task(&request, |_| Ok(())).unwrap();
            assert_eq!(
                store
                    .read_versioned_file("fresh-owner", &admitted.workspace, "input.txt")
                    .unwrap(),
                b"owner"
            );
            let mut revision = store.snapshot_scope("fresh-owner").unwrap().revision;
            if matches!(mode, "restart-before" | "restart-after") {
                assert_eq!(revision, 0);
                assert_eq!(store.snapshot().unwrap().revision, completed + 1);
                let proposal = Proposal {
                    id: "fresh-start-commit".into(),
                    scope: "fresh-owner".into(),
                    observation: Observation::Admission(admitted.admission.sequence),
                    expected_revision: 0,
                    owner_epoch: store.epoch(),
                    engine: ENGINE.into(),
                    continuation: next.checkpoint().unwrap(),
                    workspace: Some(admitted.workspace),
                    effects: new_intents(None, &next).unwrap(),
                    state: None,
                };
                revision = store.commit_turn_workspace_start(&proposal).unwrap();
            }
            assert_eq!(revision, completed + 2);
            let intent = store.intents(revision).unwrap().remove(0);
            let mut dispatch =
                scoped_dispatch(&store, "fresh-owner", revision, intent, "new-model");
            if matches!(mode, "reserved" | "in-flight" | "receipt") {
                dispatch.attempt.owner_epoch = db
                    .query_row(
                        "SELECT owner_epoch FROM attempts WHERE id='fresh-owner-new-model'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap() as u64;
                let result = store
                    .dispatch_turn_model(
                        &dispatch,
                        &AtomicBool::new(false),
                        &transport(),
                        |_| Ok(()),
                        |_| panic!("recovery sent another provider request"),
                    )
                    .await
                    .unwrap();
                if mode != "receipt" {
                    assert!(matches!(result, DispatchOutcome::Reconcile), "{mode}");
                    assert!(store.receipt(&dispatch.attempt.id).unwrap().is_none());
                    assert!(
                        store
                            .reconciliation()
                            .unwrap()
                            .unresolved_attempts
                            .contains(&dispatch.attempt.id),
                        "{mode}"
                    );
                } else {
                    let DispatchOutcome::Reused(bytes) = result else {
                        panic!("reused")
                    };
                    let before = scoped_state(&mut store, "fresh-owner");
                    let after = before
                        .apply_model(&ModelWait::restore(&bytes).unwrap(), "route-v1")
                        .unwrap();
                    let proposal = Proposal {
                        id: "recovered-model".into(),
                        scope: "fresh-owner".into(),
                        observation: Observation::Receipt(dispatch.attempt.id.clone()),
                        expected_revision: revision,
                        owner_epoch: store.epoch(),
                        engine: ENGINE.into(),
                        continuation: after.checkpoint().unwrap(),
                        workspace: None,
                        effects: vec![],
                        state: None,
                    };
                    store
                        .commit_turn_receipt(
                            &proposal,
                            &receipt_binding(&dispatch.attempt, revision, &dispatch.intent, &bytes),
                        )
                        .unwrap();
                }
            } else {
                let attempts: i64 = db.query_row("SELECT count(*) FROM attempts a JOIN intents i ON i.id=a.effect JOIN revisions r ON r.revision=i.revision WHERE r.scope='fresh-owner'", [], |row| row.get(0)).unwrap();
                assert_eq!(attempts, 0);
            }
            assert_eq!(store.snapshot_scope("child").unwrap(), child_before);
            assert_eq!(
                store.snapshot_scope("owner").unwrap().revision,
                completed + 1
            );
            if let Some(slow) = &slow {
                assert_eq!(
                    slow.sends.load(Ordering::SeqCst),
                    1,
                    "provider saw a resend after the in-flight kill"
                );
            }
            drop(db);
            drop(store);
            assert!(Store::open(&realm.0).is_ok(), "reopen at {mode}");
        }
    }

    #[cfg(feature = "dext-strict-workspace")]
    #[tokio::test]
    async fn rewound_task_requires_resume_and_fresh_policy_then_runs_only_the_new_effect() {
        let mock = Mock::good();
        let transport = transport();
        let (realm, mut store, request, next) = rewound_task_fixture(&mock.endpoint).await;
        let source = store.snapshot_scope("owner").unwrap();
        let child = store.snapshot_scope("child").unwrap();
        assert!(matches!(
            store.admit_rewound_task(&request, |_| Ok(())),
            Err(Error::Paused)
        ));
        store.resume_rewind("owner", "restart").unwrap();
        assert!(matches!(
            store.admit_rewound_task(&request, |_| Err(Error::Cancelled)),
            Err(Error::Cancelled)
        ));
        let calls = Cell::new(0);
        let admitted = store
            .admit_rewound_task(&request, |_| {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(calls.get(), 2);
        assert_eq!(mock.sends.load(Ordering::SeqCst), 0);
        assert_eq!(store.snapshot_scope("fresh-owner").unwrap().revision, 0);
        assert_eq!(
            store
                .read_versioned_file("fresh-owner", &admitted.workspace, "input.txt")
                .unwrap(),
            b"owner"
        );
        assert_eq!(store.snapshot_scope("owner").unwrap(), source);
        assert_eq!(store.snapshot_scope("child").unwrap(), child);
        let reserved_stage = spindle_store::artifact::Stage {
            id: format!("rewound-task-{}", admitted.admission.sequence),
            scope: request.command.scope.clone(),
            owner_epoch: store.epoch(),
        };
        assert!(matches!(
            store.begin_stage(&reserved_stage),
            Err(Error::Invalid)
        ));
        assert!(matches!(
            store.put_object(&reserved_stage, b"unexpected pin"),
            Err(Error::Invalid)
        ));
        assert_eq!(
            store
                .read_versioned_file("fresh-owner", &admitted.workspace, "input.txt")
                .unwrap(),
            b"owner"
        );
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        assert_eq!(
            store.admit_rewound_task(&request, |_| Ok(())).unwrap(),
            admitted
        );
        let mut proposal = Proposal {
            id: "fresh-start-commit".into(),
            scope: request.command.scope.clone(),
            observation: Observation::Admission(admitted.admission.sequence),
            expected_revision: 0,
            owner_epoch: store.epoch(),
            engine: ENGINE.into(),
            continuation: next.checkpoint().unwrap(),
            workspace: Some(admitted.workspace.clone()),
            effects: new_intents(None, &next).unwrap(),
            state: None,
        };
        let mut wrong = proposal.clone();
        wrong.workspace = None;
        assert!(store.commit_turn_action(&wrong).is_err());
        let revision = store.commit_turn_workspace_start(&proposal).unwrap();
        assert_ne!(revision, 1);
        let dispatch = scoped_dispatch(
            &store,
            "fresh-owner",
            revision,
            proposal.effects[0].clone(),
            "new-model",
        );
        let waiting = scoped_state(&mut store, "fresh-owner");
        assert_ne!(
            waiting.execution(),
            scoped_state(&mut store, "owner").execution()
        );
        apply_scoped_model(&mut store, &dispatch, &transport, "fresh-model-apply").await;
        let before = scoped_state(&mut store, "fresh-owner");
        let done = before.finish().unwrap();
        let admission = store
            .admit(&Command {
                scope: "fresh-owner".into(),
                id: "finish".into(),
                lineage: 1,
                kind: ENGINE.into(),
                payload: Action::Finish.encode().unwrap(),
            })
            .unwrap();
        proposal.id = "fresh-finish".into();
        proposal.observation = Observation::Admission(admission.sequence);
        proposal.expected_revision = store.snapshot_scope("fresh-owner").unwrap().revision;
        proposal.continuation = done.checkpoint().unwrap();
        proposal.workspace = None;
        proposal.effects.clear();
        store.commit_turn_action(&proposal).unwrap();
        store.settle_owned("fresh-owner").unwrap();
        store.cancel_scope("owner", "later-source-cancel").unwrap();
        assert_eq!(
            store.admit_rewound_task(&request, |_| Ok(())).unwrap(),
            admitted
        );
        assert!(matches!(
            store.admit_rewound_task(&request, |_| Err(Error::Cancelled)),
            Err(Error::Cancelled)
        ));
        assert_eq!(mock.sends.load(Ordering::SeqCst), 1);
        assert_eq!(store.snapshot_scope("child").unwrap(), child);
        drop(store);
        assert!(Store::open(&realm.0).is_ok());
    }

    #[cfg(feature = "dext-strict-workspace")]
    #[tokio::test]
    async fn rewound_task_admission_faults_roll_back_ownership_provenance_and_all_pins() {
        for table in [
            "admissions",
            "owned_scopes",
            "artifact_stages",
            "staging_pins",
            "rewound_tasks",
        ] {
            for fault in ["ABORT,'injected'", "IGNORE"] {
                let (realm, mut store, request, _) =
                    rewound_task_fixture("https://example.invalid/responses").await;
                store.resume_rewind("owner", "restart").unwrap();
                let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
                let counts = |db: &rusqlite::Connection| -> Vec<i64> {
                    [
                        "admissions",
                        "owned_scopes",
                        "artifact_stages",
                        "staging_pins",
                        "rewound_tasks",
                    ]
                    .into_iter()
                    .map(|name| {
                        db.query_row(&format!("SELECT count(*) FROM {name}"), [], |row| {
                            row.get(0)
                        })
                        .unwrap()
                    })
                    .collect()
                };
                let before = counts(&db);
                let head = store.snapshot().unwrap();
                db.execute_batch(&format!("CREATE TRIGGER fail_restart BEFORE INSERT ON {table} BEGIN SELECT RAISE({fault}); END;")).unwrap();
                assert!(
                    matches!(
                        store.admit_rewound_task(&request, |_| Ok(())),
                        Err(Error::Sql(_) | Error::Incompatible)
                    ),
                    "{table}/{fault}"
                );
                assert!(matches!(store.snapshot(), Err(Error::Paused)));
                assert_eq!(counts(&db), before, "{table}/{fault}");
                db.execute_batch("DROP TRIGGER fail_restart").unwrap();
                drop(store);
                let mut store = Store::open(&realm.0).unwrap();
                assert_eq!(store.snapshot().unwrap(), head);
                assert!(store.admit_rewound_task(&request, |_| Ok(())).is_ok());
            }
        }
    }

    #[cfg(feature = "dext-strict-workspace")]
    #[tokio::test]
    async fn rewound_task_damage_blocks_takeover_before_owner_epoch_change() {
        for sql in [
            "UPDATE rewound_tasks SET source_revision=source_revision-1",
            "UPDATE rewound_tasks SET source_scope='child'",
            "UPDATE rewound_tasks SET workspace=(SELECT workspace FROM revisions WHERE scope='child' LIMIT 1)",
            "DELETE FROM rewound_tasks",
            "DELETE FROM staging_pins WHERE stage GLOB 'rewound-task-*'",
            "UPDATE owned_scopes SET kind=1 WHERE scope='fresh-owner'",
            "UPDATE admissions SET payload=X'00' WHERE scope='fresh-owner'",
        ] {
            let (realm, mut store, request, _) =
                rewound_task_fixture("https://example.invalid/responses").await;
            store.resume_rewind("owner", "restart").unwrap();
            store.admit_rewound_task(&request, |_| Ok(())).unwrap();
            drop(store);
            let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
            let epoch: i64 = db
                .query_row("SELECT epoch FROM realm", [], |row| row.get(0))
                .unwrap();
            db.execute_batch(&format!("PRAGMA foreign_keys=OFF; {sql}"))
                .unwrap();
            assert!(Store::open(&realm.0).is_err(), "accepted {sql}");
            assert_eq!(
                db.query_row("SELECT epoch FROM realm", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                epoch
            );
        }
    }

    #[cfg(feature = "dext-strict-workspace")]
    #[tokio::test]
    async fn rewound_task_denied_second_check_and_changed_inputs_do_not_create_new_work() {
        let (realm, mut store, mut request, _) =
            rewound_task_fixture("https://example.invalid/responses").await;
        store.resume_rewind("owner", "restart").unwrap();
        let before = store.owned_scopes().unwrap();
        let calls = Cell::new(0);
        assert!(matches!(
            store.admit_rewound_task(&request, |_| {
                calls.set(calls.get() + 1);
                if calls.get() == 2 {
                    Err(Error::Cancelled)
                } else {
                    Ok(())
                }
            }),
            Err(Error::Cancelled)
        ));
        assert_eq!(store.owned_scopes().unwrap(), before);
        let original = request.command.clone();
        request.command.payload.push(0);
        assert!(store.admit_rewound_task(&request, |_| Ok(())).is_err());
        request.command = original;
        let admitted = store.admit_rewound_task(&request, |_| Ok(())).unwrap();
        request.command.id = "changed".into();
        assert!(matches!(
            store.admit_rewound_task(&request, |_| Ok(())),
            Err(Error::Conflict)
        ));
        request.command.id = "fresh-start".into();
        request.source_revision -= 1;
        assert!(matches!(
            store.admit_rewound_task(&request, |_| Ok(())),
            Err(Error::Conflict)
        ));
        request.source_revision += 1;
        assert_eq!(
            store.admit_rewound_task(&request, |_| Ok(())).unwrap(),
            admitted
        );
        drop(store);
        assert!(Store::open(&realm.0).is_ok());
    }

    #[cfg(feature = "dext-strict-workspace")]
    #[tokio::test]
    async fn owned_cut_storage_faults_never_acknowledge_or_leave_partial_manifests() {
        for fault in ["ABORT,'injected'", "IGNORE"] {
            let (realm, mut store, revision) = completed_owned_fixture().await;
            let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
            let before = store.snapshot().unwrap();
            db.execute_batch(&format!("CREATE TRIGGER fail_cut BEFORE INSERT ON execution_cuts BEGIN SELECT RAISE({fault}); END;")).unwrap();
            assert!(matches!(
                store.capture_owned_cut("owner", revision),
                Err(Error::Sql(_) | Error::Incompatible)
            ));
            assert!(matches!(store.snapshot(), Err(Error::Paused)));
            assert_eq!(
                db.query_row("SELECT count(*) FROM execution_cuts", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
            db.execute_batch("DROP TRIGGER fail_cut").unwrap();
            drop(store);
            let mut store = Store::open(&realm.0).unwrap();
            assert_eq!(store.snapshot().unwrap(), before);
            let cut = store.capture_owned_cut("owner", revision).unwrap();
            assert_eq!(cut.members.len(), 2);
        }
    }

    #[cfg(feature = "dext-strict-workspace")]
    #[tokio::test]
    async fn owned_cut_corruption_or_missing_rewind_binding_blocks_takeover_before_epoch_change() {
        for damage in [
            "manifest",
            "epoch",
            "watermark",
            "membership",
            "missing-pending",
            "missing-final",
        ] {
            let (realm, mut store, revision) = completed_owned_fixture().await;
            store.capture_owned_cut("owner", revision).unwrap();
            store.settle_owned("owner").unwrap();
            if damage.starts_with("missing-") {
                store
                    .begin_rewind("owner", "rewind", revision, revision, 0)
                    .unwrap();
                if damage == "missing-final" {
                    store.finish_rewind("owner", "rewind").unwrap();
                }
            }
            drop(store);
            let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
            let epoch: i64 = db
                .query_row("SELECT epoch FROM realm", [], |row| row.get(0))
                .unwrap();
            let sql = match damage {
                "manifest" => "UPDATE execution_cuts SET manifest=X'53454331'",
                "epoch" => "UPDATE execution_cuts SET epoch=epoch+100",
                "watermark" => "UPDATE execution_cuts SET admission=1",
                "membership" => "UPDATE owned_scopes SET parent=NULL WHERE scope='child'",
                _ => "DELETE FROM execution_cuts",
            };
            db.execute_batch(sql).unwrap();
            assert!(Store::open(&realm.0).is_err(), "accepted {damage}");
            assert_eq!(
                db.query_row("SELECT epoch FROM realm", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                epoch
            );
        }
    }

    #[cfg(feature = "dext-strict-workspace")]
    #[tokio::test]
    async fn owned_cut_refuses_live_tasks_services_and_pending_admissions_without_changing_head() {
        use spindle_store::ownership::ScopeKind;
        for kind in [ScopeKind::Task, ScopeKind::Service] {
            let (realm, mut store, revision) = completed_owned_fixture().await;
            let command = Command {
                scope: "new-child".into(),
                id: "start".into(),
                lineage: 0,
                kind: "fixture".into(),
                payload: vec![],
            };
            store.admit_owned(Some("owner"), kind, &command).unwrap();
            let before = store.snapshot().unwrap();
            assert!(store.capture_owned_cut("owner", revision).is_err());
            assert_eq!(store.snapshot().unwrap(), before);
            assert!(
                store
                    .owned_execution_cut("owner", revision)
                    .unwrap()
                    .is_none()
            );
            drop(store);
            assert!(Store::open(&realm.0).is_ok());
        }
    }

    #[tokio::test]
    async fn model_reserves_before_credentials_then_adopts_and_reuses_after_takeover() {
        let realm = Realm::new();
        let mock = Mock::good();
        let transport = transport();
        let mut store = Store::open(&realm.0).unwrap();
        let (proposal, mut request) = prepare(&mut store, &mock.endpoint);
        let interrupt = AtomicBool::new(false);
        assert!(
            store
                .dispatch_turn_model(
                    &request,
                    &interrupt,
                    &transport,
                    |_| Ok(()),
                    |_| panic!("uncommitted credentials")
                )
                .await
                .is_err()
        );
        store.commit_turn_action(&proposal).unwrap();
        let baseline = state(&mut store);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let approvals = Cell::new(0);
        let result = store
            .dispatch_turn_model(
                &request,
                &interrupt,
                &transport,
                |_| {
                    approvals.set(approvals.get() + 1);
                    Ok(())
                },
                |committed| {
                    let count: i64 = db
                        .query_row(
                            "SELECT count(*) FROM attempts WHERE id=?1",
                            [&request.attempt.id],
                            |row| row.get(0),
                        )
                        .unwrap();
                    assert_eq!(count, 1);
                    live(committed)
                },
            )
            .await
            .unwrap();
        assert_eq!(approvals.get(), 2);
        assert_eq!(mock.sends.load(Ordering::SeqCst), 1);
        let DispatchOutcome::Recorded(bytes) = result else {
            panic!("ingress")
        };
        assert_eq!(store.snapshot().unwrap().revision, 1);
        let recorded = ModelWait::restore(&bytes).unwrap();
        let review = baseline.apply_model(&recorded, "route-v1").unwrap();
        let proposed = receipt_proposal(
            &mut store,
            "provider-apply",
            &request.attempt,
            &baseline,
            &review,
        );
        store
            .commit_turn_receipt(
                &proposed,
                &receipt_binding(&request.attempt, 1, &request.intent, &bytes),
            )
            .unwrap();
        drop(db);
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        request.owner_epoch = store.epoch();
        assert!(matches!(
            store
                .dispatch_turn_model(
                    &request,
                    &interrupt,
                    &transport,
                    |_| Err(Error::Cancelled),
                    |_| panic!("revoked credentials")
                )
                .await,
            Err(Error::Cancelled)
        ));
        assert!(
            matches!(store.dispatch_turn_model(&request, &interrupt, &transport, |_| Ok(()), |_| panic!("reused credentials")).await.unwrap(), DispatchOutcome::Reused(saved) if saved == bytes)
        );
        assert_eq!(mock.sends.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn model_failure_revocation_and_route_conflict_remain_unresolved_after_restart() {
        for failure in 0..5 {
            let realm = Realm::new();
            let mock = Mock::new(
                if failure == 0 { 503 } else { 200 },
                "data: invalid-json\n\n",
                Duration::ZERO,
            );
            let transport = transport();
            let mut store = Store::open(&realm.0).unwrap();
            let (proposal, mut request) = prepare(&mut store, &mock.endpoint);
            store.commit_turn_action(&proposal).unwrap();
            let interrupt = AtomicBool::new(false);
            let approvals = Cell::new(0);
            let result = store
                .dispatch_turn_model(
                    &request,
                    &interrupt,
                    &transport,
                    |_| {
                        approvals.set(approvals.get() + 1);
                        if failure == 2 && approvals.get() == 2 {
                            return Err(Error::Cancelled);
                        }
                        Ok(())
                    },
                    |committed| {
                        if failure == 3 {
                            return Err(Error::Cancelled);
                        }
                        if failure == 4 {
                            let Effect::Model { waiting, .. } = committed.effect() else {
                                panic!("model")
                            };
                            let mut identity = waiting.request().identity().clone();
                            identity.credential_scope = "changed-version".into();
                            return Ok(LiveRoute::new(identity, Default::default()).unwrap());
                        }
                        live(committed)
                    },
                )
                .await;
            match failure {
                0 => assert!(matches!(result, Err(Error::Model(ModelError::Http(503))))),
                1 => assert!(matches!(result, Err(Error::Model(ModelError::Protocol)))),
                4 => assert!(matches!(
                    result,
                    Err(Error::Model(ModelError::Unauthorized))
                )),
                _ => assert!(matches!(result, Err(Error::Cancelled))),
            }
            assert_eq!(mock.sends.load(Ordering::SeqCst), usize::from(failure < 2));
            assert!(store.receipt(&request.attempt.id).unwrap().is_none());
            drop(store);
            let mut store = Store::open(&realm.0).unwrap();
            request.owner_epoch = store.epoch();
            assert!(matches!(
                store
                    .dispatch_turn_model(
                        &request,
                        &interrupt,
                        &transport,
                        |_| Ok(()),
                        |_| panic!("uncertain resend")
                    )
                    .await
                    .unwrap(),
                DispatchOutcome::Reconcile
            ));
            request.attempt.id = "replacement-provider-attempt".into();
            request.attempt.owner_epoch = store.epoch();
            assert!(matches!(
                store
                    .dispatch_turn_model(
                        &request,
                        &interrupt,
                        &transport,
                        |_| Ok(()),
                        |_| panic!("replacement resend")
                    )
                    .await,
                Err(Error::Conflict)
            ));
        }
    }

    #[tokio::test]
    async fn dropped_model_future_retains_reservation_without_receipt_or_resend() {
        let realm = Realm::new();
        let mock = Mock::new(200, "", Duration::from_secs(5));
        let transport = transport();
        let mut store = Store::open(&realm.0).unwrap();
        let (proposal, mut request) = prepare(&mut store, &mock.endpoint);
        store.commit_turn_action(&proposal).unwrap();
        let interrupt = AtomicBool::new(false);
        {
            let future =
                store.dispatch_turn_model(&request, &interrupt, &transport, |_| Ok(()), live);
            tokio::pin!(future);
            let observed = async {
                while mock.sends.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            };
            tokio::select! {
                _ = tokio::time::timeout(Duration::from_secs(2), observed) => assert_eq!(mock.sends.load(Ordering::SeqCst), 1),
                result = &mut future => panic!("unexpected completion: {result:?}"),
            }
        }
        assert!(store.receipt(&request.attempt.id).unwrap().is_none());
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        request.owner_epoch = store.epoch();
        assert!(matches!(
            store
                .dispatch_turn_model(
                    &request,
                    &interrupt,
                    &transport,
                    |_| Ok(()),
                    |_| panic!("dropped request resent")
                )
                .await
                .unwrap(),
            DispatchOutcome::Reconcile
        ));
        assert_eq!(mock.sends.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn broker_reserves_before_execution_and_reuses_receipt_after_restart_and_head_advance() {
    use spindle_store::broker::DispatchOutcome;
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = start(&mut store, "broker", empty_transcript());
    let mut request = dispatch_request(&store, &prepared);
    assert!(
        store
            .dispatch_turn_effect(
                &request,
                &AtomicBool::new(false),
                |_| Ok(()),
                |_, _| panic!("uncommitted effect ran")
            )
            .is_err()
    );
    store.commit_turn_action(&prepared).unwrap();
    let baseline = state(&mut store);
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    let approvals = Cell::new(0);
    let calls = Cell::new(0);
    let outcome = store
        .dispatch_turn_effect(
            &request,
            &AtomicBool::new(false),
            |_| {
                approvals.set(approvals.get() + 1);
                Ok(())
            },
            |committed, _| {
                calls.set(calls.get() + 1);
                let retained: String = db
                    .query_row(
                        "SELECT effect FROM attempts WHERE id=?1",
                        [&request.attempt.id],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(retained, request.intent.id);
                let Effect::Model { waiting, .. } = committed.effect() else {
                    panic!("model");
                };
                Ok(response(waiting, 0).checkpoint())
            },
        )
        .unwrap();
    assert_eq!(approvals.get(), 2);
    assert_eq!(calls.get(), 1);
    let DispatchOutcome::Recorded(bytes) = outcome else {
        panic!("receipt ingress");
    };
    assert_eq!(
        store.snapshot().unwrap().revision,
        1,
        "ingress is not application"
    );
    let recorded = ModelWait::restore(&bytes).unwrap();
    let review = baseline.apply_model(&recorded, "route-v1").unwrap();
    let proposed = receipt_proposal(
        &mut store,
        "broker-apply",
        &request.attempt,
        &baseline,
        &review,
    );
    store
        .commit_turn_receipt(
            &proposed,
            &receipt_binding(&request.attempt, 1, &request.intent, &bytes),
        )
        .unwrap();
    drop(db);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    request.owner_epoch = store.epoch();
    store.cancel_scope("operator", "cancel").unwrap();
    let reused = store
        .dispatch_turn_effect(
            &request,
            &AtomicBool::new(false),
            |_| Ok(()),
            |_, _| panic!("saved effect reran"),
        )
        .unwrap();
    assert!(matches!(reused, DispatchOutcome::Reused(saved) if saved == bytes));
    assert!(matches!(
        store.dispatch_turn_effect(
            &request,
            &AtomicBool::new(false),
            |_| Err(Error::Cancelled),
            |_, _| panic!("denied retry ran")
        ),
        Err(Error::Cancelled)
    ));
}

#[test]
fn broker_errors_panics_and_revocation_require_reconciliation_not_automatic_retry() {
    use spindle_store::broker::DispatchOutcome;
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;
    for failure in 0..3 {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let prepared = start(&mut store, "broker-failure", empty_transcript());
        store.commit_turn_action(&prepared).unwrap();
        let mut request = dispatch_request(&store, &prepared);
        let approvals = Cell::new(0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store.dispatch_turn_effect(
                &request,
                &AtomicBool::new(false),
                |_| {
                    approvals.set(approvals.get() + 1);
                    if failure == 0 && approvals.get() == 2 {
                        return Err(Error::Cancelled);
                    }
                    Ok(())
                },
                |_, _| {
                    if failure == 2 {
                        panic!("simulated adapter panic after reservation");
                    }
                    Err(Error::Unsupported)
                },
            )
        }));
        if failure == 2 {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().is_err());
        }
        assert!(store.receipt(&request.attempt.id).unwrap().is_none());
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        request.owner_epoch = store.epoch();
        assert!(matches!(
            store
                .dispatch_turn_effect(
                    &request,
                    &AtomicBool::new(false),
                    |_| Ok(()),
                    |_, _| panic!("unresolved attempt reran")
                )
                .unwrap(),
            DispatchOutcome::Reconcile
        ));
        request.attempt.id = "replacement-attempt".into();
        request.attempt.owner_epoch = store.epoch();
        assert!(matches!(
            store.dispatch_turn_effect(
                &request,
                &AtomicBool::new(false),
                |_| Ok(()),
                |_, _| panic!("replacement attempt ran")
            ),
            Err(Error::Conflict)
        ));
    }
}

#[test]
fn broker_failed_reservation_and_changed_intent_authority_never_execute() {
    use std::sync::atomic::AtomicBool;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = start(&mut store, "broker-fence", empty_transcript());
    store.commit_turn_action(&prepared).unwrap();
    let request = dispatch_request(&store, &prepared);
    let mut wrong = dispatch_request(&store, &prepared);
    wrong.intent.input.push(0);
    assert!(matches!(
        store.dispatch_turn_effect(
            &wrong,
            &AtomicBool::new(false),
            |_| Ok(()),
            |_, _| panic!("wrong bytes ran")
        ),
        Err(Error::Conflict)
    ));
    let mut wrong = dispatch_request(&store, &prepared);
    wrong.expected_revision = 0;
    assert!(matches!(
        store.dispatch_turn_effect(
            &wrong,
            &AtomicBool::new(false),
            |_| Ok(()),
            |_, _| panic!("wrong head ran")
        ),
        Err(Error::Stale)
    ));
    let mut wrong = dispatch_request(&store, &prepared);
    wrong.owner_epoch += 1;
    assert!(matches!(
        store.dispatch_turn_effect(
            &wrong,
            &AtomicBool::new(false),
            |_| Ok(()),
            |_, _| panic!("wrong owner ran")
        ),
        Err(Error::Stale)
    ));
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_reservation BEFORE INSERT ON attempts BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(matches!(
        store.dispatch_turn_effect(
            &request,
            &AtomicBool::new(false),
            |_| Ok(()),
            |_, _| panic!("failed reservation ran")
        ),
        Err(Error::Sql(_))
    ));
    assert!(matches!(store.snapshot(), Err(Error::Paused)));
    assert_eq!(
        db.query_row("SELECT count(*) FROM attempts", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    db.execute_batch("DROP TRIGGER fail_reservation").unwrap();
    drop(db);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    let mut fresh = dispatch_request(&store, &prepared);
    fresh.expected_revision = store.snapshot().unwrap().revision;
    store.cancel_scope("operator", "cancel").unwrap();
    assert!(matches!(
        store.dispatch_turn_effect(
            &fresh,
            &AtomicBool::new(false),
            |_| Ok(()),
            |_, _| panic!("cancelled work ran")
        ),
        Err(Error::Cancelled)
    ));
}

fn prepared_tool(store: &mut Store) -> Proposal {
    prepared_tools(store, 1)
}

fn prepared_tools(store: &mut Store, calls: usize) -> Proposal {
    prepared_tool_blocks(
        store,
        (0..calls)
            .map(|i| Block::ToolUse {
                id: format!("call-{i}"),
                name: "inert-tool".into(),
                input: serde_json::json!({"position":i}),
            })
            .collect(),
    )
}

fn prepared_tool_blocks(store: &mut Store, blocks: Vec<Block>) -> Proposal {
    let prepared = start(store, "broker-tool", empty_transcript());
    store.commit_turn_action(&prepared).unwrap();
    let before = state(store);
    let Effect::Model { waiting, .. } = before.pending_effects().unwrap().remove(0) else {
        panic!("model");
    };
    let attempt = attempt(store, &prepared.effects[0].id, "model-attempt");
    let recorded = response_blocks(&waiting, blocks);
    let bytes = recorded.checkpoint();
    store.ingest_receipt(&attempt.id, &bytes).unwrap();
    let after = before.apply_model(&recorded, "route-v1").unwrap();
    let tools = receipt_proposal(store, "broker-tools", &attempt, &before, &after);
    store
        .commit_turn_receipt(
            &tools,
            &receipt_binding(&attempt, 1, &prepared.effects[0], &bytes),
        )
        .unwrap();
    tools
}

#[cfg(feature = "dext-strict-workspace")]
#[cfg(target_os = "linux")]
fn strict_executor() -> dext_native::strict::StrictExecutor {
    use sha2::{Digest, Sha256};
    let path = std::path::Path::new("/usr/bin/bwrap");
    let digest = Sha256::digest(std::fs::read(path).unwrap()).into();
    dext_native::strict::StrictExecutor::open(path, digest).unwrap()
}

#[cfg(feature = "dext-strict-workspace")]
#[test]
fn app_reservation_answer_and_adoption_survive_each_reopen_boundary() {
    use spindle_store::broker::{AppReservation, AppResult, DispatchOutcome};
    use std::sync::atomic::AtomicBool;
    for boundary in 0..3 {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let (tools, base) =
            prepared_workspace_call(&mut store, "lookup", serde_json::json!({"q": "notes"}));
        let mut request = dispatch_request(&store, &tools);
        let helper = [17; 32];
        request.attempt.implementation = spindle_store::broker::app_label(&"11".repeat(32));
        let interrupt = AtomicBool::new(false);
        let answer = || AppResult {
            content: "Found.".into(),
            success: true,
            writes: vec![("notes.txt".into(), b"notes".to_vec(), false)],
            deletes: vec![],
            links: vec![],
            directories: vec![],
            state: Some(br#"{"found":true}"#.to_vec()),
            details: None,
            max_output: 64 * 1024,
        };
        let reserved = store
            .reserve_app_call(&request, &interrupt, &helper)
            .unwrap();
        assert!(matches!(
            reserved,
            AppReservation::Reserved { again: false, .. }
        ));
        if boundary < 2 {
            // Before asking, or after asking but before retaining the answer: both
            // have exactly the same durable uncertainty and keep the effect id.
            if boundary == 1 {
                let _not_recorded = answer();
            }
            drop(store);
            store = Store::open(&realm.0).unwrap();
            request.owner_epoch = store.epoch();
            assert!(matches!(
                store.reserve_app_call(&request, &interrupt, &helper).unwrap(),
                AppReservation::Reserved { call, again: true } if call.name == "lookup"
            ));
        }
        assert!(matches!(
            store.record_app_answer(&request, &[18; 32], answer()),
            Err(Error::Conflict)
        ));
        request.intent_revision += 1;
        assert!(matches!(
            store.record_app_answer(&request, &helper, answer()),
            Err(Error::Conflict)
        ));
        request.intent_revision -= 1;
        let DispatchOutcome::Recorded(bytes) = store
            .record_app_answer(&request, &helper, answer())
            .unwrap()
        else {
            panic!("fresh answer")
        };
        assert!(matches!(
            store.record_app_answer(&request, &helper, answer()),
            Err(Error::Conflict)
        ));
        assert_eq!(store.snapshot().unwrap().workspace, Some(base));
        assert_eq!(
            store
                .app_state("operator", request.expected_revision)
                .unwrap(),
            b"{}"
        );
        drop(store);
        store = Store::open(&realm.0).unwrap();
        request.owner_epoch = store.epoch();
        assert!(matches!(
            store.reserve_app_call(&request, &interrupt, &helper).unwrap(),
            AppReservation::Settled(DispatchOutcome::Reused(saved)) if saved == bytes
        ));
        let before = state(&mut store);
        let block = serde_json::from_slice(&bytes).unwrap();
        let after = before.apply_tool(&request.intent.id, vec![block]).unwrap();
        let mut apply =
            receipt_proposal(&mut store, "app-apply", &request.attempt, &before, &after);
        apply.workspace = store.workspace_receipt(&request.attempt.id).unwrap();
        apply.state = store
            .adopted_state("operator", request.expected_revision, &request.attempt.id)
            .unwrap();
        let revision = store
            .commit_turn_workspace_receipt(
                &apply,
                &receipt_binding(
                    &request.attempt,
                    request.intent_revision,
                    &request.intent,
                    &bytes,
                ),
                helper,
            )
            .unwrap();
        assert_eq!(
            store.app_state("operator", revision).unwrap(),
            br#"{"found":true}"#
        );
        let root = store.snapshot().unwrap().workspace.unwrap();
        assert_eq!(
            store
                .read_versioned_file("operator", &root, "notes.txt")
                .unwrap(),
            b"notes"
        );
        assert_eq!(state(&mut store).control().phase(), Phase::AfterTools);
        assert!(
            store
                .reconciliation()
                .unwrap()
                .unresolved_attempts
                .is_empty()
        );
    }
}

#[cfg(feature = "dext-strict-workspace")]
#[test]
fn split_and_wrapped_app_dispatch_record_identical_receipts() {
    use spindle_store::broker::{AppReservation, AppResult, DispatchOutcome};
    use std::sync::atomic::AtomicBool;
    let helper = [17; 32];
    let mut results = vec![];
    for split in [false, true] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let (tools, _) = prepared_workspace_call(&mut store, "lookup", serde_json::json!({}));
        let mut request = dispatch_request(&store, &tools);
        request.attempt.implementation = spindle_store::broker::app_label(&"11".repeat(32));
        let answer = || AppResult {
            content: "Found.".into(),
            success: true,
            writes: vec![("notes.txt".into(), b"notes".to_vec(), false)],
            deletes: vec![],
            links: vec![],
            directories: vec![],
            state: Some(br#"{"found":true}"#.to_vec()),
            details: None,
            max_output: 64 * 1024,
        };
        let interrupt = AtomicBool::new(false);
        let outcome = if split {
            assert!(matches!(
                store
                    .reserve_app_call(&request, &interrupt, &helper)
                    .unwrap(),
                AppReservation::Reserved { again: false, .. }
            ));
            store
                .app_workspace(&request, |workspace| {
                    assert_eq!(workspace.files()[0].path, "input.txt");
                })
                .unwrap();
            store
                .record_app_answer(&request, &helper, answer())
                .unwrap()
        } else {
            store
                .dispatch_app_workspace(&request, &interrupt, &helper, |call, again, workspace| {
                    assert_eq!(call.name, "lookup");
                    assert!(!again);
                    assert_eq!(workspace.files()[0].path, "input.txt");
                    Some(answer())
                })
                .unwrap()
        };
        let DispatchOutcome::Recorded(bytes) = outcome else {
            panic!("fresh answer")
        };
        results.push((
            bytes,
            store.workspace_receipt(&request.attempt.id).unwrap(),
            store
                .adopted_state("operator", request.expected_revision, &request.attempt.id)
                .unwrap(),
        ));
    }
    assert_eq!(results[0], results[1]);
}

#[cfg(all(feature = "dext-strict-workspace", unix))]
#[test]
fn app_split_kill_worker() {
    use spindle_store::broker::{AppReservation, AppResult};
    use std::sync::atomic::AtomicBool;
    let Some(root) = std::env::var_os("SPINDLE_APP_KILL_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let mut store = Store::open(&root).unwrap();
    let head = store.snapshot().unwrap();
    let intent = store.intents(head.revision).unwrap().remove(0);
    let request = spindle_store::broker::DispatchRequest {
        scope: "operator".into(),
        owner_epoch: store.epoch(),
        expected_revision: head.revision,
        intent_revision: head.revision,
        attempt: Attempt {
            id: "app-kill".into(),
            effect: intent.id.clone(),
            owner_epoch: store.epoch(),
            implementation: spindle_store::broker::app_label(&"11".repeat(32)),
            grant_reference: "app-fixture".into(),
        },
        intent,
    };
    assert!(matches!(
        store
            .reserve_app_call(&request, &AtomicBool::new(false), &[17; 32])
            .unwrap(),
        AppReservation::Reserved { again: false, .. }
    ));
    let boundary = std::env::var("SPINDLE_APP_KILL_BOUNDARY").unwrap();
    let answer = AppResult {
        content: "Found.".into(),
        success: true,
        writes: vec![("notes.txt".into(), b"notes".to_vec(), false)],
        deletes: vec![],
        links: vec![],
        directories: vec![],
        state: Some(br#"{"found":true}"#.to_vec()),
        details: None,
        max_output: 64 * 1024,
    };
    if boundary == "answer" {
        store
            .record_app_answer(&request, &[17; 32], answer)
            .unwrap();
    } else if boundary == "asked" {
        std::fs::write(root.join("asked"), &request.intent.id).unwrap();
    }
    std::fs::write(root.join("ready"), &request.intent.id).unwrap();
    loop {
        std::thread::park();
    }
}

#[cfg(all(feature = "dext-strict-workspace", unix))]
#[test]
fn app_split_sigkill_matrix_never_loses_or_repeats_an_answer() {
    use spindle_store::broker::{AppReservation, AppResult, DispatchOutcome};
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};
    for boundary in ["reserved", "asked", "answer"] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let (tools, base) = prepared_workspace_call(&mut store, "lookup", serde_json::json!({}));
        drop(store);
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "app_split_kill_worker", "--nocapture"])
            .env("SPINDLE_APP_KILL_ROOT", &realm.0)
            .env("SPINDLE_APP_KILL_BOUNDARY", boundary)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !realm.0.join("ready").exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "worker exited before {boundary}"
            );
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("worker timed out before {boundary}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
        let mut store = Store::open(&realm.0).unwrap();
        let mut request = dispatch_request(&store, &tools);
        request.attempt = store.attempt_for(&request.intent.id).unwrap().unwrap();
        let outcome = store
            .reserve_app_call(&request, &AtomicBool::new(false), &[17; 32])
            .unwrap();
        let bytes = match outcome {
            AppReservation::Settled(DispatchOutcome::Reused(bytes)) => {
                assert_eq!(boundary, "answer");
                bytes
            }
            AppReservation::Reserved { call, again } => {
                assert_ne!(boundary, "answer");
                assert!(again);
                assert_eq!(call.name, "lookup");
                let DispatchOutcome::Recorded(bytes) = store
                    .record_app_answer(
                        &request,
                        &[17; 32],
                        AppResult {
                            content: "Found.".into(),
                            success: true,
                            writes: vec![("notes.txt".into(), b"notes".to_vec(), false)],
                            deletes: vec![],
                            links: vec![],
                            directories: vec![],
                            state: Some(br#"{"found":true}"#.to_vec()),
                            details: None,
                            max_output: 64 * 1024,
                        },
                    )
                    .unwrap()
                else {
                    panic!("fresh answer")
                };
                bytes
            }
            _ => panic!("unexpected outcome"),
        };
        assert_eq!(store.snapshot().unwrap().workspace, Some(base));
        let before = state(&mut store);
        let after = before
            .apply_tool(
                &request.intent.id,
                vec![serde_json::from_slice(&bytes).unwrap()],
            )
            .unwrap();
        let mut apply = receipt_proposal(
            &mut store,
            "app-kill-adopt",
            &request.attempt,
            &before,
            &after,
        );
        apply.workspace = store.workspace_receipt(&request.attempt.id).unwrap();
        apply.state = store
            .adopted_state("operator", request.expected_revision, &request.attempt.id)
            .unwrap();
        let revision = store
            .commit_turn_workspace_receipt(
                &apply,
                &receipt_binding(
                    &request.attempt,
                    request.intent_revision,
                    &request.intent,
                    &bytes,
                ),
                [17; 32],
            )
            .unwrap();
        assert_eq!(
            store.app_state("operator", revision).unwrap(),
            br#"{"found":true}"#
        );
        assert_eq!(state(&mut store).control().phase(), Phase::AfterTools);
        assert!(
            store
                .reconciliation()
                .unwrap()
                .unresolved_attempts
                .is_empty()
        );
    }
}

#[cfg(feature = "dext-strict-workspace")]
fn prepared_workspace_tool(store: &mut Store, command: &str) -> (Proposal, String) {
    prepared_workspace_call(
        store,
        "bash",
        serde_json::json!({"command":command,"timeout":5}),
    )
}

#[cfg(feature = "dext-strict-workspace")]
fn prepared_workspace_call(
    store: &mut Store,
    name: &str,
    input: serde_json::Value,
) -> (Proposal, String) {
    prepared_workspace_blocks(
        store,
        vec![Block::ToolUse {
            id: "workspace-call".into(),
            name: name.into(),
            input,
        }],
    )
}

#[cfg(feature = "dext-strict-workspace")]
fn prepared_workspace_blocks(store: &mut Store, calls: Vec<Block>) -> (Proposal, String) {
    let mut prepared = start(store, "workspace-turn", empty_transcript());
    let stage = spindle_store::artifact::Stage {
        id: "initial-workspace".into(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    store.begin_stage(&stage).unwrap();
    let object = store.put_object(&stage, b"old").unwrap();
    let root = store
        .stage_file_map(
            &stage,
            &[spindle_store::workspace::Entry {
                path: "input.txt".into(),
                object,
                executable: false,
            }],
        )
        .unwrap();
    prepared.workspace = Some(root.clone());
    store.commit_turn_workspace_start(&prepared).unwrap();
    let before = state(store);
    let Effect::Model { waiting, .. } = before.pending_effects().unwrap().remove(0) else {
        panic!("model")
    };
    let model_attempt = attempt(store, &prepared.effects[0].id, "model-attempt");
    let recorded = response_blocks(&waiting, calls);
    let bytes = recorded.checkpoint();
    store.ingest_receipt(&model_attempt.id, &bytes).unwrap();
    let after = before.apply_model(&recorded, "route-v1").unwrap();
    let tools = receipt_proposal(store, "workspace-tools", &model_attempt, &before, &after);
    store
        .commit_turn_receipt(
            &tools,
            &receipt_binding(&model_attempt, 1, &prepared.effects[0], &bytes),
        )
        .unwrap();
    (tools, root)
}

#[cfg(feature = "dext-strict-workspace")]
fn strict_dispatch_request(
    store: &Store,
    tools: &Proposal,
    executor: &dext_native::strict::StrictExecutor,
) -> spindle_store::broker::DispatchRequest {
    let mut request = dispatch_request(store, tools);
    request.attempt.implementation = executor.implementation();
    request
}

#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn strict_workspace_dispatch_reopens_and_publishes_exact_root_and_continuation_together() {
    use spindle_store::broker::DispatchOutcome;
    use std::sync::atomic::AtomicBool;
    let executor = strict_executor();
    for exit in [0, 7] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let (tools, base) = prepared_workspace_tool(
            &mut store,
            &format!(
                "cat input.txt; printf new > input.txt; printf created > new.txt; exit {exit}"
            ),
        );
        let mut request = strict_dispatch_request(&store, &tools, &executor);
        assert!(matches!(
            store.dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Err(
                Error::Cancelled
            )),
            Err(Error::Cancelled)
        ));
        let DispatchOutcome::Recorded(bytes) = store
            .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
            .unwrap()
        else {
            panic!("strict ingress")
        };
        let block: Block = serde_json::from_slice(&bytes).unwrap();
        assert!(
            matches!(&block, Block::ToolResult { tool_use_id, content, is_error: Some(error), .. } if tool_use_id == "workspace-call" && content == "old" && *error == (exit != 0))
        );
        let before = state(&mut store);
        let after = before.apply_tool(&request.intent.id, vec![block]).unwrap();
        let mut proposal = receipt_proposal(
            &mut store,
            "workspace-apply",
            &request.attempt,
            &before,
            &after,
        );
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let root: String = db
            .query_row(
                "SELECT root FROM strict_receipts WHERE attempt=?1",
                [&request.attempt.id],
                |row| row.get(0),
            )
            .unwrap();
        proposal.workspace = Some(root.clone());
        assert_eq!(
            store.snapshot().unwrap().workspace.as_deref(),
            Some(base.as_str())
        );
        assert_eq!(
            store
                .read_versioned_file("operator", &base, "input.txt")
                .unwrap(),
            b"old"
        );
        assert_eq!(
            store
                .read_versioned_file("operator", &root, "input.txt")
                .unwrap(),
            b"new"
        );
        assert!(
            store
                .commit_turn_receipt(
                    &proposal,
                    &receipt_binding(&request.attempt, 2, &request.intent, &bytes)
                )
                .is_err()
        );
        assert!(matches!(
            store.commit_turn_workspace_receipt(
                &proposal,
                &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
                [0; 32]
            ),
            Err(Error::Conflict)
        ));
        let mut wrong = proposal.clone();
        wrong.workspace = Some(base.clone());
        assert!(
            store
                .commit_turn_workspace_receipt(
                    &wrong,
                    &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
                    *executor.helper_digest()
                )
                .is_err()
        );
        db.execute_batch("CREATE TRIGGER fail_workspace_adoption BEFORE INSERT ON revisions BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        assert!(matches!(
            store.commit_turn_workspace_receipt(
                &proposal,
                &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
                *executor.helper_digest()
            ),
            Err(Error::Sql(_))
        ));
        db.execute_batch("DROP TRIGGER fail_workspace_adoption")
            .unwrap();
        drop(db);
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        request.owner_epoch = store.epoch();
        assert!(
            matches!(store.dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(())).unwrap(), DispatchOutcome::Reused(saved) if saved == bytes)
        );
        assert_eq!(store.snapshot().unwrap().revision, 2);
        assert_eq!(
            store.snapshot().unwrap().workspace.as_deref(),
            Some(base.as_str())
        );
        proposal.owner_epoch = store.epoch();
        assert_eq!(
            store
                .commit_turn_workspace_receipt(
                    &proposal,
                    &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
                    *executor.helper_digest()
                )
                .unwrap(),
            3
        );
        assert_eq!(
            store.snapshot().unwrap().workspace.as_deref(),
            Some(root.as_str())
        );
        assert_eq!(
            state(&mut store).checkpoint().unwrap(),
            after.checkpoint().unwrap()
        );
        assert_eq!(
            store
                .commit_turn_workspace_receipt(
                    &proposal,
                    &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
                    *executor.helper_digest()
                )
                .unwrap(),
            3
        );
        assert!(
            store
                .commit_turn_workspace_receipt(
                    &proposal,
                    &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
                    [0; 32]
                )
                .is_err()
        );
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        assert_eq!(
            store.snapshot().unwrap().workspace.as_deref(),
            Some(root.as_str())
        );
        assert_eq!(
            store
                .read_versioned_file("operator", &base, "input.txt")
                .unwrap(),
            b"old"
        );
    }
}

#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn strict_bash_reuses_native_guardrails_and_reports_pipeline_failure() {
    use spindle_store::broker::DispatchOutcome;
    use std::sync::atomic::AtomicBool;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let (tools, _) = prepared_workspace_tool(&mut store, "false | true");
    let executor = strict_executor();
    let request = strict_dispatch_request(&store, &tools, &executor);
    let DispatchOutcome::Recorded(bytes) = store
        .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
        .unwrap()
    else {
        panic!("receipt")
    };
    let result: Block = serde_json::from_slice(&bytes).unwrap();
    assert!(
        matches!(
            result,
            Block::ToolResult {
                is_error: Some(true),
                ..
            }
        ),
        "pipeline failure was hidden"
    );
}

#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn strict_workspace_invalid_candidate_or_ingress_fault_cannot_publish_or_redispatch() {
    use spindle_store::broker::DispatchOutcome;
    use std::sync::atomic::AtomicBool;
    let executor = strict_executor();
    for case in ["receipt-abort", "provenance-ignore"] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let (tools, base) = prepared_workspace_tool(&mut store, "printf changed > input.txt");
        let mut request = strict_dispatch_request(&store, &tools, &executor);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        if case == "receipt-abort" {
            db.execute_batch("CREATE TRIGGER fail_strict BEFORE INSERT ON receipts BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        }
        if case == "provenance-ignore" {
            db.execute_batch("CREATE TRIGGER fail_strict BEFORE INSERT ON strict_receipts BEGIN SELECT RAISE(IGNORE); END;").unwrap();
        }
        assert!(
            store
                .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
                .is_err()
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM receipts WHERE attempt=?1",
                [&request.attempt.id],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM strict_receipts", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        db.execute_batch("DROP TRIGGER IF EXISTS fail_strict")
            .unwrap();
        drop(db);
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        request.owner_epoch = store.epoch();
        assert!(matches!(
            store
                .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
                .unwrap(),
            DispatchOutcome::Reconcile
        ));
        assert_eq!(
            store.snapshot().unwrap().workspace.as_deref(),
            Some(base.as_str())
        );
        request.attempt.id = "replacement".into();
        request.attempt.owner_epoch = store.epoch();
        assert!(
            store
                .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
                .is_err()
        );
    }
}

/// A run whose output cannot be sealed (here a FIFO) is a recorded failure over the
/// unchanged base that says why and keeps the output, never an unresolved attempt;
/// it is reused, not run again.
#[cfg(feature = "dext-strict-workspace")]
#[cfg(target_os = "linux")]
#[test]
fn unsealable_candidate_records_why_and_never_strands_or_reruns() {
    use spindle_store::broker::{DispatchOutcome, UNSEALED_RUN};
    use std::sync::atomic::AtomicBool;
    let executor = strict_executor();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let (tools, base) = prepared_workspace_tool(
        &mut store,
        "printf changed > input.txt; printf x > $'bad\\xff'; mkfifo pipe; echo made it",
    );
    let mut request = strict_dispatch_request(&store, &tools, &executor);
    let DispatchOutcome::Recorded(bytes) = store
        .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
        .unwrap()
    else {
        panic!("fresh receipt")
    };
    let Block::ToolResult {
        content, is_error, ..
    } = serde_json::from_slice::<Block>(&bytes).unwrap()
    else {
        panic!("tool result")
    };
    assert_eq!(is_error, Some(true));
    assert!(content.starts_with("made it\n"), "{content}");
    assert!(
        content.contains("a file name is not UTF-8 text"),
        "{content}"
    );
    assert!(content.ends_with(UNSEALED_RUN), "{content}");
    assert_eq!(
        store.workspace_receipt(&request.attempt.id).unwrap(),
        Some(base)
    );
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    request.owner_epoch = store.epoch();
    assert!(matches!(
        store
            .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
            .unwrap(),
        DispatchOutcome::Reused(saved) if saved == bytes
    ));
}

/// A host tool (`dispatch_host_workspace`) runs once under the strict adapter's
/// reservation: a subtree it replaces is staged as the call's root and handed to its
/// hook before anything is recorded (a failing hook records a failed result over the
/// unchanged workspace), the root adopts like any workspace receipt, and a reopened
/// store reuses the receipt without running the tool again.
#[cfg(feature = "dext-strict-workspace")]
#[cfg(target_os = "linux")]
#[test]
fn host_tools_replace_a_subtree_after_their_hook_and_never_run_twice() {
    use spindle_store::broker::{DispatchOutcome, HostResult, Replace, Staged};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    let executor = strict_executor();
    let input = serde_json::json!({"action": "checkout"});
    let replace = |staged: Staged| Replace {
        prefix: "sub/".into(),
        files: vec![
            ("a".into(), b"A".to_vec(), false),
            ("b/c".into(), b"C".to_vec(), true),
        ],
        links: vec![("engine".into(), "/engine".into())],
        staged: Some(staged),
    };

    // A hook that fails leaves the workspace as it was.
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let (tools, base) = prepared_workspace_call(&mut store, "spindle_source", input.clone());
    let request = strict_dispatch_request(&store, &tools, &executor);
    let DispatchOutcome::Recorded(bytes) = store
        .dispatch_host_workspace(
            &request,
            &AtomicBool::new(false),
            &executor,
            |_| Ok(()),
            |_, _, _| HostResult {
                content: "checked out".into(),
                success: true,
                replace: Some(replace(Box::new(|_| Err("no record".into())))),
            },
        )
        .unwrap()
    else {
        panic!("fresh receipt")
    };
    assert!(matches!(
        serde_json::from_slice::<Block>(&bytes).unwrap(),
        Block::ToolResult { content, is_error: Some(true), .. } if content == "no record; nothing changed"
    ));
    assert_eq!(
        store.workspace_receipt(&request.attempt.id).unwrap(),
        Some(base)
    );
    drop(store);

    // A hook that succeeds sees the root that is recorded and adopted.
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let (tools, base) = prepared_workspace_call(&mut store, "spindle_source", input);
    let mut request = strict_dispatch_request(&store, &tools, &executor);
    let staged = Arc::new(Mutex::new(None::<String>));
    let seen = staged.clone();
    let expected = request.expected_revision;
    let DispatchOutcome::Recorded(bytes) = store
        .dispatch_host_workspace(
            &request,
            &AtomicBool::new(false),
            &executor,
            |_| Ok(()),
            |name, input, workspace| {
                assert_eq!(
                    (name, input["action"].as_str()),
                    ("spindle_source", Some("checkout"))
                );
                assert_eq!(workspace.revision(), expected);
                let entry = workspace.files()[0].clone();
                assert_eq!(entry.path, "input.txt");
                assert_eq!(workspace.read(&entry.object).unwrap(), b"old");
                assert_eq!(workspace.size(&entry.object).unwrap(), 3);
                HostResult {
                    content: "checked out".into(),
                    success: true,
                    replace: Some(replace(Box::new(move |root| {
                        *seen.lock().unwrap() = Some(root.to_string());
                        Ok(())
                    }))),
                }
            },
        )
        .unwrap()
    else {
        panic!("fresh receipt")
    };
    let root = staged.lock().unwrap().clone().unwrap();
    assert_ne!(root, base);
    assert_eq!(
        store.workspace_receipt(&request.attempt.id).unwrap(),
        Some(root.clone())
    );
    for (path, bytes) in [
        ("input.txt", &b"old"[..]),
        ("sub/a", b"A"),
        ("sub/b/c", b"C"),
    ] {
        assert_eq!(
            store.read_versioned_file("operator", &root, path).unwrap(),
            bytes,
            "{path}"
        );
    }
    let tree = store.tree_map("operator", &root).unwrap();
    assert_eq!(tree.links[0].target, "/engine");
    assert!(
        tree.files
            .iter()
            .any(|f| f.path == "sub/b/c" && f.executable)
    );
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    request.owner_epoch = store.epoch();
    assert!(matches!(
        store
            .dispatch_host_workspace(
                &request,
                &AtomicBool::new(false),
                &executor,
                |_| Ok(()),
                |_, _, _| panic!("a recorded host tool never runs again"),
            )
            .unwrap(),
        DispatchOutcome::Reused(saved) if saved == bytes
    ));
    let block: Block = serde_json::from_slice(&bytes).unwrap();
    let before = state(&mut store);
    let after = before.apply_tool(&request.intent.id, vec![block]).unwrap();
    let mut proposal = receipt_proposal(
        &mut store,
        "host-tool-apply",
        &request.attempt,
        &before,
        &after,
    );
    proposal.workspace = Some(root.clone());
    store
        .commit_turn_workspace_receipt(
            &proposal,
            &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
            *executor.helper_digest(),
        )
        .unwrap();
    assert_eq!(store.snapshot().unwrap().workspace, Some(root));
}

/// A batch's second call interrupted after the first changed the workspace is sealed
/// as interrupted over the tree it ran on (the head's), and adopts.
#[cfg(feature = "dext-strict-workspace")]
#[cfg(target_os = "linux")]
#[test]
fn interrupted_call_after_a_changing_call_of_its_batch_seals_and_adopts() {
    use spindle_store::broker::DispatchOutcome;
    use std::sync::atomic::AtomicBool;
    let executor = strict_executor();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let call = |id: &str, name: &str, input: serde_json::Value| Block::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    };
    let (tools, base) = prepared_workspace_blocks(
        &mut store,
        vec![
            call(
                "write",
                "write_file",
                serde_json::json!({"path": "made.txt", "content": "made"}),
            ),
            call("run", "bash", serde_json::json!({"command": "sleep 30"})),
        ],
    );
    let request = |store: &mut Store, index: usize| {
        let mut request = strict_dispatch_request(store, &tools, &executor);
        request.expected_revision = store.snapshot().unwrap().revision;
        request.intent = tools.effects[index].clone();
        request.attempt.effect = tools.effects[index].id.clone();
        request.attempt.id = format!("batch-attempt-{index}");
        request
    };
    let adopt =
        |store: &mut Store, request: &spindle_store::broker::DispatchRequest, bytes: &[u8]| {
            let before = state(store);
            let block = serde_json::from_slice(bytes).unwrap();
            let after = before.apply_tool(&request.intent.id, vec![block]).unwrap();
            let mut apply = receipt_proposal(
                store,
                &format!("{}-apply", request.attempt.id),
                &request.attempt,
                &before,
                &after,
            );
            apply.workspace = store.workspace_receipt(&request.attempt.id).unwrap();
            store
                .commit_turn_workspace_receipt(
                    &apply,
                    &receipt_binding(
                        &request.attempt,
                        tools.expected_revision + 1,
                        &request.intent,
                        bytes,
                    ),
                    *executor.helper_digest(),
                )
                .unwrap();
            apply.workspace.unwrap()
        };
    let first = request(&mut store, 0);
    let DispatchOutcome::Recorded(bytes) = store
        .dispatch_strict_workspace(&first, &AtomicBool::new(false), &executor, |_| Ok(()))
        .unwrap()
    else {
        panic!("write")
    };
    let written = adopt(&mut store, &first, &bytes);
    assert_ne!(written, base);
    // The second call is reserved by this owner, which then dies mid-run.
    let mut second = request(&mut store, 1);
    store.reserve_attempt(&second.attempt).unwrap();
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    second.owner_epoch = store.epoch();
    assert!(matches!(
        store
            .dispatch_strict_workspace(&second, &AtomicBool::new(false), &executor, |_| Ok(()))
            .unwrap(),
        DispatchOutcome::Reconcile
    ));
    let sealed = store.seal_interrupted_strict(&second, &executor).unwrap();
    assert_eq!(adopt(&mut store, &second, &sealed), written);
    assert_eq!(
        store
            .read_versioned_file("operator", &written, "made.txt")
            .unwrap(),
        b"made"
    );
}

/// start_job, read_job and stop_job run in order through the strict dispatch path:
/// the job sees the workspace, its output is read by offset after waiting, its file
/// changes never reach the workspace, stopping ends it, and an unknown id is an error
/// result. Every call adopts the unchanged root.
#[cfg(feature = "dext-strict-workspace")]
#[cfg(target_os = "linux")]
#[test]
fn background_jobs_run_beside_the_turn_and_never_change_the_workspace() {
    use spindle_store::broker::DispatchOutcome;
    use std::sync::atomic::AtomicBool;
    let executor = strict_executor();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let call = |id: &str, name: &str, input: serde_json::Value| Block::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    };
    // Effect ids depend only on the command and position, so a dry run names the job.
    let job = {
        use sha2::{Digest, Sha256};
        let dry = Realm::new();
        let mut dry = Store::open(&dry.0).unwrap();
        let (tools, _) = prepared_workspace_call(&mut dry, "read_job", serde_json::json!({}));
        let digest = Sha256::digest(tools.effects[0].id.as_bytes());
        format!(
            "job-{}",
            digest[..4]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    };
    let (tools, base) = prepared_workspace_blocks(
        &mut store,
        vec![
            call(
                "start",
                "start_job",
                serde_json::json!({"command": "cat input.txt; echo; echo made > made.txt; echo hello from job; sleep 600", "timeout": 120}),
            ),
            call("list", "read_job", serde_json::json!({})),
            call(
                "read",
                "read_job",
                serde_json::json!({"job": job, "offset": 0, "wait": 10}),
            ),
            call("stop", "stop_job", serde_json::json!({"job": job})),
            call(
                "missing",
                "read_job",
                serde_json::json!({"job": "job-nope"}),
            ),
        ],
    );
    let mut results = vec![];
    for (index, intent) in tools.effects.iter().enumerate() {
        let mut request = strict_dispatch_request(&store, &tools, &executor);
        request.expected_revision = store.snapshot().unwrap().revision;
        request.intent = intent.clone();
        request.attempt.effect = intent.id.clone();
        request.attempt.id = format!("job-attempt-{index}");
        let DispatchOutcome::Recorded(bytes) = store
            .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
            .unwrap()
        else {
            panic!("job call {index}")
        };
        let block: Block = serde_json::from_slice(&bytes).unwrap();
        let before = state(&mut store);
        let after = before.apply_tool(&intent.id, vec![block.clone()]).unwrap();
        let mut apply = receipt_proposal(
            &mut store,
            &format!("job-apply-{index}"),
            &request.attempt,
            &before,
            &after,
        );
        apply.workspace = store.workspace_receipt(&request.attempt.id).unwrap();
        assert_eq!(apply.workspace.as_deref(), Some(base.as_str()));
        store
            .commit_turn_workspace_receipt(
                &apply,
                &receipt_binding(
                    &request.attempt,
                    tools.expected_revision + 1,
                    intent,
                    &bytes,
                ),
                *executor.helper_digest(),
            )
            .unwrap();
        let Block::ToolResult {
            content, is_error, ..
        } = block
        else {
            panic!("tool result")
        };
        results.push((content, is_error));
        if index == 0 {
            assert_eq!(store.running_jobs(), 1);
        }
    }
    let (started, error) = &results[0];
    assert_eq!(*error, Some(false), "{started}");
    assert!(
        started.starts_with(&format!("Started background job {job} ")),
        "{started}"
    );
    assert!(
        started.contains("old"),
        "the job sees the workspace: {started}"
    );
    let (listed, _) = &results[1];
    assert!(listed.contains(&format!("{job}  running")), "{listed}");
    let (read, error) = &results[2];
    assert_eq!(*error, Some(false));
    assert!(read.starts_with("old\n"), "{read}");
    assert!(read.contains(&format!("[{job} running after ")), "{read}");
    let (stopped, error) = &results[3];
    assert_eq!(*error, Some(false));
    assert!(stopped.starts_with(&format!("Stopped {job}.")), "{stopped}");
    assert!(stopped.contains("ended: stopped"), "{stopped}");
    let (missing, error) = &results[4];
    assert_eq!(*error, Some(true));
    assert!(
        missing.contains("no background job `job-nope`"),
        "{missing}"
    );
    assert_eq!(store.running_jobs(), 0);
    assert_eq!(store.stop_jobs(), 0);
    let root = store.snapshot().unwrap().workspace.unwrap();
    assert_eq!(root, base);
    assert!(
        store
            .read_versioned_file("operator", &root, "made.txt")
            .is_err()
    );
}

#[cfg(feature = "dext-strict-workspace")]
#[cfg(target_os = "linux")]
fn finish_workspace_turn(store: &mut Store, tools: &Proposal, name: &str) -> u64 {
    use spindle_store::broker::DispatchOutcome;
    use std::sync::atomic::AtomicBool;
    let executor = strict_executor();
    let mut request = strict_dispatch_request(store, tools, &executor);
    request.attempt.id = format!("{name}-bash");
    let DispatchOutcome::Recorded(bytes) = store
        .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
        .unwrap()
    else {
        panic!("strict dispatch")
    };
    let before = state(store);
    let block = serde_json::from_slice(&bytes).unwrap();
    let after = before.apply_tool(&request.intent.id, vec![block]).unwrap();
    let mut apply = receipt_proposal(
        store,
        &format!("{name}-apply"),
        &request.attempt,
        &before,
        &after,
    );
    let root = store
        .workspace_receipt(&request.attempt.id)
        .unwrap()
        .unwrap();
    apply.workspace = Some(root);
    store
        .commit_turn_workspace_receipt(
            &apply,
            &receipt_binding(
                &request.attempt,
                tools.expected_revision + 1,
                &request.intent,
                &bytes,
            ),
            *executor.helper_digest(),
        )
        .unwrap();
    let prepared = request_for_after_tools(store, name);
    let before = state(store);
    let Effect::Model { waiting, .. } = before.pending_effects().unwrap().remove(0) else {
        panic!("model")
    };
    let model = attempt(
        store,
        &prepared.effects[0].id,
        &format!("{name}-final-model"),
    );
    let recorded = response(&waiting, 0);
    let bytes = recorded.checkpoint();
    store.ingest_receipt(&model.id, &bytes).unwrap();
    let review = before.apply_model(&recorded, "route-v1").unwrap();
    let apply = receipt_proposal(store, &format!("{name}-review"), &model, &before, &review);
    store
        .commit_turn_receipt(
            &apply,
            &receipt_binding(
                &model,
                prepared.expected_revision + 1,
                &prepared.effects[0],
                &bytes,
            ),
        )
        .unwrap();
    let done = review.finish().unwrap();
    let finish = action_proposal(
        store,
        &format!("{name}-finish"),
        &Action::Finish,
        &review,
        &done,
    );
    store.commit_turn_action(&finish).unwrap()
}

fn request_for_after_tools(store: &mut Store, name: &str) -> Proposal {
    let before = state(store);
    let prepared = request(&before);
    let after = before.prepare(prepared.clone(), "route-v1").unwrap();
    let action = Action::Prepare {
        request: ModelWait::new(prepared).checkpoint(),
    };
    let proposal = action_proposal(
        store,
        &format!("{name}-prepare-final"),
        &action,
        &before,
        &after,
    );
    store.commit_turn_action(&proposal).unwrap();
    proposal
}

#[cfg(feature = "dext-strict-workspace")]
#[cfg(target_os = "linux")]
fn rewind_history(store: &mut Store) -> (u64, u64) {
    let (tools, _) = prepared_workspace_tool(store, "printf first > input.txt");
    let target = finish_workspace_turn(store, &tools, "first");
    let mut history = state(store).transcript().unwrap();
    history.messages.push(Message {
        role: "user".into(),
        content: vec![Block::Text {
            text: "second prompt".into(),
        }],
    });
    let prepared = start(store, "future-turn", history);
    store.commit_turn_action(&prepared).unwrap();
    let before = state(store);
    let Effect::Model { waiting, .. } = before.pending_effects().unwrap().remove(0) else {
        panic!("model")
    };
    let model = attempt(store, &prepared.effects[0].id, "second-model");
    let recorded = response_blocks(
        &waiting,
        vec![Block::ToolUse {
            id: "second-call".into(),
            name: "bash".into(),
            input: serde_json::json!({"command":"printf second > input.txt","timeout":5}),
        }],
    );
    let bytes = recorded.checkpoint();
    store.ingest_receipt(&model.id, &bytes).unwrap();
    let after = before.apply_model(&recorded, "route-v1").unwrap();
    let tools = receipt_proposal(store, "second-tools", &model, &before, &after);
    store
        .commit_turn_receipt(
            &tools,
            &receipt_binding(
                &model,
                prepared.expected_revision + 1,
                &prepared.effects[0],
                &bytes,
            ),
        )
        .unwrap();
    let current = finish_workspace_turn(store, &tools, "second");
    (target, current)
}

#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn rewind_restarts_pending_then_publishes_target_pair_paused_and_preserves_receipts() {
    use spindle_store::rewind::RewindPhase;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let (target, current) = rewind_history(&mut store);
    let old = store.snapshot().unwrap();
    let selected = store.snapshot_at("operator", target).unwrap();
    let target_state = TurnContinuation::restore(&selected.continuation).unwrap();
    let receipts = store.receipt("first-bash").unwrap().unwrap();
    let epoch = store.epoch();
    let plan = store
        .begin_rewind("operator", "rewind-one", target, current, 0)
        .unwrap();
    assert_eq!(plan.phase, RewindPhase::Pending);
    assert_eq!(store.epoch(), epoch + 1);
    assert_eq!(store.snapshot().unwrap(), old);
    assert_eq!(
        store
            .begin_rewind("operator", "rewind-one", target, current, 0)
            .unwrap(),
        plan
    );
    assert!(matches!(
        store.begin_rewind("operator", "rewind-one", target + 1, current, 0),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        store.admit(&Command {
            scope: "operator".into(),
            id: "paused-input".into(),
            lineage: 0,
            kind: "turn".into(),
            payload: vec![]
        }),
        Err(Error::Paused)
    ));
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(
        store.rewind_status("operator", "rewind-one").unwrap(),
        Some(plan)
    );
    let finished = store.finish_rewind("operator", "rewind-one").unwrap();
    assert_eq!(finished.phase, RewindPhase::FinishedPaused);
    let restored = store.snapshot().unwrap();
    assert_eq!(restored.revision, current + 1);
    assert_eq!(restored.workspace, selected.workspace);
    let restored_state = state(&mut store);
    assert_eq!(
        restored_state.transcript().unwrap().checkpoint().unwrap(),
        target_state.transcript().unwrap().checkpoint().unwrap()
    );
    assert_ne!(restored_state.execution(), target_state.execution());
    assert_eq!(
        store
            .read_versioned_file(
                "operator",
                restored.workspace.as_ref().unwrap(),
                "input.txt"
            )
            .unwrap(),
        b"first"
    );
    assert_eq!(
        store
            .read_versioned_file("operator", old.workspace.as_ref().unwrap(), "input.txt")
            .unwrap(),
        b"second"
    );
    assert_eq!(store.receipt("first-bash").unwrap(), Some(receipts));
    assert!(store.receipt("second-bash").unwrap().is_some());
    assert_eq!(
        store.finish_rewind("operator", "rewind-one").unwrap(),
        finished
    );
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.snapshot().unwrap(), restored);
    assert_eq!(
        store.resume_rewind("operator", "rewind-one").unwrap().phase,
        RewindPhase::Resumed
    );
    assert!(matches!(
        store.admit(&Command {
            scope: "operator".into(),
            id: "stale-input".into(),
            lineage: 0,
            kind: "turn".into(),
            payload: vec![]
        }),
        Err(Error::Stale)
    ));
    store
        .admit(&Command {
            scope: "operator".into(),
            id: "fresh-input".into(),
            lineage: 1,
            kind: "turn".into(),
            payload: vec![],
        })
        .unwrap();
    drop(store);
    assert!(Store::open(&realm.0).is_ok());
}

#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn rewind_storage_faults_and_cancel_preserve_old_pair_and_original_identity() {
    use spindle_store::rewind::RewindPhase;
    for boundary in ["revisions", "head", "rewinds", "realm"] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let (target, current) = rewind_history(&mut store);
        let old = store.snapshot().unwrap();
        store
            .begin_rewind("operator", "rewind", target, current, 0)
            .unwrap();
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let op = if boundary == "revisions" {
            "INSERT"
        } else {
            "UPDATE"
        };
        db.execute_batch(&format!("CREATE TRIGGER fail_rewind BEFORE {op} ON {boundary} BEGIN SELECT RAISE(ABORT,'injected'); END;")).unwrap();
        assert!(matches!(
            store.finish_rewind("operator", "rewind"),
            Err(Error::Sql(_))
        ));
        db.execute_batch("DROP TRIGGER fail_rewind").unwrap();
        drop(db);
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        assert_eq!(store.snapshot().unwrap(), old);
        assert_eq!(
            store
                .rewind_status("operator", "rewind")
                .unwrap()
                .unwrap()
                .phase,
            RewindPhase::Pending
        );
        if boundary == "head" {
            assert_eq!(
                store.cancel_rewind("operator", "rewind").unwrap().phase,
                RewindPhase::CancelledPaused
            );
            assert_eq!(store.snapshot().unwrap(), old);
            assert!(store.finish_rewind("operator", "rewind").is_err());
            assert_eq!(
                store.resume_rewind("operator", "rewind").unwrap().phase,
                RewindPhase::CancelledResumed
            );
            drop(store);
            assert!(Store::open(&realm.0).is_ok());
        } else {
            store.finish_rewind("operator", "rewind").unwrap();
            assert_eq!(store.snapshot().unwrap().revision, current + 1);
        }
    }
}

#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn rewind_lineage_damage_refuses_takeover_before_advancing_epoch() {
    for damage in ["missing", "ahead", "rewritten", "cancelled"] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let (target, current) = rewind_history(&mut store);
        store
            .begin_rewind("operator", "rewind", target, current, 0)
            .unwrap();
        if damage == "cancelled" {
            store.cancel_rewind("operator", "rewind").unwrap();
            store.resume_rewind("operator", "rewind").unwrap();
        } else {
            store.finish_rewind("operator", "rewind").unwrap();
            store.resume_rewind("operator", "rewind").unwrap();
        }
        drop(store);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let epoch: i64 = db
            .query_row("SELECT epoch FROM realm", [], |row| row.get(0))
            .unwrap();
        match damage {
            "missing" => {
                db.execute("UPDATE realm SET lineage=0", []).unwrap();
            }
            "ahead" | "cancelled" => {
                db.execute("UPDATE realm SET lineage=lineage+1", [])
                    .unwrap();
            }
            "rewritten" => {
                let payload = [
                    b"SRW1".as_slice(),
                    &target.to_be_bytes(),
                    &current.to_be_bytes(),
                    &1u64.to_be_bytes(),
                ]
                .concat();
                db.execute("UPDATE realm SET lineage=2", []).unwrap();
                db.execute("UPDATE rewinds SET old_lineage=1", []).unwrap();
                db.execute(
                    "UPDATE admissions SET lineage=1,payload=?1 WHERE kind='spindle-rewind-v1'",
                    [payload],
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            Store::open(&realm.0).is_err(),
            "accepted {damage} lineage history"
        );
        assert_eq!(
            db.query_row("SELECT epoch FROM realm", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            epoch
        );
    }
}

#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn repeated_rewinds_and_cancelled_plans_keep_contiguous_lineage_after_reopen() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let (target, mut current) = rewind_history(&mut store);
    for lineage in 0..3 {
        let cancel = format!("cancel-{lineage}");
        store
            .begin_rewind("operator", &cancel, target, current, lineage)
            .unwrap();
        store.cancel_rewind("operator", &cancel).unwrap();
        store.resume_rewind("operator", &cancel).unwrap();
        let command = format!("rewind-{lineage}");
        store
            .begin_rewind("operator", &command, target, current, lineage)
            .unwrap();
        let receipt = store.finish_rewind("operator", &command).unwrap();
        current += 1;
        assert_eq!(receipt.revision, Some(current));
        store.resume_rewind("operator", &command).unwrap();
        drop(store);
        store = Store::open(&realm.0).unwrap();
        assert_eq!(store.snapshot().unwrap().revision, current);
        assert!(store.receipt("second-bash").unwrap().is_some());
    }
}

#[cfg(feature = "dext-strict-workspace")]
#[test]
#[ignore = "invoked by parent rewind process-kill test"]
#[cfg(target_os = "linux")]
fn rewind_cut_child() {
    let root = PathBuf::from(std::env::var_os("SPINDLE_REWIND_REALM").unwrap());
    let cut = std::env::var("SPINDLE_REWIND_CUT").unwrap();
    let mut store = Store::open(&root).unwrap();
    let (target, current) = rewind_history(&mut store);
    std::fs::write(root.join("rewind-revisions"), format!("{target} {current}")).unwrap();
    store
        .begin_rewind("operator", "rewind-kill", target, current, 0)
        .unwrap();
    if cut == "pending" {
        wait_for_kill(&root);
    }
    if cut.starts_with("cancelled") {
        store.cancel_rewind("operator", "rewind-kill").unwrap();
        if cut == "cancelled-paused" {
            wait_for_kill(&root);
        }
        store.resume_rewind("operator", "rewind-kill").unwrap();
    } else {
        store.finish_rewind("operator", "rewind-kill").unwrap();
        if cut == "finished-paused" {
            wait_for_kill(&root);
        }
        assert_eq!(cut, "resumed");
        store.resume_rewind("operator", "rewind-kill").unwrap();
    }
    wait_for_kill(&root);
}

#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn rewind_process_kills_preserve_coherent_head_pause_and_monotonic_receipts() {
    use spindle_store::rewind::RewindPhase;
    use std::time::{Duration, Instant};
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for cut in [
        "pending",
        "finished-paused",
        "resumed",
        "cancelled-paused",
        "cancelled-resumed",
    ] {
        let realm = Realm::new();
        let mut child = Guard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "rewind_cut_child", "--ignored"])
                .env("SPINDLE_REWIND_REALM", &realm.0)
                .env("SPINDLE_REWIND_CUT", cut)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !realm.0.join("cut-ready").is_file() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "child exited at {cut}"
            );
            assert!(Instant::now() < deadline, "child stalled at {cut}");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(child);
        let revisions: Vec<u64> = std::fs::read_to_string(realm.0.join("rewind-revisions"))
            .unwrap()
            .split_whitespace()
            .map(|value| value.parse().unwrap())
            .collect();
        let (target, old) = (revisions[0], revisions[1]);
        let mut store = Store::open(&realm.0).unwrap();
        let plan = store
            .rewind_status("operator", "rewind-kill")
            .unwrap()
            .unwrap();
        let finalized = matches!(cut, "finished-paused" | "resumed");
        assert_eq!(
            store.snapshot().unwrap().revision,
            old + u64::from(finalized),
            "{cut}"
        );
        let expected = match cut {
            "pending" => RewindPhase::Pending,
            "finished-paused" => RewindPhase::FinishedPaused,
            "resumed" => RewindPhase::Resumed,
            "cancelled-paused" => RewindPhase::CancelledPaused,
            _ => RewindPhase::CancelledResumed,
        };
        assert_eq!(plan.phase, expected);
        assert!(store.receipt("first-bash").unwrap().is_some());
        assert!(store.receipt("second-bash").unwrap().is_some());
        let input = Command {
            scope: "operator".into(),
            id: "post-kill-input".into(),
            lineage: u64::from(finalized),
            kind: "probe".into(),
            payload: vec![],
        };
        if matches!(cut, "pending" | "finished-paused" | "cancelled-paused") {
            assert!(matches!(store.admit(&input), Err(Error::Paused)));
        }
        if cut == "pending" {
            store.finish_rewind("operator", "rewind-kill").unwrap();
        }
        if !cut.starts_with("cancelled") {
            let status = store.finish_rewind("operator", "rewind-kill").unwrap();
            assert_eq!(status.revision, Some(old + 1));
            let selected = store.snapshot_at("operator", target).unwrap();
            assert_eq!(store.snapshot().unwrap().workspace, selected.workspace);
        } else {
            assert!(store.finish_rewind("operator", "rewind-kill").is_err());
        }
        store.resume_rewind("operator", "rewind-kill").unwrap();
        let input = Command {
            lineage: u64::from(!cut.starts_with("cancelled")),
            ..input
        };
        store.admit(&input).unwrap();
        drop(store);
        assert!(Store::open(&realm.0).is_ok());
    }
}

#[cfg(all(feature = "dext-native-image", target_os = "linux"))]
fn image_worker() -> dext_native::image::PinnedWorker {
    use sha2::{Digest, Sha256};
    let path = std::path::Path::new(env!("CARGO_BIN_EXE_spindle-image-worker"));
    let digest: [u8; 32] = Sha256::digest(std::fs::read(path).unwrap()).into();
    dext_native::image::PinnedWorker::open(path, digest).unwrap()
}

#[cfg(all(feature = "dext-native-image", target_os = "linux"))]
fn prepared_read_image(store: &mut Store) -> Proposal {
    prepared_tool_blocks(
        store,
        vec![Block::ToolUse {
            id: "image-call".into(),
            name: "read_image".into(),
            input: serde_json::json!({"path":"approved.jpg"}),
        }],
    )
}

#[cfg(all(feature = "dext-native-image", target_os = "linux"))]
fn native_image_request(
    store: &Store,
    tools: &Proposal,
    worker: &dext_native::image::PinnedWorker,
) -> spindle_store::broker::DispatchRequest {
    let mut request = dispatch_request(store, tools);
    request.attempt.implementation = format!(
        "dext-read-image-v1-{}",
        worker
            .digest()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    request
}

#[cfg(all(feature = "dext-native-image", target_os = "linux"))]
fn image_source(root: &std::path::Path) -> spindle_store::broker::ImageSource {
    use sha2::{Digest, Sha256};
    let bytes = include_bytes!("fixtures/dext-worker-2x1.jpg");
    let path = root.join("approved.jpg");
    std::fs::write(&path, bytes).unwrap();
    spindle_store::broker::ImageSource::new(
        std::fs::File::open(path).unwrap(),
        Sha256::digest(bytes).into(),
    )
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn native_image_dispatch_reserves_before_source_and_atomically_ingests_real_worker_output() {
    use spindle_store::broker::DispatchOutcome;
    use spindle_store::image::ImageReceipt;
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;
    let worker = image_worker();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let tools = prepared_read_image(&mut store);
    let mut request = native_image_request(&store, &tools, &worker);
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    let approvals = Cell::new(0);
    let outcome = store.dispatch_read_image(&request, &AtomicBool::new(false), &worker,
        |_| { approvals.set(approvals.get() + 1); Ok(()) },
        |committed, _| {
            assert!(matches!(committed.effect(), Effect::Tool { call, .. } if call.id == "image-call" && call.name == "read_image"));
            assert_eq!(db.query_row("SELECT count(*) FROM attempts WHERE id=?1", [&request.attempt.id], |r| r.get::<_, i64>(0)).unwrap(), 1);
            assert_eq!(db.query_row("SELECT count(*) FROM artifact_stages WHERE id=?1", [&request.attempt.id], |r| r.get::<_, i64>(0)).unwrap(), 1);
            assert_eq!(db.query_row("SELECT count(*) FROM objects", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
            Ok(image_source(&realm.0))
        }).unwrap();
    assert_eq!(approvals.get(), 2);
    let DispatchOutcome::Recorded(bytes) = outcome else {
        panic!("native image receipt")
    };
    let images = ImageReceipt::decode(&bytes).unwrap();
    assert_eq!((images.images[0].width, images.images[0].height), (2, 1));
    assert_eq!(
        store.receipt(&request.attempt.id).unwrap(),
        Some(bytes.clone())
    );
    assert_eq!(
        store.snapshot().unwrap().revision,
        2,
        "ingress is not adoption"
    );
    for table in ["objects", "staging_pins", "image_provenance"] {
        assert_eq!(
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    drop(db);
    drop(store);
    std::fs::remove_file(realm.0.join("approved.jpg")).unwrap();
    let mut store = Store::open(&realm.0).unwrap();
    request.owner_epoch = store.epoch();
    assert!(matches!(
        store.dispatch_read_image(
            &request,
            &AtomicBool::new(false),
            &worker,
            |_| Err(Error::Cancelled),
            |_, _| panic!("denied source opened")
        ),
        Err(Error::Cancelled)
    ));
    assert!(
        matches!(store.dispatch_read_image(&request, &AtomicBool::new(false), &worker,
        |_| Ok(()), |_, _| panic!("saved source reopened")).unwrap(), DispatchOutcome::Reused(saved) if saved == bytes)
    );
    let before = state(&mut store);
    let after =
        spindle_store::turn::image_receipt_transition(&before, &request.intent, &images).unwrap();
    let proposed = receipt_proposal(
        &mut store,
        "native-image-apply",
        &request.attempt,
        &before,
        &after,
    );
    store
        .commit_turn_image_receipt(
            &proposed,
            &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
            *worker.digest(),
        )
        .unwrap();
    assert_eq!(state(&mut store).control().phase(), Phase::AfterTools);
    assert_eq!(store.snapshot().unwrap().revision, 3);
    assert!(
        matches!(store.dispatch_read_image(&request, &AtomicBool::new(false), &worker,
        |_| Ok(()), |_, _| panic!("adopted source reopened")).unwrap(), DispatchOutcome::Reused(saved) if saved == bytes)
    );
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn native_image_ingress_faults_roll_back_pixels_pins_provenance_and_receipt() {
    use spindle_store::broker::DispatchOutcome;
    use std::sync::atomic::AtomicBool;
    let worker = image_worker();
    for table in ["objects", "staging_pins", "image_provenance", "receipts"] {
        for fault in ["ABORT,'injected'", "IGNORE"] {
            let realm = Realm::new();
            let mut store = Store::open(&realm.0).unwrap();
            let tools = prepared_read_image(&mut store);
            let mut request = native_image_request(&store, &tools, &worker);
            let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
            db.execute_batch(&format!("CREATE TRIGGER fail_ingress BEFORE INSERT ON {table} BEGIN SELECT RAISE({fault}); END;")).unwrap();
            assert!(matches!(
                store.dispatch_read_image(
                    &request,
                    &AtomicBool::new(false),
                    &worker,
                    |_| Ok(()),
                    |_, _| Ok(image_source(&realm.0))
                ),
                Err(Error::Sql(_) | Error::Incompatible)
            ));
            assert!(matches!(store.snapshot(), Err(Error::Paused)));
            for checked in ["objects", "staging_pins", "image_provenance"] {
                assert_eq!(
                    db.query_row(&format!("SELECT count(*) FROM {checked}"), [], |r| r
                        .get::<_, i64>(0))
                        .unwrap(),
                    0,
                    "{table} rollback"
                );
            }
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM receipts WHERE attempt=?1",
                    [&request.attempt.id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                0
            );
            db.execute_batch("DROP TRIGGER fail_ingress").unwrap();
            drop(db);
            drop(store);
            let mut store = Store::open(&realm.0).unwrap();
            request.owner_epoch = store.epoch();
            assert!(matches!(
                store
                    .dispatch_read_image(
                        &request,
                        &AtomicBool::new(false),
                        &worker,
                        |_| Ok(()),
                        |_, _| panic!("failed native attempt reran")
                    )
                    .unwrap(),
                DispatchOutcome::Reconcile
            ));
            request.attempt.id = "replacement-attempt".into();
            request.attempt.owner_epoch = store.epoch();
            assert!(matches!(
                store.dispatch_read_image(
                    &request,
                    &AtomicBool::new(false),
                    &worker,
                    |_| Ok(()),
                    |_, _| panic!("replacement native attempt ran")
                ),
                Err(Error::Conflict)
            ));
        }
    }
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn native_image_source_is_content_bound_regular_bounded_and_not_path_reopened() {
    use sha2::{Digest, Sha256};
    use spindle_store::broker::{DispatchOutcome, ImageSource};
    use std::io::{Seek, SeekFrom};
    use std::sync::atomic::AtomicBool;
    let worker = image_worker();
    let original = include_bytes!("fixtures/dext-worker-2x1.jpg");
    let approved: [u8; 32] = Sha256::digest(original).into();
    for case in 0..6 {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let tools = prepared_read_image(&mut store);
        let request = native_image_request(&store, &tools, &worker);
        let path = realm.0.join("approved.jpg");
        std::fs::write(&path, original).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        match case {
            0 => {
                std::fs::write(&path, b"changed after approval").unwrap();
            }
            1 => {
                file = std::fs::File::open(&realm.0).unwrap();
            }
            2 => {
                std::fs::File::create(&path)
                    .unwrap()
                    .set_len(dext_native::image::SOURCE_BYTE_CAP as u64 + 1)
                    .unwrap();
            }
            3 => {
                std::fs::write(&path, []).unwrap();
            }
            4 => {
                std::fs::rename(&path, realm.0.join("old.jpg")).unwrap();
                std::fs::write(&path, b"replacement must not be opened").unwrap();
                file.seek(SeekFrom::End(0)).unwrap();
            }
            _ => {}
        }
        let result = store.dispatch_read_image(
            &request,
            &AtomicBool::new(false),
            &worker,
            |_| Ok(()),
            |_, _| {
                Ok(ImageSource::new(
                    file,
                    if case == 5 { [0; 32] } else { approved },
                ))
            },
        );
        if case == 4 {
            assert!(matches!(result.unwrap(), DispatchOutcome::Recorded(_)));
        } else {
            assert!(matches!(result, Err(Error::Invalid)));
            assert!(store.receipt(&request.attempt.id).unwrap().is_none());
            assert!(matches!(
                store
                    .dispatch_read_image(
                        &request,
                        &AtomicBool::new(false),
                        &worker,
                        |_| Ok(()),
                        |_, _| panic!("bad source reran")
                    )
                    .unwrap(),
                DispatchOutcome::Reconcile
            ));
        }
    }
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn native_image_profile_refuses_wrong_obligations_worker_and_current_authority() {
    use std::sync::atomic::AtomicBool;
    let worker = image_worker();
    for case in 0..8 {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let mut name = "read_image";
        let mut input = serde_json::json!({"path":"approved.jpg"});
        match case {
            0 => name = "inert-tool",
            1 => input = serde_json::json!({"path":"approved.jpg", "extra":true}),
            2 => input = serde_json::json!({"path":""}),
            3 => input = serde_json::json!({"path":42}),
            _ => {}
        }
        let tools = prepared_tool_blocks(
            &mut store,
            vec![Block::ToolUse {
                id: "image-call".into(),
                name: name.into(),
                input,
            }],
        );
        let mut request = native_image_request(&store, &tools, &worker);
        let interrupt = AtomicBool::new(case == 6);
        match case {
            4 => request.attempt.implementation = "wrong-worker".into(),
            5 => {
                store.cancel_scope("operator", "cancel-native").unwrap();
            }
            7 => request.expected_revision += 1,
            _ => {}
        }
        assert!(
            store
                .dispatch_read_image(
                    &request,
                    &interrupt,
                    &worker,
                    |_| Ok(()),
                    |_, _| panic!("invalid native source opened")
                )
                .is_err()
        );
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM attempts WHERE id=?1",
                [&request.attempt.id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let tools = start(&mut store, "model-not-image", empty_transcript());
    store.commit_turn_action(&tools).unwrap();
    let request = native_image_request(&store, &tools, &worker);
    assert!(matches!(
        store.dispatch_read_image(
            &request,
            &AtomicBool::new(false),
            &worker,
            |_| Ok(()),
            |_, _| panic!("model used image adapter")
        ),
        Err(Error::Invalid)
    ));
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn native_image_denied_changed_or_interrupted_source_never_executes_or_retries() {
    use spindle_store::broker::DispatchOutcome;
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;
    let worker = image_worker();
    for case in 0..5 {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let tools = prepared_read_image(&mut store);
        let request = native_image_request(&store, &tools, &worker);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let approvals = Cell::new(0);
        let interrupt = AtomicBool::new(false);
        let epoch = store.epoch();
        let result = store.dispatch_read_image(
            &request,
            &interrupt,
            &worker,
            |_| {
                approvals.set(approvals.get() + 1);
                if case == 0 || (case == 1 && approvals.get() == 2) {
                    return Err(Error::Cancelled);
                }
                Ok(())
            },
            |_, flag| {
                assert!(case >= 2, "denied source opened");
                if case == 2 {
                    return Err(Error::Cancelled);
                }
                if case == 3 {
                    flag.store(true, Ordering::SeqCst);
                }
                if case == 4 {
                    db.execute(
                        "INSERT INTO cancellations VALUES('operator','source-revoked',?1)",
                        [epoch as i64],
                    )
                    .unwrap();
                }
                Ok(image_source(&realm.0))
            },
        );
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(store.receipt(&request.attempt.id).unwrap().is_none());
        interrupt.store(false, Ordering::SeqCst);
        if case > 0 {
            assert!(matches!(
                store
                    .dispatch_read_image(
                        &request,
                        &interrupt,
                        &worker,
                        |_| Ok(()),
                        |_, _| panic!("interrupted native source reran")
                    )
                    .unwrap(),
                DispatchOutcome::Reconcile
            ));
        } else {
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM attempts WHERE id=?1",
                    [&request.attempt.id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                0
            );
        }
    }
}

#[cfg(all(feature = "dext-native-image", target_os = "linux"))]
struct CancelImageAt {
    phase: spindle_store::broker::ImagePhase,
    requested: bool,
    committed: bool,
    db: rusqlite::Connection,
}
#[cfg(all(feature = "dext-native-image", target_os = "linux"))]
impl spindle_store::broker::ImageControl for CancelImageAt {
    fn cancellation(
        &mut self,
        phase: spindle_store::broker::ImagePhase,
    ) -> spindle_store::Result<Option<String>> {
        if phase == self.phase && !self.requested {
            self.requested = true;
            Ok(Some("cancel-image".into()))
        } else {
            Ok(None)
        }
    }
    fn committed(&mut self, command: &str, epoch: u64) -> spindle_store::Result<()> {
        let saved: (String, i64) = self.db.query_row(
            "SELECT command_id,epoch FROM cancellations WHERE scope='operator'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(saved, (command.into(), epoch as i64));
        self.committed = true;
        Ok(())
    }
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn controlled_image_cancel_before_source_or_worker_is_durable_without_execution() {
    use spindle_store::broker::{DispatchOutcome, ImagePhase};
    use std::cell::Cell;
    let worker = image_worker();
    for phase in [ImagePhase::BeforeSource, ImagePhase::BeforeWorker] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let tools = prepared_read_image(&mut store);
        let mut request = native_image_request(&store, &tools, &worker);
        let epoch = store.epoch();
        let source_calls = Cell::new(0);
        let mut control = CancelImageAt {
            phase,
            requested: false,
            committed: false,
            db: rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap(),
        };
        assert!(matches!(
            store.dispatch_read_image_controlled(
                &request,
                &worker,
                |_| Ok(()),
                |_, _| {
                    assert_eq!(phase, ImagePhase::BeforeWorker);
                    source_calls.set(source_calls.get() + 1);
                    Ok(image_source(&realm.0))
                },
                &mut control
            ),
            Err(Error::Cancelled)
        ));
        assert!(control.committed);
        assert_eq!(
            store
                .image_cleanup("operator", &request.attempt.id)
                .unwrap(),
            Some(spindle_store::completion::ImageCleanup::NotStarted)
        );
        assert_eq!(
            source_calls.get(),
            u32::from(phase == ImagePhase::BeforeWorker)
        );
        assert!(store.receipt(&request.attempt.id).unwrap().is_none());
        assert_eq!(store.snapshot().unwrap().revision, 2);
        assert_eq!(
            store.cancel_scope("operator", "cancel-image").unwrap(),
            epoch
        );
        assert!(matches!(
            store.cancel_scope("operator", "different-cancel"),
            Err(Error::Conflict)
        ));
        drop(control);
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        assert_eq!(
            store.cancel_scope("operator", "cancel-image").unwrap(),
            epoch
        );
        request.owner_epoch = store.epoch();
        assert_eq!(
            store
                .image_cleanup("operator", &request.attempt.id)
                .unwrap(),
            Some(spindle_store::completion::ImageCleanup::NotStarted)
        );
        let mut control = CancelImageAt {
            phase,
            requested: false,
            committed: false,
            db: rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap(),
        };
        assert!(matches!(
            store
                .dispatch_read_image_controlled(
                    &request,
                    &worker,
                    |_| Ok(()),
                    |_, _| panic!("cancelled source reopened"),
                    &mut control
                )
                .unwrap(),
            DispatchOutcome::Reconcile
        ));
        assert!(
            !control.requested,
            "historical lookup is not a new control session"
        );
        let mut command = Command {
            scope: "operator".into(),
            id: "after-cancel".into(),
            lineage: 0,
            kind: "test".into(),
            payload: vec![],
        };
        assert!(matches!(store.admit(&command), Err(Error::Cancelled)));
        command.scope = "other-scope".into();
        assert!(
            store.admit(&command).is_ok(),
            "cancellation stays scope-bound"
        );
        request.attempt.id = "replacement".into();
        request.attempt.owner_epoch = store.epoch();
        assert!(
            store
                .dispatch_read_image_controlled(
                    &request,
                    &worker,
                    |_| Ok(()),
                    |_, _| panic!("cancelled replacement ran"),
                    &mut control
                )
                .is_err()
        );
    }
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn controlled_image_sealed_cancel_retains_receipt_but_blocks_turn_adoption_and_disclosure() {
    use spindle_store::broker::{DispatchOutcome, ImagePhase};
    use spindle_store::image::{DisclosureContext, ImageReceipt};
    let worker = image_worker();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let tools = prepared_read_image(&mut store);
    let mut request = native_image_request(&store, &tools, &worker);
    let before = state(&mut store);
    let mut control = CancelImageAt {
        phase: ImagePhase::Sealed,
        requested: false,
        committed: false,
        db: rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap(),
    };
    let outcome = store
        .dispatch_read_image_controlled(
            &request,
            &worker,
            |_| Ok(()),
            |_, _| Ok(image_source(&realm.0)),
            &mut control,
        )
        .unwrap();
    assert!(control.committed);
    let DispatchOutcome::Recorded(bytes) = outcome else {
        panic!("sealed result lost")
    };
    let images = ImageReceipt::decode(&bytes).unwrap();
    assert_eq!(
        store.receipt(&request.attempt.id).unwrap(),
        Some(bytes.clone())
    );
    assert_eq!(
        state(&mut store).checkpoint().unwrap(),
        before.checkpoint().unwrap()
    );
    let after =
        spindle_store::turn::image_receipt_transition(&before, &request.intent, &images).unwrap();
    let proposal = receipt_proposal(
        &mut store,
        "cancelled-image-apply",
        &request.attempt,
        &before,
        &after,
    );
    assert!(matches!(
        store.commit_turn_image_receipt(
            &proposal,
            &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
            *worker.digest()
        ),
        Err(Error::Cancelled)
    ));
    let stage = spindle_store::artifact::Stage {
        id: request.attempt.id.clone(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    assert!(matches!(
        store.put_object(&stage, b"cancel bypass"),
        Err(Error::Cancelled)
    ));
    let token = worker
        .sanitize(include_bytes!("fixtures/dext-worker-2x1.jpg"), None)
        .unwrap();
    assert!(matches!(
        store.put_sanitized_image(&stage, &token),
        Err(Error::Cancelled)
    ));
    let context = DisclosureContext {
        scope: "operator".into(),
        route: "a".repeat(64),
        turn: "turn".into(),
        policy_generation: 1,
        approved_worker: *worker.digest(),
    };
    store.set_image_disclosure_context(context.clone()).unwrap();
    assert!(matches!(
        store.approve_image_disclosure(&images.images[0], &context),
        Err(Error::Cancelled)
    ));
    drop(control);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    request.owner_epoch = store.epoch();
    let mut control = CancelImageAt {
        phase: ImagePhase::BeforeSource,
        requested: false,
        committed: false,
        db: rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap(),
    };
    assert!(matches!(
        store.dispatch_read_image_controlled(
            &request,
            &worker,
            |_| Err(Error::Cancelled),
            |_, _| panic!("denied saved source"),
            &mut control
        ),
        Err(Error::Cancelled)
    ));
    assert!(
        matches!(store.dispatch_read_image_controlled(&request, &worker, |_| Ok(()), |_, _| panic!("saved cancelled image reran"), &mut control).unwrap(), DispatchOutcome::Reused(saved) if saved == bytes)
    );
    assert!(matches!(
        store.commit_turn_image_receipt(
            &proposal,
            &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
            *worker.digest()
        ),
        Err(Error::Cancelled) | Err(Error::Stale)
    ));
    assert_eq!(store.snapshot().unwrap().revision, 2);
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn controlled_image_failed_cancel_storage_never_acknowledges_and_blocks_retry() {
    use spindle_store::broker::{DispatchOutcome, ImagePhase};
    let worker = image_worker();
    for fault in ["ABORT,'injected'", "IGNORE"] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let tools = prepared_read_image(&mut store);
        let mut request = native_image_request(&store, &tools, &worker);
        let mut control = CancelImageAt {
            phase: ImagePhase::BeforeSource,
            requested: false,
            committed: false,
            db: rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap(),
        };
        control.db.execute_batch(&format!("CREATE TRIGGER fail_cancel BEFORE INSERT ON cancellations BEGIN SELECT RAISE({fault}); END;")).unwrap();
        assert!(matches!(
            store.dispatch_read_image_controlled(
                &request,
                &worker,
                |_| Ok(()),
                |_, _| panic!("failed cancellation opened source"),
                &mut control
            ),
            Err(Error::Sql(_) | Error::Incompatible)
        ));
        assert!(!control.committed);
        assert!(matches!(store.snapshot(), Err(Error::Paused)));
        assert_eq!(
            control
                .db
                .query_row("SELECT count(*) FROM cancellations", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        control
            .db
            .execute_batch("DROP TRIGGER fail_cancel")
            .unwrap();
        drop(control);
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        request.owner_epoch = store.epoch();
        let mut control = CancelImageAt {
            phase: ImagePhase::BeforeSource,
            requested: false,
            committed: false,
            db: rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap(),
        };
        assert!(matches!(
            store
                .dispatch_read_image_controlled(
                    &request,
                    &worker,
                    |_| Ok(()),
                    |_, _| panic!("cancel storage failure reran"),
                    &mut control
                )
                .unwrap(),
            DispatchOutcome::Reconcile
        ));
        assert_eq!(
            store.cancel_scope("operator", "cancel-image").unwrap(),
            store.epoch()
        );
    }
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn controlled_image_cancelled_ingress_fault_retains_cancel_not_partial_output() {
    use spindle_store::broker::{DispatchOutcome, ImagePhase};
    let worker = image_worker();
    for table in ["objects", "staging_pins", "image_provenance", "receipts"] {
        for fault in ["ABORT,'injected'", "IGNORE"] {
            let realm = Realm::new();
            let mut store = Store::open(&realm.0).unwrap();
            let tools = prepared_read_image(&mut store);
            let mut request = native_image_request(&store, &tools, &worker);
            let mut control = CancelImageAt {
                phase: ImagePhase::Sealed,
                requested: false,
                committed: false,
                db: rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap(),
            };
            control.db.execute_batch(&format!("CREATE TRIGGER fail_output BEFORE INSERT ON {table} BEGIN SELECT RAISE({fault}); END;")).unwrap();
            assert!(matches!(
                store.dispatch_read_image_controlled(
                    &request,
                    &worker,
                    |_| Ok(()),
                    |_, _| Ok(image_source(&realm.0)),
                    &mut control
                ),
                Err(Error::Sql(_) | Error::Incompatible)
            ));
            assert!(control.committed);
            assert!(matches!(store.snapshot(), Err(Error::Paused)));
            for output in ["objects", "staging_pins", "image_provenance"] {
                assert_eq!(
                    control
                        .db
                        .query_row(&format!("SELECT count(*) FROM {output}"), [], |row| row
                            .get::<_, i64>(0))
                        .unwrap(),
                    0
                );
            }
            assert_eq!(
                control
                    .db
                    .query_row(
                        "SELECT count(*) FROM receipts WHERE attempt=?1",
                        [&request.attempt.id],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
            control
                .db
                .execute_batch("DROP TRIGGER fail_output")
                .unwrap();
            drop(control);
            drop(store);
            let mut store = Store::open(&realm.0).unwrap();
            request.owner_epoch = store.epoch();
            assert_eq!(
                store.cancel_scope("operator", "cancel-image").unwrap(),
                request.attempt.owner_epoch
            );
            let mut control = CancelImageAt {
                phase: ImagePhase::BeforeSource,
                requested: false,
                committed: false,
                db: rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap(),
            };
            assert!(matches!(
                store
                    .dispatch_read_image_controlled(
                        &request,
                        &worker,
                        |_| Ok(()),
                        |_, _| panic!("cancelled failed ingress reran"),
                        &mut control
                    )
                    .unwrap(),
                DispatchOutcome::Reconcile
            ));
            assert_eq!(store.snapshot().unwrap().revision, 2);
        }
    }
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn controlled_image_sealed_control_errors_retain_evidence_without_inventing_cancel() {
    use spindle_store::broker::{DispatchOutcome, ImageControl, ImagePhase};
    struct FailSealed {
        inner: CancelImageAt,
        acknowledge: bool,
    }
    impl ImageControl for FailSealed {
        fn cancellation(&mut self, phase: ImagePhase) -> spindle_store::Result<Option<String>> {
            if phase == ImagePhase::Sealed && !self.acknowledge {
                Err(Error::Invalid)
            } else {
                self.inner.cancellation(phase)
            }
        }
        fn committed(&mut self, command: &str, epoch: u64) -> spindle_store::Result<()> {
            self.inner.committed(command, epoch)?;
            Err(Error::Invalid)
        }
    }
    let worker = image_worker();
    for acknowledge in [false, true] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let tools = prepared_read_image(&mut store);
        let request = native_image_request(&store, &tools, &worker);
        let mut control = FailSealed {
            inner: CancelImageAt {
                phase: ImagePhase::Sealed,
                requested: false,
                committed: false,
                db: rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap(),
            },
            acknowledge,
        };
        assert!(matches!(
            store.dispatch_read_image_controlled(
                &request,
                &worker,
                |_| Ok(()),
                |_, _| Ok(image_source(&realm.0)),
                &mut control
            ),
            Err(Error::Invalid)
        ));
        assert_eq!(control.inner.committed, acknowledge);
        assert_eq!(
            control
                .inner
                .db
                .query_row("SELECT count(*) FROM cancellations", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            i64::from(acknowledge)
        );
        let bytes = store.receipt(&request.attempt.id).unwrap().unwrap();
        assert!(spindle_store::image::ImageReceipt::decode(&bytes).is_ok());
        assert!(
            matches!(store.dispatch_read_image_controlled(&request, &worker, |_| Ok(()), |_, _| panic!("sealed control error reran"), &mut control).unwrap(), DispatchOutcome::Reused(saved) if saved == bytes)
        );
        assert_eq!(store.snapshot().unwrap().revision, 2);
    }
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn controlled_cleanup_corruption_is_refused_before_takeover() {
    use spindle_store::broker::ImagePhase;
    for sql in [
        "UPDATE image_executions SET worker='not-a-hash'",
        "UPDATE image_executions SET worker=upper(worker)",
        "UPDATE image_executions SET worker=CASE substr(worker,1,1) WHEN '0' THEN '1' ELSE '0' END || substr(worker,2)",
        "PRAGMA ignore_check_constraints=ON; UPDATE image_executions SET state='not-an-integer'",
        "PRAGMA ignore_check_constraints=ON; UPDATE image_executions SET state=4",
        "PRAGMA ignore_check_constraints=ON; UPDATE image_executions SET state=-1",
        "PRAGMA foreign_keys=OFF; UPDATE image_executions SET attempt='missing-attempt'",
    ] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let worker = image_worker();
        let tools = prepared_read_image(&mut store);
        let request = native_image_request(&store, &tools, &worker);
        let mut control = CancelImageAt {
            phase: ImagePhase::BeforeSource,
            requested: false,
            committed: false,
            db: rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap(),
        };
        assert!(matches!(
            store.dispatch_read_image_controlled(
                &request,
                &worker,
                |_| Ok(()),
                |_, _| panic!("source should not open"),
                &mut control
            ),
            Err(Error::Cancelled)
        ));
        let epoch = store.epoch();
        drop(store);
        control.db.execute_batch(sql).unwrap();
        assert!(Store::open(&realm.0).is_err(), "accepted {sql}");
        assert_eq!(
            control
                .db
                .query_row("SELECT epoch FROM realm", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            epoch as i64
        );
    }
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn controlled_cleanup_reservation_fault_cannot_start_source_or_worker() {
    use spindle_store::broker::ImagePhase;
    let worker = image_worker();
    for fault in ["ABORT,'injected'", "IGNORE"] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let tools = prepared_read_image(&mut store);
        let request = native_image_request(&store, &tools, &worker);
        let mut control = CancelImageAt {
            phase: ImagePhase::BeforeSource,
            requested: false,
            committed: false,
            db: rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap(),
        };
        control.db.execute_batch(&format!("CREATE TRIGGER fail_cleanup_insert BEFORE INSERT ON image_executions BEGIN SELECT RAISE({fault}); END;")).unwrap();
        assert!(matches!(
            store.dispatch_read_image_controlled(
                &request,
                &worker,
                |_| Ok(()),
                |_, _| panic!("uncommitted execution ran"),
                &mut control
            ),
            Err(Error::Sql(_) | Error::Incompatible)
        ));
        assert!(matches!(store.snapshot(), Err(Error::Paused)));
        assert_eq!(
            control
                .db
                .query_row(
                    "SELECT count(*) FROM attempts WHERE id=?1",
                    [&request.attempt.id],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(
            control
                .db
                .query_row("SELECT count(*) FROM image_executions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(!control.requested);
    }
}

#[cfg(all(feature = "dext-native-image", target_os = "linux"))]
fn staged_image(
    store: &mut Store,
    worker: &dext_native::image::PinnedWorker,
) -> spindle_store::image::ImageReceipt {
    let token = worker
        .sanitize(include_bytes!("fixtures/dext-worker-2x1.jpg"), None)
        .unwrap();
    let stage = spindle_store::artifact::Stage {
        id: "turn-images".into(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    store.begin_stage(&stage).unwrap();
    spindle_store::image::ImageReceipt {
        images: vec![store.put_sanitized_image(&stage, &token).unwrap()],
    }
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn turn_images_preserve_partial_order_and_next_model_history_across_reopen() {
    use spindle_store::turn::image_receipt_transition;
    let worker = image_worker();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let tools = prepared_tools(&mut store, 2);
    let images = staged_image(&mut store, &worker);
    let bytes = images.encode().unwrap();
    for position in [1, 0] {
        let before = state(&mut store);
        let intent = &tools.effects[position];
        let mut dispatch = dispatch_request(&store, &tools);
        dispatch.expected_revision = store.snapshot().unwrap().revision;
        dispatch.intent = intent.clone();
        dispatch.attempt.effect = intent.id.clone();
        dispatch.attempt.id = format!("image-{position}");
        let outcome = store
            .dispatch_turn_image_effect(
                &dispatch,
                &std::sync::atomic::AtomicBool::new(false),
                *worker.digest(),
                |_| Ok(()),
                |committed, _| {
                    assert!(
                        matches!(committed.effect(), Effect::Tool { id, .. } if id == &intent.id)
                    );
                    Ok(bytes.clone())
                },
            )
            .unwrap();
        assert!(
            matches!(outcome, spindle_store::broker::DispatchOutcome::Recorded(saved) if saved == bytes)
        );
        let attempt = dispatch.attempt.clone();
        let after = image_receipt_transition(&before, intent, &images).unwrap();
        assert_eq!(before.control().phase(), Phase::ToolsWait);
        assert_eq!(after.pending_effects().unwrap().len(), position);
        let proposed = receipt_proposal(
            &mut store,
            &format!("image-apply-{position}"),
            &attempt,
            &before,
            &after,
        );
        assert!(proposed.effects.is_empty());
        let binding = receipt_binding(&attempt, 2, intent, &bytes);
        assert_eq!(
            store
                .commit_turn_image_receipt(&proposed, &binding, *worker.digest())
                .unwrap(),
            3 + u64::from(position == 0)
        );
        drop(store);
        store = Store::open(&realm.0).unwrap();
        assert_eq!(
            state(&mut store).checkpoint().unwrap(),
            after.checkpoint().unwrap()
        );
        dispatch.owner_epoch = store.epoch();
        assert!(matches!(store.dispatch_turn_image_effect(
            &dispatch, &std::sync::atomic::AtomicBool::new(false), *worker.digest(), |_| Ok(()),
            |_, _| panic!("saved image tool reran"),
        ).unwrap(), spindle_store::broker::DispatchOutcome::Reused(saved) if saved == bytes));
        assert_eq!(
            store
                .commit_turn_image_receipt(&proposed, &binding, *worker.digest())
                .unwrap(),
            proposed.expected_revision + 1
        );
        assert!(
            store
                .intents(proposed.expected_revision + 1)
                .unwrap()
                .is_empty()
        );
        if position == 1 {
            assert_eq!(state(&mut store).transcript().unwrap().messages.len(), 2);
        }
    }
    let before = state(&mut store);
    assert_eq!(before.control().phase(), Phase::AfterTools);
    let transcript = before.transcript().unwrap();
    assert_eq!(transcript.messages.len(), 3);
    let blocks = &transcript.messages[2].content;
    assert!(matches!(&blocks[0], Block::ToolResult { tool_use_id, .. } if tool_use_id == "call-0"));
    assert!(matches!(&blocks[2], Block::ToolResult { tool_use_id, .. } if tool_use_id == "call-1"));
    for position in [1, 3] {
        assert!(
            matches!(&blocks[position], Block::ImageArtifact { reference } if reference.scope == "operator" && reference.digest == images.images[0].object.digest)
        );
    }
    assert!(
        before.finish().is_err(),
        "tool results do not finish a turn"
    );
    let request = request(&before);
    let after = before.prepare(request.clone(), "route-v1").unwrap();
    let proposed = action_proposal(
        &mut store,
        "prepare-after-images",
        &Action::Prepare {
            request: ModelWait::new(request).checkpoint(),
        },
        &before,
        &after,
    );
    store.commit_turn_action(&proposed).unwrap();
    assert_eq!(state(&mut store).transcript().unwrap(), transcript);
    assert_eq!(proposed.effects.len(), 1);
    assert_eq!(state(&mut store).control().phase(), Phase::ModelWait);
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn turn_image_adoption_refuses_wrong_bindings_and_unrelated_proposals() {
    use spindle_store::turn::image_receipt_transition;
    let worker = image_worker();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let tools = prepared_tool(&mut store);
    let before = state(&mut store);
    let intent = &tools.effects[0];
    let images = staged_image(&mut store, &worker);
    let bytes = images.encode().unwrap();
    let attempt = attempt(&mut store, &intent.id, "image-attempt");
    store.ingest_receipt(&attempt.id, &bytes).unwrap();
    let after = image_receipt_transition(&before, intent, &images).unwrap();
    let proposed = receipt_proposal(&mut store, "adopt-turn-image", &attempt, &before, &after);
    let binding = receipt_binding(&attempt, 2, intent, &bytes);
    assert!(
        store.commit_turn_receipt(&proposed, &binding).is_err(),
        "plain results cannot hide artifact refs"
    );
    assert!(
        store
            .commit_turn_image_receipt(&proposed, &binding, [0; 32])
            .is_err()
    );
    for field in 0..6 {
        let mut wrong = proposed.clone();
        match field {
            0 => wrong.continuation = before.checkpoint().unwrap(),
            1 => wrong.engine = "unrelated-engine".into(),
            2 => wrong.scope = "other".into(),
            3 => wrong.effects = tools.effects.clone(),
            4 => wrong.workspace = Some("a".repeat(64)),
            _ => wrong.owner_epoch += 1,
        }
        assert!(
            store
                .commit_turn_image_receipt(&wrong, &binding, *worker.digest())
                .is_err()
        );
    }
    for field in 0..4 {
        let mut wrong = intent.clone();
        match field {
            0 => wrong.id = "call-0".into(),
            1 => wrong.adapter = "other".into(),
            2 => wrong.input = b"{}".to_vec(),
            _ => wrong.input.push(b' '),
        }
        assert!(image_receipt_transition(&before, &wrong, &images).is_err());
        assert!(
            store
                .commit_turn_image_receipt(
                    &proposed,
                    &receipt_binding(&attempt, 2, &wrong, &bytes),
                    *worker.digest()
                )
                .is_err()
        );
    }
    let mut changed = images.clone();
    changed.images[0].width += 1;
    assert!(
        store
            .commit_turn_image_receipt(
                &proposed,
                &receipt_binding(&attempt, 2, intent, &changed.encode().unwrap()),
                *worker.digest()
            )
            .is_err()
    );
    assert!(
        store
            .commit_turn_image_receipt(
                &proposed,
                &receipt_binding(&attempt, 1, intent, &bytes),
                *worker.digest()
            )
            .is_err()
    );
    assert_eq!(store.snapshot().unwrap().revision, 2);
    assert_eq!(store.receipt(&attempt.id).unwrap().unwrap(), bytes);
    assert_eq!(
        store
            .commit_turn_image_receipt(&proposed, &binding, *worker.digest())
            .unwrap(),
        3
    );
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn turn_image_adoption_requires_provenance_scoped_pins_and_matching_dimensions() {
    use spindle_store::turn::image_receipt_transition;
    let worker = image_worker();
    for fault in 0..3 {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let tools = prepared_tool(&mut store);
        let before = state(&mut store);
        let intent = &tools.effects[0];
        let mut images = staged_image(&mut store, &worker);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        match fault {
            0 => {
                db.execute("DELETE FROM image_provenance", []).unwrap();
            }
            1 => {
                db.execute("DELETE FROM staging_pins WHERE stage IN (SELECT id FROM artifact_stages WHERE scope='operator')", [])
                    .unwrap();
            }
            _ => images.images[0].width += 1,
        }
        let bytes = images.encode().unwrap();
        let attempt = attempt(&mut store, &intent.id, "unverified-image");
        store.ingest_receipt(&attempt.id, &bytes).unwrap();
        let after = image_receipt_transition(&before, intent, &images).unwrap();
        let proposed = receipt_proposal(
            &mut store,
            "reject-unverified-image",
            &attempt,
            &before,
            &after,
        );
        assert!(
            store
                .commit_turn_image_receipt(
                    &proposed,
                    &receipt_binding(&attempt, 2, intent, &bytes),
                    *worker.digest()
                )
                .is_err()
        );
        assert_eq!(store.snapshot().unwrap().revision, 2);
        assert!(store.intents(3).unwrap().is_empty());
        assert_eq!(store.receipt(&attempt.id).unwrap().unwrap(), bytes);
    }
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn turn_image_rollback_reopen_retry_and_cancel_keep_receipts_not_disclosure_grants() {
    use spindle_store::image::DisclosureContext;
    use spindle_store::turn::image_receipt_transition;
    let worker = image_worker();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let tools = prepared_tool(&mut store);
    let before = state(&mut store);
    let intent = &tools.effects[0];
    let images = staged_image(&mut store, &worker);
    let bytes = images.encode().unwrap();
    let attempt = attempt(&mut store, &intent.id, "image-attempt");
    store.ingest_receipt(&attempt.id, &bytes).unwrap();
    let after = image_receipt_transition(&before, intent, &images).unwrap();
    let mut proposed = receipt_proposal(&mut store, "apply-image", &attempt, &before, &after);
    let context = DisclosureContext {
        scope: "operator".into(),
        route: "a".repeat(64),
        turn: before.execution().into(),
        policy_generation: 1,
        approved_worker: *worker.digest(),
    };
    store.set_image_disclosure_context(context.clone()).unwrap();
    let grant = store
        .approve_image_disclosure(&images.images[0], &context)
        .unwrap();
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_image_turn BEFORE UPDATE ON head BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let binding = receipt_binding(&attempt, 2, intent, &bytes);
    assert!(matches!(
        store.commit_turn_image_receipt(&proposed, &binding, *worker.digest()),
        Err(Error::Sql(_))
    ));
    assert!(matches!(
        store.image_bytes_for_disclosure(&grant, &context),
        Err(Error::Paused)
    ));
    db.execute_batch("DROP TRIGGER fail_image_turn").unwrap();
    drop(db);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(
        state(&mut store).checkpoint().unwrap(),
        before.checkpoint().unwrap()
    );
    assert_eq!(store.receipt(&attempt.id).unwrap().unwrap(), bytes);
    store.set_image_disclosure_context(context.clone()).unwrap();
    assert!(matches!(
        store.image_bytes_for_disclosure(&grant, &context),
        Err(Error::Stale)
    ));
    assert!(matches!(
        store.commit_turn_image_receipt(&proposed, &binding, *worker.digest()),
        Err(Error::Stale)
    ));
    proposed.owner_epoch = store.epoch();
    assert_eq!(
        store
            .commit_turn_image_receipt(&proposed, &binding, *worker.digest())
            .unwrap(),
        3
    );
    assert!(
        store
            .commit_turn_image_receipt(&proposed, &binding, [0; 32])
            .is_err(),
        "retry rechecks current approved worker"
    );
    store.cancel_scope("operator", "cancel").unwrap();
    assert_eq!(
        store
            .commit_turn_image_receipt(&proposed, &binding, *worker.digest())
            .unwrap(),
        3
    );
    assert!(matches!(
        store.approve_image_disclosure(&images.images[0], &context),
        Err(Error::Cancelled)
    ));
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute(
        "UPDATE objects SET bytes=X'00' WHERE digest=?1",
        [&images.images[0].object.digest],
    )
    .unwrap();
    assert!(matches!(
        store.commit_turn_image_receipt(&proposed, &binding, *worker.digest()),
        Err(Error::Incompatible)
    ));
    assert!(matches!(store.snapshot(), Err(Error::Paused)));
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn turn_cancel_before_image_adoption_keeps_late_receipt_without_state_change() {
    use spindle_store::turn::image_receipt_transition;
    let worker = image_worker();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let tools = prepared_tool(&mut store);
    let before = state(&mut store);
    let intent = &tools.effects[0];
    let images = staged_image(&mut store, &worker);
    let bytes = images.encode().unwrap();
    let attempt = attempt(&mut store, &intent.id, "cancelled-image");
    store.cancel_scope("operator", "cancel").unwrap();
    store.ingest_receipt(&attempt.id, &bytes).unwrap();
    let after = image_receipt_transition(&before, intent, &images).unwrap();
    let proposed = receipt_proposal(&mut store, "late-image", &attempt, &before, &after);
    assert!(matches!(
        store.commit_turn_image_receipt(
            &proposed,
            &receipt_binding(&attempt, 2, intent, &bytes),
            *worker.digest()
        ),
        Err(Error::Cancelled)
    ));
    assert_eq!(
        state(&mut store).checkpoint().unwrap(),
        before.checkpoint().unwrap()
    );
    assert_eq!(store.receipt(&attempt.id).unwrap().unwrap(), bytes);
}

#[cfg(feature = "dext-image-continuation")]
#[test]
fn image_result_call_ids_use_native_provider_bounds_not_authority_labels() {
    use spindle_store::artifact::ObjectReference;
    use spindle_store::image::{ImageReceipt, JpegDescriptor};
    let images = ImageReceipt {
        images: vec![JpegDescriptor {
            object: ObjectReference {
                digest: "a".repeat(64),
                byte_length: 1,
            },
            width: 1,
            height: 1,
        }],
    };
    for id in ["provider call #1 🦀".into(), "x".repeat(4096)] {
        let blocks = images.result_blocks("operator", &id).unwrap();
        assert!(matches!(&blocks[0], Block::ToolResult { tool_use_id, .. } if tool_use_id == &id));
    }
    for id in ["".into(), "  ".into(), "x".repeat(4097)] {
        assert!(images.result_blocks("operator", &id).is_err());
    }
    assert!(images.result_blocks("bad scope", "call").is_err());
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn image_broker_rejects_unverified_output_but_retains_bounded_receipts() {
    use spindle_store::broker::DispatchOutcome;
    use std::sync::atomic::AtomicBool;
    let worker = image_worker();
    for fault in 0..7 {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let tools = prepared_tool(&mut store);
        let mut images = staged_image(&mut store, &worker);
        let request = dispatch_request(&store, &tools);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let mut approved = *worker.digest();
        match fault {
            0 => approved = [0; 32],
            1 => {
                db.execute("DELETE FROM image_provenance", []).unwrap();
            }
            2 => {
                db.execute("DELETE FROM staging_pins", []).unwrap();
            }
            3 => images.images[0].width += 1,
            6 => {
                db.execute(
                    "UPDATE objects SET bytes=X'00' WHERE digest=?1",
                    [&images.images[0].object.digest],
                )
                .unwrap();
            }
            _ => {}
        }
        let bytes = match fault {
            4 => b"malformed image receipt".to_vec(),
            5 => serde_json::to_vec(&Block::ToolResult {
                tool_use_id: "call-0".into(),
                content: "not an image receipt".into(),
                is_error: Some(false),
                metadata: Default::default(),
            })
            .unwrap(),
            _ => images.encode().unwrap(),
        };
        let result = store.dispatch_turn_image_effect(
            &request,
            &AtomicBool::new(false),
            approved,
            |_| Ok(()),
            |_, _| Ok(bytes.clone()),
        );
        assert!(result.is_err(), "unverified image variant {fault} admitted");
        assert_eq!(
            db.query_row(
                "SELECT result FROM receipts WHERE attempt=?1",
                [&request.attempt.id],
                |row| row.get::<_, Vec<u8>>(0)
            )
            .map(|stored| spindle_store::codec::unpack(&stored, usize::MAX).unwrap())
            .unwrap(),
            bytes
        );
        assert_eq!(
            db.query_row("SELECT revision FROM head WHERE id=1", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        if fault == 6 {
            assert!(matches!(result, Err(Error::Incompatible)));
            assert!(matches!(store.snapshot(), Err(Error::Paused)));
        } else {
            assert!(
                matches!(store.dispatch_turn_image_effect(&request, &AtomicBool::new(false), approved, |_| Ok(()), |_, _| panic!("invalid image reran")).unwrap(), DispatchOutcome::Reused(saved) if saved == bytes)
            );
            assert_eq!(state(&mut store).control().phase(), Phase::ToolsWait);
        }
    }
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn image_broker_refuses_model_obligations_and_denied_fresh_or_saved_access() {
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;
    let worker = image_worker();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = start(&mut store, "image-profile-on-model", empty_transcript());
    store.commit_turn_action(&prepared).unwrap();
    let request = dispatch_request(&store, &prepared);
    assert!(matches!(
        store.dispatch_turn_image_effect(
            &request,
            &AtomicBool::new(false),
            *worker.digest(),
            |_| Ok(()),
            |_, _| panic!("model used image channel")
        ),
        Err(Error::Invalid)
    ));
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM attempts", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    drop(db);
    drop(store);
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let tools = prepared_tool(&mut store);
    let images = staged_image(&mut store, &worker);
    let bytes = images.encode().unwrap();
    let request = dispatch_request(&store, &tools);
    assert!(matches!(
        store.dispatch_turn_image_effect(
            &request,
            &AtomicBool::new(false),
            *worker.digest(),
            |_| Err(Error::Cancelled),
            |_, _| panic!("denied image ran")
        ),
        Err(Error::Cancelled)
    ));
    let approvals = Cell::new(0);
    let interrupt = AtomicBool::new(false);
    store
        .dispatch_turn_image_effect(
            &request,
            &interrupt,
            *worker.digest(),
            |_| {
                approvals.set(approvals.get() + 1);
                Ok(())
            },
            |_, live| {
                live.store(true, Ordering::SeqCst);
                Ok(bytes.clone())
            },
        )
        .unwrap();
    assert_eq!(approvals.get(), 2);
    assert_eq!(store.receipt(&request.attempt.id).unwrap().unwrap(), bytes);
    assert!(matches!(
        store.dispatch_turn_image_effect(
            &request,
            &AtomicBool::new(false),
            *worker.digest(),
            |_| Err(Error::Cancelled),
            |_, _| panic!("denied reuse ran")
        ),
        Err(Error::Cancelled)
    ));
    assert_eq!(store.snapshot().unwrap().revision, 2);
}

#[test]
fn broker_tool_receipt_is_call_bound_canonical_and_separate_from_application() {
    use spindle_store::broker::DispatchOutcome;
    use std::sync::atomic::AtomicBool;
    for variant in 0..6 {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let tools = prepared_tool(&mut store);
        let request = dispatch_request(&store, &tools);
        let before = state(&mut store);
        let Effect::Tool { id, call } = before.pending_effects().unwrap().remove(0) else {
            panic!("tool");
        };
        let block = Block::ToolResult {
            tool_use_id: if variant == 1 {
                "other-call".into()
            } else {
                call.id.clone()
            },
            content: "sealed synthetic output".into(),
            is_error: Some(false),
            metadata: Default::default(),
        };
        let mut bytes = serde_json::to_vec(&block).unwrap();
        match variant {
            2 => bytes.push(b' '),
            3 => {
                bytes = serde_json::to_vec(&Block::Text {
                    text: "not a result".into(),
                })
                .unwrap()
            }
            4 => bytes.clear(),
            5 => bytes = vec![b'x'; spindle_store::MAX_PAYLOAD + 1],
            _ => {}
        }
        let result = store.dispatch_turn_effect(
            &request,
            &AtomicBool::new(false),
            |_| Ok(()),
            |committed, _| {
                let Effect::Tool {
                    id: committed_id,
                    call: committed_call,
                } = committed.effect()
                else {
                    panic!("tool");
                };
                assert_eq!(committed_id, &id);
                assert_eq!(committed_call.id, call.id);
                assert_eq!(committed_call.name, request.intent.adapter);
                assert_eq!(
                    serde_json::to_vec(&committed_call.input).unwrap(),
                    request.intent.input
                );
                Ok(bytes.clone())
            },
        );
        assert_eq!(store.snapshot().unwrap().revision, 2);
        if variant == 0 {
            assert!(matches!(result.unwrap(), DispatchOutcome::Recorded(saved) if saved == bytes));
            let after = before.apply_tool(&id, vec![block]).unwrap();
            let proposed = receipt_proposal(
                &mut store,
                "apply-broker-tool",
                &request.attempt,
                &before,
                &after,
            );
            store
                .commit_turn_receipt(
                    &proposed,
                    &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
                )
                .unwrap();
            assert_eq!(state(&mut store).control().phase(), Phase::AfterTools);
        } else {
            if variant == 5 {
                assert!(matches!(result, Err(Error::TooLarge { .. })));
            } else {
                assert!(matches!(result, Err(Error::Invalid)));
            }
            let mut proposed = receipt_proposal(
                &mut store,
                "bad-broker-tool",
                &request.attempt,
                &before,
                &before,
            );
            proposed.effects.clear();
            assert!(
                store
                    .commit_turn_receipt(
                        &proposed,
                        &receipt_binding(&request.attempt, 2, &request.intent, &bytes)
                    )
                    .is_err()
            );
        }
        let retry = store
            .dispatch_turn_effect(
                &request,
                &AtomicBool::new(false),
                |_| Ok(()),
                |_, _| panic!("tool reran"),
            )
            .unwrap();
        if variant >= 4 {
            assert!(store.receipt(&request.attempt.id).unwrap().is_none());
            assert!(matches!(retry, DispatchOutcome::Reconcile));
        } else {
            assert_eq!(store.receipt(&request.attempt.id).unwrap().unwrap(), bytes);
            assert!(matches!(retry, DispatchOutcome::Reused(saved) if saved == bytes));
        }
    }
}

#[test]
fn broker_model_result_must_match_saved_request_attempt_phase_and_native_encoding() {
    use spindle_store::broker::DispatchOutcome;
    use std::sync::atomic::AtomicBool;
    for variant in 0..4 {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let prepared = start(&mut store, "broker-model-binding", empty_transcript());
        store.commit_turn_action(&prepared).unwrap();
        let request = dispatch_request(&store, &prepared);
        let before = state(&mut store);
        let Effect::Model { waiting, .. } = before.pending_effects().unwrap().remove(0) else {
            panic!("model");
        };
        let recorded = response(&waiting, 0);
        let bytes = match variant {
            0 => {
                let mut identity = waiting.request().identity().clone();
                identity.credential_scope = "other-credential-version".into();
                let foreign = ModelWait::new(
                    PreparedRequest::new(
                        identity,
                        Encoding::Json,
                        waiting.request().body().to_vec(),
                    )
                    .unwrap(),
                )
                .start()
                .unwrap();
                response(&foreign, 0).checkpoint()
            }
            1 => response(
                &waiting
                    .abandon_unrecorded_for_retry()
                    .unwrap()
                    .start()
                    .unwrap(),
                0,
            )
            .checkpoint(),
            2 => recorded.apply().unwrap().checkpoint(),
            3 => waiting
                .record(
                    waiting.request(),
                    waiting.attempt(),
                    b"not a native response",
                )
                .unwrap()
                .checkpoint(),
            _ => unreachable!(),
        };
        let result = store.dispatch_turn_effect(
            &request,
            &AtomicBool::new(false),
            |_| Ok(()),
            |_, _| Ok(bytes.clone()),
        );
        if variant < 2 {
            assert!(matches!(result, Err(Error::Conflict)));
        } else {
            assert!(matches!(result, Err(Error::Invalid)));
        }
        assert_eq!(store.receipt(&request.attempt.id).unwrap().unwrap(), bytes);
        let valid_after = before.apply_model(&recorded, "route-v1").unwrap();
        let proposed = receipt_proposal(
            &mut store,
            "reject-model-result",
            &request.attempt,
            &before,
            &valid_after,
        );
        assert!(
            store
                .commit_turn_receipt(
                    &proposed,
                    &receipt_binding(&request.attempt, 1, &request.intent, &bytes)
                )
                .is_err()
        );
        assert_eq!(store.snapshot().unwrap().revision, 1);
        assert!(
            matches!(store.dispatch_turn_effect(&request, &AtomicBool::new(false), |_| Ok(()), |_, _| panic!("invalid saved model result caused retry")).unwrap(), DispatchOutcome::Reused(saved) if saved == bytes)
        );
    }
}

#[test]
fn broker_failed_receipt_persistence_pauses_without_rerunning_the_executor() {
    use spindle_store::broker::DispatchOutcome;
    use std::sync::atomic::AtomicBool;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = start(&mut store, "broker-storage", empty_transcript());
    store.commit_turn_action(&prepared).unwrap();
    let mut request = dispatch_request(&store, &prepared);
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    let result = store.dispatch_turn_effect(&request, &AtomicBool::new(false), |_| Ok(()), |committed, _| {
        db.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON receipts BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        let Effect::Model { waiting, .. } = committed.effect() else { panic!("model"); };
        Ok(response(waiting, 0).checkpoint())
    });
    assert!(matches!(result, Err(Error::Sql(_))));
    assert!(matches!(store.snapshot(), Err(Error::Paused)));
    assert!(matches!(
        store.dispatch_turn_effect(
            &request,
            &AtomicBool::new(false),
            |_| panic!("paused authority consulted grant"),
            |_, _| panic!("paused store executed")
        ),
        Err(Error::Paused)
    ));
    db.execute_batch("DROP TRIGGER fail_receipt").unwrap();
    drop(db);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    request.owner_epoch = store.epoch();
    assert!(matches!(
        store
            .dispatch_turn_effect(
                &request,
                &AtomicBool::new(false),
                |_| Ok(()),
                |_, _| panic!("failed ingress reran effect")
            )
            .unwrap(),
        DispatchOutcome::Reconcile
    ));
    assert!(store.receipt(&request.attempt.id).unwrap().is_none());
    assert_eq!(store.snapshot().unwrap().revision, 1);
}

#[test]
fn broker_rechecks_authority_after_reservation_and_retains_late_cancelled_output() {
    use spindle_store::broker::DispatchOutcome;
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;
    for change in 0..5 {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let prepared = start(&mut store, "broker-revalidate", empty_transcript());
        store.commit_turn_action(&prepared).unwrap();
        let request = dispatch_request(&store, &prepared);
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let approvals = Cell::new(0);
        let interrupt = AtomicBool::new(change == 0);
        let result = store.dispatch_turn_effect(
            &request,
            &interrupt,
            |_| {
                approvals.set(approvals.get() + 1);
                if approvals.get() == 2 {
                    match change {
                        1 => interrupt.store(true, Ordering::SeqCst),
                        2 => {
                            db.execute("UPDATE realm SET epoch=epoch+1 WHERE id=1", [])
                                .unwrap();
                        }
                        3 => {
                            db.execute(
                                "INSERT INTO cancellations VALUES('operator','revalidation',?1)",
                                [request.owner_epoch as i64],
                            )
                            .unwrap();
                        }
                        _ => {}
                    }
                }
                Ok(())
            },
            |committed, live_interrupt| {
                assert_eq!(change, 4, "authority change reached execution");
                live_interrupt.store(true, Ordering::SeqCst);
                db.execute(
                    "INSERT INTO cancellations VALUES('operator','late-cancel',?1)",
                    [request.owner_epoch as i64],
                )
                .unwrap();
                let Effect::Model { waiting, .. } = committed.effect() else {
                    panic!("model");
                };
                Ok(response(waiting, 0).checkpoint())
            },
        );
        if change == 2 {
            assert!(matches!(result, Err(Error::Stale)));
        } else if change == 4 {
            assert!(matches!(result.unwrap(), DispatchOutcome::Recorded(_)));
        } else {
            assert!(matches!(result, Err(Error::Cancelled)));
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM attempts", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            i64::from(change != 0)
        );
        assert_eq!(
            store.receipt(&request.attempt.id).unwrap().is_some(),
            change == 4
        );
        assert_eq!(store.snapshot().unwrap().revision, 1);
    }
}

// Its callers are the Unix kill-cut cases.
#[cfg_attr(not(unix), allow(dead_code))]
fn wait_for_kill(root: &std::path::Path) -> ! {
    std::fs::write(root.join("cut-ready"), b"ready").unwrap();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
#[ignore = "invoked by parent broker process-kill test"]
#[cfg(target_os = "linux")]
fn broker_cut_child() {
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;
    let root = PathBuf::from(std::env::var_os("SPINDLE_BROKER_REALM").unwrap());
    let cut = std::env::var("SPINDLE_BROKER_CUT").unwrap();
    #[cfg(feature = "dext-native-image")]
    if let Some(cut) = cut.strip_prefix("native-") {
        native_image_cut(&root, cut);
    }
    #[cfg(feature = "dext-native-image")]
    if let Some(cut) = cut.strip_prefix("image-") {
        image_broker_cut(&root, cut);
    }
    #[cfg(feature = "dext-strict-workspace")]
    if let Some(cut) = cut.strip_prefix("strict-") {
        strict_owner_cut(&root, cut);
    }
    let mut store = Store::open(&root).unwrap();
    let prepared = start(&mut store, "broker-kill", empty_transcript());
    store.commit_turn_action(&prepared).unwrap();
    let request = dispatch_request(&store, &prepared);
    let approvals = Cell::new(0);
    store
        .dispatch_turn_effect(
            &request,
            &AtomicBool::new(false),
            |_| {
                approvals.set(approvals.get() + 1);
                if cut == "reservation" && approvals.get() == 2 {
                    wait_for_kill(&root);
                }
                Ok(())
            },
            |committed, _| {
                let count = root.join("synthetic-executions");
                assert!(!count.exists());
                std::fs::write(count, b"1").unwrap();
                if cut == "execution" {
                    wait_for_kill(&root);
                }
                let Effect::Model { waiting, .. } = committed.effect() else {
                    panic!("model");
                };
                Ok(response(waiting, 0).checkpoint())
            },
        )
        .unwrap();
    assert_eq!(cut, "receipt");
    wait_for_kill(&root);
}

#[cfg(all(feature = "dext-native-image", target_os = "linux"))]
fn native_image_cut(root: &std::path::Path, cut: &str) -> ! {
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;
    let worker = image_worker();
    let mut store = Store::open(root).unwrap();
    let tools = prepared_read_image(&mut store);
    let request = native_image_request(&store, &tools, &worker);
    let approvals = Cell::new(0);
    store
        .dispatch_read_image(
            &request,
            &AtomicBool::new(false),
            &worker,
            |_| {
                approvals.set(approvals.get() + 1);
                if cut == "reservation" && approvals.get() == 2 {
                    wait_for_kill(root);
                }
                Ok(())
            },
            |_, _| {
                let source = image_source(root);
                if cut == "source" {
                    wait_for_kill(root);
                }
                Ok(source)
            },
        )
        .unwrap();
    assert_eq!(cut, "receipt");
    wait_for_kill(root);
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn native_image_kill_reopens_reserved_or_real_worker_receipts_without_reexecution() {
    use spindle_store::broker::{DispatchOutcome, DispatchRequest};
    use spindle_store::image::ImageReceipt;
    use std::sync::atomic::AtomicBool;
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let worker = image_worker();
    for cut in ["native-reservation", "native-source", "native-receipt"] {
        let realm = Realm::new();
        let mut child = Guard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "broker_cut_child", "--ignored"])
                .env("SPINDLE_BROKER_REALM", &realm.0)
                .env("SPINDLE_BROKER_CUT", cut)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !realm.0.join("cut-ready").is_file() {
            assert!(child.0.try_wait().unwrap().is_none());
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(child);
        let mut store = Store::open(&realm.0).unwrap();
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let (epoch, implementation): (i64, String) = db
            .query_row(
                "SELECT owner_epoch,implementation FROM attempts WHERE id='broker-attempt'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let intent = store.intents(2).unwrap().remove(0);
        let request = DispatchRequest {
            scope: "operator".into(),
            owner_epoch: store.epoch(),
            expected_revision: 2,
            intent_revision: 2,
            attempt: Attempt {
                id: "broker-attempt".into(),
                effect: intent.id.clone(),
                owner_epoch: epoch as u64,
                implementation,
                grant_reference: "fresh-host-grant".into(),
            },
            intent,
        };
        let outcome = store
            .dispatch_read_image(
                &request,
                &AtomicBool::new(false),
                &worker,
                |_| Ok(()),
                |_, _| panic!("killed native source reopened"),
            )
            .unwrap();
        let persisted = cut == "native-receipt";
        for table in ["objects", "staging_pins", "image_provenance"] {
            assert_eq!(
                db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                i64::from(persisted)
            );
        }
        if persisted {
            let DispatchOutcome::Reused(bytes) = outcome else {
                panic!("lost native image receipt")
            };
            let images = ImageReceipt::decode(&bytes).unwrap();
            let before = state(&mut store);
            let after =
                spindle_store::turn::image_receipt_transition(&before, &request.intent, &images)
                    .unwrap();
            let proposal = receipt_proposal(
                &mut store,
                "native-kill-apply",
                &request.attempt,
                &before,
                &after,
            );
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
        }
    }
}

#[cfg(all(feature = "dext-native-image", target_os = "linux"))]
fn image_broker_cut(root: &std::path::Path, cut: &str) -> ! {
    use std::sync::atomic::AtomicBool;
    let worker = image_worker();
    let mut store = Store::open(root).unwrap();
    let tools = prepared_tool(&mut store);
    let images = staged_image(&mut store, &worker);
    let bytes = images.encode().unwrap();
    let request = dispatch_request(&store, &tools);
    store
        .dispatch_turn_image_effect(
            &request,
            &AtomicBool::new(false),
            *worker.digest(),
            |_| Ok(()),
            |_, _| {
                std::fs::write(root.join("synthetic-executions"), b"1").unwrap();
                if cut == "execution" {
                    wait_for_kill(root);
                }
                Ok(bytes)
            },
        )
        .unwrap();
    assert_eq!(cut, "receipt");
    wait_for_kill(root);
}

#[cfg(feature = "dext-native-image")]
#[test]
#[cfg(target_os = "linux")]
fn image_broker_kill_recovers_provenance_without_redispatch_or_disclosure_grants() {
    use spindle_store::broker::DispatchOutcome;
    use spindle_store::image::ImageReceipt;
    use spindle_store::turn::image_receipt_transition;
    use std::sync::atomic::AtomicBool;
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let worker = image_worker();
    for cut in ["image-execution", "image-receipt"] {
        let realm = Realm::new();
        let mut child = Guard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "broker_cut_child", "--ignored"])
                .env("SPINDLE_BROKER_REALM", &realm.0)
                .env("SPINDLE_BROKER_CUT", cut)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !realm.0.join("cut-ready").is_file() {
            assert!(child.0.try_wait().unwrap().is_none());
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(child);
        let mut store = Store::open(&realm.0).unwrap();
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let attempt_epoch = db
            .query_row(
                "SELECT owner_epoch FROM attempts WHERE id='broker-attempt'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap() as u64;
        let intent = store.intents(2).unwrap().remove(0);
        let request = spindle_store::broker::DispatchRequest {
            scope: "operator".into(),
            owner_epoch: store.epoch(),
            expected_revision: 2,
            intent_revision: 2,
            attempt: Attempt {
                id: "broker-attempt".into(),
                effect: intent.id.clone(),
                owner_epoch: attempt_epoch,
                implementation: "fixture-v1".into(),
                grant_reference: "fresh-host-grant".into(),
            },
            intent,
        };
        let outcome = store
            .dispatch_turn_image_effect(
                &request,
                &AtomicBool::new(false),
                *worker.digest(),
                |_| Ok(()),
                |_, _| panic!("killed image tool reran"),
            )
            .unwrap();
        if cut == "image-receipt" {
            let DispatchOutcome::Reused(bytes) = outcome else {
                panic!("lost image receipt");
            };
            let before = state(&mut store);
            let images = ImageReceipt::decode(&bytes).unwrap();
            let after = image_receipt_transition(&before, &request.intent, &images).unwrap();
            let proposed = receipt_proposal(
                &mut store,
                "image-kill-apply",
                &request.attempt,
                &before,
                &after,
            );
            assert_eq!(
                store
                    .commit_turn_image_receipt(
                        &proposed,
                        &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
                        *worker.digest()
                    )
                    .unwrap(),
                3
            );
            assert_eq!(state(&mut store).control().phase(), Phase::AfterTools);
            assert!(matches!(
                &state(&mut store).transcript().unwrap().messages[2].content[1],
                Block::ImageArtifact { .. }
            ));
        } else {
            assert!(matches!(outcome, DispatchOutcome::Reconcile));
            assert!(store.receipt(&request.attempt.id).unwrap().is_none());
            assert_eq!(state(&mut store).control().phase(), Phase::ToolsWait);
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM image_provenance", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM attempts WHERE id='broker-attempt'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn broker_kill_after_reservation_execution_or_ingress_never_redispatches() {
    use spindle_store::broker::{DispatchOutcome, DispatchRequest};
    use std::sync::atomic::AtomicBool;
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for cut in ["reservation", "execution", "receipt"] {
        let realm = Realm::new();
        let mut child = Guard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "broker_cut_child", "--ignored"])
                .env("SPINDLE_BROKER_REALM", &realm.0)
                .env("SPINDLE_BROKER_CUT", cut)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !realm.0.join("cut-ready").is_file() {
            assert!(child.0.try_wait().unwrap().is_none());
            assert!(
                std::time::Instant::now() < deadline,
                "broker child missed {cut}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(child);
        let mut store = Store::open(&realm.0).unwrap();
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let epoch = db
            .query_row(
                "SELECT owner_epoch FROM attempts WHERE id='broker-attempt'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap() as u64;
        let intent = store.intents(1).unwrap().remove(0);
        let request = DispatchRequest {
            scope: "operator".into(),
            owner_epoch: store.epoch(),
            expected_revision: 1,
            intent_revision: 1,
            attempt: Attempt {
                id: "broker-attempt".into(),
                effect: intent.id.clone(),
                owner_epoch: epoch,
                implementation: "fixture-v1".into(),
                grant_reference: "fresh-host-grant".into(),
            },
            intent,
        };
        let result = store
            .dispatch_turn_effect(
                &request,
                &AtomicBool::new(false),
                |_| Ok(()),
                |_, _| panic!("killed effect reran"),
            )
            .unwrap();
        if cut == "receipt" {
            let DispatchOutcome::Reused(bytes) = result else {
                panic!("missing durable receipt");
            };
            let before = state(&mut store);
            let after = before
                .apply_model(&ModelWait::restore(&bytes).unwrap(), "route-v1")
                .unwrap();
            let proposed = receipt_proposal(
                &mut store,
                "apply-after-kill",
                &request.attempt,
                &before,
                &after,
            );
            store
                .commit_turn_receipt(
                    &proposed,
                    &receipt_binding(&request.attempt, 1, &request.intent, &bytes),
                )
                .unwrap();
            assert_eq!(store.snapshot().unwrap().revision, 2);
        } else {
            assert!(matches!(result, DispatchOutcome::Reconcile));
            assert_eq!(store.snapshot().unwrap().revision, 1);
            assert!(store.receipt(&request.attempt.id).unwrap().is_none());
        }
        assert_eq!(
            realm.0.join("synthetic-executions").exists(),
            cut != "reservation"
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM attempts", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}

#[cfg(feature = "dext-strict-workspace")]
#[cfg(target_os = "linux")]
fn strict_owner_cut(root: &std::path::Path, cut: &str) -> ! {
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;
    let executor = strict_executor();
    let mut store = Store::open(root).unwrap();
    let command = if cut == "running" {
        let marker = std::env::var("SPINDLE_STRICT_MARKER").unwrap();
        format!("printf x > {marker}; while :; do date +%s%N > tick; sleep 0.05; done")
    } else {
        "printf new > input.txt".into()
    };
    let (tools, _) = prepared_workspace_tool(&mut store, &command);
    let request = strict_dispatch_request(&store, &tools, &executor);
    let approvals = Cell::new(0);
    let result =
        store.dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| {
            approvals.set(approvals.get() + 1);
            if cut == "reservation" && approvals.get() == 2 {
                wait_for_kill(root);
            }
            Ok(())
        });
    assert_eq!(cut, "sealed", "{result:?}");
    result.unwrap();
    wait_for_kill(root);
}

#[cfg(feature = "dext-strict-workspace")]
#[cfg(target_os = "linux")]
fn strict_candidates() -> Vec<PathBuf> {
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

/// Owner SIGKILL after reservation, while the sandbox runs, and after the sealed
/// receipt commits: a reopened owner reconciles or reuses, never re-executes, and
/// only a sealed receipt can publish its root.
#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn strict_owner_kill_at_reservation_execution_or_seal_never_redispatches_or_publishes() {
    use spindle_store::broker::{DispatchOutcome, DispatchRequest};
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let executor = strict_executor();
    let helper: String = executor
        .helper_digest()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    for cut in ["reservation", "running", "sealed"] {
        let realm = Realm::new();
        let marker = format!(
            "owner-cut-{}",
            realm.0.file_name().unwrap().to_str().unwrap()
        );
        let mut child = Guard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "broker_cut_child", "--ignored"])
                .env("SPINDLE_BROKER_REALM", &realm.0)
                .env("SPINDLE_BROKER_CUT", format!("strict-{cut}"))
                .env("SPINDLE_STRICT_MARKER", &marker)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        let candidate = loop {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "strict child exited before {cut}"
            );
            assert!(Instant::now() < deadline, "strict child missed {cut}");
            if cut == "running" {
                if let Some(path) = strict_candidates().into_iter().find(|path| {
                    path.join("w").join(&marker).is_file() && path.join("w/tick").is_file()
                }) {
                    break Some(path);
                }
            } else if realm.0.join("cut-ready").is_file() {
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(child);
        if let Some(candidate) = &candidate {
            std::thread::sleep(Duration::from_millis(300));
            let first = std::fs::read(candidate.join("w/tick")).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            let second = std::fs::read(candidate.join("w/tick")).unwrap();
            assert_eq!(first, second, "sandbox kept writing after owner death");
            dext_native::strict::sweep_abandoned_candidates().unwrap();
            assert!(!candidate.exists(), "abandoned candidate not reclaimed");
        }
        let mut store = Store::open(&realm.0).unwrap();
        let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
        let attempt_epoch = db
            .query_row(
                "SELECT owner_epoch FROM attempts WHERE id='broker-attempt'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap() as u64;
        let head = store.snapshot().unwrap();
        assert_eq!(head.revision, 2, "{cut}");
        let base = head.workspace.unwrap();
        let intent = store.intents(2).unwrap().remove(0);
        let mut request = DispatchRequest {
            scope: "operator".into(),
            owner_epoch: store.epoch(),
            expected_revision: 2,
            intent_revision: 2,
            attempt: Attempt {
                id: "broker-attempt".into(),
                effect: intent.id.clone(),
                owner_epoch: attempt_epoch,
                implementation: format!("dext-strict-bash-v1-{helper}"),
                grant_reference: "fresh-host-grant".into(),
            },
            intent,
        };
        let outcome = store
            .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
            .unwrap();
        let strict_rows = db
            .query_row("SELECT count(*) FROM strict_receipts", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        let unresolved = store.reconciliation().unwrap().unresolved_attempts;
        if cut == "sealed" {
            let DispatchOutcome::Reused(bytes) = outcome else {
                panic!("sealed receipt lost: {outcome:?}");
            };
            assert_eq!(strict_rows, 1);
            assert!(unresolved.is_empty(), "{unresolved:?}");
            let root = store.workspace_receipt("broker-attempt").unwrap().unwrap();
            assert_eq!(
                store
                    .read_versioned_file("operator", &base, "input.txt")
                    .unwrap(),
                b"old"
            );
            assert_eq!(
                store
                    .read_versioned_file("operator", &root, "input.txt")
                    .unwrap(),
                b"new"
            );
            let before = state(&mut store);
            let block = serde_json::from_slice(&bytes).unwrap();
            let after = before.apply_tool(&request.intent.id, vec![block]).unwrap();
            let mut apply = receipt_proposal(
                &mut store,
                "apply-after-kill",
                &request.attempt,
                &before,
                &after,
            );
            apply.workspace = Some(root.clone());
            assert_eq!(
                store
                    .commit_turn_workspace_receipt(
                        &apply,
                        &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
                        *executor.helper_digest()
                    )
                    .unwrap(),
                3
            );
            assert_eq!(store.snapshot().unwrap().workspace, Some(root));
        } else {
            assert!(
                matches!(outcome, DispatchOutcome::Reconcile),
                "{cut}: {outcome:?}"
            );
            assert_eq!(strict_rows, 0);
            assert!(store.receipt("broker-attempt").unwrap().is_none());
            assert_eq!(unresolved, vec!["broker-attempt".to_string()]);
            assert_eq!(store.snapshot().unwrap().workspace, Some(base.clone()));
            assert_eq!(
                store
                    .read_versioned_file("operator", &base, "input.txt")
                    .unwrap(),
                b"old"
            );
            request.attempt.id = "replacement".into();
            request.attempt.owner_epoch = store.epoch();
            assert!(
                store
                    .dispatch_strict_workspace(
                        &request,
                        &AtomicBool::new(false),
                        &executor,
                        |_| Ok(())
                    )
                    .is_err(),
                "{cut}: replacement attempt dispatched"
            );
        }
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM attempts WHERE effect=?1",
                [&request.attempt.effect],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
}

#[test]
fn durable_model_tool_model_review_finish_and_next_turn_keep_identity_and_history() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = start(&mut store, "start", empty_transcript());
    assert_eq!(store.commit_turn_action(&prepared).unwrap(), 1);
    let intent = prepared.effects[0].clone();
    let Effect::Model { waiting, .. } = state(&mut store).pending_effects().unwrap().remove(0)
    else {
        panic!("model");
    };
    let model_attempt = attempt(&mut store, &intent.id, "model-attempt-1");
    let recorded = response(&waiting, 2);
    let receipt = recorded.checkpoint();
    store.ingest_receipt(&model_attempt.id, &receipt).unwrap();
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    let baseline = state(&mut store);
    let after = baseline.apply_model(&recorded, "route-v1").unwrap();
    let proposed = receipt_proposal(&mut store, "apply-model", &model_attempt, &baseline, &after);
    let binding = receipt_binding(&model_attempt, 1, &intent, &receipt);
    assert_eq!(store.commit_turn_receipt(&proposed, &binding).unwrap(), 2);
    assert_eq!(proposed.effects.len(), 2);
    assert_ne!(proposed.effects[0].id, "call-0");
    let tools = proposed.effects.clone();
    for position in [1, 0] {
        let baseline = state(&mut store);
        let Effect::Tool { id, call } = baseline
            .pending_effects()
            .unwrap()
            .into_iter()
            .find(|effect| matches!(effect, Effect::Tool { id, .. } if id == &tools[position].id))
            .unwrap()
        else {
            panic!("tool");
        };
        let tool_attempt = attempt(&mut store, &id, &format!("tool-attempt-{position}"));
        let block = Block::ToolResult {
            tool_use_id: call.id,
            content: format!("result-{position}"),
            is_error: Some(false),
            metadata: Default::default(),
        };
        let receipt = serde_json::to_vec(&block).unwrap();
        store.ingest_receipt(&tool_attempt.id, &receipt).unwrap();
        let after = baseline.apply_tool(&id, vec![block]).unwrap();
        let proposed = receipt_proposal(
            &mut store,
            &format!("apply-tool-{position}"),
            &tool_attempt,
            &baseline,
            &after,
        );
        assert!(
            proposed.effects.is_empty(),
            "uncompleted old tools are not new intents"
        );
        store
            .commit_turn_receipt(
                &proposed,
                &receipt_binding(&tool_attempt, 2, &tools[position], &receipt),
            )
            .unwrap();
        drop(store);
        store = Store::open(&realm.0).unwrap();
        assert_eq!(
            store
                .commit_turn_receipt(
                    &proposed,
                    &receipt_binding(&tool_attempt, 2, &tools[position], &receipt)
                )
                .unwrap(),
            proposed.expected_revision + 1
        );
    }
    let baseline = state(&mut store);
    assert_eq!(baseline.control().phase(), Phase::AfterTools);
    let transcript = baseline.transcript().unwrap();
    assert!(
        matches!(&transcript.messages[2].content[0], Block::ToolResult { content, .. } if content == "result-0")
    );
    let request = request(&baseline);
    let after = baseline.prepare(request.clone(), "route-v1").unwrap();
    let proposed = action_proposal(
        &mut store,
        "prepare-2",
        &Action::Prepare {
            request: ModelWait::new(request).checkpoint(),
        },
        &baseline,
        &after,
    );
    store.commit_turn_action(&proposed).unwrap();
    let intent = proposed.effects[0].clone();
    let model_attempt = attempt(&mut store, &intent.id, "model-attempt-2");
    let Effect::Model { waiting, .. } = after.pending_effects().unwrap().remove(0) else {
        panic!("model");
    };
    let recorded = response(&waiting, 0);
    let receipt = recorded.checkpoint();
    store.ingest_receipt(&model_attempt.id, &receipt).unwrap();
    let baseline = state(&mut store);
    let review = baseline.apply_model(&recorded, "route-v1").unwrap();
    let proposed = receipt_proposal(&mut store, "review", &model_attempt, &baseline, &review);
    store
        .commit_turn_receipt(
            &proposed,
            &receipt_binding(&model_attempt, 5, &intent, &receipt),
        )
        .unwrap();
    assert_eq!(state(&mut store).control().phase(), Phase::FinalReview);
    let done = review.finish().unwrap();
    let proposed = action_proposal(&mut store, "finish", &Action::Finish, &review, &done);
    store.commit_turn_action(&proposed).unwrap();
    let mut transcript = done.transcript().unwrap();
    transcript.messages.push(Message {
        role: "user".into(),
        content: vec![Block::Text {
            text: "next prompt".into(),
        }],
    });
    let next = start(&mut store, "next-turn", transcript);
    assert_ne!(next.effects[0].id, intent.id);
    assert_eq!(store.commit_turn_action(&next).unwrap(), 8);
}

#[test]
fn checked_turn_transitions_reject_unrelated_state_invented_intents_and_retry_after_rollback() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = start(&mut store, "start", empty_transcript());
    let mut wrong = prepared.clone();
    wrong.continuation[0] ^= 1;
    assert!(store.commit_turn_action(&wrong).is_err());
    let mut wrong = prepared.clone();
    wrong.effects[0].input.push(0);
    assert!(store.commit_turn_action(&wrong).is_err());
    assert_eq!(store.snapshot().unwrap().revision, 0);
    store.commit_turn_action(&prepared).unwrap();
    let intent = prepared.effects[0].clone();
    let baseline = state(&mut store);
    let Effect::Model { waiting, .. } = baseline.pending_effects().unwrap().remove(0) else {
        panic!("model");
    };
    let model_attempt = attempt(&mut store, &intent.id, "attempt");
    let recorded = response(&waiting, 1);
    let receipt = recorded.checkpoint();
    store.ingest_receipt(&model_attempt.id, &receipt).unwrap();
    let after = baseline.apply_model(&recorded, "route-v1").unwrap();
    let mut proposed = receipt_proposal(&mut store, "apply", &model_attempt, &baseline, &after);
    let binding = receipt_binding(&model_attempt, 1, &intent, &receipt);
    let mut wrong = proposed.clone();
    wrong.continuation = baseline.checkpoint().unwrap();
    assert!(matches!(
        store.commit_turn_receipt(&wrong, &binding),
        Err(Error::Conflict)
    ));
    let mut wrong = proposed.clone();
    wrong.effects.clear();
    assert!(matches!(
        store.commit_turn_receipt(&wrong, &binding),
        Err(Error::Conflict)
    ));
    let db = rusqlite::Connection::open(realm.0.join("authority.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_turn BEFORE UPDATE ON head BEGIN SELECT RAISE(ABORT,'injected'); END;",
    )
    .unwrap();
    assert!(matches!(
        store.commit_turn_receipt(&proposed, &binding),
        Err(Error::Sql(_))
    ));
    db.execute_batch("DROP TRIGGER fail_turn").unwrap();
    drop(db);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(
        state(&mut store).checkpoint().unwrap(),
        baseline.checkpoint().unwrap()
    );
    assert_eq!(store.receipt(&model_attempt.id).unwrap().unwrap(), receipt);
    assert!(store.intents(2).unwrap().is_empty());
    assert!(matches!(
        store.commit_turn_receipt(&proposed, &binding),
        Err(Error::Stale)
    ));
    proposed.owner_epoch = store.epoch();
    assert_eq!(store.commit_turn_receipt(&proposed, &binding).unwrap(), 2);
    assert_eq!(store.commit_turn_receipt(&proposed, &binding).unwrap(), 2);
}

#[test]
fn action_codec_is_exact_bounded_redacted_and_execution_namespace_is_scope_bound() {
    for action in [
        Action::Finish,
        Action::Followup {
            text: "sensitive prompt".into(),
        },
        Action::Prepare {
            request: vec![1, 2, 3],
        },
        Action::Start {
            initial: vec![4],
            request: vec![5],
        },
    ] {
        let bytes = action.encode().unwrap();
        assert_eq!(Action::decode(&bytes).unwrap(), action);
        for end in 0..bytes.len() {
            assert!(Action::decode(&bytes[..end]).is_err());
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(Action::decode(&extra).is_err());
        assert!(!format!("{action:?}").contains("sensitive"));
    }
    assert!(
        Action::Followup {
            text: "x".repeat(spindle_store::MAX_PAYLOAD)
        }
        .encode()
        .is_err()
    );
    assert_ne!(
        execution_id("operator", "start").unwrap(),
        execution_id("other", "start").unwrap()
    );
    assert_ne!(
        execution_id("operator", "start").unwrap(),
        execution_id("operator", "start-2").unwrap()
    );
    assert_eq!(
        TurnContinuation::restore(&initial_bytes())
            .unwrap()
            .control()
            .phase(),
        Phase::Ready
    );
}
fn initial_bytes() -> Vec<u8> {
    TurnContinuation::new(
        "operator".into(),
        execution_id("operator", "codec").unwrap(),
        Contract::OpenAiResponses,
        Some(1),
        empty_transcript(),
    )
    .unwrap()
    .checkpoint()
    .unwrap()
}

#[test]
fn cancelled_scope_retains_late_model_receipt_but_blocks_turn_adoption() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = start(&mut store, "start", empty_transcript());
    store.commit_turn_action(&prepared).unwrap();
    let intent = prepared.effects[0].clone();
    let baseline = state(&mut store);
    let Effect::Model { waiting, .. } = baseline.pending_effects().unwrap().remove(0) else {
        panic!("model");
    };
    let model_attempt = attempt(&mut store, &intent.id, "attempt");
    store.cancel_scope("operator", "cancel").unwrap();
    let recorded = response(&waiting, 0);
    let receipt = recorded.checkpoint();
    store.ingest_receipt(&model_attempt.id, &receipt).unwrap();
    let after = baseline.apply_model(&recorded, "route-v1").unwrap();
    let proposed = receipt_proposal(&mut store, "apply", &model_attempt, &baseline, &after);
    assert!(matches!(
        store.commit_turn_receipt(
            &proposed,
            &receipt_binding(&model_attempt, 1, &intent, &receipt)
        ),
        Err(Error::Cancelled)
    ));
    assert_eq!(store.snapshot().unwrap().revision, 1);
    assert_eq!(store.receipt(&model_attempt.id).unwrap().unwrap(), receipt);
    assert_eq!(state(&mut store).control().phase(), Phase::ModelWait);
    assert_ne!(
        state(&mut store).control().phase(),
        Phase::Stopped(Stop::Complete)
    );
}

fn guard_setup(realm: &Realm) -> (Store, spindle_store::transition::EffectIntent, u64) {
    fn model_round(
        store: &mut Store,
        label: &str,
        intent: &spindle_store::transition::EffectIntent,
        revision: u64,
        id: &str,
    ) -> spindle_store::transition::EffectIntent {
        let before = state(store);
        let Effect::Model { waiting, .. } = before.pending_effects().unwrap().remove(0) else {
            panic!("model");
        };
        let recorded = response_blocks(
            &waiting,
            vec![Block::ToolUse {
                id: id.into(),
                name: "http".into(),
                input: serde_json::json!({"args": ["GET", "https://api.example/item?id=1"]}),
            }],
        );
        let model = attempt(store, &intent.id, &format!("{label}-model"));
        store
            .ingest_receipt(&model.id, &recorded.checkpoint())
            .unwrap();
        let after = before.apply_model(&recorded, "route-v1").unwrap();
        let proposal = receipt_proposal(store, &format!("{label}-apply"), &model, &before, &after);
        let tool = proposal.effects[0].clone();
        store
            .commit_turn_receipt(
                &proposal,
                &receipt_binding(&model, revision, intent, &recorded.checkpoint()),
            )
            .unwrap();
        tool
    }
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = start(&mut store, "guard", empty_transcript());
    let revision = store.commit_turn_action(&prepared).unwrap();
    let first = model_round(
        &mut store,
        "round-1",
        &prepared.effects[0],
        revision,
        "call-0",
    );
    let tool_revision = store.snapshot().unwrap().revision;
    let before = state(&mut store);
    assert!(before.guard_tool(&first.id).unwrap().is_none());
    let block = Block::ToolResult {
        tool_use_id: "call-0".into(),
        content: "item payload".into(),
        is_error: Some(false),
        metadata: Default::default(),
    };
    let receipt = serde_json::to_vec(&block).unwrap();
    let executed = attempt(&mut store, &first.id, "round-1-tool");
    store.ingest_receipt(&executed.id, &receipt).unwrap();
    let after = before.apply_tool(&first.id, vec![block]).unwrap();
    let applied = receipt_proposal(&mut store, "round-1-tool-apply", &executed, &before, &after);
    store
        .commit_turn_receipt(
            &applied,
            &receipt_binding(&executed, tool_revision, &first, &receipt),
        )
        .unwrap();
    let next = request_for_after_tools(&mut store, "round-2");
    let revision = store.snapshot().unwrap().revision;
    let repeat = model_round(&mut store, "round-2", &next.effects[0], revision, "call-1");
    let revision = store.snapshot().unwrap().revision;
    (store, repeat, revision)
}

#[test]
fn repeat_of_an_earlier_round_is_never_dispatched_and_commits_the_cli_guard_verdict() {
    use spindle_store::broker::DispatchRequest;
    use std::sync::atomic::AtomicBool;
    let realm = Realm::new();
    let (store, repeat, revision) = guard_setup(&realm);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    let request = DispatchRequest {
        scope: "operator".into(),
        owner_epoch: store.epoch(),
        expected_revision: revision,
        intent_revision: revision,
        attempt: Attempt {
            id: "round-2-tool".into(),
            effect: repeat.id.clone(),
            owner_epoch: store.epoch(),
            implementation: "fixture-v1".into(),
            grant_reference: "fresh-host-grant".into(),
        },
        intent: repeat.clone(),
    };
    assert!(matches!(
        store.dispatch_turn_effect(
            &request,
            &AtomicBool::new(false),
            |_| Ok(()),
            |_, _| panic!("guarded repeat dispatched")
        ),
        Err(Error::Conflict)
    ));
    let before = state(&mut store);
    let guarded = before.guard_tool(&repeat.id).unwrap().unwrap();
    let action = Action::Guard {
        effect: repeat.id.clone(),
    };
    assert!(Action::decode(&action.encode().unwrap()).unwrap() == action);
    let proposal = action_proposal(&mut store, "guard-commit", &action, &before, &guarded);
    store.commit_turn_action(&proposal).unwrap();
    let head = state(&mut store);
    assert_eq!(head.control().phase(), Phase::AfterTools);
    let last = head.transcript().unwrap().messages.pop().unwrap();
    assert!(last.content.iter().any(|block| matches!(
        block,
        Block::ToolResult { tool_use_id, content, .. }
            if tool_use_id == "call-1" && content.contains("request dedupe cache hit")
    )));
    let stale = action_proposal(&mut store, "guard-again", &action, &before, &guarded);
    assert!(store.commit_turn_action(&stale).is_err());
}

#[test]
fn guard_verdict_never_replaces_a_reserved_call() {
    let realm = Realm::new();
    let (mut store, repeat, _) = guard_setup(&realm);
    // A raw reservation may already have produced a result, so a verdict must
    // not overwrite it.
    attempt(&mut store, &repeat.id, "raw-reservation");
    let before = state(&mut store);
    let guarded = before.guard_tool(&repeat.id).unwrap().unwrap();
    let action = Action::Guard {
        effect: repeat.id.clone(),
    };
    let proposal = action_proposal(&mut store, "guard-reserved", &action, &before, &guarded);
    assert!(matches!(
        store.commit_turn_action(&proposal),
        Err(Error::Conflict)
    ));
}

#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn rewind_targets_are_settled_or_before_model_and_refusals_change_nothing() {
    use spindle_store::rewind::{RewindTarget, RewindTargetKind};
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let (target, current) = rewind_history(&mut store);
    let head = store.snapshot().unwrap();
    let epoch = store.epoch();
    let listed = store.rewind_targets("operator").unwrap();
    let mut expected = vec![];
    let mut refused = 0;
    for revision in (1..=current).rev() {
        let selected = store.snapshot_at("operator", revision).unwrap();
        let state = TurnContinuation::restore(&selected.continuation).unwrap();
        let idle = state.pending_effects().unwrap().is_empty();
        let kind = match state.control().phase() {
            Phase::Stopped(Stop::Complete) if idle => Some(RewindTargetKind::Settled),
            Phase::Ready | Phase::AfterTools if idle => Some(RewindTargetKind::BeforeModel),
            _ => None,
        };
        if let Some(kind) = kind {
            expected.push(RewindTarget { revision, kind });
            continue;
        }
        refused += 1;
        assert!(
            store
                .begin_rewind(
                    "operator",
                    &format!("active-{revision}"),
                    revision,
                    current,
                    0
                )
                .is_err(),
            "in-flight cut {revision} was accepted"
        );
        assert_eq!(store.epoch(), epoch, "refusal advanced the epoch");
        assert_eq!(store.snapshot().unwrap(), head, "refusal changed the head");
    }
    assert_eq!(listed, expected);
    assert!(refused >= 6, "fixture must include in-flight cuts");
    assert!(expected.contains(&RewindTarget {
        revision: target,
        kind: RewindTargetKind::Settled
    }));
    let before_model: Vec<u64> = expected
        .iter()
        .filter(|target| target.kind == RewindTargetKind::BeforeModel)
        .map(|target| target.revision)
        .collect();
    assert_eq!(
        before_model.len(),
        2,
        "one idle point after each turn's tools"
    );
    assert!(
        store
            .begin_rewind("operator", "before-model", before_model[0], current, 0)
            .is_ok()
    );
}

#[cfg(feature = "dext-strict-workspace")]
fn answer_model(
    store: &mut Store,
    attempt_id: &str,
    intent: &spindle_store::transition::EffectIntent,
    blocks: Vec<Block>,
    label: &str,
) -> Proposal {
    let revision = store.snapshot().unwrap().revision;
    let before = state(store);
    let Effect::Model { waiting, .. } = before.pending_effects().unwrap().remove(0) else {
        panic!("model")
    };
    let model = attempt(store, &intent.id, attempt_id);
    let recorded = response_blocks(&waiting, blocks);
    let bytes = recorded.checkpoint();
    store.ingest_receipt(&model.id, &bytes).unwrap();
    let after = before.apply_model(&recorded, "route-v1").unwrap();
    let proposal = receipt_proposal(store, label, &model, &before, &after);
    store
        .commit_turn_receipt(
            &proposal,
            &receipt_binding(&model, revision, intent, &bytes),
        )
        .unwrap();
    proposal
}

#[cfg(all(target_os = "linux", feature = "dext-strict-workspace"))]
fn next_turn(store: &mut Store, lineage: u64, id: &str, prompt: &str) -> Proposal {
    let mut history = state(store).transcript().unwrap();
    history.messages.push(Message {
        role: "user".into(),
        content: vec![Block::Text {
            text: prompt.into(),
        }],
    });
    let prepared = start_in(store, lineage, id, history);
    store.commit_turn_action(&prepared).unwrap();
    prepared
}

/// The middle-option rewind: the head may be mid-turn with a model call in flight,
/// the target is the idle point before a model request, the re-run is a fresh
/// effect, and nothing from the abandoned future can be reserved or adopted.
#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn before_model_rewind_reruns_under_a_fresh_identity_and_never_revives_the_abandoned_future() {
    use spindle_store::broker::{DispatchOutcome, DispatchRequest};
    use spindle_store::rewind::{RewindTarget, RewindTargetKind};
    use std::sync::atomic::AtomicBool;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let (_, completed) = rewind_history(&mut store);
    let third = next_turn(&mut store, 0, "third-turn", "third prompt");
    let third_revision = completed + 1;
    let Effect::Model { waiting: lost, .. } =
        state(&mut store).pending_effects().unwrap().remove(0)
    else {
        panic!("model")
    };
    let in_flight = attempt(&mut store, &third.effects[0].id, "third-model");
    let head = store.snapshot().unwrap().revision;
    assert_eq!(head, third_revision);

    let targets = store.rewind_targets("operator").unwrap();
    let target = targets
        .iter()
        .find(|target| target.kind == RewindTargetKind::BeforeModel)
        .unwrap()
        .revision;
    assert_eq!(
        targets[0],
        RewindTarget {
            revision: completed,
            kind: RewindTargetKind::Settled
        }
    );
    let selected = store.snapshot_at("operator", target).unwrap();
    let target_state = TurnContinuation::restore(&selected.continuation).unwrap();
    assert_eq!(target_state.control().phase(), Phase::AfterTools);

    store
        .begin_rewind("operator", "redo-answer", target, head, 0)
        .unwrap();
    assert_eq!(
        store
            .finish_rewind("operator", "redo-answer")
            .unwrap()
            .revision,
        Some(head + 1)
    );
    store.resume_rewind("operator", "redo-answer").unwrap();
    let rewound = state(&mut store);
    assert_eq!(rewound.control().phase(), Phase::AfterTools);
    assert_eq!(
        rewound.transcript().unwrap(),
        target_state.transcript().unwrap()
    );
    assert_ne!(rewound.execution(), target_state.execution());
    let root = selected.workspace.clone().unwrap();
    assert_eq!(store.snapshot().unwrap().workspace, Some(root.clone()));
    assert_eq!(
        store
            .read_versioned_file("operator", &root, "input.txt")
            .unwrap(),
        b"second"
    );

    let report = store.reconciliation().unwrap();
    assert_eq!(report.abandoned_attempts, ["third-model"]);
    assert!(report.unresolved_attempts.is_empty());
    // The rewind advanced the owner epoch: a retry under the old epoch is stale, and
    // re-reserving the abandoned attempt under the new owner conflicts.
    assert!(matches!(
        store.reserve_attempt(&in_flight),
        Err(Error::Stale)
    ));
    let revived = Attempt {
        owner_epoch: store.epoch(),
        ..in_flight.clone()
    };
    assert!(
        matches!(store.reserve_attempt(&revived), Err(Error::Conflict)),
        "the abandoned attempt was re-reserved under the new owner"
    );
    let lost_request = DispatchRequest {
        scope: "operator".into(),
        owner_epoch: store.epoch(),
        expected_revision: head + 1,
        intent_revision: third_revision,
        attempt: in_flight.clone(),
        intent: third.effects[0].clone(),
    };
    assert!(matches!(
        store
            .dispatch_turn_effect(
                &lost_request,
                &AtomicBool::new(false),
                |_| Ok(()),
                |_, _| panic!("abandoned model call was sent again")
            )
            .unwrap(),
        DispatchOutcome::Reconcile
    ));

    let rerun_before = state(&mut store);
    let prepared = request(&rerun_before);
    assert!(
        prepared
            .identity()
            .effect
            .starts_with(&format!("{}/", rewound.execution()))
    );
    let rerun_after = rerun_before.prepare(prepared.clone(), "route-v1").unwrap();
    let rerun = action_proposal_in(
        &mut store,
        1,
        "rerun-prepare",
        &Action::Prepare {
            request: ModelWait::new(prepared).checkpoint(),
        },
        &rerun_before,
        &rerun_after,
    );
    store.commit_turn_action(&rerun).unwrap();
    answer_model(
        &mut store,
        "rerun-model",
        &rerun.effects[0],
        vec![Block::Text {
            text: "better answer".into(),
        }],
        "rerun-review",
    );
    let review = state(&mut store);
    let done = review.finish().unwrap();
    let finish = action_proposal_in(
        &mut store,
        1,
        "rerun-finish",
        &Action::Finish,
        &review,
        &done,
    );
    let settled = store.commit_turn_action(&finish).unwrap();
    let history = state(&mut store).transcript().unwrap();
    assert!(
        !history
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, Block::Text { text } if text == "third prompt")),
        "abandoned prompt leaked into the new branch"
    );

    let late = response(&lost, 0).checkpoint();
    store.ingest_receipt("third-model", &late).unwrap();
    let current = state(&mut store);
    let adopt = Proposal {
        id: "adopt-late".into(),
        scope: "operator".into(),
        observation: Observation::Receipt("third-model".into()),
        expected_revision: settled,
        owner_epoch: store.epoch(),
        engine: ENGINE.into(),
        continuation: current.checkpoint().unwrap(),
        workspace: None,
        effects: vec![],
        state: None,
    };
    assert!(
        store
            .commit_turn_receipt(
                &adopt,
                &receipt_binding(&in_flight, third_revision, &third.effects[0], &late)
            )
            .is_err(),
        "late result from the abandoned future was adopted"
    );
    assert_eq!(store.snapshot().unwrap().revision, settled);

    let fourth = next_turn(&mut store, 1, "fourth-turn", "fourth prompt");
    let tools = answer_model(
        &mut store,
        "fourth-model",
        &fourth.effects[0],
        vec![Block::ToolUse {
            id: "fourth-call".into(),
            name: "bash".into(),
            input: serde_json::json!({"command":"printf fourth > input.txt","timeout":5}),
        }],
        "fourth-tools",
    );
    let head = store.snapshot().unwrap().revision;
    assert_eq!(state(&mut store).control().phase(), Phase::ToolsWait);
    assert!(
        store
            .rewind_targets("operator")
            .unwrap()
            .contains(&RewindTarget {
                revision: settled,
                kind: RewindTargetKind::Settled
            })
    );
    store
        .begin_rewind("operator", "drop-fourth", settled, head, 1)
        .unwrap();
    store.finish_rewind("operator", "drop-fourth").unwrap();
    store.resume_rewind("operator", "drop-fourth").unwrap();
    let undispatched = Attempt {
        id: "fourth-bash".into(),
        effect: tools.effects[0].id.clone(),
        owner_epoch: store.epoch(),
        implementation: "fixture-v1".into(),
        grant_reference: "test-only".into(),
    };
    assert!(matches!(
        store.reserve_attempt(&undispatched),
        Err(Error::Conflict)
    ));
    let executor = strict_executor();
    let mut request = strict_dispatch_request(&store, &tools, &executor);
    request.attempt.id = "fourth-bash".into();
    assert!(
        store
            .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
            .is_err(),
        "superseded bash call dispatched"
    );
    assert_eq!(
        store
            .read_versioned_file("operator", &root, "input.txt")
            .unwrap(),
        b"second"
    );
    let epoch = store.epoch();
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.epoch(), epoch + 1);
    assert_eq!(
        store.reconciliation().unwrap().abandoned_attempts,
        Vec::<String>::new(),
        "the late receipt resolved the only abandoned attempt"
    );
}

/// A native effect of unknown outcome at the head blocks every rewind: unlike a
/// model call, re-running it could repeat an external action. Undispatched tool
/// intents do not block; the finalized rewind supersedes them.
#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn native_attempt_without_receipt_at_the_head_blocks_rewind() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let (target, _) = rewind_history(&mut store);
    let third = next_turn(&mut store, 0, "third-turn", "third prompt");
    let tools = answer_model(
        &mut store,
        "third-model",
        &third.effects[0],
        vec![Block::ToolUse {
            id: "third-call".into(),
            name: "bash".into(),
            input: serde_json::json!({"command":"printf third > input.txt","timeout":5}),
        }],
        "third-tools",
    );
    let head = store.snapshot().unwrap().revision;
    assert!(!store.rewind_targets("operator").unwrap().is_empty());
    attempt(&mut store, &tools.effects[0].id, "third-bash");
    let epoch = store.epoch();
    assert!(matches!(
        store.rewind_targets("operator"),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        store.begin_rewind("operator", "blocked", target, head, 0),
        Err(Error::Conflict)
    ));
    assert_eq!(store.epoch(), epoch);
    assert_eq!(store.snapshot().unwrap().revision, head);
}

#[test]
fn managed_steering_commits_cli_update_text_and_survives_reopen() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = start(&mut store, "steer", empty_transcript());
    store.commit_turn_action(&prepared).unwrap();
    let intent = prepared.effects[0].clone();
    let Effect::Model { waiting, .. } = state(&mut store).pending_effects().unwrap().remove(0)
    else {
        panic!("model");
    };
    let recorded = response(&waiting, 0);
    let model = attempt(&mut store, &intent.id, "steer-model");
    store
        .ingest_receipt(&model.id, &recorded.checkpoint())
        .unwrap();
    let before = state(&mut store);
    let review = before.apply_model(&recorded, "route-v1").unwrap();
    let applied = receipt_proposal(&mut store, "steer-apply", &model, &before, &review);
    store
        .commit_turn_receipt(
            &applied,
            &receipt_binding(&model, 1, &intent, &recorded.checkpoint()),
        )
        .unwrap();
    let before = state(&mut store);
    assert_eq!(before.control().phase(), Phase::FinalReview);

    let action = Action::Steer {
        text: "check the second file too".into(),
    };
    assert_eq!(Action::decode(&action.encode().unwrap()).unwrap(), action);
    let empty = Action::Steer { text: " ".into() };
    let refused = action_proposal(&mut store, "steer-empty", &empty, &before, &before);
    assert!(store.commit_turn_action(&refused).is_err());
    let steered = before.steer("check the second file too".into()).unwrap();
    let mismatched = before.steer("different text".into()).unwrap();
    let forged = action_proposal(&mut store, "steer-forged", &action, &before, &mismatched);
    assert!(
        store.commit_turn_action(&forged).is_err(),
        "store recomputes the text"
    );
    let proposal = action_proposal(&mut store, "steer-commit", &action, &before, &steered);
    store.commit_turn_action(&proposal).unwrap();
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    let head = state(&mut store);
    assert_eq!(head.control().phase(), Phase::Ready);
    assert_eq!(head.checkpoint().unwrap(), steered.checkpoint().unwrap());
    let last = head.transcript().unwrap().messages.pop().unwrap();
    assert!(matches!(
        &last.content[..],
        [Block::Text { text }] if text.starts_with("[queued-user-update]")
            && text.ends_with("User update:\ncheck the second file too")
    ));
}

#[test]
fn managed_budget_stop_and_compaction_commit_through_admission() {
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let prepared = start(&mut store, "budget", empty_transcript());
    store.commit_turn_action(&prepared).unwrap();
    let intent = prepared.effects[0].clone();
    let Effect::Model { waiting, .. } = state(&mut store).pending_effects().unwrap().remove(0)
    else {
        panic!("model");
    };
    let recorded = response(&waiting, 1);
    let model = attempt(&mut store, &intent.id, "budget-model");
    store
        .ingest_receipt(&model.id, &recorded.checkpoint())
        .unwrap();
    let before = state(&mut store);
    let tools = before.apply_model(&recorded, "route-v1").unwrap();
    let applied = receipt_proposal(&mut store, "budget-tools", &model, &before, &tools);
    let tool_intent = applied.effects[0].clone();
    store
        .commit_turn_receipt(
            &applied,
            &receipt_binding(&model, 1, &intent, &recorded.checkpoint()),
        )
        .unwrap();
    let before = state(&mut store);
    let Effect::Tool { id, call } = before.pending_effects().unwrap().remove(0) else {
        panic!("tool");
    };
    let block = Block::ToolResult {
        tool_use_id: call.id,
        content: "done".into(),
        is_error: Some(false),
        metadata: Default::default(),
    };
    let receipt = serde_json::to_vec(&block).unwrap();
    let tool = attempt(&mut store, &id, "budget-tool");
    store.ingest_receipt(&tool.id, &receipt).unwrap();
    let after = before.apply_tool(&id, vec![block]).unwrap();
    let proposal = receipt_proposal(&mut store, "budget-tool-apply", &tool, &before, &after);
    store
        .commit_turn_receipt(
            &proposal,
            &receipt_binding(&tool, 2, &tool_intent, &receipt),
        )
        .unwrap();

    let before = state(&mut store);
    let summary = TranscriptCheckpoint {
        route_binding: "route-v1".into(),
        messages: vec![Message {
            role: "user".into(),
            content: vec![Block::Text {
                text: "[compacted] summary".into(),
            }],
        }],
    };
    let compact = Action::Compact {
        transcript: summary.checkpoint().unwrap(),
    };
    assert!(Action::decode(&compact.encode().unwrap()).unwrap() == compact);
    let compacted = before.compact(summary.clone()).unwrap();
    let proposal = action_proposal(&mut store, "compact", &compact, &before, &compacted);
    store.commit_turn_action(&proposal).unwrap();
    assert_eq!(state(&mut store).transcript().unwrap(), summary);

    let before = state(&mut store);
    let usage = Usage {
        input: 4_000,
        output: 1_000,
        ..Usage::default()
    };
    let under = Action::Budget {
        usd: None,
        tokens: Some(10_000),
        usage,
    };
    let refused = action_proposal(&mut store, "budget-under", &under, &before, &before);
    assert!(
        store.commit_turn_action(&refused).is_err(),
        "under-cap budget stop refused"
    );
    let action = Action::Budget {
        usd: None,
        tokens: Some(5_000),
        usage,
    };
    assert!(Action::decode(&action.encode().unwrap()).unwrap() == action);
    let stopped = before.stop_for_budget(None, Some(5_000), &usage).unwrap();
    let proposal = action_proposal(&mut store, "budget-stop", &action, &before, &stopped);
    store.commit_turn_action(&proposal).unwrap();
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    let head = state(&mut store);
    assert_eq!(head.control().phase(), Phase::Stopped(Stop::Budget));
    assert!(head.pending_effects().unwrap().is_empty());
    assert!(head.next_model_id().is_err());
}

#[test]
fn settled_child_answer_becomes_the_parent_tool_result_through_normal_adoption() {
    use spindle_store::ownership::ScopeKind;
    fn scoped(
        store: &mut Store,
        scope: &str,
        id: &str,
        observation: Observation,
        before: &TurnContinuation,
        after: &TurnContinuation,
    ) -> Proposal {
        Proposal {
            id: format!("{scope}-{id}"),
            scope: scope.into(),
            observation,
            expected_revision: store.snapshot_scope(scope).unwrap().revision,
            owner_epoch: store.epoch(),
            engine: ENGINE.into(),
            continuation: after.checkpoint().unwrap(),
            workspace: None,
            effects: new_intents(Some(before), after).unwrap(),
            state: None,
        }
    }
    fn begin(store: &mut Store, parent: Option<&str>, scope: &str) -> (Proposal, u64) {
        let initial = TurnContinuation::new(
            scope.into(),
            execution_id(scope, "start").unwrap(),
            Contract::OpenAiResponses,
            Some(4),
            empty_transcript(),
        )
        .unwrap();
        let prepared = request(&initial);
        let next = initial.prepare(prepared.clone(), "route-v1").unwrap();
        let action = Action::Start {
            initial: initial.checkpoint().unwrap(),
            request: ModelWait::new(prepared).checkpoint(),
        };
        let admission = store
            .admit_owned(
                parent,
                ScopeKind::Task,
                &Command {
                    scope: scope.into(),
                    id: "start".into(),
                    lineage: 0,
                    kind: ENGINE.into(),
                    payload: action.encode().unwrap(),
                },
            )
            .unwrap();
        let proposal = scoped(
            store,
            scope,
            "start",
            Observation::Admission(admission.sequence),
            &initial,
            &next,
        );
        let revision = store.commit_turn_action(&proposal).unwrap();
        (proposal, revision)
    }
    fn answer(
        store: &mut Store,
        scope: &str,
        start: &Proposal,
        revision: u64,
        blocks: Vec<Block>,
    ) -> u64 {
        let intent = start.effects[0].clone();
        let before =
            TurnContinuation::restore(&store.snapshot_scope(scope).unwrap().continuation).unwrap();
        let Effect::Model { waiting, .. } = before.pending_effects().unwrap().remove(0) else {
            panic!("model")
        };
        let recorded = response_blocks(&waiting, blocks);
        let model = attempt(store, &intent.id, &format!("{scope}-model"));
        store
            .ingest_receipt(&model.id, &recorded.checkpoint())
            .unwrap();
        let after = before.apply_model(&recorded, "route-v1").unwrap();
        let proposal = scoped(
            store,
            scope,
            "model",
            Observation::Receipt(model.id.clone()),
            &before,
            &after,
        );
        store
            .commit_turn_receipt(
                &proposal,
                &receipt_binding(&model, revision, &intent, &recorded.checkpoint()),
            )
            .unwrap()
    }

    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let (parent_start, revision) = begin(&mut store, None, "parent");
    let tool_revision = answer(
        &mut store,
        "parent",
        &parent_start,
        revision,
        vec![Block::ToolUse {
            id: "delegate-1".into(),
            name: "delegate".into(),
            input: serde_json::json!({"task": "summarize"}),
        }],
    );
    let parent =
        TurnContinuation::restore(&store.snapshot_scope("parent").unwrap().continuation).unwrap();
    let Effect::Tool {
        id: tool_effect, ..
    } = parent.pending_effects().unwrap().remove(0)
    else {
        panic!("tool")
    };
    let tool_intent = new_intents(None, &parent).unwrap().remove(0);
    let delegation = attempt(&mut store, &tool_effect, "parent-delegate");

    let (child_start, revision) = begin(&mut store, Some("parent"), "child");
    assert!(
        matches!(
            store.ingest_child_result(&delegation.id, "child"),
            Err(Error::Conflict)
        ),
        "child still running"
    );
    answer(
        &mut store,
        "child",
        &child_start,
        revision,
        vec![Block::Text {
            text: "child summary".into(),
        }],
    );
    let review =
        TurnContinuation::restore(&store.snapshot_scope("child").unwrap().continuation).unwrap();
    let done = review.finish().unwrap();
    let admission = store
        .admit(&Command {
            scope: "child".into(),
            id: "finish".into(),
            lineage: 0,
            kind: ENGINE.into(),
            payload: Action::Finish.encode().unwrap(),
        })
        .unwrap();
    let finish = scoped(
        &mut store,
        "child",
        "finish",
        Observation::Admission(admission.sequence),
        &review,
        &done,
    );
    store.commit_turn_action(&finish).unwrap();
    assert!(
        matches!(
            store.ingest_child_result(&delegation.id, "child"),
            Err(Error::Conflict)
        ),
        "not yet settled"
    );
    store.settle_owned("child").unwrap();

    assert!(
        matches!(
            store.ingest_child_result(&delegation.id, "parent"),
            Err(Error::Conflict)
        ),
        "not a child"
    );
    assert!(matches!(
        store.ingest_child_result("missing", "child"),
        Err(Error::Invalid)
    ));
    let bytes = store.ingest_child_result(&delegation.id, "child").unwrap();
    assert_eq!(
        store.ingest_child_result(&delegation.id, "child").unwrap(),
        bytes,
        "idempotent"
    );
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.receipt(&delegation.id).unwrap(), Some(bytes.clone()));

    let block: Block = serde_json::from_slice(&bytes).unwrap();
    assert!(matches!(
        &block,
        Block::ToolResult { tool_use_id, content, is_error: Some(false), .. }
            if tool_use_id == "delegate-1" && content == "child summary"
    ));
    let before =
        TurnContinuation::restore(&store.snapshot_scope("parent").unwrap().continuation).unwrap();
    let after = before.apply_tool(&tool_effect, vec![block]).unwrap();
    let proposal = scoped(
        &mut store,
        "parent",
        "delegated",
        Observation::Receipt(delegation.id.clone()),
        &before,
        &after,
    );
    store
        .commit_turn_receipt(
            &proposal,
            &receipt_binding(&delegation, tool_revision, &tool_intent, &bytes),
        )
        .unwrap();
    let head =
        TurnContinuation::restore(&store.snapshot_scope("parent").unwrap().continuation).unwrap();
    assert_eq!(head.control().phase(), Phase::AfterTools);
}

/// A run stopped by its own deadline or output bound is killed and discarded by the
/// executor, so it is recorded as a failed result over the unchanged base instead of
/// stranding the attempt; long output is truncated to the transcript bound.
#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn strict_bounds_record_contained_failures_and_long_output_is_truncated() {
    use spindle_store::broker::{DispatchOutcome, MAX_TOOL_TEXT};
    use std::sync::atomic::AtomicBool;
    let executor = strict_executor();
    for (command, expected) in [
        ("printf partial > input.txt; sleep 30", "deadline exceeded"),
        (
            "printf partial > input.txt; echo before; head -c 9000000 /dev/zero",
            "output limit exceeded",
        ),
        ("head -c 20000 /dev/zero | tr '\\0' x", "bytes omitted"),
    ] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let (tools, base) = prepared_workspace_tool(&mut store, command);
        let request = strict_dispatch_request(&store, &tools, &executor);
        let DispatchOutcome::Recorded(bytes) = store
            .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
            .unwrap()
        else {
            panic!("strict ingress")
        };
        let block: Block = serde_json::from_slice(&bytes).unwrap();
        let Block::ToolResult {
            content, is_error, ..
        } = &block
        else {
            panic!("tool result")
        };
        assert!(content.contains(expected), "{command}: {content}");
        assert!(content.len() <= MAX_TOOL_TEXT + 64);
        let truncated = expected == "bytes omitted";
        if !truncated {
            // The closing line survives truncation, and output before the kill is kept.
            assert!(
                content.ends_with(spindle_store::broker::KILLED_RUN),
                "{command}"
            );
            assert!(content.starts_with("before") || !command.contains("before"));
        }
        if truncated {
            let Block::ToolResult { metadata, .. } = &block else {
                unreachable!()
            };
            let artifact = metadata.artifact.clone().expect("full output retained");
            let page = store
                .retained_output("operator", &artifact, 9000, 32)
                .unwrap();
            assert!(page.starts_with(&"x".repeat(32)) && page.contains("9032/20000"));
            assert!(store.retained_output("other", &artifact, 0, 32).is_err());
            assert!(
                store.retained_output("operator", &base, 0, 32).is_err(),
                "workspace is not tool output"
            );
            assert!(
                store
                    .retained_output("operator", &artifact, 20001, 32)
                    .is_err()
            );
        }
        assert_eq!(*is_error, Some(!truncated));
        let root = store
            .workspace_receipt(&request.attempt.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            root, base,
            "contained runs and unchanged trees keep the base"
        );
        let before = state(&mut store);
        let after = before.apply_tool(&request.intent.id, vec![block]).unwrap();
        let mut proposal = receipt_proposal(&mut store, "apply", &request.attempt, &before, &after);
        proposal.workspace = Some(root.clone());
        store
            .commit_turn_workspace_receipt(
                &proposal,
                &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
                *executor.helper_digest(),
            )
            .unwrap();
        assert_eq!(
            store
                .read_versioned_file("operator", &root, "input.txt")
                .unwrap(),
            b"old"
        );
        drop(store);
        Store::open(&realm.0).expect("retained output survives validation/reopen");
    }
}

#[cfg(all(target_os = "linux", feature = "dext-strict-workspace"))]
#[test]
fn output_windows_are_selectable_and_unicode_pages_always_advance() {
    use spindle_store::broker::{DispatchOutcome, strict_tool_refusal};
    use std::sync::atomic::AtomicBool;
    let executor = strict_executor();
    for cap in [4096, 32768] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let (tools, _) = prepared_workspace_call(
            &mut store,
            "run_code",
            serde_json::json!({
                "language":"python", "code":"print('🙂' * 10000)", "max_output":cap,
            }),
        );
        let request = strict_dispatch_request(&store, &tools, &executor);
        let DispatchOutcome::Recorded(bytes) = store
            .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
            .unwrap()
        else {
            panic!("fresh receipt")
        };
        let Block::ToolResult {
            content,
            metadata,
            is_error,
            ..
        } = serde_json::from_slice::<Block>(&bytes).unwrap()
        else {
            panic!("tool result")
        };
        assert_eq!(is_error, Some(false));
        assert!(content.len() <= cap);
        assert!(content.len() > cap - 512);
        let artifact = metadata.artifact.unwrap();
        let page = store.retained_output("operator", &artifact, 1, 1).unwrap();
        assert!(page.starts_with('🙂') && page.contains("next offset=4"));
    }
    for cap in [0, 1000, 4095, 32769] {
        assert!(
            strict_tool_refusal(
                "run_code",
                &serde_json::json!({"language":"python","code":"print(1)","max_output":cap})
            )
            .is_none(),
            "max_output {cap} is clamped, not refused"
        );
    }
    for (name, input, fragment) in [
        (
            "run_code",
            serde_json::json!({"language":"python","code":"1","max_output":"big"}),
            "max_output must be a whole number",
        ),
        (
            "bash",
            serde_json::json!({"command":"ls","max_output":-1}),
            "max_output must be a whole number",
        ),
        (
            "run_code",
            serde_json::json!({"language":"python","code":"1","images":[]}),
            "unknown field `images`",
        ),
        (
            "bash",
            serde_json::json!({"command":"ls","timeout":0}),
            "timeout must be 1-1800",
        ),
        (
            "run_code",
            serde_json::json!({"language":"ruby","code":"1"}),
            "language must be",
        ),
    ] {
        let refusal = strict_tool_refusal(name, &input).expect("refused");
        assert!(refusal.contains(fragment), "{refusal}");
    }
}

/// A strict attempt reserved by a dead owner is resolved without running anything:
/// it becomes an interrupted error over the unchanged base. Live attempts and
/// already-recorded attempts are never sealed this way.
#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn interrupted_strict_attempt_from_a_dead_owner_is_sealed_as_not_completed() {
    use spindle_store::broker::DispatchOutcome;
    use std::sync::atomic::AtomicBool;
    let executor = strict_executor();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let (tools, base) = prepared_workspace_tool(&mut store, "printf changed > input.txt");
    let mut request = strict_dispatch_request(&store, &tools, &executor);
    store.reserve_attempt(&request.attempt).unwrap();
    assert_eq!(
        store.attempt_for(&request.intent.id).unwrap(),
        Some(request.attempt.clone())
    );
    assert_eq!(
        store.intent_revision(&request.intent.id).unwrap(),
        Some(request.intent_revision)
    );
    assert!(matches!(
        store.seal_interrupted_strict(&request, &executor),
        Err(Error::Conflict)
    ));
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    request.owner_epoch = store.epoch();
    assert!(matches!(
        store
            .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
            .unwrap(),
        DispatchOutcome::Reconcile
    ));
    assert!(matches!(
        store.seal_interrupted_strict_with_digest(&request, [0; 32]),
        Err(Error::Conflict)
    ));
    let bytes = store
        .seal_interrupted_strict_with_digest(&request, *executor.helper_digest())
        .unwrap();
    assert!(matches!(
        store.seal_interrupted_strict(&request, &executor),
        Err(Error::Conflict)
    ));
    let block: Block = serde_json::from_slice(&bytes).unwrap();
    assert!(
        matches!(&block, Block::ToolResult { is_error: Some(true), content, .. }
        if content.contains("did not complete"))
    );
    assert_eq!(
        store.workspace_receipt(&request.attempt.id).unwrap(),
        Some(base.clone())
    );
    assert!(matches!(
        store
            .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
            .unwrap(),
        DispatchOutcome::Reused(saved) if saved == bytes
    ));
    let before = state(&mut store);
    let after = before.apply_tool(&request.intent.id, vec![block]).unwrap();
    let mut proposal = receipt_proposal(&mut store, "sealed", &request.attempt, &before, &after);
    proposal.workspace = Some(base.clone());
    store
        .commit_turn_workspace_receipt(
            &proposal,
            &receipt_binding(
                &request.attempt,
                request.intent_revision,
                &request.intent,
                &bytes,
            ),
            *executor.helper_digest(),
        )
        .unwrap();
    assert_eq!(state(&mut store).control().phase(), Phase::AfterTools);
    assert_eq!(store.snapshot().unwrap().workspace, Some(base));
    assert!(
        store
            .reconciliation()
            .unwrap()
            .unresolved_attempts
            .is_empty()
    );
}

/// Host refusal of a call it will not dispatch is a committed error result. It is
/// refused once the call has been reserved, like a guard verdict.
#[cfg(feature = "dext-strict-workspace")]
#[test]
fn refused_tool_call_commits_an_error_result_and_never_dispatches() {
    use spindle_store::broker::strict_tool_refusal;
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    assert_eq!(store.lineage().unwrap(), 0);
    let prepared = start(&mut store, "refuse", empty_transcript());
    store.commit_turn_action(&prepared).unwrap();
    let input = serde_json::json!({"code": "print(1)"});
    let tools = answer_model(
        &mut store,
        "refuse-model",
        &prepared.effects[0],
        vec![
            Block::ToolUse {
                id: "python-call".into(),
                name: "python".into(),
                input: input.clone(),
            },
            Block::ToolUse {
                id: "bash-call".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "true", "timeout": dext_native::strict::MAX_RUN_SECS + 1}),
            },
        ],
        "refuse-tools",
    );
    assert!(
        strict_tool_refusal("python", &input)
            .unwrap()
            .contains("unknown tool")
    );
    assert!(
        strict_tool_refusal(
            "bash",
            &serde_json::json!({"command": "true", "timeout": dext_native::strict::MAX_RUN_SECS + 1})
        )
        .is_some()
    );
    assert!(strict_tool_refusal("bash", &serde_json::json!({"command": "ls"})).is_none());
    let reserved = tools.effects[1].id.clone();
    attempt(&mut store, &reserved, "reserved-bash");
    for (effect, label) in [(tools.effects[0].id.clone(), "python"), (reserved, "bash")] {
        let before = state(&mut store);
        let reason = format!("refused {label}");
        let after = before.refuse_tool(&effect, &reason).unwrap();
        let action = Action::Refuse {
            effect: effect.clone(),
            reason,
        };
        assert_eq!(Action::decode(&action.encode().unwrap()).unwrap(), action);
        let proposal = action_proposal(
            &mut store,
            &format!("refuse-{label}"),
            &action,
            &before,
            &after,
        );
        let result = store.commit_turn_action(&proposal);
        if label == "python" {
            result.unwrap();
        } else {
            assert!(matches!(result, Err(Error::Conflict)));
        }
    }
    let state = state(&mut store);
    assert_eq!(state.control().phase(), Phase::ToolsWait);
    assert!(state.transcript().unwrap().messages.iter().all(|message| {
        message.content.iter().all(|block| {
            !matches!(block,
            Block::ToolResult { tool_use_id, .. } if tool_use_id == "bash-call")
        })
    }));
    assert_eq!(state.pending_effects().unwrap().len(), 1);
    assert_eq!(
        Action::decode(&Action::Extend.encode().unwrap()).unwrap(),
        Action::Extend
    );
    assert!(
        state.extend_steps().is_err(),
        "only a step-limit stop extends"
    );
}

#[cfg(feature = "dext-strict-workspace")]
#[test]
#[cfg(target_os = "linux")]
fn file_tools_seal_workspace_receipts_and_failures_keep_the_base() {
    use spindle_store::broker::{DispatchOutcome, strict_tool_refusal};
    use std::sync::atomic::AtomicBool;
    let executor = strict_executor();
    use serde_json::json;
    // (tool, input, expected success, expected content fragment, expected file after)
    type Case = (
        &'static str,
        serde_json::Value,
        bool,
        &'static str,
        Option<(&'static str, &'static [u8])>,
    );
    let cases: Vec<Case> = vec![
        (
            "read_file",
            json!({"path": "input.txt"}),
            true,
            "     1\told",
            None,
        ),
        (
            "read_file",
            json!({"path": "/workspace/missing.txt"}),
            false,
            "no file `missing.txt`",
            None,
        ),
        (
            "read_file",
            json!({"path": "input.txt", "offset": 5}),
            false,
            "past the end",
            None,
        ),
        (
            "write_file",
            json!({"path": "src/new.txt", "content": "fresh"}),
            true,
            "Created src/new.txt",
            Some(("src/new.txt", b"fresh")),
        ),
        (
            "write_file",
            json!({"path": "./input.txt", "content": "over"}),
            true,
            "Overwrote input.txt",
            Some(("input.txt", b"over")),
        ),
        (
            "write_file",
            json!({"path": "input.txt/x", "content": "x"}),
            false,
            "cannot be added",
            None,
        ),
        (
            "write_file",
            json!({"path": "INPUT.txt", "content": "x"}),
            true,
            "Created INPUT.txt",
            Some(("INPUT.txt", b"x")),
        ),
        (
            "write_file",
            json!({"path": "docs/with space@1+2.txt", "content": "linux names"}),
            true,
            "Created docs/with space@1+2.txt",
            Some(("docs/with space@1+2.txt", b"linux names")),
        ),
        (
            "write_file",
            json!({"path": ".git/config", "content": "[core]"}),
            true,
            "Created .git/config",
            Some((".git/config", b"[core]")),
        ),
        (
            "read_file",
            json!({"path": "input.txt", "max_output": 1_000_000}),
            true,
            "     1\told",
            None,
        ),
        (
            "edit_file",
            json!({"path": "input.txt", "old_string": "ol", "new_string": "bo"}),
            true,
            "1 replacement",
            Some(("input.txt", b"bod")),
        ),
        (
            "edit_file",
            json!({"path": "input.txt", "old_string": "zz", "new_string": "y"}),
            false,
            "not found",
            None,
        ),
        (
            "edit_file",
            json!({"path": "Input.txt", "old_string": "o", "new_string": "y"}),
            false,
            "did you mean `input.txt`",
            None,
        ),
    ];
    for (index, (name, input, success, fragment, file)) in cases.into_iter().enumerate() {
        assert!(
            strict_tool_refusal(name, &input).is_none(),
            "{name} {input}"
        );
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let (tools, base) = prepared_workspace_call(&mut store, name, input.clone());
        let request = strict_dispatch_request(&store, &tools, &executor);
        let stages = |store: &mut Store| store.capacity().unwrap()[2].1;
        let stages_before = stages(&mut store);
        let DispatchOutcome::Recorded(bytes) = store
            .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
            .unwrap()
        else {
            panic!("file tool ingress {index}")
        };
        if name == "read_file" {
            assert_eq!(stages(&mut store), stages_before, "reads stage nothing");
        }
        let block: Block = serde_json::from_slice(&bytes).unwrap();
        let Block::ToolResult {
            content, is_error, ..
        } = &block
        else {
            panic!("tool result")
        };
        assert_eq!(*is_error, Some(!success), "{index}: {content}");
        assert!(content.contains(fragment), "{index}: {content}");
        let root = store
            .workspace_receipt(&request.attempt.id)
            .unwrap()
            .unwrap();
        match file {
            Some((path, expected)) => {
                assert_ne!(root, base);
                assert_eq!(
                    store.read_versioned_file("operator", &root, path).unwrap(),
                    expected
                );
            }
            None => assert_eq!(root, base, "{index}: reads and failures keep the base"),
        }
        let before = state(&mut store);
        let after = before.apply_tool(&request.intent.id, vec![block]).unwrap();
        let mut proposal =
            receipt_proposal(&mut store, "file-apply", &request.attempt, &before, &after);
        proposal.workspace = Some(root.clone());
        store
            .commit_turn_workspace_receipt(
                &proposal,
                &receipt_binding(&request.attempt, 2, &request.intent, &bytes),
                *executor.helper_digest(),
            )
            .unwrap();
        assert_eq!(
            store.snapshot().unwrap().workspace.as_deref(),
            Some(root.as_str())
        );
    }
    for (name, input) in [
        ("python", json!({"code": "1"})),
        ("read_file", json!({"path": "../etc/passwd"})),
        ("read_file", json!({"path": "a//b"})),
        ("read_file", json!({"path": "a/./b"})),
        ("read_file", json!({"path": "x", "limit": 0})),
        (
            "write_file",
            json!({"path": "x", "content": "a".repeat(spindle_store::broker::MAX_FILE_TOOL_TEXT + 1)}),
        ),
        (
            "write_file",
            json!({"path": "x", "content": "a", "mode": 7}),
        ),
        (
            "edit_file",
            json!({"path": "x", "old_string": "", "new_string": "a"}),
        ),
        (
            "edit_file",
            json!({"path": "x", "old_string": "a", "new_string": "b", "replace_all": "yes"}),
        ),
    ] {
        assert!(
            strict_tool_refusal(name, &input).is_some(),
            "{name} {input}"
        );
    }
}

/// One run_code script does several workspace operations in one step; a failing
/// script keeps none of its changes, and only its printed output is the result.
#[cfg(all(target_os = "linux", feature = "dext-strict-workspace"))]
#[test]
fn run_code_is_one_step_and_all_or_nothing() {
    use serde_json::json;
    use spindle_store::broker::{DispatchOutcome, strict_tool_refusal};
    use std::sync::atomic::AtomicBool;
    let executor = strict_executor();
    let cases = [
        (
            json!({"language": "python", "code": "write('a/b.txt', read('input.txt') + '!')\nedit('input.txt', 'old', 'new')\nprint(ls(), sh('cat a/b.txt'))"}),
            true,
            "['a/b.txt', 'input.txt'] old!",
            Some(("a/b.txt", b"old!" as &[u8])),
        ),
        (
            json!({"language": "javascript", "code": "write('c.txt', read('input.txt').toUpperCase()); console.log(ls().length, await Promise.resolve('ok'))"}),
            true,
            "2 ok",
            Some(("c.txt", b"OLD")),
        ),
        (
            json!({"language": "python", "code": "write('kept.txt', 'x')\nedit('input.txt', 'zz', 'y')"}),
            false,
            "none of its workspace changes were kept",
            None,
        ),
        (
            json!({"language": "python", "code": "write('kept.txt', 'x')\nsh('false | cat')"}),
            false,
            "sh exited 1",
            None,
        ),
        (
            json!({"language": "javascript", "code": "write('kept.txt','x'); sh('false | cat')"}),
            false,
            "sh exited 1",
            None,
        ),
        (
            json!({"language": "javascript", "code": "write('edit.txt','a a a'); const n=edit('edit.txt','a','$&',2); if(n!==2 || read('edit.txt')!=='$& $& a') throw Error('count or literal replacement'); console.log('literal edit ok')"}),
            true,
            "literal edit ok",
            Some(("edit.txt", b"$& $& a")),
        ),
        (
            json!({"language": "python", "code": "write('edit.txt','a a a')\nassert edit('edit.txt','a','$&',9)==3\nprint('literal edit ok')"}),
            true,
            "literal edit ok",
            Some(("edit.txt", b"$& $& $&")),
        ),
        (
            json!({"language": "javascript", "code": "write('kept.txt', 'x'); throw new Error('boom')"}),
            false,
            "boom",
            None,
        ),
        (
            json!({"language": "python", "code": "write('twice.py', 'import spindle\\ndef twice(p):\\n    return spindle.read(p) * 2\\n')\nimport twice\nprint('helper', twice.twice('input.txt'))"}),
            true,
            "helper oldold",
            Some((
                "twice.py",
                b"import spindle\ndef twice(p):\n    return spindle.read(p) * 2\n" as &[u8],
            )),
        ),
        (
            json!({"language": "javascript", "code": "write('.spindle/helpers/twice.js', 'module.exports = (p) => globalThis.spindle.read(p).repeat(2)'); console.log('helper', require('twice')('input.txt'))"}),
            true,
            "helper oldold",
            Some((
                ".spindle/helpers/twice.js",
                b"module.exports = (p) => globalThis.spindle.read(p).repeat(2)",
            )),
        ),
        (
            json!({"language": "python", "code": format!("# {}\ntry:\n    models(['a', 'b'])\nexcept RuntimeError as error:\n    print('batch refused', error)", "x".repeat(20 * 1024))}),
            true,
            "batch refused no external code policy granted",
            None,
        ),
    ];
    for (index, (input, success, fragment, file)) in cases.into_iter().enumerate() {
        assert!(strict_tool_refusal("run_code", &input).is_none(), "{input}");
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let (tools, base) = prepared_workspace_call(&mut store, "run_code", input);
        let request = strict_dispatch_request(&store, &tools, &executor);
        let DispatchOutcome::Recorded(bytes) = store
            .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
            .unwrap_or_else(|e| panic!("run_code dispatch {index}: {e:?}"))
        else {
            panic!("run_code ingress {index}")
        };
        let Block::ToolResult {
            content, is_error, ..
        } = serde_json::from_slice::<Block>(&bytes).unwrap()
        else {
            panic!("tool result")
        };
        assert_eq!(is_error, Some(!success), "{index}: {content}");
        assert!(content.contains(fragment), "{index}: {content}");
        let root = store
            .workspace_receipt(&request.attempt.id)
            .unwrap()
            .unwrap();
        match file {
            Some((path, expected)) => assert_eq!(
                store
                    .read_versioned_file("operator", &root, path)
                    .unwrap_or_else(|e| panic!("{index}: {path}: {e:?}: {content}")),
                expected
            ),
            None => assert_eq!(root, base, "{index}: a failed script keeps the base"),
        }
    }
    for input in [
        json!({"language": "ruby", "code": "1"}),
        json!({"language": "python", "code": " "}),
        json!({"language": "python", "code": "1", "timeout": dext_native::strict::MAX_RUN_SECS + 1}),
        json!({"language": "python", "code": "1", "extra": 1}),
    ] {
        assert!(strict_tool_refusal("run_code", &input).is_some(), "{input}");
    }
}

#[cfg(all(target_os = "linux", feature = "dext-strict-workspace"))]
#[test]
fn code_mode_brokers_calls_without_leaking_them_into_the_parent_transcript() {
    use dext_native::strict::RunOptions;
    use serde_json::{Value, json};
    use spindle_store::broker::DispatchOutcome;
    use spindle_store::code_mode::Host;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    struct Fixture {
        calls: usize,
    }
    impl Host for Fixture {
        fn discover(&mut self, _: &str, _: &Value) -> Result<Value, String> {
            Ok(json!([{"name":"fixture"}]))
        }
        fn prepare(&mut self, method: &str, input: &Value) -> Result<Value, String> {
            Ok(json!({"kind":method,"input":input,"budget":if method=="model" {100} else {0}}))
        }
        fn execute(&mut self, prepared: &Value, _: &AtomicBool, _: Duration) -> Value {
            self.calls += 1;
            json!({"ok":true,"data":if prepared["kind"]=="model" {json!({"text":"nested answer","usage":{"output":2}})} else {json!({"answer":42})}})
        }
    }
    for (language, code) in [
        (
            "python",
            "assert tools.search()[0]['name']=='fixture'\na=model('answer')\nb=tool('fixture',{'n':1})\nwrite('nested.txt',a['text'])\nprint(b['answer'])",
        ),
        (
            "javascript",
            "const image = 'scripts may shadow helper names'; if(tools.search()[0].name!=='fixture') throw Error('catalog'); const a=await model('answer'); const b=await tool('fixture',{n:1}); write('nested.txt',a.text); console.log(b.answer)",
        ),
        (
            "typescript",
            "interface Answer {answer:number}; const a:{text:string}=await model('answer'); const b:Answer=await tool('fixture',{n:1}); write('nested.txt',a.text); console.log(b.answer)",
        ),
    ] {
        if language == "typescript" && std::env::var_os("SPINDLE_TYPESCRIPT_DIR").is_none() {
            eprintln!(
                "TypeScript integration needs an explicitly installed SPINDLE_TYPESCRIPT_DIR; Python/JS still run"
            );
            continue;
        }
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let executor = strict_executor();
        let (tools, _) = prepared_workspace_call(
            &mut store,
            "run_code",
            json!({"language":language,"code":code}),
        );
        let request = strict_dispatch_request(&store, &tools, &executor);
        let mut host = Fixture { calls: 0 };
        let DispatchOutcome::Recorded(bytes) = store
            .dispatch_code_workspace(
                &request,
                &AtomicBool::new(false),
                &executor,
                RunOptions::default(),
                |_| Ok(()),
                Some(&mut host),
            )
            .unwrap()
        else {
            panic!("fresh receipt")
        };
        let block: Block = serde_json::from_slice(&bytes).unwrap();
        let Block::ToolResult {
            content, is_error, ..
        } = &block
        else {
            panic!("tool result")
        };
        assert_eq!(*is_error, Some(false), "{language}: {content}");
        assert_eq!(content.trim(), "42");
        assert_eq!(host.calls, 2);
        let root = store
            .workspace_receipt(&request.attempt.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .read_versioned_file("operator", &root, "nested.txt")
                .unwrap(),
            b"nested answer"
        );
        assert_eq!(store.code_receipts("operator").unwrap().len(), 4);
        assert!(matches!(
            store
                .dispatch_code_workspace(
                    &request,
                    &AtomicBool::new(false),
                    &executor,
                    RunOptions::default(),
                    |_| Ok(()),
                    Some(&mut host)
                )
                .unwrap(),
            DispatchOutcome::Reused(_)
        ));
        assert_eq!(host.calls, 2);
        drop(store);
        let mut store = Store::open(&realm.0).unwrap();
        assert_eq!(
            store.code_receipts("operator").unwrap().len(),
            4,
            "journal survives takeover"
        );
    }
}

#[cfg(all(target_os = "linux", feature = "dext-strict-workspace"))]
#[test]
fn code_mode_images_reach_the_host_intact_and_journal_as_digests() {
    use dext_native::strict::RunOptions;
    use serde_json::{Value, json};
    use spindle_store::broker::DispatchOutcome;
    use spindle_store::code_mode::Host;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    struct Fixture {
        seen: Vec<(String, usize)>,
    }
    impl Host for Fixture {
        fn discover(&mut self, _: &str, _: &Value) -> Result<Value, String> {
            Ok(json!([]))
        }
        fn prepare(&mut self, method: &str, input: &Value) -> Result<Value, String> {
            Ok(json!({"kind":method,"input":input,"budget":100}))
        }
        fn execute(&mut self, prepared: &Value, _: &AtomicBool, _: Duration) -> Value {
            let image = &prepared["input"]["images"][0];
            let data = image["data"].as_str().unwrap_or("");
            self.seen.push((
                image["media_type"].as_str().unwrap_or("").into(),
                data.len(),
            ));
            json!({"ok":true,"data":{"text":format!("{} {}", image["media_type"].as_str().unwrap_or(""), data.len())}})
        }
    }
    for (language, code) in [
        (
            "python",
            "import base64\nopen('/tmp/shot.png','wb').write(b'\\x89PNG\\r\\n\\x1a\\n' + b'x' * 900000)\nimg = image('/tmp/shot.png')\nr = model('describe', images=[img])\nassert r['text'] == f\"image/png {len(img['data'])}\", r\nprint('ok')",
        ),
        (
            "javascript",
            "console.log('start'); fs.writeFileSync('/tmp/shot.png', Buffer.concat([Buffer.from([0x89,0x50,0x4e,0x47,0x0d,0x0a,0x1a,0x0a]), Buffer.alloc(900000, 120)])); const img = image('/tmp/shot.png'); const r = await model('describe', {images:[img]}); if (r.text !== `image/png ${img.data.length}`) throw new Error(r.text); console.log('ok')",
        ),
    ] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let executor = strict_executor();
        let (tools, _) = prepared_workspace_call(
            &mut store,
            "run_code",
            json!({"language":language,"code":code}),
        );
        let request = strict_dispatch_request(&store, &tools, &executor);
        let mut host = Fixture { seen: vec![] };
        let DispatchOutcome::Recorded(bytes) = store
            .dispatch_code_workspace(
                &request,
                &AtomicBool::new(false),
                &executor,
                RunOptions::default(),
                |_| Ok(()),
                Some(&mut host),
            )
            .unwrap()
        else {
            panic!("fresh receipt")
        };
        let Block::ToolResult {
            content, is_error, ..
        } = serde_json::from_slice::<Block>(&bytes).unwrap()
        else {
            panic!("tool result")
        };
        assert_eq!(is_error, Some(false), "{language}: {content}");
        assert!(content.trim().ends_with("ok"), "{language}: {content}");
        assert_eq!(host.seen.len(), 1);
        assert_eq!(host.seen[0].0, "image/png");
        assert!(
            host.seen[0].1 > 1_100_000,
            "the full image reached the host"
        );
        for record in store.code_receipts("operator").unwrap() {
            assert!(
                serde_json::to_vec(&record).unwrap().len() < 16 * 1024,
                "journal keeps a digest, not the image: {language}"
            );
        }
    }
}

/// Without isolation the host runner keeps the durable contract: the run is recorded
/// with its own label, the sealed tree commits atomically with the continuation, the
/// store reopens cleanly and the receipt is reused instead of running again. A
/// receipt cannot be adopted under another runner's identity. On a host without bash
/// the call is recorded as a failed result over the unchanged base.
#[cfg(feature = "dext-strict-workspace")]
#[test]
fn host_runner_receipts_commit_reopen_and_reuse_like_sandboxed_ones() {
    use dext_native::strict::{Program, StrictExecutor, implementation_label};
    use spindle_store::broker::DispatchOutcome;
    use std::sync::atomic::AtomicBool;
    let host = StrictExecutor::host();
    let has_bash = host.program(Program::Bash).is_some();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let (tools, base) =
        prepared_workspace_tool(&mut store, "cat input.txt; printf new > input.txt");
    let mut request = strict_dispatch_request(&store, &tools, &host);
    assert!(
        request
            .attempt
            .implementation
            .starts_with("dext-host-bash-v1-")
    );
    let other = [7u8; 32];
    let mut mislabelled = strict_dispatch_request(&store, &tools, &host);
    mislabelled.attempt.implementation = implementation_label(&other);
    assert!(matches!(
        store.dispatch_strict_workspace(&mislabelled, &AtomicBool::new(false), &host, |_| Ok(())),
        Err(Error::Conflict)
    ));
    let DispatchOutcome::Recorded(bytes) = store
        .dispatch_strict_workspace(&request, &AtomicBool::new(false), &host, |_| Ok(()))
        .unwrap()
    else {
        panic!("host ingress")
    };
    let block: Block = serde_json::from_slice(&bytes).unwrap();
    let Block::ToolResult {
        content, is_error, ..
    } = &block
    else {
        panic!("tool result")
    };
    if has_bash {
        assert_eq!((content.as_str(), *is_error), ("old", Some(false)));
    } else {
        assert!(content.contains("bash is not available") && *is_error == Some(true));
    }
    let before = state(&mut store);
    let after = before.apply_tool(&request.intent.id, vec![block]).unwrap();
    let mut proposal =
        receipt_proposal(&mut store, "host-apply", &request.attempt, &before, &after);
    let root = store
        .workspace_receipt(&request.attempt.id)
        .unwrap()
        .unwrap();
    assert_eq!(root != base, has_bash);
    proposal.workspace = Some(root.clone());
    let binding = receipt_binding(&request.attempt, 2, &request.intent, &bytes);
    assert!(matches!(
        store.commit_turn_workspace_receipt(&proposal, &binding, other),
        Err(Error::Conflict)
    ));
    store
        .commit_turn_workspace_receipt(&proposal, &binding, *host.helper_digest())
        .unwrap();
    let expected: &[u8] = if has_bash { b"new" } else { b"old" };
    assert_eq!(
        store
            .read_versioned_file("operator", &root, "input.txt")
            .unwrap(),
        expected
    );
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    request.owner_epoch = store.epoch();
    assert!(matches!(
        store.dispatch_strict_workspace(&request, &AtomicBool::new(false), &host, |_| Ok(())).unwrap(),
        DispatchOutcome::Reused(saved) if saved == bytes
    ));
}

/// One prepared model response with several workspace tool calls, so they share an
/// execution (and its output budget); returns one request per call.
#[cfg(all(target_os = "linux", feature = "dext-strict-workspace"))]
fn prepared_workspace_calls(
    store: &mut Store,
    calls: Vec<(&str, serde_json::Value)>,
    executor: &dext_native::strict::StrictExecutor,
) -> (Vec<spindle_store::broker::DispatchRequest>, String) {
    let (first, rest) = calls.split_first().unwrap();
    let mut prepared = start(store, "workspace-turn", empty_transcript());
    let stage = spindle_store::artifact::Stage {
        id: "initial-workspace".into(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    store.begin_stage(&stage).unwrap();
    let object = store.put_object(&stage, b"old").unwrap();
    let root = store
        .stage_file_map(
            &stage,
            &[spindle_store::workspace::Entry {
                path: "input.txt".into(),
                object,
                executable: false,
            }],
        )
        .unwrap();
    prepared.workspace = Some(root.clone());
    store.commit_turn_workspace_start(&prepared).unwrap();
    let before = state(store);
    let Effect::Model { waiting, .. } = before.pending_effects().unwrap().remove(0) else {
        panic!("model")
    };
    let model_attempt = attempt(store, &prepared.effects[0].id, "model-attempt");
    let blocks = std::iter::once(first)
        .chain(rest)
        .enumerate()
        .map(|(index, (name, input))| Block::ToolUse {
            id: format!("workspace-call-{index}"),
            name: name.to_string(),
            input: input.clone(),
        })
        .collect();
    let recorded = response_blocks(&waiting, blocks);
    let bytes = recorded.checkpoint();
    store.ingest_receipt(&model_attempt.id, &bytes).unwrap();
    let after = before.apply_model(&recorded, "route-v1").unwrap();
    let tools = receipt_proposal(store, "workspace-tools", &model_attempt, &before, &after);
    store
        .commit_turn_receipt(
            &tools,
            &receipt_binding(&model_attempt, 1, &prepared.effects[0], &bytes),
        )
        .unwrap();
    let requests = tools
        .effects
        .iter()
        .enumerate()
        .map(|(index, intent)| {
            let mut request = strict_dispatch_request(store, &tools, executor);
            request.intent = intent.clone();
            request.attempt.effect = intent.id.clone();
            request.attempt.id = format!("strict-attempt-{index}");
            request
        })
        .collect();
    (requests, root)
}

/// Output budget: bash/run_code results share 256 KiB per prompt; paging tools are not
/// charged. Six 32 KiB results plus a 40 KB read_output page leave a full 32 KiB for
/// the next result (a charged page would have cut it), and read_file windows can be
/// far larger than the default.
#[cfg(all(target_os = "linux", feature = "dext-strict-workspace"))]
#[test]
fn output_budget_exempts_paging_tools_and_large_windows() {
    use sha2::{Digest, Sha256};
    use spindle_store::broker::{DispatchOutcome, TURN_OUTPUT_BUDGET};
    use std::sync::atomic::AtomicBool;
    let executor = strict_executor();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    // Retained outputs are content-addressed, so the page's artifact is known up front.
    let output = vec![b'x'; 40_000];
    let mut hash = Sha256::new();
    hash.update(b"spindle-object-v1\0");
    hash.update((output.len() as u64).to_be_bytes());
    hash.update(&output);
    let artifact: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let loud = serde_json::json!({"command":"head -c 40000 /dev/zero | tr '\\0' x","max_output":32768,"timeout":5});
    let charged = 6;
    assert!(charged * 32768 + 40_000 > TURN_OUTPUT_BUDGET - 32768);
    let mut calls: Vec<(&str, serde_json::Value)> = vec![("bash", loud.clone()); charged];
    calls.push((
        "read_output",
        serde_json::json!({"artifact":artifact,"limit":200000}),
    ));
    calls.push(("bash", loud));
    let (requests, _) = prepared_workspace_calls(&mut store, calls, &executor);
    let mut results = vec![];
    for request in &requests {
        let DispatchOutcome::Recorded(bytes) = store
            .dispatch_strict_workspace(request, &AtomicBool::new(false), &executor, |_| Ok(()))
            .unwrap()
        else {
            panic!("fresh receipt")
        };
        let Block::ToolResult { content, .. } = serde_json::from_slice::<Block>(&bytes).unwrap()
        else {
            panic!("tool result")
        };
        results.push(content);
    }
    assert!(
        results[charged].starts_with(&"x".repeat(40_000)),
        "one 40 KB page: {}",
        &results[charged][..100]
    );
    assert!(
        results[charged + 1].len() > 32_000,
        "the page was not charged: {}",
        results[charged + 1].len()
    );
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let (requests, _) = prepared_workspace_calls(
        &mut store,
        vec![(
            "read_file",
            serde_json::json!({"path":"input.txt","max_output":1_000_000}),
        )],
        &executor,
    );
    assert!(matches!(
        store.dispatch_strict_workspace(&requests[0], &AtomicBool::new(false), &executor, |_| Ok(
            ()
        )),
        Ok(DispatchOutcome::Recorded(_))
    ));
}

/// Garbage collection frees objects of a resolved strict attempt's stage that nothing
/// reaches, but keeps its receipt's workspace root and retained output readable.
#[cfg(all(target_os = "linux", feature = "dext-strict-workspace"))]
#[test]
fn garbage_collection_frees_unreachable_strict_objects_but_keeps_roots_and_outputs() {
    use spindle_store::broker::DispatchOutcome;
    use std::sync::atomic::AtomicBool;
    let executor = strict_executor();
    let realm = Realm::new();
    let mut store = Store::open(&realm.0).unwrap();
    let (tools, _) = prepared_workspace_tool(
        &mut store,
        "printf new > input.txt; head -c 20000 /dev/zero | tr '\\0' y",
    );
    let request = strict_dispatch_request(&store, &tools, &executor);
    let DispatchOutcome::Recorded(bytes) = store
        .dispatch_strict_workspace(&request, &AtomicBool::new(false), &executor, |_| Ok(()))
        .unwrap()
    else {
        panic!("fresh receipt")
    };
    let Block::ToolResult { metadata, .. } = serde_json::from_slice::<Block>(&bytes).unwrap()
    else {
        panic!("tool result")
    };
    let artifact = metadata.artifact.expect("retained output");
    let root = store
        .workspace_receipt(&request.attempt.id)
        .unwrap()
        .unwrap();
    let stage = spindle_store::artifact::Stage {
        id: request.attempt.id.clone(),
        scope: "operator".into(),
        owner_epoch: store.epoch(),
    };
    let garbage = store.put_object(&stage, b"reachable from nothing").unwrap();
    let objects = |store: &mut Store| store.capacity().unwrap()[4].1;
    let before = objects(&mut store);
    let collected = store.collect_garbage().unwrap();
    assert_eq!(collected.objects, 1);
    assert_eq!(objects(&mut store), before - 1);
    assert!(store.read_object("operator", &garbage).is_err());
    assert_eq!(
        store
            .read_versioned_file("operator", &root, "input.txt")
            .unwrap(),
        b"new"
    );
    assert!(
        store
            .retained_output("operator", &artifact, 0, 16)
            .unwrap()
            .starts_with("yyyyyyyyyyyyyyyy")
    );
    assert_eq!(store.collect_garbage().unwrap().objects, 0);
    drop(store);
    let mut store = Store::open(&realm.0).unwrap();
    assert!(store.retained_output("operator", &artifact, 0, 16).is_ok());
}

/// Code-mode requests travel on their own descriptors with sequence numbers: printed
/// 0x1E bytes stay ordinary output, threads each get their own reply, and sh() never
/// reads the channel (it gets no stdin).
#[cfg(all(target_os = "linux", feature = "dext-strict-workspace"))]
#[test]
fn code_mode_channel_is_separate_from_output_and_safe_for_threads() {
    use dext_native::strict::RunOptions;
    use serde_json::{Value, json};
    use spindle_store::broker::DispatchOutcome;
    use spindle_store::code_mode::Host;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    struct Echo;
    impl Host for Echo {
        fn discover(&mut self, _: &str, _: &Value) -> Result<Value, String> {
            Ok(json!([]))
        }
        fn prepare(&mut self, method: &str, input: &Value) -> Result<Value, String> {
            Ok(json!({"kind":method,"input":input,"budget":10}))
        }
        fn execute(&mut self, prepared: &Value, _: &AtomicBool, _: Duration) -> Value {
            json!({"ok":true,"data":{"text":prepared["input"]["prompt"]}})
        }
    }
    for (language, code) in [
        (
            "python",
            "import threading\nprint('\\x1e{\"id\":0,\"method\":\"model\"}')\nanswers={}\ndef ask(n):\n    answers[n]=model(f'p{n}')['text']\nthreads=[threading.Thread(target=ask,args=(n,)) for n in range(6)]\n[t.start() for t in threads]; [t.join() for t in threads]\nassert answers=={n:f'p{n}' for n in range(6)}, answers\nassert sh('cat')==''\nprint('ok')",
        ),
        (
            "javascript",
            "console.log('\\x1e{\"id\":0}'); const a = await model('first'); if (a.text !== 'first') throw Error(a.text); if (sh('cat') !== '') throw Error('stdin'); console.log('ok')",
        ),
    ] {
        let realm = Realm::new();
        let mut store = Store::open(&realm.0).unwrap();
        let executor = strict_executor();
        let (tools, _) = prepared_workspace_call(
            &mut store,
            "run_code",
            json!({"language":language,"code":code,"timeout":20}),
        );
        let request = strict_dispatch_request(&store, &tools, &executor);
        let DispatchOutcome::Recorded(bytes) = store
            .dispatch_code_workspace(
                &request,
                &AtomicBool::new(false),
                &executor,
                RunOptions::default(),
                |_| Ok(()),
                Some(&mut Echo),
            )
            .unwrap()
        else {
            panic!("fresh receipt")
        };
        let Block::ToolResult {
            content, is_error, ..
        } = serde_json::from_slice::<Block>(&bytes).unwrap()
        else {
            panic!("tool result")
        };
        assert_eq!(is_error, Some(false), "{language}: {content}");
        assert!(
            content.starts_with("\u{1e}{\"id\":0"),
            "{language}: {content}"
        );
        assert!(content.trim_end().ends_with("ok"), "{language}: {content}");
    }
}
