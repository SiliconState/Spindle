//! Historical reconstruction only: no executor, capability, or live dispatch API.
//! The caller supplies a trusted retained checkpoint; self-consistency does not
//! establish authenticity or detect rollback to another internally valid history.

use crate::{Authority, CommitId, Error, Fault, MAX_COMMITS, MODEL_SCHEMA, State};

pub const MODEL_RUNTIME: &str = "spindle-reference/lifecycle-v2";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplayError {
    Schema,
    Runtime,
    Bound,
    Sequence,
    Rejected { commit: CommitId, error: Error },
    Invariant(&'static str),
    Divergence,
}

/// Returns inspection data, never a live authority or permission to dispatch.
/// Runtime identity here is a symbolic model pin, not a production binary hash.
pub fn reconstruct(schema: u8, runtime: &str, expected: &State) -> Result<State, ReplayError> {
    if schema != MODEL_SCHEMA {
        return Err(ReplayError::Schema);
    }
    if runtime != MODEL_RUNTIME {
        return Err(ReplayError::Runtime);
    }
    if expected.commits.len() > MAX_COMMITS {
        return Err(ReplayError::Bound);
    }
    let mut ordered: Vec<_> = expected.commits.iter().collect();
    ordered.sort_by_key(|(_, commit)| commit.seq);
    let mut historical = Authority::default();
    for (index, (id, commit)) in ordered.into_iter().enumerate() {
        if usize::from(commit.seq) != index + 1 || usize::from(commit.expected_seq) != index {
            return Err(ReplayError::Sequence);
        }
        historical
            .commit(
                *id,
                commit.expected_seq,
                commit.operation.clone(),
                Fault::None,
            )
            .map_err(|error| ReplayError::Rejected { commit: *id, error })?;
        historical
            .state()
            .check_invariants()
            .map_err(ReplayError::Invariant)?;
    }
    if historical.state() != expected {
        return Err(ReplayError::Divergence);
    }
    Ok(historical.state().clone())
}
