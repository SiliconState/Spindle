use dext_core::model::{Encoding, Error, Identity, ModelWait, Phase, PreparedRequest};

fn request() -> PreparedRequest<Vec<u8>> {
    PreparedRequest::new(
        Identity {
            effect: "lineage-1/model-1".into(),
            provider: "test".into(),
            contract: "openai-responses".into(),
            endpoint: "https://example.invalid/v1/responses".into(),
            model: "test-model".into(),
            credential_scope: "tenant-1/credential-version-2".into(),
            configuration: "effort=high;policy=3".into(),
        },
        Encoding::Json,
        br#"{"input":"hello","tools":[]}"#.to_vec(),
    )
    .unwrap()
}

#[test]
fn prepared_wait_recorded_applied_checkpoints_preserve_exact_values() {
    let prepared = ModelWait::new(request());
    assert_eq!(prepared.phase(), Phase::Prepared);
    assert!(matches!(prepared.apply(), Err(Error::State)));
    let waiting = prepared.start().unwrap();
    assert_eq!(
        prepared.phase(),
        Phase::Prepared,
        "speculation mutated baseline"
    );
    assert_eq!(waiting.attempt(), 1);
    assert!(matches!(waiting.start(), Err(Error::State)));
    let recorded = waiting
        .record(waiting.request(), 1, b"validated response")
        .unwrap();
    assert_eq!(recorded.response(), Some(b"validated response".as_slice()));
    assert_eq!(
        recorded
            .record(waiting.request(), 1, b"validated response")
            .unwrap(),
        recorded
    );
    assert!(matches!(
        recorded.record(waiting.request(), 1, b"different"),
        Err(Error::Conflict)
    ));
    let applied = recorded.apply().unwrap();
    assert_eq!(applied.apply().unwrap(), applied);
    for state in [prepared, waiting, recorded, applied] {
        let restored = ModelWait::restore(&state.checkpoint()).unwrap();
        assert_eq!(state, restored);
        assert_eq!(state.checkpoint(), restored.checkpoint());
        if matches!(state.phase(), Phase::Recorded | Phase::Applied) {
            assert!(matches!(restored.start(), Err(Error::State)));
            assert!(matches!(
                restored.abandon_unrecorded_for_retry(),
                Err(Error::State)
            ));
        }
    }
}

#[test]
fn unfinished_attempt_needs_explicit_retry_and_keeps_request_identity() {
    let waiting = ModelWait::new(request()).start().unwrap();
    let restored = ModelWait::restore(&waiting.checkpoint()).unwrap();
    assert!(matches!(restored.start(), Err(Error::State)));
    let prepared_retry = restored.abandon_unrecorded_for_retry().unwrap();
    assert_eq!(prepared_retry.attempt(), 1);
    let retried = ModelWait::restore(&prepared_retry.checkpoint())
        .unwrap()
        .start()
        .unwrap();
    assert_eq!(retried.attempt(), 2);
    assert!(waiting.request().matches(retried.request()));
    assert!(matches!(
        retried.record(waiting.request(), 1, b"late first response"),
        Err(Error::Conflict)
    ));
    assert!(
        retried
            .record(retried.request(), 2, b"second response")
            .is_ok()
    );
}

#[test]
fn operation_scope_config_route_and_body_all_bind_response_identity() {
    let waiting = ModelWait::new(request()).start().unwrap();
    for field in 0..7 {
        let mut identity = waiting.request().identity().clone();
        let target = match field {
            0 => &mut identity.effect,
            1 => &mut identity.provider,
            2 => &mut identity.contract,
            3 => &mut identity.endpoint,
            4 => &mut identity.model,
            5 => &mut identity.credential_scope,
            _ => &mut identity.configuration,
        };
        target.push_str("-changed");
        let other =
            PreparedRequest::new(identity, Encoding::Json, waiting.request().body().to_vec())
                .unwrap();
        assert!(matches!(
            waiting.record(&other, 1, b"response"),
            Err(Error::Conflict)
        ));
    }
    let body = PreparedRequest::new(
        waiting.request().identity().clone(),
        Encoding::Json,
        b"changed".to_vec(),
    )
    .unwrap();
    let gzip = PreparedRequest::new(
        waiting.request().identity().clone(),
        Encoding::Gzip,
        waiting.request().body().to_vec(),
    )
    .unwrap();
    for other in [body, gzip] {
        assert!(matches!(
            waiting.record(&other, 1, b"response"),
            Err(Error::Conflict)
        ));
    }
}

