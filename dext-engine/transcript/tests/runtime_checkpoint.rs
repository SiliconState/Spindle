use dext_transcript::checkpoint::Error;
use dext_transcript::runtime::{
    ExternalOutcomeInput, PhaseTrigger, TurnRuntimeState, WorkPhase, normalize_bash_similarity_key,
};

fn observe(
    state: &mut TurnRuntimeState,
    tool: &str,
    hosts: &[&str],
    cache_key: Option<&str>,
    content: &str,
    is_error: Option<bool>,
) {
    let hosts: Vec<String> = hosts.iter().map(|h| h.to_string()).collect();
    let similarity = normalize_bash_similarity_key(content);
    let mut content = content.to_string();
    state.record_external_outcome(ExternalOutcomeInput {
        tool_name: tool,
        hosts: &hosts,
        cache_key,
        bash_similarity_key: Some(&similarity),
        command: Some("curl"),
        content: &mut content,
        is_error,
    });
}

fn busy_state(order: &[&str]) -> TurnRuntimeState {
    let mut state = TurnRuntimeState::new();
    state.advance_phase(PhaseTrigger::ScaleCollection);
    for host in order {
        observe(&mut state, "http", &[host], Some(host), "ok payload", None);
    }
    for _ in 0..2 {
        observe(
            &mut state,
            "bash",
            &["auth.example"],
            None,
            "HTTP 401 unauthorized",
            Some(true),
        );
    }
    state.mark_mutation_succeeded();
    for tool in order {
        state.tool_retry_guard(tool, "same failure");
    }
    state.record_empty_tool_calls(1);
    state.record_http_retry();
    state
}

#[test]
fn runtime_checkpoint_is_canonical_and_round_trips_decisions() {
    let a = busy_state(&["a.example", "b.example", "c.example", "d.example"]);
    let b = busy_state(&["d.example", "c.example", "b.example", "a.example"]);
    let bytes = a.checkpoint().unwrap();
    assert!(bytes.starts_with(b"DRS1"));
    assert_eq!(bytes, b.checkpoint().unwrap(), "insertion order leaked");

    let mut restored = TurnRuntimeState::restore(&bytes).unwrap();
    assert_eq!(restored.checkpoint().unwrap(), bytes);
    assert_eq!(restored.phase(), WorkPhase::Scale);
    assert_eq!(restored.telemetry(), a.telemetry());
    let blocked = vec!["auth.example".to_string()];
    assert!(restored.blocked_host_guard(&blocked).is_some());
    assert!(
        restored
            .feasibility_guard(&["a.example".into()], true)
            .is_none()
    );
    assert!(
        restored
            .feasibility_guard(&["new.example".into()], true)
            .is_some()
    );
    let hit = restored.dedupe_guard(Some("b.example")).unwrap();
    assert!(hit.0.contains("ok payload"));
    assert_eq!(
        restored.telemetry().dedupe_hits,
        a.telemetry().dedupe_hits + 1
    );
}

#[test]
fn restored_retry_budgets_and_streaks_continue_instead_of_resetting() {
    let mut live = TurnRuntimeState::new();
    for _ in 0..3 {
        assert!(live.tool_retry_guard("bash", "boom").is_none());
    }
    for _ in 0..3 {
        live.record_empty_tool_calls(1);
    }
    let mut restored = TurnRuntimeState::restore(&live.checkpoint().unwrap()).unwrap();
    assert!(restored.tool_retry_guard("bash", "boom").is_some());
    assert!(live.tool_retry_guard("bash", "boom").is_some());
    restored.record_empty_tool_calls(1);
    live.record_empty_tool_calls(1);
    assert_eq!(
        restored.empty_tool_call_loop_note(),
        live.empty_tool_call_loop_note()
    );
    assert!(restored.empty_tool_call_loop_note().is_none());
    assert_eq!(restored.checkpoint().unwrap(), live.checkpoint().unwrap());
}