#[test]
fn repeated_prompt_is_not_a_global_response_cache_key() {
    let first = ModelWait::new(request()).start().unwrap();
    let recorded = first.record(first.request(), 1, b"first sample").unwrap();
    let mut identity = first.request().identity().clone();
    identity.effect = "lineage-1/model-2".into();
    let second =
        PreparedRequest::new(identity, Encoding::Json, first.request().body().to_vec()).unwrap();
    assert!(matches!(
        recorded.record(&second, 1, b"first sample"),
        Err(Error::Conflict)
    ));
    assert_eq!(
        ModelWait::new(second).start().unwrap().phase(),
        Phase::Waiting
    );
}

#[test]
fn decoding_rejects_truncation_trailing_versions_bounds_and_invalid_phases() {
    let waiting = ModelWait::new(request()).start().unwrap();
    let original = waiting.checkpoint();
    for length in 0..original.len() {
        assert!(
            ModelWait::restore(&original[..length]).is_err(),
            "accepted prefix {length}"
        );
    }
    let mut changed = original.clone();
    changed.push(0);
    assert!(matches!(ModelWait::restore(&changed), Err(Error::State)));
    let mut changed = original.clone();
    changed[3] = b'2';
    assert!(matches!(ModelWait::restore(&changed), Err(Error::Version)));
    let mut changed = original.clone();
    changed[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(ModelWait::restore(&changed), Err(Error::Bound)));
    // Waiting checkpoint ends with attempt:u32, phase:u8, empty response length:u32.
    let phase_offset = original.len() - 5;
    let mut changed = original.clone();
    changed[phase_offset] = 2;
    assert!(matches!(ModelWait::restore(&changed), Err(Error::State)));
    let mut changed = original.clone();
    changed[phase_offset - 4..phase_offset].fill(0);
    assert!(matches!(ModelWait::restore(&changed), Err(Error::State)));
    let mut changed = original.clone();
    changed[phase_offset - 4..phase_offset].fill(255);
    let max_attempt = ModelWait::restore(&changed).unwrap();
    assert!(matches!(
        max_attempt.abandon_unrecorded_for_retry().unwrap().start(),
        Err(Error::Bound)
    ));
}

#[test]
fn debug_omits_sensitive_body_response_and_route_values() {
    let mut identity = request().identity().clone();
    identity.endpoint = "https://example.invalid/private-route".into();
    identity.credential_scope = "private-scope".into();
    assert!(!format!("{identity:?}").contains("private-route"));
    assert!(!format!("{identity:?}").contains("private-scope"));
    let prepared =
        PreparedRequest::new(identity, Encoding::Json, b"sensitive request".to_vec()).unwrap();
    assert!(!format!("{prepared:?}").contains("sensitive"));
    assert!(!format!("{prepared:?}").contains("private-route"));
    let wait = ModelWait::new(prepared).start().unwrap();
    let recorded = wait
        .record(wait.request(), 1, b"sensitive response")
        .unwrap();
    assert!(!format!("{recorded:?}").contains("sensitive"));
}

#[test]
fn rejects_missing_identity_and_empty_payloads() {
    let mut identity = request().identity().clone();
    identity.credential_scope.clear();
    assert!(matches!(
        PreparedRequest::new(identity, Encoding::Json, vec![1]),
        Err(Error::Identity)
    ));
    assert!(matches!(
        PreparedRequest::new(
            request().identity().clone(),
            Encoding::Json,
            Vec::<u8>::new()
        ),
        Err(Error::Bound)
    ));
    let waiting = ModelWait::new(request()).start().unwrap();
    assert!(matches!(
        waiting.record(waiting.request(), 1, &[]),
        Err(Error::Bound)
    ));
}