#[test]
fn similarity_history_survives_restore_and_mutation_epoch_scopes_it() {
    let mut live = TurnRuntimeState::new();
    let key = normalize_bash_similarity_key("curl https://x.example/a");
    for _ in 0..3 {
        observe(
            &mut live,
            "bash",
            &[],
            None,
            "curl https://x.example/a",
            Some(true),
        );
    }
    let mut restored = TurnRuntimeState::restore(&live.checkpoint().unwrap()).unwrap();
    assert!(restored.bash_similarity_guard(Some(&key)).is_some());
    restored.mark_mutation_succeeded();
    assert!(restored.bash_similarity_guard(Some(&key)).is_none());
}

fn valid_bytes() -> Vec<u8> {
    busy_state(&["a.example", "b.example"])
        .checkpoint()
        .unwrap()
}

fn mutate(from: &str, to: &str) -> Vec<u8> {
    let text = String::from_utf8(valid_bytes()).unwrap();
    assert!(text.contains(from), "fixture drift: {from}");
    text.replacen(from, to, 1).into_bytes()
}

#[test]
fn malformed_runtime_checkpoints_fail_closed() {
    let mut trailing = valid_bytes();
    trailing.push(b' ');
    assert_eq!(
        TurnRuntimeState::restore(&trailing).unwrap_err(),
        Error::Encoding
    );
    let mut wrong_magic = valid_bytes();
    wrong_magic[..4].copy_from_slice(b"DNT1");
    assert_eq!(
        TurnRuntimeState::restore(&wrong_magic).unwrap_err(),
        Error::Encoding
    );
    assert_eq!(
        TurnRuntimeState::restore(&mutate("{\"phase\"", "{\"grant\":true,\"phase\"")).unwrap_err(),
        Error::Encoding
    );

    // Non-canonical order and duplicate keys would let two encodings mean one state.
    assert_eq!(
        TurnRuntimeState::restore(&mutate(
            "\"host_probe_passed\":[\"a.example\",\"b.example\"]",
            "\"host_probe_passed\":[\"b.example\",\"a.example\"]"
        ))
        .unwrap_err(),
        Error::InvalidRuntime
    );
    assert_eq!(
        TurnRuntimeState::restore(&mutate(
            "\"host_probe_passed\":[\"a.example\",\"b.example\"]",
            "\"host_probe_passed\":[\"a.example\",\"a.example\"]"
        ))
        .unwrap_err(),
        Error::InvalidRuntime
    );
    // Unblocking a host that crossed the auth threshold would reopen a tripped breaker.
    assert_eq!(
        TurnRuntimeState::restore(&mutate(
            "\"blocked_hosts\":[\"auth.example\"]",
            "\"blocked_hosts\":[]"
        ))
        .unwrap_err(),
        Error::InvalidRuntime
    );
    // A history entry newer than the current epoch is impossible. Lowering the epoch
    // itself stays consistent; tamper detection is the host's integrity binding.
    assert_eq!(
        TurnRuntimeState::restore(&mutate(
            "[\"ok payload\",false,0]",
            "[\"ok payload\",false,2]"
        ))
        .unwrap_err(),
        Error::InvalidRuntime
    );
    assert_eq!(
        TurnRuntimeState::restore(&mutate(
            "\"empty_tool_call_note_emitted\":false",
            "\"empty_tool_call_note_emitted\":true"
        ))
        .unwrap_err(),
        Error::InvalidRuntime
    );
}

#[test]
fn oversized_runtime_state_refuses_instead_of_truncating() {
    let mut state = TurnRuntimeState::new();
    for index in 0..=dext_transcript::runtime::MAX_RUNTIME_ENTRIES {
        state.tool_retry_guard(&format!("tool{index}"), "x");
    }
    assert_eq!(state.checkpoint().unwrap_err(), Error::Bounds);

    let long = "k".repeat(5000);
    let mut state = TurnRuntimeState::new();
    state.tool_retry_guard(&long, "x");
    assert_eq!(state.checkpoint().unwrap_err(), Error::Bounds);
}
